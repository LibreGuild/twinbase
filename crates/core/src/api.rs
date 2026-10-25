//! The single API used by the desktop UI (Tauri IPC), the MCP server and the CLI.
//! Every method takes JSON params and returns JSON, and records activity so people can see
//! what an agent did.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};
use twinbase_client::{ChangeSet, DbKind, RedisValue, RowQuery, SqlClient, SqlDialect};
use twinbase_mock::{Backend, MockKind};

use crate::app::{App, HistoryEntry, is_read_only_redis, is_read_only_sql};
use crate::error::{CoreError, Result};
use crate::events::{Actor, Event};
use crate::model::{Collection, ConnectionDef, MockDef, SavedQuery, unique_id};

fn p<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T> {
    Ok(serde_json::from_value(params.clone())?)
}

#[derive(Deserialize)]
struct ConnParam {
    connection: String,
}

#[derive(Deserialize)]
struct SchemaParam {
    connection: String,
    #[serde(default)]
    schema: Option<String>,
}

#[derive(Deserialize)]
struct TableParam {
    connection: String,
    #[serde(default)]
    schema: Option<String>,
    table: String,
}

#[derive(Deserialize)]
struct IdParam {
    id: String,
}

fn ddl_keyword(sql: &str) -> bool {
    sql.split(';').any(|s| {
        let w = s.trim_start().split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or("").to_ascii_uppercase();
        matches!(w.as_str(), "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT")
    })
}

impl App {
    async fn sql_client(&self, id: &str) -> Result<(Arc<twinbase_client::Client>, ConnectionDef)> {
        let def = self.connection_def(id)?;
        let client = self.client(id).await?;
        if client.sql().is_none() {
            return Err(CoreError::invalid(format!("'{}' is not a SQL connection", def.name)));
        }
        Ok((client, def))
    }

    /// Default schema for a connection when the caller omits it.
    async fn default_schema(&self, client: &dyn SqlClient, def: &ConnectionDef, given: Option<String>) -> Result<String> {
        if let Some(s) = given.filter(|s| !s.is_empty()) {
            return Ok(s);
        }
        Ok(match client.dialect() {
            SqlDialect::Postgres => "public".into(),
            SqlDialect::Sqlite => "main".into(),
            SqlDialect::Mssql => client.schemas().await?.into_iter().next().unwrap_or_else(|| "dbo".into()),
            // Oracle lists the connected user's schema first.
            SqlDialect::Oracle => client.schemas().await?.into_iter().next().unwrap_or_else(|| def.user.to_ascii_uppercase()),
            // Spanner's default schema has the empty name; BigQuery lists the default dataset first.
            SqlDialect::Spanner => String::new(),
            SqlDialect::Bigquery => {
                client.schemas().await?.into_iter().next().ok_or_else(|| CoreError::invalid("this project has no datasets"))?
            }
            // ClickHouse lists the session's current database first; Cassandra the connection's keyspace.
            SqlDialect::Clickhouse | SqlDialect::Cassandra => match def.database.clone().filter(|d| !d.is_empty()) {
                Some(d) => d,
                None => client.schemas().await?.into_iter().next().unwrap_or_else(|| "default".into()),
            },
            SqlDialect::Mysql => match def.database.clone().filter(|d| !d.is_empty()) {
                Some(d) => d,
                None => client.schemas().await?.into_iter().next().ok_or_else(|| CoreError::invalid("no database selected"))?,
            },
        })
    }
}

/// Dispatch one API call.
pub async fn call(app: &Arc<App>, method: &str, params: Value, actor: &Actor) -> Result<Value> {
    let started = Instant::now();
    let result = dispatch(app, method, &params, actor).await;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let mutating = !matches!(
        method,
        "app.info"
            | "settings.get"
            | "workspace.get"
            | "workspace.recent"
            | "mock.list"
            | "mock.traffic"
            | "mock.snippets"
            | "mock.seed.get"
            | "mock.native.detect"
            | "db.schemas"
            | "db.tables"
            | "db.describe"
            | "db.graph"
            | "db.rows"
            | "db.history"
            | "db.changes_sql"
            | "db.ddl"
            | "redis.scan"
            | "redis.get"
            | "redis.info"
            | "mongo.databases"
            | "mongo.collections"
            | "mongo.find"
            | "mongo.indexes"
            | "mongo.schema"
            | "dynamo.tables"
            | "dynamo.describe"
            | "dynamo.read"
            | "firestore.collections"
            | "firestore.documents"
            | "firestore.get"
            | "firestore.count"
            | "search.indices"
            | "search.mapping"
            | "search.documents"
            | "search.get"
            | "activity.list"
            | "bundle.inspect"
            | "mcp.config"
            | "connection.test"
            | "schema.snapshot.list"
            | "db.explain"
            | "schema.snapshot.get"
            | "schema.diff"
    );
    let is_agent = matches!(actor, Actor::Agent { .. });
    if mutating || is_agent {
        let connection = params.get("connection").and_then(|v| v.as_str()).map(str::to_owned);
        let sql = params.get("sql").or_else(|| params.get("command")).and_then(|v| v.as_str()).map(str::to_owned);
        let summary = summarize(method, &params);
        app.record(actor, method, summary, connection, sql, result.as_ref().err().map(|e| e.message.clone()), elapsed);
    }
    result
}

