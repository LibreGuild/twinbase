//! Google BigQuery over the REST API (or the BigQuery emulator).
//!
//! Datasets are the schemas. Browsing a table without filters reads it with `tabledata.list`, which
//! BigQuery does not bill; filtered or sorted pages and the SQL editor run query jobs (billed by
//! bytes scanned — Explain shows a dry-run estimate first). Tables are not edited from the grid:
//! BigQuery DML runs as jobs with quotas, so changes are made with SQL.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use http_body_util::Full;
use serde_json::{Value, json};

use crate::google_auth::GoogleAuth;
use crate::http::{HttpConn, url_encode};
use crate::sqlgen::RowQuery;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, RowsPage, SqlClient,
    SqlDialect, TableInfo, TableSchema, TlsMode, float_cell, hex_cell, int_cell, split_statements,
};

const AUDIENCE: &str = "https://bigquery.googleapis.com/";

pub struct BigQueryClient {
    http: HttpConn,
    auth: GoogleAuth,
    pub project: String,
    /// Default dataset for unqualified table names (`project.dataset` in the database field).
    pub dataset: Option<String>,
}

pub fn endpoint(config: &ConnectionConfig) -> (bool, String, u16) {
    let host = config.host.trim();
    if host.is_empty() {
        return (true, "bigquery.googleapis.com".into(), 443);
    }
    if let Some((scheme, rest)) = host.split_once("://") {
        let https = scheme.eq_ignore_ascii_case("https");
        let authority = rest.split('/').next().unwrap_or(rest);
        return match authority.rsplit_once(':') {
            Some((h, p)) => (https, h.to_owned(), p.parse().unwrap_or(if https { 443 } else { 9050 })),
            None => (https, authority.to_owned(), if https { 443 } else { 9050 }),
        };
    }
    (config.tls != TlsMode::Disable, host.to_owned(), if config.port == 0 { 9050 } else { config.port })
}

