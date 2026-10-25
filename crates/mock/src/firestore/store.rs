//! Firestore documents per database, commits (with field transforms and preconditions),
//! optimistic transactions, queries, a change feed for listeners, and JSON snapshots.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use prost_types::Timestamp;
use serde_json::{Value as Json, json};
use tokio::sync::broadcast;
use tonic::Status;
use twinbase_firestore_proto::json as fjson;
use twinbase_firestore_proto::v1::document_transform::field_transform::{ServerValue, TransformType};
use twinbase_firestore_proto::v1::value::ValueType;
use twinbase_firestore_proto::v1::write::Operation;
use twinbase_firestore_proto::v1::{Document, Precondition, StructuredQuery, Value, Write, WriteResult, precondition};

use super::query::{self, Fields, Row};

#[derive(Debug, Clone)]
pub struct StoredDoc {
    pub fields: Fields,
    pub create_time: Timestamp,
    pub update_time: Timestamp,
}

/// Documents changed by one commit (for listeners).
#[derive(Debug, Clone)]
pub struct Change {
    pub database: String,
    pub paths: Vec<String>,
    pub time: Timestamp,
}

struct Txn {
    database: String,
    read_only: bool,
    /// Documents read in the transaction and the update time they had (None = missing).
    reads: HashMap<String, Option<Timestamp>>,
    started: Instant,
}

pub struct FirestoreStore {
    data: RwLock<BTreeMap<String, BTreeMap<String, StoredDoc>>>,
    last_time: Mutex<i64>,
    txns: Mutex<HashMap<Vec<u8>, Txn>>,
    changes: broadcast::Sender<Change>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

pub const MAX_DOC_BYTES: usize = 1024 * 1024;

/// `projects/p/databases/d` and the path below `/documents` of a resource name.
pub fn split_name(name: &str) -> Result<(String, String), Status> {
    let rest = name.strip_prefix("projects/").ok_or_else(|| Status::invalid_argument(format!("Invalid resource name: {name}")))?;
    let mut parts = rest.splitn(4, '/');
    let (project, dbs, db) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if project.is_empty() || dbs != "databases" || db.is_empty() {
        return Err(Status::invalid_argument(format!("Invalid resource name: {name}")));
    }
    let tail = parts.next().unwrap_or("");
    let rel = match tail {
        "" | "documents" => String::new(),
        t => t.strip_prefix("documents/").ok_or_else(|| Status::invalid_argument(format!("Invalid resource name: {name}")))?.to_owned(),
    };
    Ok((format!("projects/{project}/databases/{db}"), rel))
}

/// The database id part (`(default)`) — data is shared across project ids, like one emulator.
pub fn db_key(database: &str) -> String {
    database.rsplit('/').next().unwrap_or("(default)").to_owned()
}

pub fn doc_name(database: &str, rel: &str) -> String {
    format!("{database}/documents/{rel}")
}

/// Document paths have an even number of segments; collection paths an odd number.
pub fn check_doc_path(rel: &str) -> Result<(), Status> {
    let segs: Vec<&str> = rel.split('/').collect();
    if rel.is_empty() || !segs.len().is_multiple_of(2) || segs.iter().any(|s| s.is_empty()) {
        return Err(Status::invalid_argument(format!(
            "Document name \"{rel}\" lacks \"/\" at index or has an odd number of path components (a collection, not a document)"
        )));
    }
    if segs.iter().any(|s| *s == "." || *s == ".." || (s.starts_with("__") && s.ends_with("__"))) {
        return Err(Status::invalid_argument(format!("Invalid document path: {rel}")));
    }
    Ok(())
}

pub fn parent_of(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

fn approx_size(fields: &Fields) -> usize {
    fields.iter().map(|(k, v)| k.len() + 1 + value_size(v)).sum::<usize>() + 32
}

fn value_size(v: &Value) -> usize {
    match &v.value_type {
        Some(ValueType::StringValue(s)) => s.len() + 1,
        Some(ValueType::BytesValue(b)) => b.len(),
        Some(ValueType::ReferenceValue(r)) => r.len() + 1,
        Some(ValueType::ArrayValue(a)) => a.values.iter().map(value_size).sum(),
        Some(ValueType::MapValue(m)) => approx_size(&m.fields),
        Some(ValueType::GeoPointValue(_)) => 16,
        Some(ValueType::TimestampValue(_)) => 8,
        _ => 8,
    }
}

pub fn auto_id() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..20).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect()
}