fn summarize(method: &str, params: &Value) -> String {
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    match method {
        "db.query" => "Ran SQL".into(),
        "db.apply" => format!("Saved edits to {}", s("table")),
        "db.commit" => "Committed previewed changes".into(),
        "db.preview" => "Previewed changes on a local twin".into(),
        "db.generate" => format!("Generated {} sample rows in {}", params.get("count").and_then(|v| v.as_u64()).unwrap_or(0), s("table")),
        "db.rows" => format!("Read rows from {}", s("table")),
        "db.describe" => format!("Described {}", s("table")),
        "db.tables" | "db.graph" => format!("Listed tables in {}", s("schema")),
        "mock.start" => format!("Started mock {}", s("id")),
        "mock.stop" => format!("Stopped mock {}", s("id")),
        "mock.reset" => format!("Reset mock {} to its seed", s("id")),
        "mock.save" => "Saved mock server".into(),
        "mock.faults.set" => {
            let f = params.get("faults");
            let on = f.and_then(|f| f.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(false);
            let n = f.and_then(|f| f.get("rules")).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            if on {
                format!("Enabled fault injection on mock {} ({n} rules)", s("id"))
            } else {
                format!("Disabled fault injection on mock {}", s("id"))
            }
        }
        "redis.command" => "Ran Redis command".into(),
        "mongo.run" => "Ran MongoDB statements".into(),
        "dynamo.run" => "Ran DynamoDB requests".into(),
        "firestore.run" => "Ran Firestore statements".into(),
        "firestore.set" => format!("Saved document {}", s("path")),
        "firestore.add" => format!("Added a document to {}", s("collection")),
        "firestore.delete" => "Deleted Firestore documents".into(),
        "search.run" => "Ran Elasticsearch requests".into(),
        "search.put" => format!("Saved a document in {}", s("index")),
        "search.delete" => format!("Deleted documents from {}", s("index")),
        "dynamo.read" => format!("Read items from {}", s("table")),
        "dynamo.put" => format!("Saved an item in {}", s("table")),
        "dynamo.delete" => format!("Deleted items from {}", s("table")),
        "dynamo.table.create" => "Created a DynamoDB table".into(),
        "dynamo.table.delete" => format!("Deleted table {}", s("table")),
        "mongo.find" => format!("Read documents from {}", s("collection")),
        "mongo.aggregate" => format!("Ran an aggregation on {}", s("collection")),
        "mongo.insert" => format!("Inserted documents into {}", s("collection")),
        "mongo.replace" => format!("Edited a document in {}", s("collection")),
        "mongo.update" => format!("Updated documents in {}", s("collection")),
        "mongo.delete" => format!("Deleted documents from {}", s("collection")),
        "mongo.index.create" => format!("Created an index on {}", s("collection")),
        "mongo.index.drop" => format!("Dropped index {} on {}", s("name"), s("collection")),
        "mongo.collection.create" => format!("Created collection {}", s("collection")),
        "mongo.collection.drop" => format!("Dropped collection {}", s("collection")),
        "schema.snapshot.create" => format!("Took a schema snapshot of {}", s("connection")),
        "schema.snapshot.delete" => format!("Deleted schema snapshot {}", s("id")),
        "query.save" => format!("Saved query \"{}\"", params.pointer("/query/name").and_then(|v| v.as_str()).unwrap_or("")),
        "query.delete" => "Deleted a saved query".into(),
        "collection.save" => format!("Saved collection \"{}\"", params.pointer("/collection/name").and_then(|v| v.as_str()).unwrap_or("")),
        "collection.delete" => "Deleted a collection".into(),
        "scenario.run" => format!("Ran scenarios in {}", s("collection")),
        "connection.save" => format!("Saved connection \"{}\"", params.pointer("/connection/name").and_then(|v| v.as_str()).unwrap_or("")),
        "connection.delete" => format!("Deleted connection {}", s("id")),
        "mock.delete" => format!("Deleted mock {}", s("id")),
        "mock.seed.set" => format!("Saved seed of mock {}", s("id")),
        "mock.seed.capture" => format!("Captured mock {} data as its seed", s("id")),
        "bundle.export" => "Exported a bundle".into(),
        "bundle.import" => "Imported a bundle".into(),
        "settings.update" => "Changed settings".into(),
        "workspace.open" => format!("Opened workspace {}", s("path")),
        "redis.set" => format!("Set key {}", s("key")),
        "redis.delete" => "Deleted keys".into(),
        other => other.replace('.', " "),
    }
}

async fn dispatch(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    match method {
        // ------------------------------------------------------------------ app & settings
        "app.info" => {
            let s = app.settings();
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "app_dir": app.local.dir().to_string_lossy(),
                "first_run": app.first_run,
                "default_workspace": crate::settings::default_workspace_dir().to_string_lossy(),
                "platform": std::env::consts::OS,
                "secret_store": app.local.secret_backend().name(),
                "workspace": app.workspace_key(),
                "mcp": {
                    "enabled": s.mcp.enabled,
                    "port": s.mcp.port,
                    "listening": app.mcp_listening(),
                    "url": format!("http://127.0.0.1:{}/mcp", app.mcp_listening().unwrap_or(s.mcp.port)),
                },
            }))
        }
        "settings.get" => Ok(serde_json::to_value(app.settings())?),
        "settings.update" => {
            let patch = params.get("settings").cloned().unwrap_or_default();
            let current = serde_json::to_value(app.settings())?;
            let merged = merge(current, patch);
            let new: crate::settings::Settings = serde_json::from_value(merged)?;
            Ok(serde_json::to_value(app.update_settings(|s| *s = new)?)?)
        }
        // ------------------------------------------------------------------ workspace
        "workspace.get" => {
            let data = app.data();
            Ok(json!({
                "name": data.name,
                "path": data.path,
                "connections": data.connections.iter().map(|c| {
                    let mut v = serde_json::to_value(c).unwrap_or_default();
                    v["has_password"] = json!(app.has_password(&c.id));
                v["has_ssh_secret"] = json!(app.has_ssh_secret(&c.id));
                    v
                }).collect::<Vec<_>>(),
                "mocks": app.mock_statuses().await,
                "collections": data.collections,
                "queries": data.queries,
                "connected": app.connected_ids().await,
            }))
        }
        "workspace.open" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
                #[serde(default)]
                name: Option<String>,
            }
            let q: P = p(params)?;
            app.open_workspace(PathBuf::from(&q.path), q.name.as_deref()).await?;
            Ok(json!({ "path": q.path }))
        }
        "workspace.recent" => Ok(json!(app.settings().recent_workspaces)),
        // ------------------------------------------------------------------ connections
        "connection.save" => {
            #[derive(Deserialize)]
            struct P {
                connection: ConnectionDef,
                /// None = keep, Some("") = remove, Some(x) = set.
                #[serde(default)]
                password: Option<String>,
                /// SSH password or key passphrase, same rules as `password`.
                #[serde(default)]
                ssh_secret: Option<String>,
            }
            let mut q: P = p(params)?;
            let data = app.data();
            if q.connection.id.is_empty() {
                let taken: Vec<String> = data.connections.iter().map(|c| c.id.clone()).collect();
                q.connection.id = unique_id(&q.connection.name, &taken);
            }
            if q.connection.id.starts_with("mock:") {
                return Err(CoreError::invalid("mock connections are managed by their mock server"));
            }
            // Workspace files may be committed: keep passwords in connection strings out of them.
            if let Some((uri, user, password)) = split_uri_password(&q.connection.host) {
                q.connection.host = uri;
                if q.connection.user.is_empty() {
                    q.connection.user = user;
                }
                if q.password.as_deref().is_none_or(str::is_empty) {
                    q.password = Some(password);
                }
            }
            app.workspace().save_connection(&q.connection)?;
            if let Some(pw) = &q.password {
                app.set_password(&q.connection.id, if pw.is_empty() { None } else { Some(pw) })?;
            }
            if let Some(s) = &q.ssh_secret {
                app.set_ssh_secret(&q.connection.id, if s.is_empty() || q.connection.ssh.is_none() { None } else { Some(s) })?;
            }
            app.disconnect(&q.connection.id).await;
            app.reload()?;
            Ok(serde_json::to_value(&q.connection)?)
        }
        "connection.delete" => {
            let q: IdParam = p(params)?;
            app.disconnect(&q.id).await;
            app.workspace().delete_connection(&q.id)?;
            app.set_password(&q.id, None)?;
            app.set_ssh_secret(&q.id, None)?;
            app.reload()?;
            Ok(json!({ "deleted": q.id }))
        }
        "connection.test" => {
            #[derive(Deserialize)]
            struct P {
                connection: ConnectionDef,
                #[serde(default)]
                password: Option<String>,
                #[serde(default)]
                ssh_secret: Option<String>,
            }
            let q: P = p(params)?;
            let config = app.config_for(&q.connection, q.password.filter(|p| !p.is_empty()), q.ssh_secret.filter(|s| !s.is_empty()));
            let started = Instant::now();
            match twinbase_client::Client::connect(&config).await {
                Ok(c) => {
                    let version = match &c {
                        twinbase_client::Client::Sql(s) => s.server_version().await.unwrap_or_default(),
                        twinbase_client::Client::Redis(r) => r
                            .info()
                            .await
                            .ok()
                            .and_then(|i| i.lines().find(|l| l.starts_with("redis_version")).map(str::to_owned))
                            .unwrap_or_default(),
                        twinbase_client::Client::Mongo(m) => m.server_version().await.map(|v| format!("MongoDB {v}")).unwrap_or_default(),
                        twinbase_client::Client::Dynamo(d) => format!("DynamoDB API at {} ({})", d.endpoint().url, d.endpoint().region),
                        twinbase_client::Client::Firestore(f) => format!("Firestore at {} ({})", f.endpoint, f.database),
                        twinbase_client::Client::Search(s) => format!("{} · cluster {}", s.product(), s.info.cluster),
                    };
                    Ok(json!({ "ok": true, "version": version, "elapsed_ms": started.elapsed().as_millis() as u64 }))
                }
                Err(e) => Ok(json!({ "ok": false, "error": e.message })),
            }
        }
        "connection.connect" => {
            let q: IdParam = p(params)?;
            let client = app.client(&q.id).await?;
            let version = match client.as_ref() {
                twinbase_client::Client::Sql(s) => s.server_version().await?,
                twinbase_client::Client::Redis(r) => {
                    r.info().await?.lines().find(|l| l.starts_with("redis_version")).unwrap_or("").to_owned()
                }
                twinbase_client::Client::Mongo(m) => format!("MongoDB {}", m.server_version().await?),
                twinbase_client::Client::Dynamo(d) => format!("DynamoDB API at {}", d.endpoint().url),
                twinbase_client::Client::Firestore(f) => format!("Firestore at {}", f.endpoint),
                twinbase_client::Client::Search(s) => s.product(),
            };
            Ok(json!({ "id": q.id, "version": version }))
        }
        "connection.disconnect" => {
            let q: IdParam = p(params)?;
            app.disconnect(&q.id).await;
            Ok(json!({ "id": q.id }))
        }
        // ------------------------------------------------------------------ mock servers
        "mock.list" => Ok(serde_json::to_value(app.mock_statuses().await)?),
        "mock.save" => {
            #[derive(Deserialize)]
            struct P {
                mock: MockDef,
            }
            let mut q: P = p(params)?;
            let data = app.data();
            if q.mock.id.is_empty() {
                let taken: Vec<String> = data.mocks.iter().map(|m| m.id.clone()).collect();
                q.mock.id = unique_id(&q.mock.name, &taken);
            }
            if q.mock.port == 0 {
                q.mock.port = suggest_port(app, q.mock.kind).await;
            }
            if let Some(other) = data.mocks.iter().find(|m| m.port == q.mock.port && m.id != q.mock.id) {
                return Err(CoreError::conflict(format!("port {} is already used by mock '{}'", q.mock.port, other.name)));
            }
            let restart = app.is_mock_running(&q.mock.id).await;
            app.save_mock_def(&q.mock)?;
            if restart {
                app.stop_mock(&q.mock.id).await?;
                app.start_mock(&q.mock.id).await?;
            }
            Ok(serde_json::to_value(&q.mock)?)
        }
        "mock.delete" => {
            let q: IdParam = p(params)?;
            app.stop_mock(&q.id).await?;
            app.workspace().delete_mock(&q.id)?;
            app.reload()?;
            Ok(json!({ "deleted": q.id }))
        }
        "mock.start" => {
            let q: IdParam = p(params)?;
            app.start_mock(&q.id).await?;
            Ok(json!({ "id": q.id, "running": true }))
        }
        "mock.stop" => {
            let q: IdParam = p(params)?;
            app.stop_mock(&q.id).await?;
            Ok(json!({ "id": q.id, "running": false }))
        }
        "mock.reset" => {
            let q: IdParam = p(params)?;
            app.reset_mock(&q.id).await?;
            Ok(json!({ "id": q.id, "running": true }))
        }
        "mock.faults.set" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                faults: twinbase_mock::FaultConfig,
            }
            let q: P = p(params)?;
            let mut def = app.mock_def(&q.id)?;
            // Validate (patterns, probabilities) before saving anything.
            twinbase_mock::Faults::new(q.faults.clone()).map_err(CoreError::invalid)?;
            def.faults = q.faults;
            app.save_mock_def(&def)?;
            app.apply_mock_faults(&q.id, &def.faults).await?;
            Ok(serde_json::to_value(&def.faults)?)
        }
        "mock.traffic" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                #[serde(default)]
                after: u64,
                #[serde(default = "default_traffic")]
                limit: usize,
            }
            let q: P = p(params)?;
            Ok(serde_json::to_value(app.mock_traffic(&q.id, q.after, q.limit).await)?)
        }
        "mock.snippets" => {
            let q: IdParam = p(params)?;
            let m = app.mock_def(&q.id)?;
            let (user, password) = m.credentials();
            let host = if m.host == "0.0.0.0" { "127.0.0.1".to_owned() } else { m.host.clone() };
            let db = if m.kind == MockKind::Redis { "0".to_owned() } else { m.database.clone() };
            let pw = if m.kind == MockKind::Redis && matches!(m.auth, twinbase_mock::AuthMode::Any) { String::new() } else { password };
            Ok(serde_json::to_value(crate::snippets::snippets(&crate::snippets::Target {
                kind: m.db_kind(),
                host: &host,
                port: m.port,
                user: &user,
                password: &pw,
                database: &db,
            }))?)
        }
        "mock.native.detect" => {
            let configured = app.settings().postgres_bin;
            let found = tokio::task::spawn_blocking(move || {
                twinbase_mock::native_pg::detect(Some(&configured).filter(|c| !c.trim().is_empty()).map(std::path::Path::new))
            })
            .await
            .map_err(|e| CoreError::new("mock", e.to_string()))?;
            Ok(json!({ "found": found }))
        }
        "mock.seed.get" => {
            let q: IdParam = p(params)?;
            let path = app.workspace().seed_path(&q.id, app.mock_def(&q.id)?.kind);
            Ok(json!({ "sql": std::fs::read_to_string(path).unwrap_or_default() }))
        }
        "mock.seed.set" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                sql: String,
            }
            let q: P = p(params)?;
            let def = app.mock_def(&q.id)?;
            // Reject seeds that would fail when the mock starts.
            if def.kind == MockKind::Mongodb && !q.sql.trim().is_empty() {
                let probe = twinbase_mock::mongo::MongoStore::new(None);
                twinbase_mock::mongo::apply_seed(&probe, &def.database, &q.sql).map_err(|e| CoreError::invalid(format!("seed: {e}")))?;
            }
            if def.kind == MockKind::Firestore && !q.sql.trim().is_empty() {
                let probe = twinbase_mock::firestore::FirestoreStore::new(None);
                twinbase_mock::firestore::apply_seed(&probe, &q.sql).map_err(|e| CoreError::invalid(format!("seed: {e}")))?;
            }
            if def.kind == MockKind::Dynamodb && !q.sql.trim().is_empty() {
                let probe = twinbase_mock::dynamo::DynamoStore::new(None);
                twinbase_mock::dynamo::apply_seed(&probe, &q.sql).map_err(|e| CoreError::invalid(format!("seed: {e}")))?;
            }
            let path = app.workspace().seed_path(&q.id, def.kind);
            std::fs::write(&path, q.sql).map_err(|e| CoreError::io(&path, e))?;
            Ok(json!({ "saved": true }))
        }
        "mock.seed.capture" => {
            let q: IdParam = p(params)?;
            let sql = capture_seed(app, &q.id).await?;
            let path = app.workspace().seed_path(&q.id, app.mock_def(&q.id)?.kind);
            std::fs::write(&path, &sql).map_err(|e| CoreError::io(&path, e))?;
            Ok(json!({ "sql": sql }))
        }
        // ------------------------------------------------------------------ SQL
        "db.schemas" => {
            let q: ConnParam = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            Ok(json!(c.sql().unwrap().schemas().await?))
        }
        "db.tables" => {
            let q: SchemaParam = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            Ok(serde_json::to_value(sql.tables(&schema).await?)?)
        }
        "db.describe" => {
            let q: TableParam = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            Ok(serde_json::to_value(sql.describe(&schema, &q.table).await?)?)
        }
        "db.graph" => {
            let q: SchemaParam = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            Ok(json!({ "schema": schema, "tables": sql.schema_graph(&schema).await? }))
        }
        "db.ddl" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                #[serde(default)]
                dialect: Option<SqlDialect>,
                /// Only this table (default: the whole schema).
                #[serde(default)]
                table: Option<String>,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            let mut tables = sql.schema_graph(&schema).await?;
            if let Some(t) = &q.table {
                tables.retain(|x| &x.name == t);
            }
            Ok(json!({ "sql": crate::ddl::schema_ddl(&tables, sql.dialect(), q.dialect.unwrap_or(sql.dialect())) }))
        }
        "db.query" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                sql: String,
                #[serde(default)]
                max_rows: Option<usize>,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            let writes = !is_read_only_sql(&q.sql, def.kind);
            app.check_access(&def, actor, writes)?;
            app.check_agent_sql(&def, actor, &q.sql)?;
            let max = q.max_rows.unwrap_or(app.settings().row_limit as usize).clamp(1, 100_000);
            let started = Instant::now();
            let result = if app.reads_only(&def, actor) {
                c.sql().unwrap().query_read_only(&q.sql, max).await
            } else {
                c.sql().unwrap().query(&q.sql, max).await
            };
            app.add_history(
                &q.connection,
                HistoryEntry {
                    at: chrono::Utc::now().timestamp_millis(),
                    sql: q.sql.clone(),
                    ok: result.is_ok(),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error: result.as_ref().err().map(|e| e.message.clone()),
                },
            );
            if writes {
                if ddl_keyword(&q.sql) {
                    app.emit(Event::SchemaChanged { connection: q.connection.clone() });
                }
                app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
            }
            Ok(serde_json::to_value(result?)?)
        }
        "db.explain" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                sql: String,
                /// Run the statement (in a rolled-back transaction) to get real timings and row counts.
                #[serde(default)]
                analyze: bool,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            let sql = c.sql().unwrap();
            let statement = twinbase_client::split_statements(&q.sql, sql.dialect())
                .into_iter()
                .find(|s| !s.trim().is_empty())
                .ok_or_else(|| CoreError::invalid("nothing to explain"))?;
            let writes = !is_read_only_sql(&statement, def.kind);
            // ANALYZE executes the statement; writes are rolled back but still need write access.
            app.check_access(&def, actor, q.analyze && writes)?;
            app.check_agent_sql(&def, actor, &statement)?;
            let mut v = serde_json::to_value(crate::explain::run(sql, &statement, q.analyze).await?)?;
            v["statement"] = json!(statement);
            Ok(v)
        }
        "db.rows" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                table: String,
                #[serde(default)]
                query: RowQuery,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            Ok(serde_json::to_value(twinbase_client::fetch_rows(sql, &schema, &q.table, &q.query).await?)?)
        }
        "db.changes_sql" | "db.apply" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                table: String,
                changes: ChangeSet,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            if method == "db.changes_sql" {
                return Ok(json!(q.changes.to_sql(sql.dialect(), &schema, &q.table)));
            }
            app.check_access(&def, actor, true)?;
            let r = twinbase_client::apply_changes(sql, &schema, &q.table, &q.changes).await?;
            app.emit(Event::DataChanged { connection: q.connection.clone(), table: Some(q.table.clone()) });
            Ok(serde_json::to_value(r)?)
        }
        "db.preview" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                sql: String,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            Ok(serde_json::to_value(crate::twin::preview(sql, &schema, &q.sql).await?)?)
        }
        "db.commit" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                sql: String,
                #[serde(default)]
                expected_affected: Option<u64>,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, true)?;
            app.check_agent_sql(&def, actor, &q.sql)?;
            let sql = c.sql().unwrap();
            let statements = twinbase_client::split_statements(&q.sql, sql.dialect());
            let affected = sql.execute_in_transaction(&statements, q.expected_affected).await?;
            if ddl_keyword(&q.sql) {
                app.emit(Event::SchemaChanged { connection: q.connection.clone() });
            }
            app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
            Ok(json!({ "affected": affected }))
        }
        "db.generate" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                table: String,
                #[serde(default = "default_count")]
                count: usize,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, true)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            let r = crate::sample::generate(sql, &schema, &q.table, q.count).await?;
            app.emit(Event::DataChanged { connection: q.connection.clone(), table: Some(q.table.clone()) });
            Ok(serde_json::to_value(r)?)
        }
        "db.history" => {
            let q: ConnParam = p(params)?;
            Ok(serde_json::to_value(app.history(&q.connection))?)
        }
        // ------------------------------------------------------------------ schema snapshots & diffs
        "schema.snapshot.create" => {
            #[derive(Deserialize)]
            struct P {
                connection: String,
                #[serde(default)]
                schema: Option<String>,
                #[serde(default)]
                label: Option<String>,
            }
            let q: P = p(params)?;
            let (c, def) = app.sql_client(&q.connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, q.schema).await?;
            let tables = sql.schema_graph(&schema).await?;
            let now = chrono::Utc::now();
            let label =
                q.label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| format!("{} {}", def.name, now.format("%Y-%m-%d %H:%M")));
            let taken: Vec<String> = app.workspace().list_snapshots().into_iter().map(|s| s.id).collect();
            let snap = crate::schema_diff::Snapshot {
                id: unique_id(&format!("{}-{}", def.name, now.format("%Y%m%d-%H%M%S")), &taken),
                label,
                taken_at: now.to_rfc3339(),
                connection: q.connection.clone(),
                source: def.name.clone(),
                dialect: sql.dialect(),
                schema,
                tables,
            };
            app.workspace().save_snapshot(&snap)?;
            app.emit(Event::WorkspaceChanged);
            Ok(serde_json::to_value(snap.info())?)
        }
        "schema.snapshot.list" => Ok(serde_json::to_value(app.workspace().list_snapshots())?),
        "schema.snapshot.get" => {
            let q: IdParam = p(params)?;
            Ok(serde_json::to_value(app.workspace().load_snapshot(&q.id)?)?)
        }
        "schema.snapshot.delete" => {
            let q: IdParam = p(params)?;
            app.workspace().delete_snapshot(&q.id)?;
            app.emit(Event::WorkspaceChanged);
            Ok(json!({ "deleted": q.id }))
        }
        "schema.diff" => {
            #[derive(Deserialize)]
            struct P {
                from: SchemaSource,
                to: SchemaSource,
                /// Dialect of the migration script (default: the `from` side, which it is run on).
                #[serde(default)]
                dialect: Option<SqlDialect>,
            }
            let q: P = p(params)?;
            let (from_tables, from_dialect, from_label) = load_schema_source(app, &q.from, actor).await?;
            let (to_tables, to_dialect, to_label) = load_schema_source(app, &q.to, actor).await?;
            let target = q.dialect.unwrap_or(from_dialect);
            let d = crate::schema_diff::diff(
                crate::schema_diff::Side { tables: &from_tables, dialect: from_dialect },
                crate::schema_diff::Side { tables: &to_tables, dialect: to_dialect },
                target,
            );
            let mut v = serde_json::to_value(&d)?;
            v["from_label"] = json!(from_label);
            v["to_label"] = json!(to_label);
            v["up_sql"] = json!(d.up.join("\n\n"));
            v["down_sql"] = json!(d.down.join("\n\n"));
            Ok(v)
        }
        // ------------------------------------------------------------------ Redis
        "redis.scan" | "redis.get" | "redis.set" | "redis.delete" | "redis.expire" | "redis.command" | "redis.info" => {
            redis_call(app, method, params, actor).await
        }
        m if m.starts_with("mongo.") => mongo_call(app, method, params, actor).await,
        m if m.starts_with("dynamo.") => dynamo_call(app, method, params, actor).await,
        m if m.starts_with("firestore.") => firestore_call(app, method, params, actor).await,
        m if m.starts_with("search.") => search_call(app, method, params, actor).await,
        // ------------------------------------------------------------------ saved queries
        "collection.save" => {
            #[derive(Deserialize)]
            struct P {
                collection: Collection,
            }
            let mut q: P = p(params)?;
            if q.collection.id.is_empty() {
                let taken: Vec<String> = app.data().collections.iter().map(|c| c.id.clone()).collect();
                q.collection.id = unique_id(&q.collection.name, &taken);
            }
            app.workspace().save_collection(&q.collection)?;
            app.reload()?;
            Ok(serde_json::to_value(&q.collection)?)
        }
        "collection.delete" => {
            let q: IdParam = p(params)?;
            app.workspace().delete_collection(&q.id)?;
            app.reload()?;
            Ok(json!({ "deleted": q.id }))
        }
        "query.save" => {
            #[derive(Deserialize)]
            struct P {
                query: SavedQuery,
                /// Previous location when the query moved collections.
                #[serde(default)]
                previous_collection: Option<String>,
            }
            let mut q: P = p(params)?;
            // Accept a collection id or name; create the collection when it does not exist yet.
            let wanted = if q.query.collection.trim().is_empty() { "Queries".to_owned() } else { q.query.collection.trim().to_owned() };
            let data = app.data();
            let existing = data
                .collections
                .iter()
                .find(|c| c.id == wanted)
                .or_else(|| data.collections.iter().find(|c| c.name.eq_ignore_ascii_case(&wanted)));
            q.query.collection = match existing {
                Some(c) => c.id.clone(),
                None => {
                    let taken: Vec<String> = data.collections.iter().map(|c| c.id.clone()).collect();
                    let c = Collection { id: unique_id(&wanted, &taken), name: wanted, docs: String::new() };
                    app.workspace().save_collection(&c)?;
                    c.id
                }
            };
            if q.query.id.is_empty() {
                let taken: Vec<String> =
                    app.data().queries.iter().filter(|x| x.collection == q.query.collection).map(|x| x.id.clone()).collect();
                q.query.id = unique_id(&q.query.name, &taken);
            }
            if let Some(prev) = q.previous_collection.filter(|p| p != &q.query.collection) {
                app.workspace().delete_query(&prev, &q.query.id)?;
            }
            app.workspace().save_query(&q.query)?;
            app.reload()?;
            Ok(serde_json::to_value(&q.query)?)
        }
        "query.delete" => {
            #[derive(Deserialize)]
            struct P {
                collection: String,
                id: String,
            }
            let q: P = p(params)?;
            app.workspace().delete_query(&q.collection, &q.id)?;
            app.reload()?;
            Ok(json!({ "deleted": q.id }))
        }
        "scenario.run" => {
            #[derive(Deserialize)]
            struct P {
                collection: String,
                #[serde(default)]
                connection: Option<String>,
            }
            let q: P = p(params)?;
            Ok(serde_json::to_value(crate::scenario::run(app, &q.collection, q.connection.as_deref(), actor).await?)?)
        }
        // ------------------------------------------------------------------ bundles
        "bundle.export" => {
            #[derive(Deserialize)]
            struct P {
                selection: crate::bundle::Selection,
                path: String,
            }
            let q: P = p(params)?;
            let manifest = crate::bundle::export(&app.workspace(), &app.data(), &q.selection, std::path::Path::new(&q.path))?;
            Ok(serde_json::to_value(manifest)?)
        }
        "bundle.inspect" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let q: P = p(params)?;
            Ok(serde_json::to_value(crate::bundle::inspect(std::path::Path::new(&q.path))?)?)
        }
        "bundle.import" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let q: P = p(params)?;
            let summary = crate::bundle::import(&app.workspace(), &app.data(), std::path::Path::new(&q.path))?;
            app.reload()?;
            Ok(serde_json::to_value(summary)?)
        }
        // ------------------------------------------------------------------ misc
        "activity.list" => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            Ok(serde_json::to_value(app.activity(limit))?)
        }
        "mcp.config" => {
            // Point agents at the port actually in use (e.g. `twinbase serve --port`).
            let mut s = app.settings();
            if let Some(port) = app.mcp_listening() {
                s.mcp.port = port;
            }
            let mut v = crate::mcp::install_instructions(&s);
            v["listening"] = json!(app.mcp_listening().is_some());
            Ok(v)
        }
        other => Err(CoreError::not_found(format!("unknown method '{other}'"))),
    }
}

