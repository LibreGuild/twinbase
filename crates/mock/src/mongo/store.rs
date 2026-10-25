//! In-memory MongoDB databases with unique indexes and BSON snapshot persistence.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bson::{Bson, Document, doc, oid::ObjectId};
use indexmap::IndexMap;
use parking_lot::RwLock;

use super::agg::{self, apply_pipeline_update};
use super::query::{
    self, MResult, MongoError, apply_update, compare, is_update_operators, key_of, lookup, matches, project, sort_docs, upsert_seed,
    values_equal,
};

pub const MAX_DOC_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub key: Document,
    pub unique: bool,
    pub sparse: bool,
    /// Options echoed by `listIndexes` (e.g. `expireAfterSeconds`).
    pub extra: Document,
    /// Unique indexes only: index key → document `_id` key.
    entries: HashMap<Vec<u8>, Vec<u8>>,
}

impl Index {
    fn id_index() -> Self {
        Index { name: "_id_".into(), key: doc! { "_id": 1 }, unique: true, sparse: false, extra: Document::new(), entries: HashMap::new() }
    }

    /// The index key of a document, or `None` when a sparse index skips it.
    fn key_for(&self, d: &Document) -> Option<Vec<u8>> {
        let vals: Vec<Bson> = self.key.keys().map(|k| lookup(d, k).cloned().unwrap_or(Bson::Null)).collect();
        if self.sparse && self.key.keys().all(|k| lookup(d, k).is_none()) {
            return None;
        }
        Some(key_of(&Bson::Array(vals)))
    }

    pub fn describe(&self) -> Document {
        let mut out = doc! { "v": 2, "key": self.key.clone(), "name": self.name.clone() };
        if self.unique && self.name != "_id_" {
            out.insert("unique", true);
        }
        if self.sparse {
            out.insert("sparse", true);
        }
        out.extend(self.extra.clone());
        out
    }

    fn duplicate(&self, ns: &str, d: &Document) -> MongoError {
        let key_value: Document = self.key.keys().map(|k| (k.clone(), lookup(d, k).cloned().unwrap_or(Bson::Null))).collect();
        let shown = Bson::Document(key_value.clone()).into_relaxed_extjson().to_string();
        let mut e = MongoError::new(
            11000,
            "DuplicateKey",
            format!("E11000 duplicate key error collection: {ns} index: {} dup key: {shown}", self.name),
        );
        e.details = Some(Box::new(doc! { "keyPattern": self.key.clone(), "keyValue": key_value }));
        e
    }
}

#[derive(Debug, Clone, Default)]
pub struct Collection {
    pub docs: IndexMap<Vec<u8>, Document>,
    pub indexes: Vec<Index>,
    pub options: Document,
}

impl Collection {
    fn new(options: Document) -> Self {
        Collection { docs: IndexMap::new(), indexes: vec![Index::id_index()], options }
    }

    fn id_key(d: &Document) -> Vec<u8> {
        key_of(d.get("_id").unwrap_or(&Bson::Null))
    }

    /// Check unique indexes for `d` (ignoring the document `own` itself).
    fn check_unique(&self, ns: &str, d: &Document, own: Option<&[u8]>) -> MResult<()> {
        for ix in self.indexes.iter().filter(|i| i.unique && i.name != "_id_") {
            if let Some(k) = ix.key_for(d)
                && let Some(holder) = ix.entries.get(&k)
                && own != Some(holder.as_slice())
            {
                return Err(ix.duplicate(ns, d));
            }
        }
        Ok(())
    }

    fn index_add(&mut self, id: &[u8], d: &Document) {
        for ix in self.indexes.iter_mut().filter(|i| i.unique && i.name != "_id_") {
            if let Some(k) = ix.key_for(d) {
                ix.entries.insert(k, id.to_vec());
            }
        }
    }

    fn index_remove(&mut self, d: &Document) {
        for ix in self.indexes.iter_mut().filter(|i| i.unique && i.name != "_id_") {
            if let Some(k) = ix.key_for(d) {
                ix.entries.remove(&k);
            }
        }
    }

    fn insert(&mut self, ns: &str, d: Document) -> MResult<()> {
        let id = Self::id_key(&d);
        if self.docs.contains_key(&id) {
            return Err(Index::id_index().duplicate(ns, &d));
        }
        self.check_unique(ns, &d, None)?;
        self.index_add(&id, &d);
        self.docs.insert(id, d);
        Ok(())
    }

    fn replace(&mut self, ns: &str, id: &[u8], new: Document) -> MResult<()> {
        self.check_unique(ns, &new, Some(id))?;
        if let Some(old) = self.docs.get(id).cloned() {
            self.index_remove(&old);
        }
        self.index_add(id, &new);
        if let Some(slot) = self.docs.get_mut(id) {
            *slot = new;
        }
        Ok(())
    }

    fn remove(&mut self, id: &[u8]) -> Option<Document> {
        let d = self.docs.shift_remove(id)?;
        self.index_remove(&d);
        Some(d)
    }