/// `{"error": {"code": 400, "message": "…", "errors": [{"reason": "invalidQuery"}]}}`
fn error_from(status: u16, v: &Value, raw: &[u8]) -> ClientError {
    let e = &v["error"];
    let mut err = ClientError::new(
        e["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("HTTP {status}: {}", String::from_utf8_lossy(raw).chars().take(300).collect::<String>())),
    );
    err.code = e["errors"][0]["reason"].as_str().or(e["status"].as_str()).map(str::to_owned);
    err
}

/// Type as GoogleSQL writes it: `INT64`, `ARRAY<STRING>`, `STRUCT<a INT64, b STRING>`.
fn type_name(f: &Value) -> String {
    let base = match f["type"].as_str().unwrap_or("") {
        "INTEGER" => "INT64".to_owned(),
        "FLOAT" => "FLOAT64".to_owned(),
        "BOOLEAN" => "BOOL".to_owned(),
        "RECORD" | "STRUCT" => format!(
            "STRUCT<{}>",
            f["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| format!("{} {}", x["name"].as_str().unwrap_or(""), type_name(x)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_owned(),
    };
    if f["mode"].as_str() == Some("REPEATED") { format!("ARRAY<{base}>") } else { base }
}

/// A value of the `{f: [{v: …}]}` row encoding, by field.
fn cell(v: &Value, f: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if f["mode"].as_str() == Some("REPEATED") {
        let scalar = {
            let mut x = f.clone();
            x["mode"] = json!("NULLABLE");
            x
        };
        return Value::Array(v.as_array().into_iter().flatten().map(|x| cell(&x["v"], &scalar)).collect());
    }
    let s = v.as_str();
    match f["type"].as_str().unwrap_or("") {
        "INTEGER" | "INT64" => s.and_then(|s| s.parse::<i64>().ok()).map(int_cell).unwrap_or_else(|| v.clone()),
        "FLOAT" | "FLOAT64" => s.and_then(|s| s.parse::<f64>().ok()).map(float_cell).unwrap_or_else(|| v.clone()),
        "BOOLEAN" | "BOOL" => s.map(|s| Value::Bool(s == "true")).unwrap_or_else(|| v.clone()),
        // Seconds since the epoch, with microseconds.
        "TIMESTAMP" => s
            .and_then(|s| s.parse::<f64>().ok())
            .and_then(|secs| chrono::DateTime::from_timestamp_micros((secs * 1_000_000.0).round() as i64))
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.6f UTC").to_string().replace(".000000 UTC", " UTC")))
            .unwrap_or_else(|| v.clone()),
        "BYTES" => {
            s.and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()).map(|b| hex_cell(&b)).unwrap_or_else(|| v.clone())
        }
        "JSON" => s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| v.clone()),
        "RECORD" | "STRUCT" => {
            let fields = f["fields"].as_array().cloned().unwrap_or_default();
            Value::Object(
                v["f"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .zip(&fields)
                    .map(|(x, sf)| (sf["name"].as_str().unwrap_or("").to_owned(), cell(&x["v"], sf)))
                    .collect(),
            )
        }
        _ => v.clone(),
    }
}

fn rows_of(v: &Value, fields: &[Value], max: usize) -> Vec<Vec<Value>> {
    v["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .take(max)
        .map(|r| r["f"].as_array().into_iter().flatten().zip(fields).map(|(x, f)| cell(&x["v"], f)).collect())
        .collect()
}

impl BigQueryClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<BigQueryClient> {
        let (https, host, port) = endpoint(config);
        let emulator = !config.host.trim().is_empty();
        let (auth, cred_project) = crate::google_auth::for_connection(config.path.as_deref(), config.password.as_deref(), emulator)?;
        let field = config
            .database
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or(cred_project)
            .ok_or_else(|| ClientError::new("enter the Google Cloud project id (optionally project.dataset)"))?;
        let (project, dataset) = match field.trim().split_once('.') {
            Some((p, d)) => (p.to_owned(), Some(d.to_owned()).filter(|d| !d.is_empty())),
            None => (field.trim().to_owned(), None),
        };
        let http = HttpConn::new(https, host, port, config.via, config.tls, Duration::from_secs(config.timeout_secs.max(1)));
        let client = BigQueryClient { http, auth, project, dataset };
        client.call("GET", &format!("projects/{}/datasets?maxResults=1", url_encode(&client.project)), None).await?;
        Ok(client)
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let token = self.auth.token(AUDIENCE).await?;
        let bytes = Bytes::from(body.map(|b| b.to_string()).unwrap_or_default());
        let uri = format!("/bigquery/v2/{path}");
        let res = self
            .http
            .send(path, self.http.timeout.max(Duration::from_secs(120)), || {
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

    fn p(&self) -> String {
        url_encode(&self.project)
    }

    /// Run one query job and wait for it; `max_rows` rows of the first page are kept.
    async fn job(&self, sql: &str, max_rows: usize) -> Result<Value> {
        let mut body = json!({ "query": sql, "useLegacySql": false, "maxResults": (max_rows + 1).min(100_000), "timeoutMs": 30_000 });
        if let Some(d) = &self.dataset {
            body["defaultDataset"] = json!({ "projectId": self.project, "datasetId": d });
        }
        let mut v = self.call("POST", &format!("projects/{}/queries", self.p()), Some(&body)).await?;
        let started = Instant::now();
        while v["jobComplete"].as_bool() == Some(false) {
            if started.elapsed() > Duration::from_secs(1800) {
                return Err(ClientError::new("the query is still running after 30 minutes; it continues in BigQuery"));
            }
            let job = v["jobReference"]["jobId"].as_str().unwrap_or("").to_owned();
            let loc = v["jobReference"]["location"].as_str().map(|l| format!("&location={}", url_encode(l))).unwrap_or_default();
            v = self
                .call(
                    "GET",
                    &format!(
                        "projects/{}/queries/{}?timeoutMs=30000&maxResults={}{loc}",
                        self.p(),
                        url_encode(&job),
                        (max_rows + 1).min(100_000)
                    ),
                    None,
                )
                .await?;
        }
        Ok(v)
    }

    async fn table(&self, dataset: &str, table: &str) -> Result<Value> {
        self.call("GET", &format!("projects/{}/datasets/{}/tables/{}", self.p(), url_encode(dataset), url_encode(table)), None).await
    }

    fn schema_of(&self, dataset: &str, t: &Value) -> TableSchema {
        let name = t["tableReference"]["tableId"].as_str().unwrap_or("").to_owned();
        let kind = if matches!(t["type"].as_str(), Some("VIEW" | "MATERIALIZED_VIEW")) { "view" } else { "table" };
        let pk: Vec<String> = t["tableConstraints"]["primaryKey"]["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_owned))
            .collect();
        let columns = t["schema"]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let n = f["name"].as_str().unwrap_or("").to_owned();
                ColumnInfo {
                    primary_key: pk.contains(&n),
                    name: n,
                    data_type: type_name(f),
                    nullable: f["mode"].as_str() != Some("REQUIRED"),
                    default: f["defaultValueExpression"].as_str().map(str::to_owned),
                    unique: false,
                    auto: false,
                }
            })
            .collect();
        let foreign_keys = t["tableConstraints"]["foreignKeys"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|fk| {
                let refs = fk["columnReferences"].as_array().cloned().unwrap_or_default();
                ForeignKeyInfo {
                    name: fk["name"].as_str().unwrap_or("fk").to_owned(),
                    columns: refs.iter().filter_map(|r| r["referencingColumn"].as_str().map(str::to_owned)).collect(),
                    ref_schema: fk["referencedTable"]["datasetId"].as_str().unwrap_or(dataset).to_owned(),
                    ref_table: fk["referencedTable"]["tableId"].as_str().unwrap_or("").to_owned(),
                    ref_columns: refs.iter().filter_map(|r| r["referencedColumn"].as_str().map(str::to_owned)).collect(),
                    on_delete: None,
                }
            })
            .collect();
        let mut indexes = Vec::new();
        if !pk.is_empty() {
            indexes.push(IndexInfo { name: "primary key (not enforced)".into(), columns: pk.clone(), unique: true, primary: true });
        }
        for c in t["clustering"]["fields"].as_array().into_iter().flatten().filter_map(|c| c.as_str()) {
            match indexes.iter_mut().find(|i: &&mut IndexInfo| i.name == "clustering") {
                Some(i) => i.columns.push(c.to_owned()),
                None => indexes.push(IndexInfo { name: "clustering".into(), columns: vec![c.to_owned()], unique: false, primary: false }),
            }
        }
        TableSchema { schema: dataset.to_owned(), name, kind: kind.into(), columns, primary_key: pk, foreign_keys, indexes }
    }
}

#[async_trait]
impl SqlClient for BigQueryClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Bigquery
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("BigQuery project {}{}", self.project, if self.http.https { "" } else { " · emulator" }))
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = token.as_ref().map(|t| format!("&pageToken={}", url_encode(t))).unwrap_or_default();
            let v = self.call("GET", &format!("projects/{}/datasets?maxResults=1000{page}", self.p()), None).await?;
            out.extend(
                v["datasets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d["datasetReference"]["datasetId"].as_str().map(str::to_owned)),
            );
            match v["nextPageToken"].as_str() {
                Some(t) if out.len() < 50_000 => token = Some(t.to_owned()),
                _ => break,
            }
        }
        out.sort();
        if let Some(d) = &self.dataset
            && let Some(p) = out.iter().position(|x| x == d)
        {
            let d = out.remove(p);
            out.insert(0, d);
        }
        Ok(out)
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = token.as_ref().map(|t| format!("&pageToken={}", url_encode(t))).unwrap_or_default();
            let v = self
                .call("GET", &format!("projects/{}/datasets/{}/tables?maxResults=1000{page}", self.p(), url_encode(schema)), None)
                .await?;
            out.extend(v["tables"].as_array().into_iter().flatten().map(|t| TableInfo {
                schema: schema.to_owned(),
                name: t["tableReference"]["tableId"].as_str().unwrap_or("").to_owned(),
                kind: if matches!(t["type"].as_str(), Some("VIEW" | "MATERIALIZED_VIEW")) { "view".into() } else { "table".into() },
            }));
            match v["nextPageToken"].as_str() {
                Some(t) if out.len() < 50_000 => token = Some(t.to_owned()),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let tables = self.tables(schema).await?;
        // Table metadata is one request per table: fetch a few at a time.
        let mut out = Vec::with_capacity(tables.len());
        for chunk in tables.chunks(8) {
            let got = futures::future::join_all(chunk.iter().map(|t| self.table(schema, &t.name))).await;
            for t in got {
                out.push(self.schema_of(schema, &t?));
            }
        }
        Ok(out)
    }

    async fn describe(&self, schema: &str, table: &str) -> Result<TableSchema> {
        Ok(self.schema_of(schema, &self.table(schema, table).await?))
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut out = Vec::new();
        for (i, stmt) in split_statements(sql, SqlDialect::Bigquery).iter().enumerate() {
            let started = Instant::now();
            let v = self.job(stmt, max_rows).await.map_err(|mut e| {
                e.statement_index = Some(i);
                e
            })?;
            let fields = v["schema"]["fields"].as_array().cloned().unwrap_or_default();
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let affected = v["numDmlAffectedRows"].as_str().and_then(|n| n.parse().ok());
            if fields.is_empty() {
                out.push(QueryResult {
                    statement: stmt.clone(),
                    affected,
                    elapsed_ms,
                    message: affected.is_none().then(|| "OK".to_owned()),
                    ..Default::default()
                });
                continue;
            }
            let rows = rows_of(&v, &fields, max_rows);
            let total = v["totalRows"].as_str().and_then(|n| n.parse::<usize>().ok()).unwrap_or(rows.len());
            out.push(QueryResult {
                statement: stmt.clone(),
                columns: fields
                    .iter()
                    .map(|f| ResultColumn { name: f["name"].as_str().unwrap_or("").to_owned(), type_name: type_name(f) })
                    .collect(),
                truncated: total > rows.len(),
                rows,
                elapsed_ms,
                message: v["cacheHit"].as_bool().filter(|c| *c).map(|_| "Served from the query cache (no charge)".to_owned()),
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn execute_in_transaction(&self, _statements: &[String], _expected: Option<u64>) -> Result<u64> {
        Err(ClientError::new(
            "TwinBase does not edit BigQuery tables from the grid: run INSERT, UPDATE or DELETE statements (DML jobs) in the SQL editor.",
        ))
    }

    /// Unfiltered, unsorted pages come from `tabledata.list`, which is not billed.
    async fn fetch_page(&self, schema: &str, table: &str, query: &RowQuery) -> Result<Option<RowsPage>> {
        if !query.filters.is_empty() || !query.sort.is_empty() || query.where_sql.as_ref().is_some_and(|w| !w.trim().is_empty()) {
            return Ok(None);
        }
        let started = Instant::now();
        let meta = self.table(schema, table).await?;
        if matches!(meta["type"].as_str(), Some("VIEW" | "MATERIALIZED_VIEW" | "EXTERNAL")) {
            return Ok(None);
        }
        let fields = meta["schema"]["fields"].as_array().cloned().unwrap_or_default();
        let v = self
            .call(
                "GET",
                &format!(
                    "projects/{}/datasets/{}/tables/{}/data?startIndex={}&maxResults={}",
                    self.p(),
                    url_encode(schema),
                    url_encode(table),
                    query.offset,
                    query.limit.max(1)
                ),
                None,
            )
            .await?;
        let rows = rows_of(&v, &fields, query.limit.max(1) as usize);
        let total = meta["numRows"].as_str().and_then(|n| n.parse().ok()).or_else(|| v["totalRows"].as_str().and_then(|n| n.parse().ok()));
        let result = QueryResult {
            statement: format!("tabledata.list {schema}.{table}"),
            columns: fields
                .iter()
                .map(|f| ResultColumn { name: f["name"].as_str().unwrap_or("").to_owned(), type_name: type_name(f) })
                .collect(),
            rows,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            ..Default::default()
        };
        Ok(Some(RowsPage { result, total, total_at_least: None, key_columns: vec![], row_id_column: None }))
    }

    /// A dry run: what the query would scan (and so cost), without running it.
    async fn native_plan(&self, sql: &str, _analyze: bool) -> Result<Option<Value>> {
        let mut q = json!({ "query": sql, "useLegacySql": false });
        if let Some(d) = &self.dataset {
            q["defaultDataset"] = json!({ "projectId": self.project, "datasetId": d });
        }
        let v = self
            .call("POST", &format!("projects/{}/jobs", self.p()), Some(&json!({ "configuration": { "dryRun": true, "query": q } })))
            .await?;
        Ok(Some(json!({ "dry_run": v["statistics"] })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rows() {
        let fields = vec![
            json!({ "name": "id", "type": "INTEGER", "mode": "REQUIRED" }),
            json!({ "name": "ts", "type": "TIMESTAMP" }),
            json!({ "name": "tags", "type": "STRING", "mode": "REPEATED" }),
            json!({ "name": "rec", "type": "RECORD", "fields": [{ "name": "a", "type": "INTEGER" }, { "name": "b", "type": "STRING" }] }),
            json!({ "name": "ok", "type": "BOOLEAN" }),
        ];
        let v = json!({ "rows": [{ "f": [{ "v": "1" }, { "v": "1704164645.500000" }, { "v": [{ "v": "x" }, { "v": "y" }] }, { "v": { "f": [{ "v": "1" }, { "v": "q" }] } }, { "v": "true" }] }] });
        assert_eq!(
            rows_of(&v, &fields, 10)[0],
            vec![json!(1), json!("2024-01-02 03:04:05.500000 UTC"), json!(["x", "y"]), json!({ "a": 1, "b": "q" }), json!(true)]
        );
        assert_eq!(type_name(&fields[2]), "ARRAY<STRING>");
        assert_eq!(type_name(&fields[3]), "STRUCT<a INT64, b STRING>");
        let e = error_from(
            400,
            &json!({ "error": { "code": 400, "message": "Unrecognized name: nope", "errors": [{ "reason": "invalidQuery" }] } }),
            b"",
        );
        assert_eq!((e.message.as_str(), e.code.as_deref()), ("Unrecognized name: nope", Some("invalidQuery")));
    }
}