/// Where one side of a schema comparison comes from.
#[derive(Deserialize)]
#[serde(untagged)]
enum SchemaSource {
    Snapshot {
        snapshot: String,
    },
    Live {
        connection: String,
        #[serde(default)]
        schema: Option<String>,
    },
}

async fn load_schema_source(
    app: &Arc<App>,
    src: &SchemaSource,
    actor: &Actor,
) -> Result<(Vec<twinbase_client::TableSchema>, SqlDialect, String)> {
    match src {
        SchemaSource::Snapshot { snapshot } => {
            let s = app.workspace().load_snapshot(snapshot)?;
            Ok((s.tables, s.dialect, format!("snapshot \"{}\"", s.label)))
        }
        SchemaSource::Live { connection, schema } => {
            let (c, def) = app.sql_client(connection).await?;
            app.check_access(&def, actor, false)?;
            let sql = c.sql().unwrap();
            let schema = app.default_schema(sql, &def, schema.clone()).await?;
            Ok((sql.schema_graph(&schema).await?, sql.dialect(), format!("{} ({schema})", def.name)))
        }
    }
}

fn default_traffic() -> usize {
    200
}

fn default_count() -> usize {
    25
}

/// Deep-merge `patch` into `base` (objects merge, other values replace).
fn merge(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut b), Value::Object(p)) => {
            for (k, v) in p {
                let merged = match b.remove(&k) {
                    Some(old) => merge(old, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        (_, p) => p,
    }
}

async fn suggest_port(app: &App, kind: MockKind) -> u16 {
    let used: Vec<u16> = app.data().mocks.iter().map(|m| m.port).collect();
    let start = match kind {
        MockKind::Postgres => 15432,
        MockKind::Mysql => 13306,
        MockKind::Mssql => 11433,
        MockKind::Redis => 16379,
        MockKind::Mongodb => 27018,
        MockKind::Dynamodb => 18000,
        MockKind::Firestore => 18080,
    };
    let mut port = start;
    loop {
        if !used.contains(&port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
        port += 1;
    }
}

async fn redis_call(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    let q: ConnParam = p(params)?;
    let def = app.connection_def(&q.connection)?;
    if def.kind != DbKind::Redis {
        return Err(CoreError::invalid(format!("'{}' is not a Redis connection", def.name)));
    }
    let client = app.client(&q.connection).await?;
    let r = client.redis().unwrap();
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    match method {
        "redis.info" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "info": r.info().await?, "keys": r.dbsize().await? }))
        }
        "redis.scan" => {
            app.check_access(&def, actor, false)?;
            let cursor = params.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0);
            let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(200);
            let ty = s("type");
            let (next, keys) = r.scan(&s("pattern"), cursor, count, Some(&ty)).await?;
            Ok(json!({ "cursor": next, "keys": keys }))
        }
        "redis.get" => {
            app.check_access(&def, actor, false)?;
            Ok(serde_json::to_value(r.get(&s("key")).await?)?)
        }
        "redis.set" => {
            app.check_access(&def, actor, true)?;
            let value: RedisValue = serde_json::from_value(params.get("value").cloned().unwrap_or_default())?;
            let ttl = params.get("ttl_ms").and_then(|v| v.as_i64());
            r.set(&s("key"), &value, ttl).await?;
            app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
            Ok(json!({ "ok": true }))
        }
        "redis.delete" => {
            app.check_access(&def, actor, true)?;
            let keys: Vec<String> = serde_json::from_value(params.get("keys").cloned().unwrap_or_default())?;
            let n = r.delete(&keys).await?;
            app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
            Ok(json!({ "deleted": n }))
        }
        "redis.expire" => {
            app.check_access(&def, actor, true)?;
            r.expire(&s("key"), params.get("ttl_ms").and_then(|v| v.as_i64())).await?;
            Ok(json!({ "ok": true }))
        }
        _ => {
            let line = s("command");
            app.check_access(&def, actor, !is_read_only_redis(&line))?;
            let v = r.command(&line).await?;
            if !is_read_only_redis(&line) {
                app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
            }
            Ok(v)
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `scheme://user:password@rest` → (`scheme://user@rest`, user, password).
pub(crate) fn split_uri_password(uri: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let at = rest[..authority_end].rfind('@')?;
    let (user, password) = rest[..at].split_once(':')?;
    Some((format!("{scheme}://{user}@{}", &rest[at + 1..]), percent_decode(user), percent_decode(password)))
}

async fn firestore_call(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    let q: ConnParam = p(params)?;
    let def = app.connection_def(&q.connection)?;
    if def.kind != DbKind::Firestore {
        return Err(CoreError::invalid(format!("'{}' is not a Firestore connection", def.name)));
    }
    let client = app.client(&q.connection).await?;
    let f = client.firestore().unwrap();
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let changed = || {
        app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
        app.emit(Event::SchemaChanged { connection: q.connection.clone() });
    };
    let query = || -> Result<twinbase_client::firestore_client::FsQuery> {
        Ok(serde_json::from_value(params.get("query").cloned().ok_or_else(|| CoreError::invalid("query is required"))?)?)
    };
    match method {
        "firestore.collections" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "collections": f.collection_ids(&s("parent")).await?, "database": f.database, "endpoint": f.endpoint }))
        }
        "firestore.documents" => {
            app.check_access(&def, actor, false)?;
            let mut fq = query()?;
            fq.limit = Some(fq.limit.unwrap_or(50).clamp(1, 1000));
            let started = Instant::now();
            let docs = f.query(&fq).await?;
            Ok(json!({ "documents": docs, "duration_ms": started.elapsed().as_secs_f64() * 1000.0 }))
        }
        "firestore.count" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "count": f.count(&query()?).await? }))
        }
        "firestore.get" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "document": f.get(&s("path")).await? }))
        }
        "firestore.set" => {
            app.check_access(&def, actor, true)?;
            let expect = match params.get("expect").and_then(|v| v.as_str()) {
                Some("exists") => Some(true),
                Some("new") => Some(false),
                _ => None,
            };
            let merge = params.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);
            let doc =
                f.set(&s("path"), params.get("data").unwrap_or(&json!({})), merge, expect).await.map_err(|e| match e.code.as_deref() {
                    Some("NOT_FOUND") => CoreError::invalid("the document no longer exists; reload and try again"),
                    Some("ALREADY_EXISTS") => CoreError::invalid("a document with this id already exists"),
                    _ => e.into(),
                })?;
            changed();
            Ok(json!({ "document": doc }))
        }
        "firestore.add" => {
            app.check_access(&def, actor, true)?;
            let doc = f.add(&s("collection"), params.get("data").unwrap_or(&json!({}))).await?;
            changed();
            Ok(json!({ "document": doc }))
        }
        "firestore.delete" => {
            app.check_access(&def, actor, true)?;
            let paths: Vec<String> = serde_json::from_value(params.get("paths").cloned().unwrap_or_default())?;
            let n = f.delete(&paths).await?;
            changed();
            Ok(json!({ "deleted": n }))
        }
        "firestore.run" => {
            let script = s("command");
            let writes = twinbase_client::firestore_client::script_writes(&script);
            app.check_access(&def, actor, writes)?;
            let max_docs = params
                .get("max_documents")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(app.settings().row_limit as usize)
                .clamp(1, 10_000);
            let started = Instant::now();
            let result = f.run(&script, max_docs).await;
            let failed = match &result {
                Err(e) => Some(e.message.clone()),
                Ok(outs) => outs.iter().find_map(|o| o.error.as_ref().map(|e| e.message.clone())),
            };
            app.add_history(
                &q.connection,
                HistoryEntry {
                    at: chrono::Utc::now().timestamp_millis(),
                    sql: script,
                    ok: failed.is_none(),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error: failed,
                },
            );
            let outputs = result?;
            if writes {
                changed();
            }
            Ok(json!({ "outputs": outputs }))
        }
        other => Err(CoreError::not_found(format!("unknown method '{other}'"))),
    }
}