impl FirestoreStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let (changes, _) = broadcast::channel(1024);
        let store = FirestoreStore {
            data: RwLock::new(BTreeMap::new()),
            last_time: Mutex::new(0),
            txns: Mutex::new(HashMap::new()),
            changes,
            path,
            dirty: AtomicBool::new(false),
        };
        if let Some(p) = &store.path
            && p.exists()
            && let Err(e) = store.load(p)
        {
            let aside = p.with_extension(format!("json.unreadable-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
            tracing::warn!("cannot read Firestore snapshot {}: {e}; moved to {}", p.display(), aside.display());
            let _ = std::fs::rename(p, aside);
            store.data.write().clear();
        }
        store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// A strictly increasing commit time (microsecond precision, like Firestore).
    pub fn now(&self) -> Timestamp {
        let mut last = self.last_time.lock();
        let micros = (chrono::Utc::now().timestamp_micros()).max(*last + 1);
        *last = micros;
        Timestamp { seconds: micros.div_euclid(1_000_000), nanos: (micros.rem_euclid(1_000_000) * 1000) as i32 }
    }

    /// Current time without advancing the commit clock (read times).
    pub fn read_time(&self) -> Timestamp {
        let last = *self.last_time.lock();
        let micros = chrono::Utc::now().timestamp_micros().max(last);
        Timestamp { seconds: micros.div_euclid(1_000_000), nanos: (micros.rem_euclid(1_000_000) * 1000) as i32 }
    }

    // ---------------------------------------------------------- persistence

    fn load(&self, path: &std::path::Path) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let v: Json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let dbs = v.get("databases").and_then(Json::as_object).ok_or("missing \"databases\"")?;
        let mut data = self.data.write();
        let mut latest = 0i64;
        for (db, docs) in dbs {
            let entry = data.entry(db.clone()).or_default();
            for (rel, d) in docs.as_object().ok_or("documents must be an object")? {
                let ts = |k: &str| d.get(k).and_then(Json::as_str).map(fjson::parse_timestamp).transpose().map(|t| t.unwrap_or_default());
                let (c, u) = (ts("createTime")?, ts("updateTime")?);
                latest = latest.max(u.seconds * 1_000_000 + (u.nanos / 1000) as i64);
                entry.insert(
                    rel.clone(),
                    StoredDoc { fields: fjson::fields_from_rest(d.get("fields").unwrap_or(&json!({})))?, create_time: c, update_time: u },
                );
            }
        }
        *self.last_time.lock() = latest;
        Ok(())
    }

    pub fn save_if_dirty(&self) -> std::io::Result<()> {
        if self.dirty.swap(false, AtomicOrdering::SeqCst)
            && let Err(e) = self.save()
        {
            self.dirty.store(true, AtomicOrdering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let snapshot = {
            let data = self.data.read();
            json!({ "databases": data.iter().map(|(db, docs)| (db.clone(), Json::Object(docs.iter().map(|(rel, d)| (rel.clone(), json!({
                "fields": fjson::fields_to_rest(&d.fields), "createTime": fjson::timestamp_text(&d.create_time), "updateTime": fjson::timestamp_text(&d.update_time),
            }))).collect()))).collect::<serde_json::Map<_, _>>() })
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Seed: `{"documents": {"users/alice": {...plain JSON fields...}}}` into `(default)`.
    pub fn seed(&self, seed: &Json) -> Result<usize, String> {
        let docs = seed.get("documents").and_then(Json::as_object).ok_or("seed needs a \"documents\" object")?;
        let db = "projects/twinbase/databases/(default)";
        let mut writes = Vec::new();
        for (rel, fields) in docs {
            let rel = rel.trim_matches('/');
            check_doc_path(rel).map_err(|e| e.message().to_owned())?;
            writes.push(Write {
                operation: Some(Operation::Update(Document {
                    name: doc_name(db, rel),
                    fields: fjson::fields_from_plain(fields, db)?,
                    ..Default::default()
                })),
                ..Default::default()
            });
        }
        let n = writes.len();
        self.commit(db, &writes, None).map_err(|e| e.message().to_owned())?;
        Ok(n)
    }

    /// The default database as a seed (plain JSON with markers).
    pub fn export(&self) -> Json {
        let data = self.data.read();
        let docs = data
            .get("(default)")
            .map(|d| d.iter().map(|(rel, doc)| (rel.clone(), fjson::fields_to_plain(&doc.fields))).collect::<serde_json::Map<_, _>>())
            .unwrap_or_default();
        json!({ "documents": docs })
    }

    // ---------------------------------------------------------- reads

    pub fn to_document(database: &str, rel: &str, d: &StoredDoc) -> Document {
        Document {
            name: doc_name(database, rel),
            fields: d.fields.clone(),
            create_time: Some(d.create_time),
            update_time: Some(d.update_time),
        }
    }

    fn note_read(&self, txn: Option<&[u8]>, rel: &str, doc: Option<&StoredDoc>) -> Result<(), Status> {
        if let Some(id) = txn.filter(|t| !t.is_empty()) {
            let mut txns = self.txns.lock();
            let t = txns.get_mut(id).ok_or_else(|| Status::invalid_argument("Transaction is invalid or has expired"))?;
            t.reads.entry(rel.to_owned()).or_insert_with(|| doc.map(|d| d.update_time));
        }
        Ok(())
    }

    pub fn get(&self, database: &str, rel: &str, txn: Option<&[u8]>) -> Result<Option<Document>, Status> {
        check_doc_path(rel)?;
        let data = self.data.read();
        let d = data.get(&db_key(database)).and_then(|m| m.get(rel));
        self.note_read(txn, rel, d)?;
        Ok(d.map(|d| Self::to_document(database, rel, d)))
    }

    /// Documents directly in a collection, by id.
    pub fn list(&self, database: &str, collection: &str) -> Vec<Document> {
        let data = self.data.read();
        let Some(m) = data.get(&db_key(database)) else { return vec![] };
        let prefix = format!("{collection}/");
        m.range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k[prefix.len()..].contains('/'))
            .map(|(k, d)| Self::to_document(database, k, d))
            .collect()
    }

    /// Collection ids directly under a document (or the root when `parent` is empty).
    pub fn collection_ids(&self, database: &str, parent: &str) -> Vec<String> {
        let data = self.data.read();
        let Some(m) = data.get(&db_key(database)) else { return vec![] };
        let prefix = if parent.is_empty() { String::new() } else { format!("{parent}/") };
        let mut out = BTreeSet::new();
        for k in m.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k) {
            if let Some(c) = k[prefix.len()..].split('/').next() {
                out.insert(c.to_owned());
            }
        }
        out.into_iter().collect()
    }

    /// Run a structured query under `parent` (database root or a document).
    pub fn run_query(&self, database: &str, parent: &str, q: &StructuredQuery, txn: Option<&[u8]>) -> Result<Vec<Document>, Status> {
        let from = q.from.first().ok_or_else(|| Status::invalid_argument("A query needs a collection in 'from'"))?;
        if q.from.len() > 1 {
            return Err(Status::invalid_argument("Only one collection selector is supported"));
        }
        let orders = query::effective_orders(q);
        let data = self.data.read();
        let empty = BTreeMap::new();
        let docs = data.get(&db_key(database)).unwrap_or(&empty);
        let prefix = if parent.is_empty() { String::new() } else { format!("{parent}/") };
        let mut rows: Vec<(Vec<Value>, &String, &StoredDoc)> = Vec::new();
        for (path, d) in docs.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)) {
            let (coll_path, _) = parent_of(path);
            let coll_id = coll_path.rsplit('/').next().unwrap_or(coll_path);
            let in_scope =
                if from.all_descendants { coll_id == from.collection_id } else { coll_path == format!("{prefix}{}", from.collection_id) };
            if !in_scope {
                continue;
            }
            let row = Row { path, fields: &d.fields };
            if let Some(w) = &q.r#where
                && !query::matches(w, &row, database)?
            {
                continue;
            }
            if let Some(key) = query::order_key(&row, &orders, database)? {
                rows.push((key, path, d));
            }
        }
        rows.sort_by(|a, b| query::cmp_keys(&a.0, &b.0, &orders));
        let offset = q.offset.max(0) as usize;
        let limit = q.limit.map(|l| l.max(0) as usize).unwrap_or(usize::MAX);
        let mut out = Vec::new();
        for (key, path, d) in rows
            .iter()
            .filter(|(k, _, _)| query::after_start(k, q.start_at.as_ref(), &orders) && query::before_end(k, q.end_at.as_ref(), &orders))
            .skip(offset)
            .take(limit)
        {
            let _ = key;
            self.note_read(txn, path, Some(d))?;
            let mut doc = Self::to_document(database, path, d);
            if let Some(sel) = &q.select {
                doc.fields = query::project(&d.fields, sel)?;
            }
            out.push(doc);
        }
        Ok(out)
    }

    // ---------------------------------------------------------- transactions

    pub fn begin(&self, database: &str, read_only: bool) -> Vec<u8> {
        let id: Vec<u8> = (0..16).map(|_| rand::random::<u8>()).collect();
        let mut txns = self.txns.lock();
        // Abandoned transactions expire after 270 s, like Firestore's idle limit.
        txns.retain(|_, t| t.started.elapsed() < Duration::from_secs(270));
        txns.insert(id.clone(), Txn { database: db_key(database), read_only, reads: HashMap::new(), started: Instant::now() });
        id
    }

    pub fn rollback(&self, id: &[u8]) {
        self.txns.lock().remove(id);
    }

    // ---------------------------------------------------------- writes

    /// Apply writes atomically. With a transaction, fail with ABORTED when a document it read
    /// changed since (optimistic concurrency; clients retry).
    pub fn commit(&self, database: &str, writes: &[Write], txn: Option<&[u8]>) -> Result<(Timestamp, Vec<WriteResult>), Status> {
        let key = db_key(database);
        let mut data = self.data.write();
        if let Some(id) = txn.filter(|t| !t.is_empty()) {
            let t = self.txns.lock().remove(id).ok_or_else(|| Status::invalid_argument("Transaction is invalid or has expired"))?;
            if t.database != key {
                return Err(Status::invalid_argument("Transaction belongs to another database"));
            }
            if t.read_only && !writes.is_empty() {
                return Err(Status::invalid_argument("Cannot modify entities in a read-only transaction"));
            }
            let docs = data.get(&key);
            for (rel, seen) in &t.reads {
                let now = docs.and_then(|m| m.get(rel)).map(|d| d.update_time);
                let same = match (&now, seen) {
                    (None, None) => true,
                    (Some(a), Some(b)) => query::ts_cmp(a, b).is_eq(),
                    _ => false,
                };
                if !same {
                    return Err(Status::aborted(
                        "Transaction lock timeout / contention: a document read in this transaction was changed by another write. Retry the transaction.",
                    ));
                }
            }
        }
        let time = self.now();
        let docs = data.entry(key.clone()).or_default();
        // Work on copies so a failing write leaves everything unchanged.
        let mut staged: BTreeMap<String, Option<StoredDoc>> = BTreeMap::new();
        let mut results = Vec::with_capacity(writes.len());
        for w in writes {
            let (rel, op) = match &w.operation {
                Some(Operation::Update(d)) => (split_name(&d.name)?.1, w.operation.as_ref().unwrap()),
                Some(Operation::Delete(n)) => (split_name(n)?.1, w.operation.as_ref().unwrap()),
                Some(Operation::Transform(t)) => (split_name(&t.document)?.1, w.operation.as_ref().unwrap()),
                None => return Err(Status::invalid_argument("A write needs an operation")),
            };
            check_doc_path(&rel)?;
            let current: Option<StoredDoc> = match staged.get(&rel) {
                Some(s) => s.clone(),
                None => docs.get(&rel).cloned(),
            };
            check_precondition(w.current_document.as_ref(), current.as_ref(), &rel)?;
            let mut transform_results = Vec::new();
            let next = match op {
                Operation::Delete(_) => None,
                Operation::Update(d) => {
                    let mut fields = match (&w.update_mask, &current) {
                        (Some(_), Some(c)) => c.fields.clone(),
                        _ => Fields::new(),
                    };
                    match &w.update_mask {
                        Some(mask) => {
                            for fp in &mask.field_paths {
                                let path = query::split_field_path(fp)?;
                                match query::get_field(&d.fields, &path) {
                                    Some(v) => query::set_field(&mut fields, &path, v.clone()),
                                    None => query::remove_field(&mut fields, &path),
                                }
                            }
                        }
                        None => fields = d.fields.clone(),
                    }
                    transform_results = apply_transforms(&mut fields, &w.update_transforms, &time)?;
                    Some(fields)
                }
                Operation::Transform(t) => {
                    let mut fields = current.as_ref().map(|c| c.fields.clone()).unwrap_or_default();
                    transform_results = apply_transforms(&mut fields, &t.field_transforms, &time)?;
                    Some(fields)
                }
            };
            let staged_doc = next.map(|fields| StoredDoc {
                create_time: current.as_ref().map(|c| c.create_time).unwrap_or(time),
                update_time: time,
                fields,
            });
            if let Some(d) = &staged_doc
                && approx_size(&d.fields) > MAX_DOC_BYTES
            {
                return Err(Status::invalid_argument(format!(
                    "Document '{rel}' cannot be written because its size exceeds the maximum allowed size of 1,048,576 bytes."
                )));
            }
            results.push(WriteResult { update_time: staged_doc.as_ref().map(|_| time).or(Some(time)), transform_results });
            staged.insert(rel, staged_doc);
        }
        let mut changed = Vec::with_capacity(staged.len());
        for (rel, d) in staged {
            match d {
                Some(d) => {
                    docs.insert(rel.clone(), d);
                }
                None => {
                    docs.remove(&rel);
                }
            }
            changed.push(rel);
        }
        drop(data);
        if !changed.is_empty() {
            self.dirty.store(true, AtomicOrdering::SeqCst);
            let _ = self.changes.send(Change { database: key, paths: changed, time });
        }
        Ok((time, results))
    }
}

