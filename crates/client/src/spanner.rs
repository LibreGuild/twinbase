//! Google Cloud Spanner (GoogleSQL databases) over the REST API, or the Spanner emulator.
//!
//! Queries run in single-use read-only transactions; each DML statement runs in its own read-write
//! transaction and reports its exact row count; consecutive DDL statements go to the database admin
//! API as one schema update. Staged grid edits share one read-write transaction with the usual
//! affected-row check.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use http_body_util::Full;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::google_auth::GoogleAuth;
use crate::http::HttpConn;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, TlsMode, float_cell, hex_cell, int_cell, split_statements,
};

const AUDIENCE: &str = "https://spanner.googleapis.com/";

pub struct SpannerClient {
    http: HttpConn,
    auth: GoogleAuth,
    /// `projects/{p}/instances/{i}/databases/{d}`.
    pub database: String,
    session: Mutex<Option<String>>,
}

/// `project/instance/database` (or the full resource name) from the connection's database field.
pub fn database_name(field: &str, credentials_project: Option<&str>) -> Result<String> {
    let f = field.trim().trim_matches('/');
    if f.starts_with("projects/") {
        return Ok(f.to_owned());
    }
    let parts: Vec<&str> = f.split('/').filter(|p| !p.is_empty()).collect();
    match (parts.as_slice(), credentials_project) {
        ([p, i, d], _) => Ok(format!("projects/{p}/instances/{i}/databases/{d}")),
        ([i, d], Some(p)) => Ok(format!("projects/{p}/instances/{i}/databases/{d}")),
        _ => Err(ClientError::new("enter the database as project/instance/database")),
    }
}

/// Endpoint: Google Cloud when the host is empty, else the emulator (`host:port`, REST port 9020).
pub fn endpoint(config: &ConnectionConfig) -> (bool, String, u16) {
    let host = config.host.trim();
    if host.is_empty() {
        return (true, "spanner.googleapis.com".into(), 443);
    }
    if let Some((scheme, rest)) = host.split_once("://") {
        let https = scheme.eq_ignore_ascii_case("https");
        let authority = rest.split('/').next().unwrap_or(rest);
        return match authority.rsplit_once(':') {
            Some((h, p)) => (https, h.to_owned(), p.parse().unwrap_or(if https { 443 } else { 9020 })),
            None => (https, authority.to_owned(), if https { 443 } else { 9020 }),
        };
    }
    (config.tls != TlsMode::Disable, host.to_owned(), if config.port == 0 { 9020 } else { config.port })
}

