//! MongoDB driver: database / collection browsing, paged documents, edits, indexes, schema
//! sampling and a mongosh-style console.

use std::time::{Duration, Instant};

use bson::{Bson, Document, doc};
use futures::TryStreamExt;
use mongodb::options::{ClientOptions, ServerAddress, Tls, TlsOptions};
use serde::Serialize;

use crate::mongo_shell::{self, Segment, Statement};
use crate::{ClientError, ConnectionConfig, Result, TlsMode};

#[derive(Clone)]
pub struct MongoClient {
    client: mongodb::Client,
    /// Database used when a call does not name one.
    pub database: String,
    tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
}

fn m_err(e: mongodb::error::Error) -> ClientError {
    use mongodb::error::{ErrorKind, WriteFailure};
    let (code, message) = match e.kind.as_ref() {
        ErrorKind::Command(c) => (Some(c.code), c.message.clone()),
        ErrorKind::Write(WriteFailure::WriteError(w)) => (Some(w.code), w.message.clone()),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => (Some(w.code), w.message.clone()),
        ErrorKind::InsertMany(i) => match i.write_errors.as_ref().and_then(|w| w.first()) {
            Some(w) => (Some(w.code), w.message.clone()),
            None => (None, e.to_string()),
        },
        ErrorKind::BulkWrite(_) => (None, e.to_string()),
        ErrorKind::Authentication { message, .. } => (Some(18), format!("authentication failed: {message}")),
        ErrorKind::ServerSelection { message, .. } => (None, format!("cannot reach the server: {message}")),
        _ => (None, e.to_string()),
    };
    let mut err = ClientError::new(message);
    err.code = code.map(|c| c.to_string());
    err
}

fn parse_err(e: mongo_shell::ParseError) -> ClientError {
    ClientError::new(e.to_string())
}

/// A document as relaxed Extended JSON for the UI and agents.
pub fn to_json(d: Document) -> serde_json::Value {
    Bson::Document(d).into_relaxed_extjson()
}