    /// Ids of matching documents, in natural order. Uses the `_id` index for `_id` equality.
    fn matching(&self, filter: &Document, limit: usize) -> MResult<Vec<Vec<u8>>> {
        if let Some(id) = filter.get("_id").filter(|v| {
            !matches!(v, Bson::Document(d) if d.keys().next().is_some_and(|k| k.starts_with('$')))
                && !matches!(v, Bson::RegularExpression(_))
        }) {
            let k = key_of(id);
            return Ok(match self.docs.get(&k) {
                Some(d) if matches(d, filter)? => vec![k],
                _ => vec![],
            });
        }
        let mut out = Vec::new();
        for (k, d) in &self.docs {
            if matches(d, filter)? {
                out.push(k.clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn add_index(&mut self, ns: &str, mut ix: Index) -> MResult<bool> {
        if let Some(existing) = self.indexes.iter().find(|i| i.name == ix.name || i.key == ix.key) {
            if existing.key == ix.key && existing.name == ix.name && existing.unique == ix.unique {
                return Ok(false);
            }
            return Err(MongoError::new(
                85,
                "IndexOptionsConflict",
                format!("An existing index has the same name or key as the requested index: {}", existing.name),
            ));
        }
        if ix.unique {
            for (id, d) in &self.docs {
                if let Some(k) = ix.key_for(d)
                    && ix.entries.insert(k, id.clone()).is_some()
                {
                    return Err(ix.duplicate(ns, d));
                }
            }
        }
        self.indexes.push(ix);
        Ok(true)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Database {
    pub collections: BTreeMap<String, Collection>,
}

#[derive(Default)]
pub struct Data {
    pub databases: BTreeMap<String, Database>,
}

impl Data {
    pub fn collection(&self, db: &str, coll: &str) -> Option<&Collection> {
        self.databases.get(db)?.collections.get(coll)
    }

    fn collection_mut(&mut self, db: &str, coll: &str, create: bool) -> MResult<Option<&mut Collection>> {
        if create {
            validate_ns(db, coll)?;
            let d = self.databases.entry(db.to_owned()).or_default();
            Ok(Some(d.collections.entry(coll.to_owned()).or_insert_with(|| Collection::new(Document::new()))))
        } else {
            Ok(self.databases.get_mut(db).and_then(|d| d.collections.get_mut(coll)))
        }
    }

    /// Documents of another collection, for `$lookup` / `$unionWith`.
    fn docs_of(&self, db: &str, coll: &str) -> Vec<Document> {
        self.collection(db, coll).map(|c| c.docs.values().cloned().collect()).unwrap_or_default()
    }
}

pub fn validate_ns(db: &str, coll: &str) -> MResult<()> {
    if db.is_empty() || db.len() > 63 || db.contains(['/', '\\', '.', ' ', '"', '$', '\0']) {
        return Err(MongoError::new(73, "InvalidNamespace", format!("Invalid database name: '{db}'")));
    }
    if coll.is_empty() || coll.contains(['$', '\0']) || coll.starts_with('.') || coll.len() > 255 {
        return Err(MongoError::new(73, "InvalidNamespace", format!("Invalid collection name: '{coll}'")));
    }
    Ok(())
}

/// Prepare a document for insertion: `_id` first (generated when missing), size checked.
pub fn prepare_insert(mut d: Document) -> MResult<Document> {
    let id = match d.remove("_id") {
        Some(Bson::Array(_)) => return Err(MongoError::new(53, "InvalidIdField", "can't use an array for _id")),
        Some(Bson::RegularExpression(_)) => return Err(MongoError::new(53, "InvalidIdField", "can't use a regex for _id")),
        Some(id) => id,
        None => Bson::ObjectId(ObjectId::new()),
    };
    let mut out = doc! { "_id": id };
    out.extend(d);
    check_size(&out)?;
    Ok(out)
}

fn check_size(d: &Document) -> MResult<()> {
    let size = d.to_vec().map(|v| v.len()).unwrap_or(0);
    if size > MAX_DOC_SIZE {
        return Err(MongoError::new(
            10334,
            "BSONObjectTooLarge",
            format!("object to insert too large. size in bytes: {size}, max size: {MAX_DOC_SIZE}"),
        ));
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct UpdateOutcome {
    pub matched: u64,
    pub modified: u64,
    pub upserted: Option<Bson>,
}

pub struct UpdateSpec<'a> {
    pub filter: &'a Document,
    pub update: &'a Bson,
    pub upsert: bool,
    pub multi: bool,
    pub array_filters: &'a [Document],
    pub sort: Option<&'a Document>,
}

/// The whole mock: databases behind one lock, saved as a BSON snapshot when changed.
pub struct MongoStore {
    data: RwLock<Data>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

impl MongoStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let store = MongoStore { data: RwLock::new(Data::default()), path, dirty: AtomicBool::new(false) };
        if let Some(p) = &store.path
            && p.exists()
            && let Err(e) = store.load(p)
        {
            // Keep the unreadable file instead of overwriting it with an empty snapshot.
            let aside = p.with_extension(format!("bson.unreadable-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
            tracing::warn!("cannot read MongoDB snapshot {}: {e}; moved to {}", p.display(), aside.display());
            let _ = std::fs::rename(p, aside);
            *store.data.write() = Data::default();
        }
        store
    }

    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, Data> {
        self.data.read()
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    // ---------------------------------------------------------- persistence

    fn load(&self, path: &Path) -> std::io::Result<()> {
        let bytes = std::fs::read(path)?;
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
        let header = Document::from_reader(&mut cur).map_err(std::io::Error::other)?;
        if header.get_str("format").ok() != Some("twinbase-mongo") {
            return Err(bad("not a TwinBase MongoDB snapshot"));
        }
        let mut data = self.data.write();
        while (cur.position() as usize) < bytes.len() {
            let h = Document::from_reader(&mut cur).map_err(std::io::Error::other)?;
            let (db, coll) = (h.get_str("db").map_err(|_| bad("db"))?, h.get_str("collection").map_err(|_| bad("collection"))?);
            let count = h.get_i64("count").map_err(|_| bad("count"))?;
            let mut c = Collection::new(h.get_document("options").cloned().unwrap_or_default());
            let ns = format!("{db}.{coll}");
            for spec in h.get_array("indexes").map(|a| a.to_vec()).unwrap_or_default() {
                if let Bson::Document(spec) = spec
                    && spec.get_str("name").ok() != Some("_id_")
                {
                    let _ = c.add_index(&ns, index_from_spec(&spec).map_err(|e| bad(&e.message))?);
                }
            }
            for _ in 0..count {
                let d = Document::from_reader(&mut cur).map_err(std::io::Error::other)?;
                let id = Collection::id_key(&d);
                c.index_add(&id, &d);
                c.docs.insert(id, d);
            }
            data.databases.entry(db.to_owned()).or_default().collections.insert(coll.to_owned(), c);
        }
        Ok(())
    }

    /// Write a snapshot if data changed since the last one.
    pub fn save_if_dirty(&self) -> std::io::Result<()> {
        if self.dirty.swap(false, Ordering::SeqCst)
            && let Err(e) = self.save()
        {
            self.dirty.store(true, Ordering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut buf = Vec::new();
        {
            let data = self.data.read();
            doc! { "format": "twinbase-mongo", "version": 1 }.to_writer(&mut buf).map_err(std::io::Error::other)?;
            for (db, d) in &data.databases {
                for (name, c) in &d.collections {
                    let header = doc! {
                        "db": db, "collection": name, "count": c.docs.len() as i64, "options": c.options.clone(),
                        "indexes": c.indexes.iter().map(|i| Bson::Document(i.describe())).collect::<Vec<_>>(),
                    };
                    header.to_writer(&mut buf).map_err(std::io::Error::other)?;
                    for doc in c.docs.values() {
                        doc.to_writer(&mut buf).map_err(std::io::Error::other)?;
                    }
                }
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("bson.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    // ---------------------------------------------------------- namespaces

    pub fn list_databases(&self) -> Vec<(String, u64, bool)> {
        let data = self.data.read();
        data.databases
            .iter()
            .map(|(name, d)| {
                let size: u64 =
                    d.collections.values().flat_map(|c| c.docs.values()).map(|doc| doc.to_vec().map(|v| v.len() as u64).unwrap_or(0)).sum();
                (name.clone(), size.max(8192), d.collections.is_empty())
            })
            .collect()
    }

    pub fn list_collections(&self, db: &str) -> Vec<(String, Document)> {
        let data = self.data.read();
        data.databases.get(db).map(|d| d.collections.iter().map(|(n, c)| (n.clone(), c.options.clone())).collect()).unwrap_or_default()
    }

    pub fn create_collection(&self, db: &str, coll: &str, options: Document) -> MResult<()> {
        validate_ns(db, coll)?;
        let mut data = self.data.write();
        let d = data.databases.entry(db.to_owned()).or_default();
        if d.collections.contains_key(coll) {
            return Err(MongoError::new(48, "NamespaceExists", format!("Collection {db}.{coll} already exists.")));
        }
        d.collections.insert(coll.to_owned(), Collection::new(options));
        self.touch();
        Ok(())
    }

    pub fn drop_collection(&self, db: &str, coll: &str) -> bool {
        let mut data = self.data.write();
        let dropped = data.databases.get_mut(db).is_some_and(|d| d.collections.remove(coll).is_some());
        if dropped {
            if data.databases.get(db).is_some_and(|d| d.collections.is_empty()) {
                data.databases.remove(db);
            }
            self.touch();
        }
        dropped
    }

    pub fn drop_database(&self, db: &str) -> bool {
        let dropped = self.data.write().databases.remove(db).is_some();
        if dropped {
            self.touch();
        }
        dropped
    }

    pub fn rename_collection(&self, from: (&str, &str), to: (&str, &str), drop_target: bool) -> MResult<()> {
        validate_ns(to.0, to.1)?;
        let mut data = self.data.write();
        if data.collection(from.0, from.1).is_none() {
            return Err(MongoError::new(26, "NamespaceNotFound", "source namespace does not exist"));
        }
        if data.collection(to.0, to.1).is_some() && !drop_target {
            return Err(MongoError::new(48, "NamespaceExists", "target namespace exists"));
        }
        let c = data.databases.get_mut(from.0).and_then(|d| d.collections.remove(from.1)).unwrap();
        data.databases.entry(to.0.to_owned()).or_default().collections.insert(to.1.to_owned(), c);
        if data.databases.get(from.0).is_some_and(|d| d.collections.is_empty()) {
            data.databases.remove(from.0);
        }
        self.touch();
        Ok(())
    }

    // ---------------------------------------------------------- reads

    #[allow(clippy::too_many_arguments)]
    pub fn find(
        &self,
        db: &str,
        coll: &str,
        filter: &Document,
        sort: Option<&Document>,
        skip: usize,
        limit: Option<usize>,
        projection: Option<&Document>,
    ) -> MResult<Vec<Document>> {
        let data = self.data.read();
        let Some(c) = data.collection(db, coll) else { return Ok(vec![]) };
        let sorted = sort.is_some_and(|s| !s.is_empty());
        // Without a sort, stop scanning once skip + limit documents matched.
        let cap = if sorted { usize::MAX } else { limit.map(|l| l.saturating_add(skip)).unwrap_or(usize::MAX) };
        let ids = c.matching(filter, cap)?;
        let mut docs: Vec<Document> = ids.iter().filter_map(|k| c.docs.get(k).cloned()).collect();
        drop(data);
        if let Some(s) = sort.filter(|s| !s.is_empty()) {
            sort_docs(&mut docs, s);
        }
        let docs = docs.into_iter().skip(skip).take(limit.unwrap_or(usize::MAX));
        match projection.filter(|p| !p.is_empty()) {
            Some(p) => docs.map(|d| project_find(&d, p)).collect(),
            None => Ok(docs.collect()),
        }
    }

    pub fn count(&self, db: &str, coll: &str, filter: &Document) -> MResult<u64> {
        let data = self.data.read();
        let Some(c) = data.collection(db, coll) else { return Ok(0) };
        if filter.is_empty() {
            return Ok(c.docs.len() as u64);
        }
        Ok(c.matching(filter, usize::MAX)?.len() as u64)
    }

    pub fn distinct(&self, db: &str, coll: &str, key: &str, filter: &Document) -> MResult<Vec<Bson>> {
        let data = self.data.read();
        let Some(c) = data.collection(db, coll) else { return Ok(vec![]) };
        let mut seen: IndexMap<Vec<u8>, Bson> = IndexMap::new();
        for k in c.matching(filter, usize::MAX)? {
            let d = &c.docs[&k];
            for v in query::path_values(d, key) {
                let items: Vec<&Bson> = match v {
                    Bson::Array(a) => a.iter().collect(),
                    other => vec![other],
                };
                for i in items {
                    seen.entry(key_of(i)).or_insert_with(|| i.clone());
                }
            }
        }
        let mut out: Vec<Bson> = seen.into_values().collect();
        out.sort_by(compare);
        Ok(out)
    }

    /// Run a pipeline; a trailing `$out` / `$merge` writes the result into a collection.
    pub fn aggregate(&self, db: &str, coll: Option<&str>, pipeline: &[Document]) -> MResult<Vec<Document>> {
        let (body, sink) = match pipeline.last().and_then(|s| s.keys().next().map(String::as_str)) {
            Some("$out" | "$merge") => (&pipeline[..pipeline.len() - 1], pipeline.last()),
            _ => (pipeline, None),
        };
        if body.iter().any(|s| s.contains_key("$out") || s.contains_key("$merge")) {
            return Err(MongoError::new(40601, "Location40601", "$out/$merge can only be the final stage in the pipeline"));
        }
        let result = {
            let data = self.data.read();
            let input = match coll {
                Some(c) => data.docs_of(db, c),
                None => {
                    // `aggregate: 1` with a leading $documents stage.
                    match body.first().and_then(|s| s.get("$documents")) {
                        Some(Bson::Array(a)) => a.iter().filter_map(|v| v.as_document().cloned()).collect(),
                        _ => {
                            return Err(MongoError::new(
                                73,
                                "InvalidNamespace",
                                "{aggregate: 1} is not valid for '$match'; a collection is required.",
                            ));
                        }
                    }
                }
            };
            let body = if coll.is_none() { &body[1..] } else { body };
            let colls = |name: &str| data.docs_of(db, name);
            agg::run_pipeline(input, body, &colls)?
        };
        if let Some(sink) = sink {
            let (target_db, target) = match sink.iter().next().unwrap() {
                (_, Bson::String(c)) => (db.to_owned(), c.clone()),
                (_, Bson::Document(d)) => {
                    let into = d.get("into").or_else(|| d.get("coll"));
                    match into {
                        Some(Bson::String(c)) => (d.get_str("db").unwrap_or(db).to_owned(), c.clone()),
                        Some(Bson::Document(i)) => {
                            (i.get_str("db").unwrap_or(db).to_owned(), i.get_str("coll").unwrap_or_default().to_owned())
                        }
                        _ => return Err(MongoError::bad_value("$out/$merge requires a target collection")),
                    }
                }
                _ => return Err(MongoError::bad_value("$out/$merge requires a target collection")),
            };
            let replace_all = sink.contains_key("$out");
            let mut data = self.data.write();
            let ns = format!("{target_db}.{target}");
            let c = data.collection_mut(&target_db, &target, true)?.unwrap();
            if replace_all {
                let mut fresh = Collection::new(c.options.clone());
                fresh.indexes = c.indexes.iter().map(|i| Index { entries: HashMap::new(), ..i.clone() }).collect();
                for d in &result {
                    fresh.insert(&ns, prepare_insert(d.clone())?)?;
                }
                *c = fresh;
            } else {
                for d in &result {
                    let d = prepare_insert(d.clone())?;
                    let id = Collection::id_key(&d);
                    if c.docs.contains_key(&id) {
                        let mut merged = c.docs[&id].clone();
                        merged.extend(d);
                        c.replace(&ns, &id, merged)?;
                    } else {
                        c.insert(&ns, d)?;
                    }
                }
            }
            self.touch();
            return Ok(vec![]);
        }
        Ok(result)
    }

    // ---------------------------------------------------------- writes

    /// Insert documents; returns the number inserted and per-document errors.
    pub fn insert(&self, db: &str, coll: &str, docs: Vec<Document>, ordered: bool) -> MResult<(u64, Vec<(usize, MongoError)>)> {
        let ns = format!("{db}.{coll}");
        let mut data = self.data.write();
        let c = data.collection_mut(db, coll, true)?.unwrap();
        let mut n = 0;
        let mut errors = Vec::new();
        for (i, d) in docs.into_iter().enumerate() {
            match prepare_insert(d).and_then(|d| c.insert(&ns, d)) {
                Ok(()) => n += 1,
                Err(e) => {
                    errors.push((i, e));
                    if ordered {
                        break;
                    }
                }
            }
        }
        if n > 0 {
            self.touch();
        }
        Ok((n, errors))
    }

    pub fn update(&self, db: &str, coll: &str, spec: &UpdateSpec) -> MResult<UpdateOutcome> {
        let ns = format!("{db}.{coll}");
        let mut data = self.data.write();
        let exists = data.collection(db, coll).is_some();
        let mut out = UpdateOutcome::default();
        let ids = match data.collection(db, coll) {
            Some(c) => {
                let mut ids = c.matching(spec.filter, if spec.multi || spec.sort.is_some() { usize::MAX } else { 1 })?;
                if let Some(s) = spec.sort.filter(|s| !s.is_empty()) {
                    let mut docs: Vec<(Vec<u8>, Document)> = ids.into_iter().map(|k| (k.clone(), c.docs[&k].clone())).collect();
                    sort_pairs(&mut docs, s);
                    ids = docs.into_iter().map(|(k, _)| k).collect();
                }
                if !spec.multi {
                    ids.truncate(1);
                }
                ids
            }
            None => vec![],
        };
        if ids.is_empty() {
            if !spec.upsert {
                return Ok(out);
            }
            let seed = upsert_seed(spec.filter);
            let new = match spec.update {
                Bson::Document(u) if is_update_operators(u) => {
                    let mut d = seed;
                    let u = query::resolve_positional(&d, u, spec.filter, spec.array_filters, true)?;
                    apply_update(&mut d, &u, true)?;
                    d
                }
                Bson::Document(replacement) => {
                    let mut d = replacement.clone();
                    if let (Some(id), false) = (seed.get("_id"), d.contains_key("_id")) {
                        d.insert("_id", id.clone());
                    }
                    d
                }
                Bson::Array(p) => apply_pipeline_update(&seed, &pipeline_of(p)?)?,
                _ => return Err(MongoError::failed_to_parse("Update argument must be either an object or an array")),
            };
            let new = prepare_insert(new)?;
            let id = new.get("_id").cloned();
            data.collection_mut(db, coll, true)?.unwrap().insert(&ns, new)?;
            out.upserted = id;
            self.touch();
            return Ok(out);
        }
        let _ = exists;
        let c = data.collection_mut(db, coll, false)?.unwrap();
        for id in ids {
            let old = c.docs[&id].clone();
            let new = match spec.update {
                Bson::Document(u) if is_update_operators(u) => {
                    let mut d = old.clone();
                    let u = query::resolve_positional(&d, u, spec.filter, spec.array_filters, false)?;
                    apply_update(&mut d, &u, false)?;
                    d
                }
                Bson::Document(replacement) => {
                    if let Some(rid) = replacement.get("_id")
                        && !values_equal(rid, old.get("_id").unwrap_or(&Bson::Null))
                    {
                        return Err(MongoError::new(
                            66,
                            "ImmutableField",
                            "After applying the update, the (immutable) field '_id' was found to have been altered",
                        ));
                    }
                    let mut d = doc! { "_id": old.get("_id").cloned().unwrap_or(Bson::Null) };
                    d.extend(replacement.iter().filter(|(k, _)| k.as_str() != "_id").map(|(k, v)| (k.clone(), v.clone())));
                    d
                }
                Bson::Array(p) => apply_pipeline_update(&old, &pipeline_of(p)?)?,
                _ => return Err(MongoError::failed_to_parse("Update argument must be either an object or an array")),
            };
            out.matched += 1;
            if new != old {
                check_size(&new)?;
                c.replace(&ns, &id, new)?;
                out.modified += 1;
            }
        }
        if out.modified > 0 {
            self.touch();
        }
        Ok(out)
    }

    pub fn delete(&self, db: &str, coll: &str, filter: &Document, limit_one: bool) -> MResult<u64> {
        let mut data = self.data.write();
        let Some(c) = data.collection_mut(db, coll, false)? else { return Ok(0) };
        let ids = c.matching(filter, if limit_one { 1 } else { usize::MAX })?;
        let n = ids.len() as u64;
        for id in ids {
            c.remove(&id);
        }
        if n > 0 {
            self.touch();
        }
        Ok(n)
    }

    /// `findAndModify`: returns the document before or after the change, and the update outcome.
    #[allow(clippy::too_many_arguments)]
    pub fn find_and_modify(
        &self,
        db: &str,
        coll: &str,
        filter: &Document,
        sort: Option<&Document>,
        remove: bool,
        update: Option<&Bson>,
        upsert: bool,
        return_new: bool,
        fields: Option<&Document>,
        array_filters: &[Document],
    ) -> MResult<(Option<Document>, UpdateOutcome)> {
        let target = {
            let data = self.data.read();
            match data.collection(db, coll) {
                Some(c) => {
                    let ids = c.matching(filter, if sort.is_some_and(|s| !s.is_empty()) { usize::MAX } else { 1 })?;
                    let mut docs: Vec<(Vec<u8>, Document)> = ids.into_iter().map(|k| (k.clone(), c.docs[&k].clone())).collect();
                    if let Some(s) = sort.filter(|s| !s.is_empty()) {
                        sort_pairs(&mut docs, s);
                    }
                    docs.into_iter().next()
                }
                None => None,
            }
        };
        let shape = |d: Option<Document>| -> MResult<Option<Document>> {
            match (d, fields.filter(|f| !f.is_empty())) {
                (Some(d), Some(f)) => Ok(Some(project_find(&d, f)?)),
                (d, _) => Ok(d),
            }
        };
        if remove {
            let Some((id, before)) = target else { return Ok((None, UpdateOutcome::default())) };
            let mut data = self.data.write();
            if let Some(c) = data.collection_mut(db, coll, false)? {
                c.remove(&id);
            }
            self.touch();
            return Ok((shape(Some(before))?, UpdateOutcome { matched: 1, modified: 0, upserted: None }));
        }
        let update = update.ok_or_else(|| MongoError::failed_to_parse("Either an update or remove=true must be specified"))?;
        let before = target.as_ref().map(|(_, d)| d.clone());
        // Pin the chosen document by _id so the update touches exactly it.
        let pinned = match &before {
            Some(d) => doc! { "_id": d.get("_id").cloned().unwrap_or(Bson::Null) },
            None => filter.clone(),
        };
        let outcome = self.update(
            db,
            coll,
            &UpdateSpec {
                filter: if before.is_some() { &pinned } else { filter },
                update,
                upsert,
                multi: false,
                array_filters,
                sort: None,
            },
        )?;
        let after = if return_new {
            let id = before.as_ref().and_then(|d| d.get("_id").cloned()).or_else(|| outcome.upserted.clone());
            match id {
                Some(id) => self.find(db, coll, &doc! { "_id": id }, None, 0, Some(1), None)?.into_iter().next(),
                None => None,
            }
        } else {
            before
        };
        Ok((shape(after)?, outcome))
    }

    // ---------------------------------------------------------- indexes

    pub fn create_index(&self, db: &str, coll: &str, spec: &Document) -> MResult<bool> {
        let ix = index_from_spec(spec)?;
        let ns = format!("{db}.{coll}");
        let mut data = self.data.write();
        let c = data.collection_mut(db, coll, true)?.unwrap();
        let created = c.add_index(&ns, ix)?;
        self.touch();
        Ok(created)
    }

    pub fn list_indexes(&self, db: &str, coll: &str) -> Option<Vec<Document>> {
        self.data.read().collection(db, coll).map(|c| c.indexes.iter().map(Index::describe).collect())
    }

    /// Drop an index by name, by key pattern, or all (`*`) except `_id_`.
    pub fn drop_index(&self, db: &str, coll: &str, which: &Bson) -> MResult<usize> {
        let mut data = self.data.write();
        let c = data
            .collection_mut(db, coll, false)?
            .ok_or_else(|| MongoError::new(26, "NamespaceNotFound", format!("ns not found {db}.{coll}")))?;
        let before = c.indexes.len();
        let names: Vec<String> = match which {
            Bson::String(s) if s == "*" => c.indexes.iter().filter(|i| i.name != "_id_").map(|i| i.name.clone()).collect(),
            Bson::String(s) => vec![s.clone()],
            Bson::Array(a) => a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
            Bson::Document(k) => c.indexes.iter().filter(|i| &i.key == k).map(|i| i.name.clone()).collect(),
            _ => return Err(MongoError::bad_value("invalid index specification")),
        };
        for n in &names {
            if n == "_id_" {
                return Err(MongoError::new(72, "InvalidOptions", "cannot drop _id index"));
            }
            if !c.indexes.iter().any(|i| &i.name == n) {
                return Err(MongoError::new(27, "IndexNotFound", format!("index not found with name [{n}]")));
            }
        }
        c.indexes.retain(|i| !names.contains(&i.name));
        self.touch();
        Ok(before)
    }

    /// Replace a database's contents with seed collections (used when a mock starts fresh).
    pub fn seed(&self, db: &str, seed: &Document) -> MResult<usize> {
        let mut total = 0;
        let collections = seed.get_document("collections").map_err(|_| MongoError::bad_value("seed needs a \"collections\" object"))?;
        for (name, spec) in collections {
            let (docs, indexes) = match spec {
                Bson::Array(a) => (a.clone(), vec![]),
                Bson::Document(d) => {
                    (d.get_array("documents").cloned().unwrap_or_default(), d.get_array("indexes").cloned().unwrap_or_default())
                }
                other => {
                    return Err(MongoError::bad_value(format!(
                        "collection '{name}' must be an array of documents or {{indexes, documents}}, found {}",
                        query::bson_type_name(other)
                    )));
                }
            };
            self.create_collection(db, name, Document::new()).or_else(|e| if e.code == 48 { Ok(()) } else { Err(e) })?;
            for ix in indexes {
                if let Bson::Document(ix) = ix {
                    self.create_index(db, name, &ix)?;
                }
            }
            let docs: Vec<Document> = docs
                .into_iter()
                .map(|d| match d {
                    Bson::Document(d) => Ok(d),
                    other => Err(MongoError::bad_value(format!(
                        "documents of '{name}' must be objects, found {}",
                        query::bson_type_name(&other)
                    ))),
                })
                .collect::<MResult<_>>()?;
            let (n, errors) = self.insert(db, name, docs, false)?;
            if let Some((_, e)) = errors.into_iter().next() {
                return Err(e);
            }
            total += n as usize;
        }
        Ok(total)
    }

    /// The whole database as a seed document (`{collections: {name: {indexes, documents}}}`).
    pub fn export(&self, db: &str) -> Document {
        let data = self.data.read();
        let mut colls = Document::new();
        if let Some(d) = data.databases.get(db) {
            for (name, c) in &d.collections {
                let indexes: Vec<Bson> = c.indexes.iter().filter(|i| i.name != "_id_").map(|i| Bson::Document(i.describe())).collect();
                colls.insert(
                    name.clone(),
                    doc! { "indexes": indexes, "documents": c.docs.values().map(|d| Bson::Document(d.clone())).collect::<Vec<_>>() },
                );
            }
        }
        doc! { "collections": colls }
    }
}

fn pipeline_of(p: &[Bson]) -> MResult<Vec<Document>> {
    p.iter()
        .map(|s| s.as_document().cloned().ok_or_else(|| MongoError::failed_to_parse("each update pipeline stage must be an object")))
        .collect()
}

fn sort_pairs(docs: &mut [(Vec<u8>, Document)], spec: &Document) {
    query::sort_by_spec(docs, spec, |(_, d)| d);
}

/// Find-style projection, including `$slice` and `$elemMatch` on arrays.
pub fn project_find(d: &Document, spec: &Document) -> MResult<Document> {
    let mut plain = Document::new();
    let mut special = Vec::new();
    for (k, v) in spec {
        match v {
            Bson::Document(op) if op.contains_key("$slice") || op.contains_key("$elemMatch") => special.push((k.clone(), op.clone())),
            Bson::Document(op) if op.keys().next().is_some_and(|k| k.starts_with('$')) => {
                // Aggregation expressions in find projections (MongoDB 4.4+).
                return agg::run_pipeline(vec![d.clone()], &[doc! { "$project": spec.clone() }], &|_: &str| Vec::new())
                    .map(|mut v| v.pop().unwrap_or_default());
            }
            other => {
                plain.insert(k.clone(), other.clone());
            }
        }
    }
    let inclusion = plain.iter().any(|(k, v)| k != "_id" && agg::truthy(v));
    let mut out = if plain.is_empty() {
        d.clone()
    } else {
        let mut with_specials = plain.clone();
        if inclusion {
            for (k, _) in &special {
                with_specials.insert(k.clone(), 1);
            }
        }
        project(d, &with_specials)?
    };
    for (k, op) in special {
        let Some(Bson::Array(items)) = lookup(d, &k).cloned() else { continue };
        let v = if let Some(s) = op.get("$slice") {
            let (start, n) = match s {
                Bson::Array(a) if a.len() == 2 => {
                    let p = query::as_f64(&a[0]).unwrap_or(0.0) as i64;
                    let start = if p < 0 { items.len().saturating_sub((-p) as usize) } else { (p as usize).min(items.len()) };
                    (start, query::as_f64(&a[1]).unwrap_or(0.0).max(0.0) as usize)
                }
                other => {
                    let n = query::as_f64(other).unwrap_or(0.0) as i64;
                    if n < 0 { (items.len().saturating_sub((-n) as usize), items.len()) } else { (0, n as usize) }
                }
            };
            Some(Bson::Array(items.into_iter().skip(start).take(n).collect()))
        } else {
            let q = op.get_document("$elemMatch").map_err(|_| MongoError::bad_value("$elemMatch needs an Object"))?;
            let mut hit = None;
            for item in items {
                let ok = match &item {
                    Bson::Document(x) => matches(x, q)?,
                    other => matches(&doc! { "v": other.clone() }, &doc! { "v": Bson::Document(q.clone()) })?,
                };
                if ok {
                    hit = Some(Bson::Array(vec![item]));
                    break;
                }
            }
            hit
        };
        match v {
            Some(v) => query::set_path(&mut out, &k, v),
            None => query::unset_path(&mut out, &k),
        }
    }
    Ok(out)
}

pub fn index_from_spec(spec: &Document) -> MResult<Index> {
    let key = spec.get_document("key").map_err(|_| MongoError::failed_to_parse("index specification must contain a 'key' object"))?.clone();
    if key.is_empty() {
        return Err(MongoError::new(67, "CannotCreateIndex", "Index keys cannot be empty."));
    }
    let name = match spec.get_str("name") {
        Ok(n) => n.to_owned(),
        Err(_) => key
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}_{}",
                    match v {
                        Bson::String(s) => s.clone(),
                        other => query::as_f64(other).map(|n| (n as i64).to_string()).unwrap_or_default(),
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("_"),
    };
    let unique = spec.get_bool("unique").unwrap_or(false);
    let sparse = spec.get_bool("sparse").unwrap_or(false);
    let mut extra = Document::new();
    for k in ["expireAfterSeconds", "partialFilterExpression", "collation", "weights", "default_language", "2dsphereIndexVersion"] {
        if let Some(v) = spec.get(k) {
            extra.insert(k, v.clone());
        }
    }
    Ok(Index { name, key, unique, sparse, extra, entries: HashMap::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_unique_indexes_and_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mongo.bson");
        let s = MongoStore::new(Some(path.clone()));
        s.create_index("app", "users", &doc! { "key": { "email": 1 }, "unique": true }).unwrap();
        let (n, errs) = s
            .insert(
                "app",
                "users",
                vec![doc! { "email": "a@x.io", "n": 1 }, doc! { "email": "b@x.io", "n": 2 }, doc! { "email": "a@x.io" }],
                false,
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(errs[0].1.code, 11000);
        assert!(errs[0].1.message.contains("email_1"));
        let out = s.update(
            "app",
            "users",
            &UpdateSpec {
                filter: &doc! { "email": "b@x.io" },
                update: &Bson::Document(doc! { "$set": { "email": "a@x.io" } }),
                upsert: false,
                multi: false,
                array_filters: &[],
                sort: None,
            },
        );
        assert_eq!(out.unwrap_err().code, 11000);
        let up = s
            .update(
                "app",
                "users",
                &UpdateSpec {
                    filter: &doc! { "email": "c@x.io" },
                    update: &Bson::Document(doc! { "$inc": { "n": 5 } }),
                    upsert: true,
                    multi: false,
                    array_filters: &[],
                    sort: None,
                },
            )
            .unwrap();
        assert!(up.upserted.is_some());
        assert_eq!(s.count("app", "users", &doc! {}).unwrap(), 3);
        let found = s
            .find("app", "users", &doc! { "n": { "$gte": 2 } }, Some(&doc! { "n": -1 }), 0, None, Some(&doc! { "_id": 0, "email": 1 }))
            .unwrap();
        assert_eq!(found, vec![doc! { "email": "c@x.io" }, doc! { "email": "b@x.io" }]);
        assert_eq!(s.delete("app", "users", &doc! { "n": { "$lt": 3 } }, false).unwrap(), 2);
        s.save_if_dirty().unwrap();
        let again = MongoStore::new(Some(path));
        assert_eq!(again.count("app", "users", &doc! {}).unwrap(), 1);
        assert_eq!(again.list_indexes("app", "users").unwrap().len(), 2);
        let dup = again.insert("app", "users", vec![doc! { "email": "c@x.io" }], true).unwrap();
        assert_eq!(dup.1[0].1.code, 11000);
    }

    #[test]
    fn find_and_modify_and_out() {
        let s = MongoStore::new(None);
        s.insert("db", "jobs", vec![doc! { "_id": 1, "state": "new", "p": 1 }, doc! { "_id": 2, "state": "new", "p": 5 }], true).unwrap();
        let (d, _) = s
            .find_and_modify(
                "db",
                "jobs",
                &doc! { "state": "new" },
                Some(&doc! { "p": -1 }),
                false,
                Some(&Bson::Document(doc! { "$set": { "state": "taken" } })),
                false,
                true,
                None,
                &[],
            )
            .unwrap();
        assert_eq!(d.unwrap(), doc! { "_id": 2, "state": "taken", "p": 5 });
        s.aggregate("db", Some("jobs"), &[doc! { "$match": { "state": "new" } }, doc! { "$out": "pending" }]).unwrap();
        assert_eq!(s.count("db", "pending", &doc! {}).unwrap(), 1);
        assert!(s.aggregate("db", Some("jobs"), &[doc! { "$out": "x" }, doc! { "$match": {} }]).is_err());
    }

    #[test]
    fn unreadable_snapshot_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mongo.bson");
        std::fs::write(&path, b"garbage").unwrap();
        let s = MongoStore::new(Some(path.clone()));
        assert!(s.list_databases().is_empty());
        assert!(!path.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("unreadable")));
    }
}