/// Elasticsearch / OpenSearch: indices, mappings, documents and the Dev Tools-style console.
async fn search_call(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    let q: ConnParam = p(params)?;
    let def = app.connection_def(&q.connection)?;
    if def.kind != DbKind::Elasticsearch {
        return Err(CoreError::invalid(format!("'{}' is not an Elasticsearch or OpenSearch connection", def.name)));
    }
    let client = app.client(&q.connection).await?;
    let es = client.search().unwrap();
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let index = || -> Result<String> {
        let i = s("index");
        if i.is_empty() { Err(CoreError::invalid("index is required")) } else { Ok(i) }
    };
    let changed = || {
        app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
        app.emit(Event::SchemaChanged { connection: q.connection.clone() });
    };
    match method {
        "search.indices" => {
            app.check_access(&def, actor, false)?;
            let hidden = params.get("include_hidden").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(json!({ "indices": es.indices(hidden).await?, "server": es.info }))
        }
        "search.mapping" => {
            app.check_access(&def, actor, false)?;
            let (fields, raw) = es.mapping(&index()?).await?;
            Ok(json!({ "fields": fields, "mapping": raw }))
        }
        "search.documents" => {
            app.check_access(&def, actor, false)?;
            // A query DSL object, or a query string (Lucene syntax) typed in the search box.
            let query = match params.get("query") {
                Some(Value::String(t)) if !t.trim().is_empty() => json!({ "query_string": { "query": t } }),
                Some(v @ Value::Object(o)) if !o.is_empty() => v.clone(),
                _ => json!({ "match_all": {} }),
            };
            let size = params.get("size").and_then(|v| v.as_u64()).unwrap_or(50).clamp(1, 1000);
            let from = params.get("from").and_then(|v| v.as_u64()).unwrap_or(0).min(100_000);
            let mut body = json!({ "query": query, "size": size, "from": from, "track_total_hits": true });
            if let Some(sort) = params.get("sort").filter(|v| !v.is_null()) {
                body["sort"] = sort.clone();
            }
            let started = Instant::now();
            let r = es.search(&index()?, &body).await?;
            Ok(
                json!({ "hits": r["hits"]["hits"], "total": r["hits"]["total"]["value"], "relation": r["hits"]["total"]["relation"], "took_ms": r["took"], "duration_ms": started.elapsed().as_secs_f64() * 1000.0 }),
            )
        }
        "search.get" => {
            app.check_access(&def, actor, false)?;
            Ok(es.get(&index()?, &s("id")).await?)
        }
        "search.put" => {
            app.check_access(&def, actor, true)?;
            let version = match (params.get("if_seq_no").and_then(|v| v.as_u64()), params.get("if_primary_term").and_then(|v| v.as_u64())) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            };
            let create = params.get("create").and_then(|v| v.as_bool()).unwrap_or(false);
            let doc =
                params.get("document").filter(|d| d.is_object()).ok_or_else(|| CoreError::invalid("document must be a JSON object"))?;
            let r = es.put(&index()?, &s("id"), doc, create, version).await.map_err(|e| match e.code.as_deref() {
                Some("version_conflict_engine_exception") if create => CoreError::invalid("a document with this id already exists"),
                Some("version_conflict_engine_exception") => {
                    CoreError::invalid("the document changed since it was loaded; reload and try again")
                }
                _ => e.into(),
            })?;
            changed();
            Ok(r)
        }
        "search.delete" => {
            app.check_access(&def, actor, true)?;
            let ids: Vec<String> = serde_json::from_value(params.get("ids").cloned().unwrap_or_default())?;
            let n = es.delete(&index()?, &ids).await?;
            changed();
            Ok(json!({ "deleted": n }))
        }
        "search.run" => {
            let script = s("command");
            let writes = twinbase_client::search_client::script_writes(&script);
            app.check_access(&def, actor, writes)?;
            let started = Instant::now();
            let result = es.run(&script).await;
            let failed = match &result {
                Err(e) => Some(e.message.clone()),
                Ok(outs) => outs.iter().find_map(|o| o.error.as_ref().map(|e| e.message.clone())),
            };
            app.add_history(
                &q.connection,
                HistoryEntry {
                    at: chrono::Utc::now().timestamp_millis(),
                    sql: script,
                    ok: failed.is_none(),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error: failed,
                },
            );
            let outputs = result?;
            if writes {
                changed();
            }
            Ok(json!({ "outputs": outputs }))
        }
        other => Err(CoreError::not_found(format!("unknown method '{other}'"))),
    }
}