/// `{"error": {"code": 400, "message": "…", "status": "INVALID_ARGUMENT"}}` or the emulator's
/// `{"code": 3, "message": "…"}`.
fn error_from(status: u16, v: &Value, raw: &[u8]) -> ClientError {
    let e = if v.get("error").is_some() { &v["error"] } else { v };
    let message = e["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("HTTP {status}: {}", String::from_utf8_lossy(raw).chars().take(300).collect::<String>()));
    let mut err = ClientError::new(message);
    err.code = e["status"].as_str().map(str::to_owned).or_else(|| {
        e["code"].as_u64().and_then(|c| {
            [
                "OK",
                "CANCELLED",
                "UNKNOWN",
                "INVALID_ARGUMENT",
                "DEADLINE_EXCEEDED",
                "NOT_FOUND",
                "ALREADY_EXISTS",
                "PERMISSION_DENIED",
                "RESOURCE_EXHAUSTED",
                "FAILED_PRECONDITION",
                "ABORTED",
                "OUT_OF_RANGE",
                "UNIMPLEMENTED",
                "INTERNAL",
                "UNAVAILABLE",
                "DATA_LOSS",
                "UNAUTHENTICATED",
            ]
            .get(c as usize)
            .map(|s| s.to_string())
        })
    });
    err
}

/// Type name as GoogleSQL writes it (`INT64`, `ARRAY<STRING>`, `STRUCT<…>`).
fn type_name(t: &Value) -> String {
    match t["code"].as_str().unwrap_or("") {
        "ARRAY" => format!("ARRAY<{}>", type_name(&t["arrayElementType"])),
        "STRUCT" => format!(
            "STRUCT<{}>",
            t["structType"]["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| format!("{} {}", f["name"].as_str().unwrap_or(""), type_name(&f["type"])))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_owned(),
    }
}

/// A result value by type: INT64 arrives as a string, BYTES as base64, JSON as text.
pub fn cell(v: &Value, t: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match t["code"].as_str().unwrap_or("") {
        "INT64" => v.as_str().and_then(|s| s.parse::<i64>().ok()).map(int_cell).unwrap_or_else(|| v.clone()),
        "FLOAT64" | "FLOAT32" => v.as_f64().map(float_cell).unwrap_or_else(|| v.clone()),
        "BYTES" => v
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .map(|b| hex_cell(&b))
            .unwrap_or_else(|| v.clone()),
        "JSON" => v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| v.clone()),
        "ARRAY" => Value::Array(v.as_array().into_iter().flatten().map(|x| cell(x, &t["arrayElementType"])).collect()),
        "STRUCT" => {
            let fields = t["structType"]["fields"].as_array().cloned().unwrap_or_default();
            Value::Object(
                v.as_array()
                    .into_iter()
                    .flatten()
                    .zip(&fields)
                    .map(|(x, f)| (f["name"].as_str().unwrap_or("").to_owned(), cell(x, &f["type"])))
                    .collect(),
            )
        }
        _ => v.clone(),
    }
}

fn first_word(stmt: &str) -> String {
    stmt.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_ascii_uppercase()
}

fn is_ddl(stmt: &str) -> bool {
    matches!(first_word(stmt).as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME" | "GRANT" | "REVOKE" | "ANALYZE")
}

fn is_dml(stmt: &str) -> bool {
    matches!(first_word(stmt).as_str(), "INSERT" | "UPDATE" | "DELETE")
}

impl SpannerClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<SpannerClient> {
        let (https, host, port) = endpoint(config);
        let emulator = !config.host.trim().is_empty();
        let (auth, project) = crate::google_auth::for_connection(config.path.as_deref(), config.password.as_deref(), emulator)?;
        let database = database_name(config.database.as_deref().unwrap_or(""), project.as_deref())?;
        let http = HttpConn::new(https, host, port, config.via, config.tls, Duration::from_secs(config.timeout_secs.max(1)));
        let client = SpannerClient { http, auth, database, session: Mutex::new(None) };
        client.session().await?;
        Ok(client)
    }

    pub fn endpoint_url(&self) -> String {
        self.http.base_url()
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let token = self.auth.token(AUDIENCE).await?;
        let bytes = Bytes::from(body.map(|b| b.to_string()).unwrap_or_default());
        let uri = format!("/v1/{path}");
        let res = self
            .http
            .send(path, self.http.timeout.max(Duration::from_secs(300)), || {
                let mut r = hyper::Request::builder()
                    .method(method)
                    .uri(uri.as_str())
                    .header("host", self.http.host_header())
                    .header("authorization", format!("Bearer {token}"));
                if body.is_some() {
                    r = r.header("content-type", "application/json");
                }
                r.body(Full::new(bytes.clone())).map_err(|e| ClientError::new(e.to_string()))
            })
            .await?;
        let v: Value = serde_json::from_slice(&res.body).unwrap_or(Value::Null);
        if res.status >= 400 {
            return Err(error_from(res.status, &v, &res.body));
        }
        Ok(v)
    }

    async fn session(&self) -> Result<String> {
        let mut s = self.session.lock().await;
        if let Some(name) = s.as_ref() {
            return Ok(name.clone());
        }
        let v = self
            .call("POST", &format!("{}/sessions", self.database), Some(&json!({ "session": { "labels": { "client": "twinbase" } } })))
            .await?;
        let name = v["name"].as_str().ok_or_else(|| ClientError::new("Spanner did not return a session"))?.to_owned();
        *s = Some(name.clone());
        Ok(name)
    }

    /// Call a session method, recreating the session once if Spanner expired it.
    async fn session_call(&self, verb: &str, body: &Value) -> Result<Value> {
        for attempt in 0..2 {
            let session = self.session().await?;
            match self.call("POST", &format!("{session}:{verb}"), Some(body)).await {
                Err(e) if attempt == 0 && e.code.as_deref() == Some("NOT_FOUND") && e.message.to_ascii_lowercase().contains("session") => {
                    *self.session.lock().await = None;
                }
                other => return other,
            }
        }
        Err(ClientError::new("the Spanner session could not be recreated"))
    }

    async fn execute(&self, sql: &str, transaction: Value, seqno: Option<u64>, mode: Option<&str>) -> Result<Value> {
        let mut body = json!({ "sql": sql, "transaction": transaction });
        if let Some(n) = seqno {
            body["seqno"] = json!(n.to_string());
        }
        if let Some(m) = mode {
            body["queryMode"] = json!(m);
        }
        self.session_call("executeSql", &body).await
    }

    async fn begin(&self) -> Result<String> {
        let v = self.session_call("beginTransaction", &json!({ "options": { "readWrite": {} } })).await?;
        v["id"].as_str().map(str::to_owned).ok_or_else(|| ClientError::new("Spanner did not start a transaction"))
    }

    async fn commit(&self, tx: &str) -> Result<()> {
        self.session_call("commit", &json!({ "transactionId": tx })).await.map(|_| ())
    }

    async fn rollback(&self, tx: &str) {
        let _ = self.session_call("rollback", &json!({ "transactionId": tx })).await;
    }

    /// Apply DDL statements and wait for the schema change to finish.
    async fn update_ddl(&self, statements: &[String]) -> Result<()> {
        let mut op = self.call("PATCH", &format!("{}/ddl", self.database), Some(&json!({ "statements": statements }))).await?;
        let started = Instant::now();
        while !op["done"].as_bool().unwrap_or(false) {
            if started.elapsed() > Duration::from_secs(600) {
                return Err(ClientError::new("the schema change is still running after 10 minutes; it continues on the server"));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            let name = op["name"].as_str().unwrap_or("").to_owned();
            if name.is_empty() {
                break;
            }
            op = self.call("GET", &name, None).await?;
        }
        if op.get("error").is_some_and(|e| !e.is_null()) {
            return Err(error_from(400, &json!({ "error": op["error"] }), b""));
        }
        Ok(())
    }

    fn result(stmt: &str, v: &Value, max_rows: usize, started: Instant) -> QueryResult {
        let fields = v["metadata"]["rowType"]["fields"].as_array().cloned().unwrap_or_default();
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if fields.is_empty() {
            let affected =
                v["stats"]["rowCountExact"].as_str().and_then(|n| n.parse().ok()).or_else(|| v["stats"]["rowCountExact"].as_u64());
            return QueryResult {
                statement: stmt.to_owned(),
                affected,
                elapsed_ms,
                message: affected.is_none().then(|| "OK".to_owned()),
                ..Default::default()
            };
        }
        let columns = fields
            .iter()
            .map(|f| ResultColumn { name: f["name"].as_str().unwrap_or("").to_owned(), type_name: type_name(&f["type"]) })
            .collect();
        let all = v["rows"].as_array().cloned().unwrap_or_default();
        let truncated = all.len() > max_rows;
        let rows = all
            .iter()
            .take(max_rows)
            .map(|r| r.as_array().into_iter().flatten().zip(&fields).map(|(x, f)| cell(x, &f["type"])).collect())
            .collect();
        QueryResult { statement: stmt.to_owned(), columns, rows, truncated, elapsed_ms, ..Default::default() }
    }

    async fn rows(&self, sql: &str) -> Result<Vec<Vec<Value>>> {
        let v = self.execute(sql, json!({ "singleUse": { "readOnly": { "strong": true } } }), None, None).await?;
        Ok(Self::result(sql, &v, usize::MAX, Instant::now()).rows)
    }

    async fn run_script(&self, sql: &str, max_rows: usize, tx: Option<&str>) -> Result<Vec<QueryResult>> {
        let statements = split_statements(sql, SqlDialect::Spanner);
        let mut out = Vec::new();
        let mut i = 0;
        let mut seqno = 0;
        while i < statements.len() {
            let started = Instant::now();
            let stmt = &statements[i];
            let res: Result<()> = async {
                if is_ddl(stmt) {
                    if tx.is_some() {
                        return Err(ClientError::new("schema changes cannot run inside a transaction"));
                    }
                    // Consecutive schema statements are applied as one update (Spanner's preference).
                    let mut batch = vec![stmt.clone()];
                    while i + 1 < statements.len() && is_ddl(&statements[i + 1]) {
                        i += 1;
                        batch.push(statements[i].clone());
                    }
                    self.update_ddl(&batch).await?;
                    out.push(QueryResult {
                        statement: batch.join(";\n"),
                        message: Some(format!("Schema updated ({} statement{})", batch.len(), if batch.len() == 1 { "" } else { "s" })),
                        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                        ..Default::default()
                    });
                } else if let Some(t) = tx {
                    seqno += 1;
                    let v = self.execute(stmt, json!({ "id": t }), Some(seqno), None).await?;
                    out.push(Self::result(stmt, &v, max_rows, started));
                } else if is_dml(stmt) {
                    let t = self.begin().await?;
                    match self.execute(stmt, json!({ "id": t }), Some(1), None).await {
                        Ok(v) => {
                            self.commit(&t).await?;
                            out.push(Self::result(stmt, &v, max_rows, started));
                        }
                        Err(e) => {
                            self.rollback(&t).await;
                            return Err(e);
                        }
                    }
                } else {
                    let v = self.execute(stmt, json!({ "singleUse": { "readOnly": { "strong": true } } }), None, None).await?;
                    out.push(Self::result(stmt, &v, max_rows, started));
                }
                Ok(())
            }
            .await;
            if let Err(mut e) = res {
                e.statement_index = Some(i);
                return Err(e);
            }
            i += 1;
        }
        Ok(out)
    }
}

/// GoogleSQL string literal.
fn lit(s: &str) -> String {
    crate::sqlgen::string_literal(SqlDialect::Spanner, s)
}

#[async_trait]
impl SqlClient for SpannerClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Spanner
    }

    async fn server_version(&self) -> Result<String> {
        let db = self.call("GET", &self.database, None).await.unwrap_or(Value::Null);
        let dialect = db["databaseDialect"].as_str().unwrap_or("GOOGLE_STANDARD_SQL");
        let short = self.database.split('/').collect::<Vec<_>>();
        let place = if short.len() >= 6 { format!("{}/{}/{}", short[1], short[3], short[5]) } else { self.database.clone() };
        Ok(format!(
            "Cloud Spanner {place} ({}){}",
            if dialect == "POSTGRESQL" { "PostgreSQL dialect" } else { "GoogleSQL" },
            if self.http.https { "" } else { " · emulator" }
        ))
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let rows = self.rows("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS') ORDER BY SCHEMA_NAME").await?;
        // The default schema is the empty name.
        Ok(rows.into_iter().filter_map(|r| r.first().and_then(|v| v.as_str()).map(str::to_owned)).collect())
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let rows = self
            .rows(&format!(
                "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
                lit(schema)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| TableInfo {
                schema: schema.to_owned(),
                name: r[0].as_str().unwrap_or("").to_owned(),
                kind: if r[1].as_str() == Some("VIEW") { "view".into() } else { "table".into() },
            })
            .collect())
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let s = lit(schema);
        let tables = self.rows(&format!("SELECT TABLE_NAME, TABLE_TYPE, PARENT_TABLE_NAME, ON_DELETE_ACTION FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = {s} ORDER BY TABLE_NAME")).await?;
        let columns = self
            .rows(&format!("SELECT TABLE_NAME, COLUMN_NAME, SPANNER_TYPE, IS_NULLABLE, COLUMN_DEFAULT, IS_GENERATED FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = {s} ORDER BY TABLE_NAME, ORDINAL_POSITION"))
            .await?;
        let index_cols = self
            .rows(&format!(
                "SELECT ic.TABLE_NAME, ic.INDEX_NAME, i.INDEX_TYPE, i.IS_UNIQUE, ic.COLUMN_NAME FROM INFORMATION_SCHEMA.INDEX_COLUMNS ic \
                 JOIN INFORMATION_SCHEMA.INDEXES i ON i.TABLE_SCHEMA = ic.TABLE_SCHEMA AND i.TABLE_NAME = ic.TABLE_NAME AND i.INDEX_NAME = ic.INDEX_NAME \
                 WHERE ic.TABLE_SCHEMA = {s} AND ic.ORDINAL_POSITION IS NOT NULL ORDER BY ic.TABLE_NAME, ic.INDEX_NAME, ic.ORDINAL_POSITION"
            ))
            .await?;
        let fks = self
            .rows(&format!(
                "SELECT rc.CONSTRAINT_NAME, k.TABLE_NAME, k.COLUMN_NAME, r.TABLE_NAME, r.COLUMN_NAME, rc.DELETE_RULE FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS rc \
                 JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = rc.CONSTRAINT_NAME \
                 JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE r ON r.CONSTRAINT_SCHEMA = rc.UNIQUE_CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = rc.UNIQUE_CONSTRAINT_NAME AND r.ORDINAL_POSITION = k.POSITION_IN_UNIQUE_CONSTRAINT \
                 WHERE rc.CONSTRAINT_SCHEMA = {s} ORDER BY rc.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ))
            .await
            .unwrap_or_default();
        let g = |r: &Vec<Value>, i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let mut out: Vec<TableSchema> = tables
            .iter()
            .map(|r| TableSchema {
                schema: schema.to_owned(),
                name: g(r, 0),
                kind: if g(r, 1) == "VIEW" { "view".into() } else { "table".into() },
                columns: vec![],
                primary_key: vec![],
                foreign_keys: vec![],
                indexes: vec![],
            })
            .collect();
        let pos: HashMap<String, usize> = out.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        for r in &columns {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            out[i].columns.push(ColumnInfo {
                name: g(r, 1),
                data_type: g(r, 2),
                nullable: g(r, 3) == "YES",
                default: r.get(4).and_then(|v| v.as_str()).map(str::to_owned),
                primary_key: false,
                unique: false,
                auto: g(r, 5) == "ALWAYS",
            });
        }
        for r in &index_cols {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let (name, kind, col) = (g(r, 1), g(r, 2), g(r, 4));
            let primary = kind == "PRIMARY_KEY";
            if primary {
                out[i].primary_key.push(col.clone());
            }
            match out[i].indexes.iter_mut().find(|x| x.name == name) {
                Some(ix) => ix.columns.push(col),
                None => out[i].indexes.push(IndexInfo {
                    name,
                    columns: vec![col],
                    unique: r.get(3).and_then(Value::as_bool).unwrap_or(false),
                    primary,
                }),
            }
        }
        for r in &fks {
            let Some(&i) = pos.get(&g(r, 1)) else { continue };
            let name = g(r, 0);
            match out[i].foreign_keys.iter_mut().find(|f| f.name == name) {
                Some(f) => {
                    f.columns.push(g(r, 2));
                    f.ref_columns.push(g(r, 4));
                }
                None => out[i].foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![g(r, 2)],
                    ref_schema: schema.to_owned(),
                    ref_table: g(r, 3),
                    ref_columns: vec![g(r, 4)],
                    on_delete: Some(g(r, 5)).filter(|a| a == "CASCADE"),
                }),
            }
        }
        // Interleaved tables reference their parent through the parent's key.
        for r in &tables {
            let parent = g(r, 2);
            if parent.is_empty() {
                continue;
            }
            let (Some(&ci), Some(&pi)) = (pos.get(&g(r, 0)), pos.get(&parent)) else { continue };
            let key = out[pi].primary_key.clone();
            out[ci].foreign_keys.push(ForeignKeyInfo {
                name: format!("INTERLEAVE IN PARENT {parent}"),
                columns: key.clone(),
                ref_schema: schema.to_owned(),
                ref_table: parent,
                ref_columns: key,
                on_delete: Some(g(r, 3)).filter(|a| a == "CASCADE"),
            });
        }
        for t in &mut out {
            let single_unique: Vec<String> =
                t.indexes.iter().filter(|x| x.unique && !x.primary && x.columns.len() == 1).map(|x| x.columns[0].clone()).collect();
            for c in &mut t.columns {
                c.primary_key = t.primary_key.contains(&c.name);
                c.unique = single_unique.contains(&c.name);
            }
        }
        Ok(out)
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.run_script(sql, max_rows, None).await
    }

    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let tx = self.begin().await?;
        let r = self.run_script(sql, max_rows, Some(&tx)).await;
        self.rollback(&tx).await;
        r
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let tx = self.begin().await?;
        let mut total = 0;
        for (i, s) in statements.iter().enumerate() {
            match self.execute(s, json!({ "id": tx }), Some(i as u64 + 1), None).await {
                Ok(v) => total += v["stats"]["rowCountExact"].as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0),
                Err(mut e) => {
                    self.rollback(&tx).await;
                    e.statement_index = Some(i);
                    return Err(e);
                }
            }
        }
        if let Some(e) = expected.filter(|e| *e != total) {
            self.rollback(&tx).await;
            return Err(ClientError::new(format!(
                "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
            ))
            .with_code("TB001"));
        }
        self.commit(&tx).await?;
        Ok(total)
    }

    async fn native_plan(&self, sql: &str, analyze: bool) -> Result<Option<Value>> {
        let v = if analyze {
            // PROFILE executes the statement: inside a read-write transaction that is rolled back.
            let tx = self.begin().await?;
            let r = self.execute(sql, json!({ "id": tx }), Some(1), Some("PROFILE")).await;
            self.rollback(&tx).await;
            r?
        } else {
            self.execute(sql, json!({ "singleUse": { "readOnly": { "strong": true } } }), None, Some("PLAN")).await?
        };
        Ok(Some(v["stats"].clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_values() {
        assert_eq!(database_name("p/i/d", None).unwrap(), "projects/p/instances/i/databases/d");
        assert_eq!(database_name("i/d", Some("p")).unwrap(), "projects/p/instances/i/databases/d");
        assert!(database_name("d", None).is_err());
        let t = json!({ "code": "ARRAY", "arrayElementType": { "code": "INT64" } });
        assert_eq!(cell(&json!(["1", "9007199254740993"]), &t), json!([1, "9007199254740993"]));
        assert_eq!(cell(&json!("yg=="), &json!({ "code": "BYTES" })), json!("\\xca"));
        assert_eq!(cell(&json!("{\"k\":1}"), &json!({ "code": "JSON" })), json!({ "k": 1 }));
        assert_eq!(type_name(&t), "ARRAY<INT64>");
        let e = error_from(400, &json!({ "code": 3, "message": "bad" }), b"");
        assert_eq!(e.code.as_deref(), Some("INVALID_ARGUMENT"));
        assert!(is_ddl("create table t (id int64) primary key (id)") && is_dml(" UPDATE t SET a = 1 WHERE TRUE"));
    }
}