fn check_precondition(p: Option<&Precondition>, current: Option<&StoredDoc>, rel: &str) -> Result<(), Status> {
    match p.and_then(|p| p.condition_type.as_ref()) {
        None => Ok(()),
        Some(precondition::ConditionType::Exists(true)) if current.is_none() => {
            Err(Status::not_found(format!("No document to update: {rel}")))
        }
        Some(precondition::ConditionType::Exists(false)) if current.is_some() => {
            Err(Status::already_exists(format!("Document already exists: {rel}")))
        }
        Some(precondition::ConditionType::UpdateTime(t)) => match current {
            Some(c) if query::ts_cmp(&c.update_time, t).is_eq() => Ok(()),
            _ => Err(Status::failed_precondition(format!("The document {rel} has been modified since the given update time"))),
        },
        _ => Ok(()),
    }
}

fn apply_transforms(
    fields: &mut Fields,
    transforms: &[twinbase_firestore_proto::v1::document_transform::FieldTransform],
    time: &Timestamp,
) -> Result<Vec<Value>, Status> {
    let mut results = Vec::with_capacity(transforms.len());
    for t in transforms {
        let path = query::split_field_path(&t.field_path)?;
        let current = query::get_field(fields, &path).cloned();
        let next = match &t.transform_type {
            Some(TransformType::SetToServerValue(sv)) if *sv == ServerValue::RequestTime as i32 => {
                Value { value_type: Some(ValueType::TimestampValue(*time)) }
            }
            Some(TransformType::SetToServerValue(_)) => return Err(Status::invalid_argument("Unknown server value")),
            Some(TransformType::Increment(by)) => numeric(current.as_ref(), by, |a, b| a.checked_add(b), |a, b| a + b),
            Some(TransformType::Maximum(by)) => extreme(current.as_ref(), by, true),
            Some(TransformType::Minimum(by)) => extreme(current.as_ref(), by, false),
            Some(TransformType::AppendMissingElements(add)) => {
                let mut vals = match current.and_then(|c| c.value_type) {
                    Some(ValueType::ArrayValue(a)) => a.values,
                    _ => vec![],
                };
                for v in &add.values {
                    if !vals.iter().any(|x| query::equal(x, v)) {
                        vals.push(v.clone());
                    }
                }
                query::array(vals)
            }
            Some(TransformType::RemoveAllFromArray(rm)) => {
                let vals = match current.and_then(|c| c.value_type) {
                    Some(ValueType::ArrayValue(a)) => {
                        a.values.into_iter().filter(|x| !rm.values.iter().any(|r| query::equal(x, r))).collect()
                    }
                    _ => vec![],
                };
                query::array(vals)
            }
            None => return Err(Status::invalid_argument("A field transform needs a type")),
        };
        query::set_field(fields, &path, next.clone());
        results.push(next);
    }
    Ok(results)
}