/// A value from the UI (relaxed or canonical Extended JSON).
pub fn from_json(v: serde_json::Value) -> Result<Bson> {
    Bson::try_from(v).map_err(|e| ClientError::new(format!("invalid Extended JSON: {e}")))
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseInfo {
    pub name: String,
    pub size_bytes: i64,
    pub empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    /// `collection`, `view` or `timeseries`.
    #[serde(rename = "type")]
    pub kind: String,
    pub count: Option<u64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FindParams {
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub projection: String,
    #[serde(default)]
    pub skip: u64,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Debug, Clone, Serialize)]
pub struct DocumentsPage {
    pub documents: Vec<serde_json::Value>,
    /// Number of documents matching the filter (`None` when counting timed out).
    pub total: Option<u64>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldStats {
    pub path: String,
    /// BSON type name → number of sampled documents with that type at this path.
    pub types: Vec<(String, u64)>,
    pub present: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaSample {
    pub sampled: u64,
    pub fields: Vec<FieldStats>,
}

/// One statement's result in the console.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MongoOutput {
    pub statement: String,
    pub database: String,
    /// Documents returned by find / aggregate / listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<Vec<serde_json::Value>>,
    /// A single value (a count, a command reply, write results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// More documents existed than were returned.
    pub truncated: bool,
    pub duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientError>,
}

impl MongoClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<MongoClient> {
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let uri_given = config.host.starts_with("mongodb://") || config.host.starts_with("mongodb+srv://");
        let mut options = if uri_given {
            if config.via.is_some() && config.host.starts_with("mongodb+srv://") {
                return Err(ClientError::new(
                    "mongodb+srv:// connection strings cannot go through an SSH tunnel; use a mongodb:// string with one host",
                ));
            }
            ClientOptions::parse(&config.host).await.map_err(m_err)?
        } else {
            let host = if config.host.is_empty() { "127.0.0.1" } else { config.host.as_str() };
            let port = if config.port == 0 { 27017 } else { config.port };
            let mut o = ClientOptions::default();
            o.hosts = vec![ServerAddress::Tcp { host: host.to_owned(), port: Some(port) }];
            o.direct_connection = Some(true);
            o
        };
        if let Some(via) = config.via {
            options.hosts = vec![ServerAddress::Tcp { host: via.ip().to_string(), port: Some(via.port()) }];
            options.direct_connection = Some(true);
        }
        if !config.user.is_empty() {
            let mut cred = options.credential.take().unwrap_or_default();
            cred.username = Some(config.user.clone());
            if config.password.is_some() {
                cred.password = config.password.clone();
            }
            options.credential = Some(cred);
        } else if let (Some(cred), Some(pw)) = (options.credential.as_mut(), &config.password)
            && cred.password.is_none()
        {
            cred.password = Some(pw.clone());
        }
        match config.tls {
            TlsMode::Disable if !uri_given => options.tls = Some(Tls::Disabled),
            TlsMode::Disable => {}
            TlsMode::Require => options.tls = Some(Tls::Enabled(TlsOptions::builder().allow_invalid_certificates(true).build())),
            TlsMode::VerifyFull => {
                if config.via.is_some() {
                    return Err(ClientError::new(
                        "certificate verification through an SSH tunnel is not supported for MongoDB; choose TLS \"Required\"",
                    ));
                }
                options.tls = Some(Tls::Enabled(TlsOptions::default()));
            }
        }
        options.app_name = Some("TwinBase".into());
        options.server_selection_timeout = Some(timeout);
        options.connect_timeout = Some(timeout);
        let database =
            config.database.clone().filter(|d| !d.is_empty()).or_else(|| options.default_database.clone()).unwrap_or_else(|| "test".into());
        let client = mongodb::Client::with_options(options).map_err(m_err)?;
        client.database("admin").run_command(doc! { "ping": 1 }).await.map_err(m_err)?;
        Ok(MongoClient { client, database, tunnel: None })
    }

    pub fn with_tunnel(mut self, t: std::sync::Arc<crate::ssh::Tunnel>) -> Self {
        self.tunnel = Some(t);
        self
    }

    fn db(&self, name: Option<&str>) -> mongodb::Database {
        self.client.database(name.filter(|n| !n.is_empty()).unwrap_or(&self.database))
    }

    pub async fn server_version(&self) -> Result<String> {
        let info = self.client.database("admin").run_command(doc! { "buildInfo": 1 }).await.map_err(m_err)?;
        Ok(info.get_str("version").unwrap_or("").to_owned())
    }

    pub async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        match self.client.list_databases().await {
            Ok(list) => Ok(list
                .into_iter()
                .map(|d| DatabaseInfo { name: d.name, size_bytes: d.size_on_disk as i64, empty: d.size_on_disk == 0 })
                .collect()),
            // Users limited to one database may not list them all.
            Err(e) if matches!(e.kind.as_ref(), mongodb::error::ErrorKind::Command(c) if c.code == 13) => {
                Ok(vec![DatabaseInfo { name: self.database.clone(), size_bytes: 0, empty: false }])
            }
            Err(e) => Err(m_err(e)),
        }
    }

    pub async fn collections(&self, db: Option<&str>) -> Result<Vec<CollectionInfo>> {
        let d = self.db(db);
        let specs: Vec<mongodb::results::CollectionSpecification> =
            d.list_collections().await.map_err(m_err)?.try_collect().await.map_err(m_err)?;
        let mut out = Vec::with_capacity(specs.len());
        for s in specs {
            let kind = format!("{:?}", s.collection_type).to_lowercase();
            let count = if kind == "collection" { d.collection::<Document>(&s.name).estimated_document_count().await.ok() } else { None };
            out.push(CollectionInfo { name: s.name, kind, count });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub async fn find(&self, db: Option<&str>, collection: &str, p: &FindParams) -> Result<DocumentsPage> {
        let started = Instant::now();
        let filter = mongo_shell::parse_document(&p.filter).map_err(parse_err)?;
        let sort = mongo_shell::parse_document(&p.sort).map_err(parse_err)?;
        let projection = mongo_shell::parse_document(&p.projection).map_err(parse_err)?;
        let coll = self.db(db).collection::<Document>(collection);
        let mut find = coll.find(filter.clone()).skip(p.skip);
        if p.limit > 0 {
            find = find.limit(p.limit);
        }
        if !sort.is_empty() {
            find = find.sort(sort);
        }
        if !projection.is_empty() {
            find = find.projection(projection);
        }
        let docs: Vec<Document> = find.await.map_err(m_err)?.try_collect().await.map_err(m_err)?;
        let total = if filter.is_empty() {
            coll.estimated_document_count().await.ok()
        } else {
            coll.count_documents(filter).max_time(Duration::from_secs(5)).await.ok()
        };
        Ok(DocumentsPage { documents: docs.into_iter().map(to_json).collect(), total, duration_ms: started.elapsed().as_millis() as u64 })
    }

    /// Insert documents written in shell syntax (one object or an array of objects).
    pub async fn insert(&self, db: Option<&str>, collection: &str, text: &str) -> Result<Vec<serde_json::Value>> {
        let docs = mongo_shell::parse_documents(text).map_err(parse_err)?;
        if docs.is_empty() {
            return Err(ClientError::new("nothing to insert"));
        }
        let r = self.db(db).collection::<Document>(collection).insert_many(docs).await.map_err(m_err)?;
        let mut ids: Vec<(usize, Bson)> = r.inserted_ids.into_iter().collect();
        ids.sort_by_key(|(i, _)| *i);
        Ok(ids.into_iter().map(|(_, id)| id.into_relaxed_extjson()).collect())
    }

    /// Replace the document with `_id` = `id`; fails when it no longer exists.
    pub async fn replace(&self, db: Option<&str>, collection: &str, id: serde_json::Value, text: &str) -> Result<()> {
        let id = from_json(id)?;
        let mut d = mongo_shell::parse_document(text).map_err(parse_err)?;
        if let Some(new_id) = d.get("_id")
            && new_id != &id
        {
            return Err(ClientError::new("_id cannot be changed; insert a copy and delete the original instead"));
        }
        d.remove("_id");
        let r = self.db(db).collection::<Document>(collection).replace_one(doc! { "_id": id }, d).await.map_err(m_err)?;
        if r.matched_count == 0 {
            return Err(ClientError::new("the document no longer exists (it was deleted or its _id changed); reload and try again"));
        }
        Ok(())
    }

    pub async fn update(&self, db: Option<&str>, collection: &str, filter: &str, update: &str, many: bool) -> Result<(u64, u64)> {
        let filter = mongo_shell::parse_document(filter).map_err(parse_err)?;
        let update = mongo_shell::parse_value(update).map_err(parse_err)?;
        let update: mongodb::options::UpdateModifications = match update {
            Bson::Document(d) => d.into(),
            Bson::Array(p) => p.into_iter().filter_map(|s| s.as_document().cloned()).collect::<Vec<_>>().into(),
            _ => return Err(ClientError::new("the update must be an object or a pipeline")),
        };
        let coll = self.db(db).collection::<Document>(collection);
        let r = if many { coll.update_many(filter, update).await } else { coll.update_one(filter, update).await }.map_err(m_err)?;
        Ok((r.matched_count, r.modified_count))
    }

    pub async fn delete(&self, db: Option<&str>, collection: &str, filter: &str, many: bool) -> Result<u64> {
        let filter = mongo_shell::parse_document(filter).map_err(parse_err)?;
        let coll = self.db(db).collection::<Document>(collection);
        let r = if many { coll.delete_many(filter).await } else { coll.delete_one(filter).await }.map_err(m_err)?;
        Ok(r.deleted_count)
    }

    /// Delete documents by `_id` (Extended JSON values from the UI).
    pub async fn delete_ids(&self, db: Option<&str>, collection: &str, ids: Vec<serde_json::Value>) -> Result<u64> {
        let ids: Vec<Bson> = ids.into_iter().map(from_json).collect::<Result<_>>()?;
        let r = self.db(db).collection::<Document>(collection).delete_many(doc! { "_id": { "$in": ids } }).await.map_err(m_err)?;
        Ok(r.deleted_count)
    }

    pub async fn aggregate(
        &self,
        db: Option<&str>,
        collection: &str,
        pipeline: &str,
        max_docs: usize,
    ) -> Result<(Vec<serde_json::Value>, bool)> {
        let pipeline = mongo_shell::parse_documents(pipeline).map_err(parse_err)?;
        let cursor = self.db(db).collection::<Document>(collection).aggregate(pipeline).await.map_err(m_err)?;
        collect_limited(cursor, max_docs).await
    }

    pub async fn indexes(&self, db: Option<&str>, collection: &str) -> Result<Vec<serde_json::Value>> {
        let specs: Vec<Document> = match self.db(db).run_command(doc! { "listIndexes": collection }).await {
            Ok(reply) => reply
                .get_document("cursor")
                .ok()
                .and_then(|c| c.get_array("firstBatch").ok())
                .map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect())
                .unwrap_or_default(),
            Err(e) if matches!(e.kind.as_ref(), mongodb::error::ErrorKind::Command(c) if c.code == 26) => vec![],
            Err(e) => return Err(m_err(e)),
        };
        Ok(specs.into_iter().map(to_json).collect())
    }

    pub async fn create_index(&self, db: Option<&str>, collection: &str, keys: &str, unique: bool, name: Option<String>) -> Result<String> {
        let keys = mongo_shell::parse_document(keys).map_err(parse_err)?;
        if keys.is_empty() {
            return Err(ClientError::new("choose at least one field for the index"));
        }
        let opts =
            mongodb::options::IndexOptions::builder().unique(unique.then_some(true)).name(name.filter(|n| !n.trim().is_empty())).build();
        let r = self
            .db(db)
            .collection::<Document>(collection)
            .create_index(mongodb::IndexModel::builder().keys(keys).options(opts).build())
            .await
            .map_err(m_err)?;
        Ok(r.index_name)
    }

    pub async fn drop_index(&self, db: Option<&str>, collection: &str, name: &str) -> Result<()> {
        self.db(db).collection::<Document>(collection).drop_index(name).await.map_err(m_err)
    }

    pub async fn create_collection(&self, db: Option<&str>, collection: &str) -> Result<()> {
        self.db(db).create_collection(collection).await.map_err(m_err)
    }

    pub async fn drop_collection(&self, db: Option<&str>, collection: &str) -> Result<()> {
        self.db(db).collection::<Document>(collection).drop().await.map_err(m_err)
    }

    /// Field paths and their types in a sample of documents.
    pub async fn schema(&self, db: Option<&str>, collection: &str, sample: u32) -> Result<SchemaSample> {
        let coll = self.db(db).collection::<Document>(collection);
        let docs: Vec<Document> = match coll.aggregate(vec![doc! { "$sample": { "size": sample as i32 } }]).await {
            Ok(c) => c.try_collect().await.map_err(m_err)?,
            Err(_) => coll.find(doc! {}).limit(sample as i64).await.map_err(m_err)?.try_collect().await.map_err(m_err)?,
        };
        Ok(sample_schema(&docs))
    }

    pub async fn command(&self, db: Option<&str>, command: Document) -> Result<serde_json::Value> {
        Ok(to_json(self.db(db).run_command(command).await.map_err(m_err)?))
    }

    /// Run a mongosh-style script; each statement gives one output (errors stop the script).
    pub async fn run(&self, db: Option<&str>, script: &str, max_docs: usize) -> Result<Vec<MongoOutput>> {
        let statements = mongo_shell::parse_script(script).map_err(parse_err)?;
        if statements.is_empty() {
            return Err(ClientError::new("nothing to run"));
        }
        let mut current = db.filter(|d| !d.is_empty()).unwrap_or(&self.database).to_owned();
        let mut out = Vec::new();
        for (st, text) in statements {
            let started = Instant::now();
            let mut o = MongoOutput { statement: text, database: current.clone(), ..Default::default() };
            let r = match st {
                Statement::Use(name) => {
                    current = name.clone();
                    o.database = name.clone();
                    o.message = Some(format!("switched to db {name}"));
                    Ok(())
                }
                Statement::ShowDatabases => self.databases().await.map(|dbs| {
                    o.documents = Some(
                        dbs.into_iter()
                            .map(|d| serde_json::json!({ "name": d.name, "sizeOnDisk": d.size_bytes, "empty": d.empty }))
                            .collect(),
                    );
                }),
                Statement::ShowCollections => self.collections(Some(&current)).await.map(|cs| {
                    o.documents =
                        Some(cs.into_iter().map(|c| serde_json::json!({ "name": c.name, "type": c.kind, "count": c.count })).collect());
                }),
                Statement::Db(segs) => self.run_chain(&current, segs, max_docs, &mut o).await,
            };
            o.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
            let failed = r.is_err();
            if let Err(e) = r {
                o.error = Some(e);
            }
            out.push(o);
            if failed {
                break;
            }
        }
        Ok(out)
    }

    async fn run_chain(&self, current: &str, segs: Vec<Segment>, max_docs: usize, o: &mut MongoOutput) -> Result<()> {
        let mut segs = segs.into_iter().peekable();
        let mut db_name = current.to_owned();
        // db.getSiblingDB("x")…
        if let Some(Segment::Call(name, args)) = segs.peek()
            && name == "getSiblingDB"
        {
            db_name = arg_str(args, 0, "getSiblingDB")?;
            o.database = db_name.clone();
            segs.next();
        }
        let db = self.client.database(&db_name);
        let first = segs.next().ok_or_else(|| ClientError::new("incomplete statement"))?;
        let collection = match first {
            Segment::Prop(name) => name,
            Segment::Call(name, args) => match name.as_str() {
                "getCollection" => arg_str(&args, 0, "getCollection")?,
                _ => return self.db_method(&db, &name, args, o).await,
            },
        };
        let coll = db.collection::<Document>(&collection);
        let Some(Segment::Call(method, args)) = segs.next() else {
            return Err(ClientError::new(format!("call a method on the collection, e.g. db.{collection}.find({{}})")));
        };
        let rest: Vec<(String, Vec<Bson>)> = segs
            .map(|s| match s {
                Segment::Call(n, a) => Ok((n, a)),
                Segment::Prop(p) => Err(ClientError::new(format!("unexpected .{p}"))),
            })
            .collect::<Result<_>>()?;
        let opt_doc = |args: &[Bson], i: usize| -> Result<Document> {
            match args.get(i) {
                None | Some(Bson::Null) => Ok(Document::new()),
                Some(Bson::Document(d)) => Ok(d.clone()),
                Some(other) => Err(ClientError::new(format!(
                    "argument {} of {method}() must be an object, not {}",
                    i + 1,
                    mongo_shell::type_name(other)
                ))),
            }
        };
        let write = |v: serde_json::Value, msg: String, o: &mut MongoOutput| {
            o.value = Some(v);
            o.message = Some(msg);
        };
        match method.as_str() {
            "find" | "findOne" => {
                let filter = opt_doc(&args, 0)?;
                let mut find = coll.find(filter.clone());
                let projection = opt_doc(&args, 1)?;
                if !projection.is_empty() {
                    find = find.projection(projection);
                }
                let mut limit: Option<i64> = (method == "findOne").then_some(1);
                let mut explain = false;
                let mut count = false;
                for (m, a) in &rest {
                    match m.as_str() {
                        "sort" => find = find.sort(opt_doc(a, 0)?),
                        "limit" => limit = Some(arg_i64(a, 0, "limit")?),
                        "skip" => find = find.skip(arg_i64(a, 0, "skip")?.max(0) as u64),
                        "projection" | "project" => find = find.projection(opt_doc(a, 0)?),
                        "hint" => {
                            find = find.hint(match a.first() {
                                Some(Bson::String(s)) => mongodb::options::Hint::Name(s.clone()),
                                Some(Bson::Document(d)) => mongodb::options::Hint::Keys(d.clone()),
                                _ => return Err(ClientError::new("hint() takes an index name or key pattern")),
                            })
                        }
                        "maxTimeMS" => find = find.max_time(Duration::from_millis(arg_i64(a, 0, "maxTimeMS")?.max(0) as u64)),
                        "explain" => explain = true,
                        "count" | "itcount" | "size" => count = true,
                        "toArray" | "pretty" | "batchSize" | "allowDiskUse" | "comment" | "collation" => {}
                        other => return Err(ClientError::new(format!("unsupported cursor method .{other}()"))),
                    }
                }
                if count {
                    let n = coll.count_documents(filter).await.map_err(m_err)?;
                    o.value = Some(serde_json::json!(n));
                    return Ok(());
                }
                if explain {
                    let mut cmd = doc! { "find": &collection, "filter": filter };
                    if let Some(l) = limit {
                        cmd.insert("limit", l);
                    }
                    o.value = Some(to_json(db.run_command(doc! { "explain": cmd, "verbosity": "executionStats" }).await.map_err(m_err)?));
                    return Ok(());
                }
                // Fetch one more than shown to know whether the result was cut.
                let cap = limit.filter(|l| *l > 0).map(|l| (l as usize).min(max_docs)).unwrap_or(max_docs);
                find = find.limit(cap as i64 + 1);
                let (docs, truncated) = collect_limited(find.await.map_err(m_err)?, cap).await?;
                o.truncated = truncated && limit.is_none_or(|l| l <= 0 || l as usize > cap);
                if method == "findOne" {
                    o.value = Some(docs.into_iter().next().unwrap_or(serde_json::Value::Null));
                } else {
                    o.documents = Some(docs);
                }
            }
            "aggregate" => {
                let pipeline = match args.first() {
                    Some(Bson::Array(a)) => a
                        .iter()
                        .map(|s| s.as_document().cloned().ok_or_else(|| ClientError::new("pipeline stages must be objects")))
                        .collect::<Result<Vec<_>>>()?,
                    None => vec![],
                    Some(Bson::Document(_)) => args.iter().filter_map(|s| s.as_document().cloned()).collect(),
                    Some(other) => {
                        return Err(ClientError::new(format!(
                            "aggregate() takes an array of stages, not {}",
                            mongo_shell::type_name(other)
                        )));
                    }
                };
                if rest.iter().any(|(m, _)| m == "explain") {
                    o.value = Some(to_json(
                        db.run_command(doc! { "aggregate": &collection, "pipeline": pipeline, "explain": true }).await.map_err(m_err)?,
                    ));
                    return Ok(());
                }
                let (docs, truncated) = collect_limited(coll.aggregate(pipeline).await.map_err(m_err)?, max_docs).await?;
                o.documents = Some(docs);
                o.truncated = truncated;
            }
            "countDocuments" | "count" => o.value = Some(serde_json::json!(coll.count_documents(opt_doc(&args, 0)?).await.map_err(m_err)?)),
            "estimatedDocumentCount" => o.value = Some(serde_json::json!(coll.estimated_document_count().await.map_err(m_err)?)),
            "distinct" => {
                let key = arg_str(&args, 0, "distinct")?;
                let values = coll.distinct(key, opt_doc(&args, 1)?).await.map_err(m_err)?;
                o.value = Some(Bson::Array(values).into_relaxed_extjson());
            }
            "insertOne" => {
                let d = match args.first() {
                    Some(Bson::Document(d)) => d.clone(),
                    _ => return Err(ClientError::new("insertOne() takes a document")),
                };
                let r = coll.insert_one(d).await.map_err(m_err)?;
                write(
                    serde_json::json!({ "acknowledged": true, "insertedId": r.inserted_id.into_relaxed_extjson() }),
                    "Inserted 1 document".into(),
                    o,
                );
            }
            "insertMany" => {
                let docs: Vec<Document> = match args.first() {
                    Some(Bson::Array(a)) => a
                        .iter()
                        .map(|d| d.as_document().cloned().ok_or_else(|| ClientError::new("insertMany() takes an array of documents")))
                        .collect::<Result<_>>()?,
                    _ => return Err(ClientError::new("insertMany() takes an array of documents")),
                };
                let n = docs.len();
                let r = coll.insert_many(docs).await.map_err(m_err)?;
                let mut ids: Vec<(usize, Bson)> = r.inserted_ids.into_iter().collect();
                ids.sort_by_key(|(i, _)| *i);
                write(
                    serde_json::json!({ "acknowledged": true, "insertedIds": ids.into_iter().map(|(_, id)| id.into_relaxed_extjson()).collect::<Vec<_>>() }),
                    format!("Inserted {n} documents"),
                    o,
                );
            }
            "updateOne" | "updateMany" | "replaceOne" => {
                let filter = opt_doc(&args, 0)?;
                let opts = opt_doc(&args, 2)?;
                let upsert = opts.get_bool("upsert").ok();
                let r = if method == "replaceOne" {
                    let Some(Bson::Document(rep)) = args.get(1) else {
                        return Err(ClientError::new("replaceOne() takes a filter and a replacement document"));
                    };
                    coll.replace_one(filter, rep.clone()).upsert(upsert.unwrap_or(false)).await
                } else {
                    let update: mongodb::options::UpdateModifications = match args.get(1) {
                        Some(Bson::Document(d)) => d.clone().into(),
                        Some(Bson::Array(p)) => p.iter().filter_map(|s| s.as_document().cloned()).collect::<Vec<_>>().into(),
                        _ => return Err(ClientError::new(format!("{method}() takes a filter and an update"))),
                    };
                    let filters: Option<Vec<Document>> =
                        opts.get_array("arrayFilters").ok().map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect());
                    if method == "updateOne" {
                        coll.update_one(filter, update).upsert(upsert.unwrap_or(false)).array_filters(filters.unwrap_or_default()).await
                    } else {
                        coll.update_many(filter, update).upsert(upsert.unwrap_or(false)).array_filters(filters.unwrap_or_default()).await
                    }
                }
                .map_err(m_err)?;
                let upserted = r.upserted_id.map(|id| id.into_relaxed_extjson());
                let msg = match &upserted {
                    Some(_) => "Inserted 1 document (upsert)".to_owned(),
                    None => format!("Matched {}, modified {}", r.matched_count, r.modified_count),
                };
                write(
                    serde_json::json!({ "acknowledged": true, "matchedCount": r.matched_count, "modifiedCount": r.modified_count, "upsertedId": upserted }),
                    msg,
                    o,
                );
            }
            "deleteOne" | "deleteMany" => {
                let filter = opt_doc(&args, 0)?;
                let r =
                    if method == "deleteOne" { coll.delete_one(filter).await } else { coll.delete_many(filter).await }.map_err(m_err)?;
                write(
                    serde_json::json!({ "acknowledged": true, "deletedCount": r.deleted_count }),
                    format!("Deleted {}", r.deleted_count),
                    o,
                );
            }
            "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" => {
                let filter = opt_doc(&args, 0)?;
                let opts = opt_doc(&args, if method == "findOneAndDelete" { 1 } else { 2 })?;
                let after = opts.get_str("returnDocument").ok() == Some("after") || opts.get_bool("returnNewDocument").unwrap_or(false);
                let rd = if after { mongodb::options::ReturnDocument::After } else { mongodb::options::ReturnDocument::Before };
                let sort = opts.get_document("sort").ok().cloned();
                let upsert = opts.get_bool("upsert").unwrap_or(false);
                let r = match method.as_str() {
                    "findOneAndDelete" => {
                        let mut q = coll.find_one_and_delete(filter);
                        if let Some(s) = sort {
                            q = q.sort(s);
                        }
                        q.await
                    }
                    "findOneAndReplace" => {
                        let Some(Bson::Document(rep)) = args.get(1) else {
                            return Err(ClientError::new("findOneAndReplace() takes a filter and a replacement"));
                        };
                        let mut q = coll.find_one_and_replace(filter, rep.clone()).return_document(rd).upsert(upsert);
                        if let Some(s) = sort {
                            q = q.sort(s);
                        }
                        q.await
                    }
                    _ => {
                        let update: mongodb::options::UpdateModifications = match args.get(1) {
                            Some(Bson::Document(d)) => d.clone().into(),
                            Some(Bson::Array(p)) => p.iter().filter_map(|s| s.as_document().cloned()).collect::<Vec<_>>().into(),
                            _ => return Err(ClientError::new("findOneAndUpdate() takes a filter and an update")),
                        };
                        let mut q = coll.find_one_and_update(filter, update).return_document(rd).upsert(upsert);
                        if let Some(s) = sort {
                            q = q.sort(s);
                        }
                        q.await
                    }
                }
                .map_err(m_err)?;
                o.value = Some(r.map(to_json).unwrap_or(serde_json::Value::Null));
            }
            "createIndex" => {
                let keys = opt_doc(&args, 0)?;
                let opts = opt_doc(&args, 1)?;
                let mut spec = doc! { "key": keys.clone() };
                spec.extend(opts);
                if !spec.contains_key("name") {
                    spec.insert(
                        "name",
                        keys.iter()
                            .map(|(k, v)| {
                                format!(
                                    "{k}_{}",
                                    match v {
                                        Bson::String(s) => s.clone(),
                                        other => other.to_string(),
                                    }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("_"),
                    );
                }
                let name = spec.get_str("name").unwrap_or_default().to_owned();
                db.run_command(doc! { "createIndexes": &collection, "indexes": [spec] }).await.map_err(m_err)?;
                o.value = Some(serde_json::json!(name));
            }
            "getIndexes" | "indexes" => o.documents = Some(self.indexes(Some(&db_name), &collection).await?),
            "dropIndex" => {
                let index = args.first().cloned().ok_or_else(|| ClientError::new("dropIndex() takes an index name"))?;
                o.value = Some(to_json(db.run_command(doc! { "dropIndexes": &collection, "index": index }).await.map_err(m_err)?));
            }
            "dropIndexes" => {
                o.value = Some(to_json(db.run_command(doc! { "dropIndexes": &collection, "index": "*" }).await.map_err(m_err)?))
            }
            "drop" => {
                coll.drop().await.map_err(m_err)?;
                o.value = Some(serde_json::json!(true));
            }
            "renameCollection" => {
                let to = arg_str(&args, 0, "renameCollection")?;
                o.value = Some(to_json(
                    self.client
                        .database("admin")
                        .run_command(doc! { "renameCollection": format!("{db_name}.{collection}"), "to": format!("{db_name}.{to}") })
                        .await
                        .map_err(m_err)?,
                ));
            }
            "stats" => o.value = Some(to_json(db.run_command(doc! { "collStats": &collection }).await.map_err(m_err)?)),
            other => return Err(ClientError::new(format!("unsupported collection method {other}()"))),
        }
        Ok(())
    }

    async fn db_method(&self, db: &mongodb::Database, name: &str, args: Vec<Bson>, o: &mut MongoOutput) -> Result<()> {
        match name {
            "runCommand" | "adminCommand" => {
                let cmd = match args.into_iter().next() {
                    Some(Bson::Document(d)) => d,
                    Some(Bson::String(s)) => doc! { s: 1 },
                    _ => return Err(ClientError::new(format!("{name}() takes a command document"))),
                };
                let target = if name == "adminCommand" { self.client.database("admin") } else { db.clone() };
                o.value = Some(to_json(target.run_command(cmd).await.map_err(m_err)?));
            }
            "getCollectionNames" => {
                let mut names = db.list_collection_names().await.map_err(m_err)?;
                names.sort();
                o.value = Some(serde_json::json!(names));
            }
            "getCollectionInfos" | "listCollections" => {
                let specs: Vec<Document> = db
                    .list_collections()
                    .await
                    .map_err(m_err)?
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(m_err)?
                    .into_iter()
                    .map(|s| doc! { "name": s.name, "type": format!("{:?}", s.collection_type).to_lowercase() })
                    .collect();
                o.documents = Some(specs.into_iter().map(to_json).collect());
            }
            "createCollection" => {
                let n = arg_str(&args, 0, "createCollection")?;
                db.create_collection(&n).await.map_err(m_err)?;
                o.message = Some(format!("Created collection {n}"));
            }
            "dropDatabase" => {
                db.drop().await.map_err(m_err)?;
                o.message = Some(format!("Dropped database {}", db.name()));
            }
            "stats" => o.value = Some(to_json(db.run_command(doc! { "dbStats": 1 }).await.map_err(m_err)?)),
            "serverStatus" => {
                o.value = Some(to_json(self.client.database("admin").run_command(doc! { "serverStatus": 1 }).await.map_err(m_err)?))
            }
            "version" => o.value = Some(serde_json::json!(self.server_version().await?)),
            "hostInfo" => o.value = Some(to_json(self.client.database("admin").run_command(doc! { "hostInfo": 1 }).await.map_err(m_err)?)),
            "getName" => o.value = Some(serde_json::json!(db.name())),
            other => return Err(ClientError::new(format!("unsupported database method db.{other}()"))),
        }
        Ok(())
    }
}

fn arg_str(args: &[Bson], i: usize, f: &str) -> Result<String> {
    match args.get(i) {
        Some(Bson::String(s)) => Ok(s.clone()),
        _ => Err(ClientError::new(format!("{f}() takes a name (string)"))),
    }
}

fn arg_i64(args: &[Bson], i: usize, f: &str) -> Result<i64> {
    match args.get(i) {
        Some(Bson::Int32(n)) => Ok(*n as i64),
        Some(Bson::Int64(n)) => Ok(*n),
        Some(Bson::Double(n)) => Ok(*n as i64),
        _ => Err(ClientError::new(format!("{f}() takes a number"))),
    }
}

async fn collect_limited(mut cursor: mongodb::Cursor<Document>, max: usize) -> Result<(Vec<serde_json::Value>, bool)> {
    let mut out = Vec::new();
    while let Some(d) = cursor.try_next().await.map_err(m_err)? {
        if out.len() >= max {
            return Ok((out, true));
        }
        out.push(to_json(d));
    }
    Ok((out, false))
}

/// Field paths with their BSON types across sampled documents (nested objects, arrays as `[]`).
pub fn sample_schema(docs: &[Document]) -> SchemaSample {
    use std::collections::BTreeMap;
    let mut fields: indexmap::IndexMap<String, (BTreeMap<String, u64>, u64)> = indexmap::IndexMap::new();
    fn type_name(v: &Bson) -> &'static str {
        match v {
            Bson::Double(_) => "double",
            Bson::String(_) => "string",
            Bson::Document(_) => "object",
            Bson::Array(_) => "array",
            Bson::Binary(b) if b.subtype == bson::spec::BinarySubtype::Uuid => "uuid",
            Bson::Binary(_) => "binData",
            Bson::ObjectId(_) => "objectId",
            Bson::Boolean(_) => "bool",
            Bson::DateTime(_) => "date",
            Bson::Null => "null",
            Bson::RegularExpression(_) => "regex",
            Bson::Int32(_) => "int",
            Bson::Int64(_) => "long",
            Bson::Decimal128(_) => "decimal",
            Bson::Timestamp(_) => "timestamp",
            _ => "other",
        }
    }
    fn walk(prefix: &str, d: &Document, depth: usize, seen: &mut Vec<(String, &'static str)>) {
        for (k, v) in d {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            seen.push((path.clone(), type_name(v)));
            if depth < 4 {
                match v {
                    Bson::Document(sub) => walk(&path, sub, depth + 1, seen),
                    Bson::Array(items) => {
                        let item_path = format!("{path}[]");
                        let mut item_types: Vec<&'static str> = Vec::new();
                        for i in items.iter().take(50) {
                            let t = type_name(i);
                            if !item_types.contains(&t) {
                                item_types.push(t);
                                seen.push((item_path.clone(), t));
                            }
                            if let Bson::Document(sub) = i {
                                walk(&item_path, sub, depth + 1, seen);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for d in docs {
        let mut seen = Vec::new();
        walk("", d, 0, &mut seen);
        let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (path, t) in seen {
            let entry = fields.entry(path.clone()).or_default();
            *entry.0.entry(t.to_owned()).or_default() += 1;
            if counted.insert(path) {
                entry.1 += 1;
            }
        }
    }
    let mut fields: Vec<FieldStats> = fields
        .into_iter()
        .map(|(path, (types, present))| {
            let mut types: Vec<(String, u64)> = types.into_iter().collect();
            types.sort_by_key(|t| std::cmp::Reverse(t.1));
            FieldStats { path, types, present }
        })
        .collect();
    // `_id` first, otherwise in order of first appearance.
    if let Some(i) = fields.iter().position(|f| f.path == "_id") {
        let id = fields.remove(i);
        fields.insert(0, id);
    }
    SchemaSample { sampled: docs.len() as u64, fields }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_sampling() {
        let docs = vec![
            doc! { "_id": 1, "name": "a", "tags": ["x", "y"], "address": { "city": "Lisbon" } },
            doc! { "_id": 2, "name": null, "tags": [], "items": [{ "sku": "s", "qty": 1 }] },
        ];
        let s = sample_schema(&docs);
        assert_eq!(s.sampled, 2);
        let get = |p: &str| s.fields.iter().find(|f| f.path == p).unwrap();
        assert_eq!(get("name").types, vec![("null".to_string(), 1), ("string".to_string(), 1)]);
        assert_eq!(get("tags[]").types, vec![("string".to_string(), 1)]);
        assert_eq!(get("address.city").present, 1);
        assert_eq!(get("items[].qty").types, vec![("int".to_string(), 1)]);
        assert_eq!(s.fields[0].path, "_id");
    }
}