/// One filter row of the item browser: `{path, op, value}` with a typed value.
#[derive(Deserialize)]
struct DynFilter {
    path: String,
    op: String,
    #[serde(default)]
    value: Option<Value>,
}

/// Placeholder for a document path: each segment gets a `#name` so reserved words are fine.
fn dyn_path(path: &str, names: &mut serde_json::Map<String, Value>) -> String {
    path.split('.')
        .map(|seg| {
            let (name, index) = match seg.find('[') {
                Some(i) => (&seg[..i], &seg[i..]),
                None => (seg, ""),
            };
            let key = format!("#n{}", names.len());
            names.insert(key.clone(), json!(name));
            format!("{key}{index}")
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn dyn_condition(
    path: &str,
    op: &str,
    value: Option<&Value>,
    value2: Option<&Value>,
    names: &mut serde_json::Map<String, Value>,
    values: &mut serde_json::Map<String, Value>,
) -> Result<String> {
    let p = dyn_path(path, names);
    let mut v = |x: Option<&Value>| -> Result<String> {
        let x = x.ok_or_else(|| CoreError::invalid(format!("a value is needed for {op}")))?;
        let key = format!(":v{}", values.len());
        values.insert(key.clone(), x.clone());
        Ok(key)
    };
    Ok(match op {
        "=" | "<>" | "<" | "<=" | ">" | ">=" => format!("{p} {op} {}", v(value)?),
        "between" => {
            let a = v(value)?;
            format!("{p} BETWEEN {a} AND {}", v(value2)?)
        }
        "begins_with" | "contains" => format!("{op}({p}, {})", v(value)?),
        "exists" => format!("attribute_exists({p})"),
        "not_exists" => format!("attribute_not_exists({p})"),
        other => return Err(CoreError::invalid(format!("unknown operator {other}"))),
    })
}

async fn dynamo_call(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    use twinbase_client::dynamo_client::is_read_operation;
    let q: ConnParam = p(params)?;
    let def = app.connection_def(&q.connection)?;
    if def.kind != DbKind::Dynamodb {
        return Err(CoreError::invalid(format!("'{}' is not a DynamoDB connection", def.name)));
    }
    let client = app.client(&q.connection).await?;
    let d = client.dynamo().unwrap();
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let table = s("table");
    let changed = |schema: bool| {
        app.emit(Event::DataChanged { connection: q.connection.clone(), table: (!table.is_empty()).then(|| table.clone()) });
        if schema {
            app.emit(Event::SchemaChanged { connection: q.connection.clone() });
        }
    };
    match method {
        "dynamo.tables" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "tables": d.list_tables().await?, "endpoint": d.endpoint() }))
        }
        "dynamo.describe" => {
            app.check_access(&def, actor, false)?;
            let mut t = d.describe_table(&table).await?;
            if let Ok(ttl) = d.request("DescribeTimeToLive", &json!({ "TableName": table })).await {
                t["TimeToLiveDescription"] = ttl["TimeToLiveDescription"].clone();
            }
            Ok(t)
        }
        "dynamo.read" => {
            app.check_access(&def, actor, false)?;
            #[derive(Deserialize)]
            struct R {
                table: String,
                #[serde(default)]
                index: Option<String>,
                #[serde(default)]
                partition: Option<Value>,
                #[serde(default)]
                sort: Option<DynFilter>,
                #[serde(default)]
                sort_value2: Option<Value>,
                #[serde(default)]
                filters: Vec<DynFilter>,
                #[serde(default)]
                limit: Option<u64>,
                #[serde(default)]
                start_key: Option<Value>,
                #[serde(default)]
                forward: Option<bool>,
            }
            let r: R = p(params)?;
            let mut names = serde_json::Map::new();
            let mut values = serde_json::Map::new();
            let mut req = json!({ "TableName": r.table, "Limit": r.limit.unwrap_or(50).clamp(1, 1000) });
            if let Some(i) = r.index.filter(|i| !i.is_empty()) {
                req["IndexName"] = json!(i);
            }
            let op = if let Some(pv) = &r.partition {
                // The partition key name comes from the table (or index) schema.
                let desc = d.describe_table(&r.table).await?;
                let schema = match req.get("IndexName").and_then(Value::as_str) {
                    Some(i) => desc["GlobalSecondaryIndexes"]
                        .as_array()
                        .into_iter()
                        .chain(desc["LocalSecondaryIndexes"].as_array())
                        .flatten()
                        .find(|x| x["IndexName"] == i)
                        .map(|x| x["KeySchema"].clone())
                        .unwrap_or_default(),
                    None => desc["KeySchema"].clone(),
                };
                let key = |kt: &str| {
                    schema
                        .as_array()
                        .and_then(|a| a.iter().find(|k| k["KeyType"] == kt))
                        .and_then(|k| k["AttributeName"].as_str())
                        .map(str::to_owned)
                };
                let hash = key("HASH").ok_or_else(|| CoreError::invalid("cannot find the partition key"))?;
                let mut kc = dyn_condition(&hash, "=", Some(pv), None, &mut names, &mut values)?;
                if let (Some(sk), Some(range)) = (&r.sort, key("RANGE")) {
                    kc.push_str(" AND ");
                    kc.push_str(&dyn_condition(&range, &sk.op, sk.value.as_ref(), r.sort_value2.as_ref(), &mut names, &mut values)?);
                }
                req["KeyConditionExpression"] = json!(kc);
                if let Some(f) = r.forward {
                    req["ScanIndexForward"] = json!(f);
                }
                "Query"
            } else {
                "Scan"
            };
            if !r.filters.is_empty() {
                let parts: Vec<String> = r
                    .filters
                    .iter()
                    .map(|f| dyn_condition(&f.path, &f.op, f.value.as_ref(), None, &mut names, &mut values))
                    .collect::<Result<_>>()?;
                req["FilterExpression"] = json!(parts.join(" AND "));
            }
            if !names.is_empty() {
                req["ExpressionAttributeNames"] = Value::Object(names);
            }
            if !values.is_empty() {
                req["ExpressionAttributeValues"] = Value::Object(values);
            }
            if let Some(k) = r.start_key.filter(|k| !k.is_null()) {
                req["ExclusiveStartKey"] = k;
            }
            let started = Instant::now();
            let res = d.request(op, &req).await?;
            Ok(json!({
                "operation": op, "request": req, "items": res["Items"], "count": res["Count"], "scanned": res["ScannedCount"],
                "last_key": res.get("LastEvaluatedKey"), "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
            }))
        }
        "dynamo.put" => {
            app.check_access(&def, actor, true)?;
            let mut req = json!({ "TableName": table, "Item": params.get("item").cloned().unwrap_or_default() });
            // Editing an existing item: fail if it was deleted meanwhile; creating: fail if it exists.
            match params.get("expect").and_then(|v| v.as_str()) {
                Some("exists") | Some("new") => {
                    let desc = d.describe_table(&table).await?;
                    let hash = desc["KeySchema"]
                        .as_array()
                        .and_then(|a| a.iter().find(|k| k["KeyType"] == "HASH"))
                        .and_then(|k| k["AttributeName"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    let f = if s("expect") == "new" { "attribute_not_exists" } else { "attribute_exists" };
                    req["ConditionExpression"] = json!(format!("{f}(#k)"));
                    req["ExpressionAttributeNames"] = json!({ "#k": hash });
                }
                _ => {}
            }
            let r = d.request("PutItem", &req).await.map_err(|e| {
                if e.code.as_deref() == Some("ConditionalCheckFailedException") {
                    CoreError::invalid(if s("expect") == "new" {
                        "an item with this key already exists"
                    } else {
                        "the item no longer exists (deleted, or its key changed); reload and try again"
                    })
                } else {
                    e.into()
                }
            })?;
            changed(false);
            Ok(r)
        }
        "dynamo.delete" => {
            app.check_access(&def, actor, true)?;
            let keys = params.get("keys").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            for chunk in keys.chunks(25) {
                let writes: Vec<Value> = chunk.iter().map(|k| json!({ "DeleteRequest": { "Key": k } })).collect();
                let mut req = json!({ "RequestItems": {} });
                req["RequestItems"][&table] = Value::Array(writes);
                d.request("BatchWriteItem", &req).await?;
            }
            changed(false);
            Ok(json!({ "deleted": keys.len() }))
        }
        "dynamo.table.create" => {
            app.check_access(&def, actor, true)?;
            let r = d.request("CreateTable", params.get("definition").unwrap_or(&Value::Null)).await?;
            changed(true);
            Ok(r)
        }
        "dynamo.table.delete" => {
            app.check_access(&def, actor, true)?;
            let r = d.request("DeleteTable", &json!({ "TableName": table })).await?;
            changed(true);
            Ok(r)
        }
        "dynamo.run" => {
            let script = s("command");
            let requests = twinbase_client::dynamo_client::parse_script(&script)?;
            if requests.is_empty() {
                return Err(CoreError::invalid(
                    "nothing to run: write an operation name and its JSON request, e.g. Scan { \"TableName\": \"orders\" }",
                ));
            }
            let writes = requests.iter().any(|(op, _)| !is_read_operation(op));
            app.check_access(&def, actor, writes)?;
            let started = Instant::now();
            let mut outputs = Vec::new();
            for (op, req) in requests {
                let t0 = Instant::now();
                let r = d.request(&op, &req).await;
                let failed = r.is_err();
                outputs.push(match r {
                    Ok(v) => json!({ "operation": op, "request": req, "response": v, "duration_ms": t0.elapsed().as_secs_f64() * 1000.0 }),
                    Err(e) => json!({ "operation": op, "request": req, "error": { "message": e.message, "code": e.code, "detail": e.detail }, "duration_ms": t0.elapsed().as_secs_f64() * 1000.0 }),
                });
                if failed {
                    break;
                }
            }
            let error = outputs.iter().find_map(|o| o["error"]["message"].as_str().map(str::to_owned));
            app.add_history(
                &q.connection,
                HistoryEntry {
                    at: chrono::Utc::now().timestamp_millis(),
                    sql: script,
                    ok: error.is_none(),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error,
                },
            );
            if writes {
                app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
                app.emit(Event::SchemaChanged { connection: q.connection.clone() });
            }
            Ok(json!({ "outputs": outputs }))
        }
        other => Err(CoreError::not_found(format!("unknown method '{other}'"))),
    }
}

async fn mongo_call(app: &Arc<App>, method: &str, params: &Value, actor: &Actor) -> Result<Value> {
    let q: ConnParam = p(params)?;
    let def = app.connection_def(&q.connection)?;
    if def.kind != DbKind::Mongodb {
        return Err(CoreError::invalid(format!("'{}' is not a MongoDB connection", def.name)));
    }
    let client = app.client(&q.connection).await?;
    let m = client.mongo().unwrap();
    let s = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let db = params.get("database").and_then(|v| v.as_str()).filter(|d| !d.is_empty());
    let coll = s("collection");
    let need_coll = || if coll.is_empty() { Err(CoreError::invalid("choose a collection")) } else { Ok(()) };
    let max_docs = params
        .get("max_documents")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(app.settings().row_limit as usize)
        .clamp(1, 100_000);
    let changed = |schema: bool| {
        app.emit(Event::DataChanged { connection: q.connection.clone(), table: (!coll.is_empty()).then(|| coll.clone()) });
        if schema {
            app.emit(Event::SchemaChanged { connection: q.connection.clone() });
        }
    };
    match method {
        "mongo.databases" => {
            app.check_access(&def, actor, false)?;
            Ok(json!({ "databases": m.databases().await?, "default": m.database }))
        }
        "mongo.collections" => {
            app.check_access(&def, actor, false)?;
            Ok(json!(m.collections(db).await?))
        }
        "mongo.find" => {
            app.check_access(&def, actor, false)?;
            need_coll()?;
            let fp: twinbase_client::FindParams = serde_json::from_value(params.clone())?;
            let mut fp = fp;
            fp.limit = fp.limit.clamp(0, 10_000);
            Ok(serde_json::to_value(m.find(db, &coll, &fp).await?)?)
        }
        "mongo.aggregate" => {
            let pipeline = s("pipeline");
            app.check_access(&def, actor, twinbase_client::mongo_shell::script_writes(&format!("db.c.aggregate({pipeline})")))?;
            need_coll()?;
            let (documents, truncated) = m.aggregate(db, &coll, &pipeline, max_docs).await?;
            Ok(json!({ "documents": documents, "truncated": truncated }))
        }
        "mongo.schema" => {
            app.check_access(&def, actor, false)?;
            need_coll()?;
            let sample = params.get("sample").and_then(|v| v.as_u64()).unwrap_or(200).clamp(1, 5000) as u32;
            let schema = m.schema(db, &coll, sample).await?;
            Ok(json!({ "schema": schema, "indexes": m.indexes(db, &coll).await? }))
        }
        "mongo.indexes" => {
            app.check_access(&def, actor, false)?;
            need_coll()?;
            Ok(json!(m.indexes(db, &coll).await?))
        }
        "mongo.insert" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            let ids = m.insert(db, &coll, &s("documents")).await?;
            changed(false);
            Ok(json!({ "inserted_ids": ids }))
        }
        "mongo.replace" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            m.replace(db, &coll, params.get("id").cloned().unwrap_or_default(), &s("document")).await?;
            changed(false);
            Ok(json!({ "ok": true }))
        }
        "mongo.update" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            let many = params.get("many").and_then(|v| v.as_bool()).unwrap_or(false);
            let (matched, modified) = m.update(db, &coll, &s("filter"), &s("update"), many).await?;
            changed(false);
            Ok(json!({ "matched": matched, "modified": modified }))
        }
        "mongo.delete" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            let n = match params.get("ids").and_then(|v| v.as_array()) {
                Some(ids) => m.delete_ids(db, &coll, ids.clone()).await?,
                None => {
                    let filter = s("filter");
                    let many = params.get("many").and_then(|v| v.as_bool()).unwrap_or(false);
                    if many
                        && twinbase_client::mongo_shell::parse_document(&filter).map(|d| d.is_empty()).unwrap_or(false)
                        && !params.get("all").and_then(|v| v.as_bool()).unwrap_or(false)
                    {
                        return Err(CoreError::invalid("an empty filter deletes every document; pass all=true to confirm"));
                    }
                    m.delete(db, &coll, &filter, many).await?
                }
            };
            changed(false);
            Ok(json!({ "deleted": n }))
        }
        "mongo.index.create" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            let unique = params.get("unique").and_then(|v| v.as_bool()).unwrap_or(false);
            let name = params.get("name").and_then(|v| v.as_str()).map(str::to_owned);
            let created = m.create_index(db, &coll, &s("keys"), unique, name).await?;
            changed(true);
            Ok(json!({ "name": created }))
        }
        "mongo.index.drop" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            m.drop_index(db, &coll, &s("name")).await?;
            changed(true);
            Ok(json!({ "ok": true }))
        }
        "mongo.collection.create" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            m.create_collection(db, &coll).await?;
            changed(true);
            Ok(json!({ "ok": true }))
        }
        "mongo.collection.drop" => {
            app.check_access(&def, actor, true)?;
            need_coll()?;
            m.drop_collection(db, &coll).await?;
            changed(true);
            Ok(json!({ "ok": true }))
        }
        "mongo.run" => {
            let script = s("command");
            let writes = twinbase_client::mongo_shell::script_writes(&script);
            app.check_access(&def, actor, writes)?;
            let started = Instant::now();
            let result = m.run(db, &script, max_docs).await;
            let failed = match &result {
                Err(e) => Some(e.message.clone()),
                Ok(outs) => outs.iter().find_map(|o| o.error.as_ref().map(|e| e.message.clone())),
            };
            app.add_history(
                &q.connection,
                HistoryEntry {
                    at: chrono::Utc::now().timestamp_millis(),
                    sql: script,
                    ok: failed.is_none(),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error: failed,
                },
            );
            let outputs = result?;
            if writes {
                app.emit(Event::DataChanged { connection: q.connection.clone(), table: None });
                app.emit(Event::SchemaChanged { connection: q.connection.clone() });
            }
            Ok(json!({ "outputs": outputs }))
        }
        other => Err(CoreError::not_found(format!("unknown method '{other}'"))),
    }
}