/// `increment`: integers stay integers (saturating on overflow); a double makes the result double.
fn numeric(current: Option<&Value>, by: &Value, int_op: impl Fn(i64, i64) -> Option<i64>, f_op: impl Fn(f64, f64) -> f64) -> Value {
    let base = match current.and_then(|c| c.value_type.clone()) {
        Some(v @ (ValueType::IntegerValue(_) | ValueType::DoubleValue(_))) => v,
        _ => ValueType::IntegerValue(0),
    };
    let v = match (base, by.value_type.clone()) {
        (ValueType::IntegerValue(a), Some(ValueType::IntegerValue(b))) => {
            ValueType::IntegerValue(int_op(a, b).unwrap_or(if b > 0 { i64::MAX } else { i64::MIN }))
        }
        (ValueType::IntegerValue(a), Some(ValueType::DoubleValue(b))) => ValueType::DoubleValue(f_op(a as f64, b)),
        (ValueType::DoubleValue(a), Some(ValueType::IntegerValue(b))) => ValueType::DoubleValue(f_op(a, b as f64)),
        (ValueType::DoubleValue(a), Some(ValueType::DoubleValue(b))) => ValueType::DoubleValue(f_op(a, b)),
        (base, _) => base,
    };
    Value { value_type: Some(v) }
}

fn extreme(current: Option<&Value>, by: &Value, max: bool) -> Value {
    match current {
        Some(c) if matches!(c.value_type, Some(ValueType::IntegerValue(_) | ValueType::DoubleValue(_))) => {
            let o = query::compare(by, c);
            if (max && o.is_gt()) || (!max && o.is_lt()) { by.clone() } else { c.clone() }
        }
        _ => by.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twinbase_firestore_proto::v1::document_transform::FieldTransform;
    use twinbase_firestore_proto::v1::structured_query::{self as sq, CollectionSelector, FieldReference, Filter};
    use twinbase_firestore_proto::v1::{DocumentMask, Precondition};

    const DB: &str = "projects/demo/databases/(default)";

    fn set(store: &FirestoreStore, rel: &str, fields: Json) {
        store
            .commit(
                DB,
                &[Write {
                    operation: Some(Operation::Update(Document {
                        name: doc_name(DB, rel),
                        fields: fjson::fields_from_plain(&fields, DB).unwrap(),
                        ..Default::default()
                    })),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
    }

    fn field_filter(path: &str, op: sq::field_filter::Operator, value: Json) -> Filter {
        Filter {
            filter_type: Some(sq::filter::FilterType::FieldFilter(sq::FieldFilter {
                field: Some(FieldReference { field_path: path.into() }),
                op: op as i32,
                value: Some(fjson::from_plain(&value, DB).unwrap()),
            })),
        }
    }

    #[test]
    fn writes_transforms_preconditions_and_queries() {
        let s = FirestoreStore::new(None);
        for (id, age, city) in [("ann", 31, "Lisbon"), ("bob", 25, "Porto"), ("cid", 40, "Lisbon"), ("dee", 19, "Faro")] {
            set(&s, &format!("users/{id}"), json!({ "age": age, "city": city, "tags": ["a"] }));
        }
        set(&s, "users/ann/orders/o1", json!({ "total": 10 }));
        set(&s, "shops/x/orders/o2", json!({ "total": 5 }));
        // Update with mask and transforms.
        let w = Write {
            operation: Some(Operation::Update(Document {
                name: doc_name(DB, "users/ann"),
                fields: fjson::fields_from_plain(&json!({ "city": "Porto" }), DB).unwrap(),
                ..Default::default()
            })),
            update_mask: Some(DocumentMask { field_paths: vec!["city".into(), "gone".into()] }),
            update_transforms: vec![
                FieldTransform {
                    field_path: "visits".into(),
                    transform_type: Some(TransformType::Increment(fjson::from_plain(&json!(2), DB).unwrap())),
                },
                FieldTransform {
                    field_path: "tags".into(),
                    transform_type: Some(TransformType::AppendMissingElements(twinbase_firestore_proto::v1::ArrayValue {
                        values: vec![fjson::from_plain(&json!("a"), DB).unwrap(), fjson::from_plain(&json!("b"), DB).unwrap()],
                    })),
                },
                FieldTransform {
                    field_path: "seen".into(),
                    transform_type: Some(TransformType::SetToServerValue(ServerValue::RequestTime as i32)),
                },
            ],
            current_document: Some(Precondition { condition_type: Some(precondition::ConditionType::Exists(true)) }),
        };
        s.commit(DB, &[w], None).unwrap();
        let ann = s.get(DB, "users/ann", None).unwrap().unwrap();
        let plain = fjson::fields_to_plain(&ann.fields);
        assert_eq!(plain["city"], "Porto");
        assert_eq!(plain["visits"], 2);
        assert_eq!(plain["tags"], json!(["a", "b"]));
        assert!(plain["seen"]["__timestamp"].is_string());
        assert_eq!(plain["age"], 31);
        let create_missing = Write {
            operation: Some(Operation::Delete(doc_name(DB, "users/zed"))),
            current_document: Some(Precondition { condition_type: Some(precondition::ConditionType::Exists(true)) }),
            ..Default::default()
        };
        assert_eq!(s.commit(DB, &[create_missing], None).unwrap_err().code(), tonic::Code::NotFound);

        // where age >= 20 order by age desc limit 2
        let q = StructuredQuery {
            from: vec![CollectionSelector { collection_id: "users".into(), all_descendants: false }],
            r#where: Some(field_filter("age", sq::field_filter::Operator::GreaterThanOrEqual, json!(20))),
            order_by: vec![sq::Order {
                field: Some(FieldReference { field_path: "age".into() }),
                direction: sq::Direction::Descending as i32,
            }],
            limit: Some(2),
            ..Default::default()
        };
        let names: Vec<String> =
            s.run_query(DB, "", &q, None).unwrap().into_iter().map(|d| d.name.rsplit('/').next().unwrap().to_owned()).collect();
        assert_eq!(names, vec!["cid", "ann"]);
        let group = StructuredQuery {
            from: vec![CollectionSelector { collection_id: "orders".into(), all_descendants: true }],
            ..Default::default()
        };
        assert_eq!(s.run_query(DB, "", &group, None).unwrap().len(), 2);
        assert_eq!(
            s.run_query(
                DB,
                "users/ann",
                &StructuredQuery {
                    from: vec![CollectionSelector { collection_id: "orders".into(), all_descendants: false }],
                    ..Default::default()
                },
                None
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(s.collection_ids(DB, ""), vec!["shops", "users"]);
        assert_eq!(s.collection_ids(DB, "users/ann"), vec!["orders"]);
        assert_eq!(s.list(DB, "users").len(), 4);
    }

    #[test]
    fn transactions_abort_on_contention_and_snapshots_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firestore.json");
        let s = FirestoreStore::new(Some(path.clone()));
        set(&s, "counters/c", json!({ "n": 1 }));
        let t = s.begin(DB, false);
        s.get(DB, "counters/c", Some(&t)).unwrap();
        set(&s, "counters/c", json!({ "n": 5 }));
        let w = Write {
            operation: Some(Operation::Update(Document {
                name: doc_name(DB, "counters/c"),
                fields: fjson::fields_from_plain(&json!({ "n": 2 }), DB).unwrap(),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(s.commit(DB, std::slice::from_ref(&w), Some(&t)).unwrap_err().code(), tonic::Code::Aborted);
        let t2 = s.begin(DB, false);
        s.get(DB, "counters/c", Some(&t2)).unwrap();
        s.commit(DB, &[w], Some(&t2)).unwrap();
        s.save_if_dirty().unwrap();
        let again = FirestoreStore::new(Some(path));
        assert_eq!(fjson::fields_to_plain(&again.get(DB, "counters/c", None).unwrap().unwrap().fields), json!({ "n": 2 }));
        assert_eq!(again.export(), json!({ "documents": { "counters/c": { "n": 2 } } }));
    }
}