/// Current mock data as a seed script (schema + rows, or Redis commands).
async fn capture_seed(app: &Arc<App>, id: &str) -> Result<String> {
    let def = app.mock_def(id)?;
    if !app.is_mock_running(id).await {
        app.start_mock(id).await?;
    }
    if let Some(Backend::Firestore(store)) = app.mock_backend(id).await {
        return tokio::task::spawn_blocking(move || twinbase_mock::firestore::export_seed(&store))
            .await
            .map_err(|e| CoreError::new("mock", e.to_string()));
    }
    if let Some(Backend::Dynamo(store)) = app.mock_backend(id).await {
        return tokio::task::spawn_blocking(move || twinbase_mock::dynamo::export_seed(&store))
            .await
            .map_err(|e| CoreError::new("mock", e.to_string()));
    }
    if let Some(Backend::Mongo(store)) = app.mock_backend(id).await {
        let db = def.database.clone();
        return tokio::task::spawn_blocking(move || twinbase_mock::mongo::export_seed(&store, &db))
            .await
            .map_err(|e| CoreError::new("mock", e.to_string()));
    }
    if let Some(Backend::Redis(_)) = app.mock_backend(id).await {
        let client = app.client(&crate::model::mock_connection_id(id)).await?;
        let r = client.redis().unwrap();
        let (_, keys) = r.scan("*", 0, 100_000, None).await?;
        let mut out = String::from("# TwinBase Redis seed: one command per line\n");
        for k in keys {
            let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            match r.get(&k.key).await? {
                RedisValue::String { value } => out.push_str(&format!("SET {} {}\n", quote(&k.key), quote(&value))),
                RedisValue::List { items } => {
                    out.push_str(&format!("RPUSH {} {}\n", quote(&k.key), items.iter().map(|i| quote(i)).collect::<Vec<_>>().join(" ")))
                }
                RedisValue::Hash { fields } => out.push_str(&format!(
                    "HSET {} {}\n",
                    quote(&k.key),
                    fields.iter().map(|(f, v)| format!("{} {}", quote(f), quote(v))).collect::<Vec<_>>().join(" ")
                )),
                RedisValue::Set { members } => {
                    out.push_str(&format!("SADD {} {}\n", quote(&k.key), members.iter().map(|i| quote(i)).collect::<Vec<_>>().join(" ")))
                }
                RedisValue::Zset { members } => out.push_str(&format!(
                    "ZADD {} {}\n",
                    quote(&k.key),
                    members.iter().map(|(m, s)| format!("{s} {}", quote(m))).collect::<Vec<_>>().join(" ")
                )),
                RedisValue::None => {}
            }
        }
        return Ok(out);
    }
    let conn = crate::model::mock_connection_id(id);
    let (c, cdef) = app.sql_client(&conn).await?;
    let sql = c.sql().unwrap();
    let schema = app.default_schema(sql, &cdef, None).await?;
    let tables = sql.schema_graph(&schema).await?;
    let d = sql.dialect();
    let mut out = format!("-- TwinBase seed for mock '{}' (generated {})\n\n", def.name, chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"));
    out.push_str(&crate::ddl::schema_ddl(&tables, d, d));
    out.push_str("\n\n");
    for t in crate::ddl::dependency_order(&tables).into_iter().filter(|t| t.kind == "table") {
        let rows = sql.query(&format!("SELECT * FROM {}", twinbase_client::sqlgen::qualified(d, &schema, &t.name)), 10_000).await?;
        let Some(rs) = rows.into_iter().next() else { continue };
        // SQL Server only takes explicit values for IDENTITY columns with IDENTITY_INSERT on.
        let identity = d == SqlDialect::Mssql && !rs.rows.is_empty() && t.columns.iter().any(|c| c.auto);
        if identity {
            out.push_str(&format!("SET IDENTITY_INSERT {} ON;\n", twinbase_client::sqlgen::quote_ident(d, &t.name)));
        }
        let cols = rs.columns.iter().map(|c| twinbase_client::sqlgen::quote_ident(d, &c.name)).collect::<Vec<_>>().join(", ");
        for chunk in rs.rows.chunks(100) {
            let vals = chunk
                .iter()
                .map(|r| format!("({})", r.iter().map(|v| twinbase_client::sqlgen::literal(d, v)).collect::<Vec<_>>().join(", ")))
                .collect::<Vec<_>>()
                .join(",\n  ");
            out.push_str(&format!("INSERT INTO {} ({cols}) VALUES\n  {vals};\n", twinbase_client::sqlgen::quote_ident(d, &t.name)));
        }
        if identity {
            out.push_str(&format!("SET IDENTITY_INSERT {} OFF;\n", twinbase_client::sqlgen::quote_ident(d, &t.name)));
        }
    }
    // PostgreSQL sequences must continue after the copied ids.
    if d == SqlDialect::Postgres {
        for t in tables.iter().filter(|t| t.kind == "table") {
            for c in t.columns.iter().filter(|c| c.auto) {
                out.push_str(&format!(
                    "SELECT setval('{}_{}_seq', (SELECT COALESCE(MAX({}), 0) + 1 FROM {}), false);\n",
                    t.name,
                    c.name,
                    twinbase_client::sqlgen::quote_ident(d, &c.name),
                    twinbase_client::sqlgen::quote_ident(d, &t.name)
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod uri_tests {
    use super::split_uri_password;

    #[test]
    fn passwords_leave_connection_strings() {
        assert_eq!(
            split_uri_password("mongodb+srv://app:p%40ss@cluster0.example.net/shop?retryWrites=true"),
            Some(("mongodb+srv://app@cluster0.example.net/shop?retryWrites=true".into(), "app".into(), "p@ss".into()))
        );
        assert_eq!(split_uri_password("mongodb://app@h:27017/x"), None);
        assert_eq!(split_uri_password("mongodb://h:27017/x?u=a:b@c"), None);
        assert_eq!(split_uri_password("db.example.com"), None);
    }
}
