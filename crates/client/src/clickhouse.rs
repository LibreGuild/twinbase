//! ClickHouse driver over the HTTP interface (port 8123, or 8443 with TLS — ClickHouse Cloud).
//!
//! Each statement is one POST with `default_format=JSONCompact`; a session id keeps `USE`, `SET` and
//! temporary tables between statements, as in `clickhouse-client`. 64-bit integers and decimals come
//! back as strings so no precision is lost. ClickHouse has no transactions: staged grid edits are
//! refused (tables are edited with SQL: `INSERT`, `ALTER TABLE … UPDATE/DELETE`, `DELETE FROM`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use serde_json::Value;

use async_trait::async_trait;

use crate::http::{HttpConn, url_encode};
use crate::sqlgen::string_literal;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect, TableInfo, TableSchema,
    TlsMode, int_cell, split_statements,
};

pub struct ClickhouseClient {
    http: HttpConn,
    user: String,
    password: String,
    database: String,
    session: String,
}

/// Endpoint from the connection fields: a URL in `host` (`https://….clickhouse.cloud:8443`), or
/// `host` + `port` (HTTPS when TLS is on).
pub fn endpoint(config: &ConnectionConfig) -> (bool, String, u16) {
    let host = config.host.trim();
    if let Some((scheme, rest)) = host.split_once("://") {
        let https = scheme.eq_ignore_ascii_case("https");
        let authority = rest.split('/').next().unwrap_or(rest);
        let (h, p) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_owned(), p.parse().unwrap_or(if https { 8443 } else { 8123 })),
            _ => (authority.to_owned(), if https { 8443 } else { 8123 }),
        };
        return (https, h, p);
    }
    let https = config.tls != TlsMode::Disable;
    let host = if host.is_empty() { "127.0.0.1".to_owned() } else { host.to_owned() };
    (https, host, if config.port == 0 { if https { 8443 } else { 8123 } } else { config.port })
}

/// `Code: 60. DB::Exception: Unknown table … (UNKNOWN_TABLE) (version …)` → message and code name.
fn error_from(status: u16, headers: &hyper::HeaderMap, body: &[u8]) -> ClientError {
    let text = String::from_utf8_lossy(body).trim().to_owned();
    let number = headers.get("x-clickhouse-exception-code").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut msg = text
        .strip_prefix("Code: ")
        .and_then(|t| t.split_once(". DB::Exception: "))
        .map(|(_, m)| m.to_owned())
        .unwrap_or_else(|| text.clone());
    if let Some(p) = msg.rfind(" (version ") {
        msg.truncate(p);
    }
    let name = msg
        .strip_suffix(')')
        .and_then(|m| m.rsplit_once(" ("))
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
        .map(|(m, n)| (m.to_owned(), n.to_owned()));
    let (message, code_name) = match name {
        Some((m, n)) => (m.trim_end_matches('.').to_owned(), Some(n)),
        None => (msg, None),
    };
    // Some errors append several paragraphs of advice; keep the first.
    let message = message.split("\n\n").next().unwrap_or("").trim().trim_end_matches('.').to_owned();
    if message.is_empty() {
        return ClientError::new(format!("HTTP {status}"));
    }
    let mut e = ClientError::new(message);
    e.code = code_name.or(number.clone());
    e.detail = number.map(|n| format!("ClickHouse error {n}"));
    e
}

/// Cells: 64-bit integers arrive as strings; small ones become numbers again.
fn cell(v: Value, ty: &str) -> Value {
    let base = ty.trim_start_matches("Nullable(").trim_start_matches("LowCardinality(");
    match &v {
        Value::String(s)
            if base.starts_with("Int64") || base.starts_with("UInt64") || base.starts_with("Int32") || base.starts_with("UInt32") =>
        {
            s.parse::<i64>().map(int_cell).unwrap_or(v)
        }
        _ => v,
    }
}

impl ClickhouseClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<ClickhouseClient> {
        let (https, host, port) = endpoint(config);
        let http = HttpConn::new(https, host, port, config.via, config.tls, Duration::from_secs(config.timeout_secs.max(1)));
        let client = ClickhouseClient {
            http,
            user: if config.user.is_empty() { "default".into() } else { config.user.clone() },
            password: config.password.clone().unwrap_or_default(),
            database: config.database.clone().filter(|d| !d.is_empty()).unwrap_or_default(),
            session: format!("twinbase-{}", uuid::Uuid::new_v4().simple()),
        };
        client.run("SELECT 1", 1).await?;
        Ok(client)
    }

    /// Run one statement; returns the result set (if any) and the rows written.
    async fn run(&self, sql: &str, max_rows: usize) -> Result<(Option<(Vec<ResultColumn>, Vec<Vec<Value>>, bool)>, Option<u64>)> {
        self.run_with(sql, max_rows, false).await
    }

    /// `read_only` sets `readonly=2`: the server refuses writes and DDL (settings may still change).
    async fn run_with(
        &self,
        sql: &str,
        max_rows: usize,
        read_only: bool,
    ) -> Result<(Option<(Vec<ResultColumn>, Vec<Vec<Value>>, bool)>, Option<u64>)> {
        let mut q = vec![
            ("session_id", self.session.clone()),
            ("session_timeout", "3600".into()),
            ("default_format", "JSONCompact".into()),
            ("output_format_json_quote_64bit_integers", "1".into()),
            ("output_format_json_quote_decimals", "1".into()),
            ("max_result_rows", (max_rows + 1).to_string()),
            ("result_overflow_mode", "break".into()),
            ("wait_end_of_query", "1".into()),
        ];
        if !self.database.is_empty() {
            q.push(("database", self.database.clone()));
        }
        if read_only {
            q.push(("readonly", "2".into()));
        }
        let uri = format!("/?{}", q.iter().map(|(k, v)| format!("{k}={}", url_encode(v))).collect::<Vec<_>>().join("&"));
        let body = Bytes::from(sql.to_owned());
        let res = self
            .http
            .send("query", self.http.timeout.max(Duration::from_secs(300)), || {
                hyper::Request::builder()
                    .method("POST")
                    .uri(uri.as_str())
                    .header("host", self.http.host_header())
                    .header("content-type", "text/plain; charset=utf-8")
                    .header("x-clickhouse-user", self.user.as_str())
                    .header("x-clickhouse-key", self.password.as_str())
                    .body(Full::new(body.clone()))
                    .map_err(|e| ClientError::new(e.to_string()))
            })
            .await?;
        if res.status != 200 {
            return Err(error_from(res.status, &res.headers, &res.body));
        }
        let written = res
            .headers
            .get("x-clickhouse-summary")
            .and_then(|v| serde_json::from_slice::<Value>(v.as_bytes()).ok())
            .and_then(|s| s["written_rows"].as_str().and_then(|n| n.parse::<u64>().ok()));
        if res.body.iter().all(u8::is_ascii_whitespace) {
            return Ok((None, written));
        }
        // JSONCompact, unless the statement asked for another FORMAT: then show the text.
        match serde_json::from_slice::<Value>(&res.body) {
            Ok(mut v) if v.get("meta").is_some() && v.get("data").is_some() => {
                let columns: Vec<ResultColumn> = v["meta"]
                    .as_array()
                    .map(|m| {
                        m.iter()
                            .map(|c| ResultColumn {
                                name: c["name"].as_str().unwrap_or("").to_owned(),
                                type_name: c["type"].as_str().unwrap_or("").to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let data = v["data"].take();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                let mut truncated = false;
                for row in data.as_array().cloned().unwrap_or_default() {
                    if rows.len() >= max_rows {
                        truncated = true;
                        break;
                    }
                    let Value::Array(cells) = row else { continue };
                    rows.push(cells.into_iter().zip(&columns).map(|(c, col)| cell(c, &col.type_name)).collect());
                }
                Ok((Some((columns, rows, truncated)), written))
            }
            _ => {
                let text = String::from_utf8_lossy(&res.body);
                let mut rows: Vec<Vec<Value>> = text.lines().map(|l| vec![Value::String(l.to_owned())]).collect();
                let truncated = rows.len() > max_rows;
                rows.truncate(max_rows);
                Ok((Some((vec![ResultColumn { name: "output".into(), type_name: "String".into() }], rows, truncated)), written))
            }
        }
    }

    async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        let (rs, _) = self.run(sql, 1_000_000).await?;
        Ok(rs
            .map(|(_, rows, _)| {
                rows.into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|v| match v {
                                Value::String(s) => s,
                                Value::Null => String::new(),
                                other => other.to_string(),
                            })
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn current_database(&self) -> Result<String> {
        Ok(self
            .text_rows("SELECT currentDatabase()")
            .await?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap_or_else(|| "default".into()))
    }
}

/// Engines whose tables are views.
fn is_view(engine: &str) -> bool {
    matches!(engine, "View" | "MaterializedView" | "LiveView" | "WindowView")
}

#[async_trait]
impl SqlClient for ClickhouseClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Clickhouse
    }

    async fn server_version(&self) -> Result<String> {
        let v = self.text_rows("SELECT version()").await?;
        Ok(format!("ClickHouse {}", v.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or_default()))
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .text_rows(
                "SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
                 ORDER BY name != currentDatabase(), name",
            )
            .await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).collect())
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let lit = string_literal(SqlDialect::Clickhouse, schema);
        let rows = self
            .text_rows(&format!("SELECT name, engine FROM system.tables WHERE database = {lit} AND NOT is_temporary ORDER BY name"))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| TableInfo {
                schema: schema.to_owned(),
                name: r[0].clone(),
                kind: if is_view(&r[1]) { "view".into() } else { "table".into() },
            })
            .collect())
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let lit = string_literal(SqlDialect::Clickhouse, schema);
        let tables = self
            .text_rows(&format!(
                "SELECT name, engine, primary_key FROM system.tables WHERE database = {lit} AND NOT is_temporary ORDER BY name"
            ))
            .await?;
        let columns = self
            .text_rows(&format!("SELECT table, name, type, default_kind, default_expression FROM system.columns WHERE database = {lit} ORDER BY table, position"))
            .await?;
        let skipping = self
            .text_rows(&format!(
                "SELECT table, name, expr, type FROM system.data_skipping_indices WHERE database = {lit} ORDER BY table, name"
            ))
            .await
            .unwrap_or_default();
        let mut out: Vec<TableSchema> = tables
            .iter()
            .map(|r| TableSchema {
                schema: schema.to_owned(),
                name: r[0].clone(),
                kind: if is_view(&r[1]) { "view".into() } else { "table".into() },
                columns: vec![],
                primary_key: r[2].split(',').map(|k| k.trim().trim_matches('`').to_owned()).filter(|k| !k.is_empty()).collect(),
                foreign_keys: vec![],
                indexes: vec![],
            })
            .collect();
        let pos: HashMap<String, usize> = out.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        for r in &columns {
            let Some(&i) = pos.get(&r[0]) else { continue };
            let ty = r[2].clone();
            let kind = r[3].as_str();
            out[i].columns.push(ColumnInfo {
                name: r[1].clone(),
                nullable: ty.starts_with("Nullable("),
                data_type: ty,
                default: (kind == "DEFAULT" && !r[4].is_empty()).then(|| r[4].clone()),
                primary_key: false,
                unique: false,
                // MATERIALIZED and ALIAS columns are computed by the server.
                auto: matches!(kind, "MATERIALIZED" | "ALIAS"),
            });
        }
        for r in &skipping {
            let Some(&i) = pos.get(&r[0]) else { continue };
            out[i].indexes.push(IndexInfo {
                name: r[1].clone(),
                columns: vec![format!("{} ({})", r[2], r[3])],
                unique: false,
                primary: false,
            });
        }
        for t in &mut out {
            // Keep only plain column names in the key (expressions such as toDate(ts) stay out).
            let names: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            t.primary_key.retain(|k| names.contains(k));
            for c in &mut t.columns {
                c.primary_key = t.primary_key.contains(&c.name);
            }
            if !t.primary_key.is_empty() {
                t.indexes.insert(0, IndexInfo { name: "PRIMARY KEY".into(), columns: t.primary_key.clone(), unique: false, primary: true });
            }
        }
        Ok(out)
    }

    async fn describe(&self, schema: &str, table: &str) -> Result<TableSchema> {
        let schema = if schema.is_empty() { self.current_database().await? } else { schema.to_owned() };
        self.schema_graph(&schema)
            .await?
            .into_iter()
            .find(|t| t.name == table)
            .ok_or_else(|| ClientError::new(format!("table {schema}.{table} not found")))
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.statements(sql, max_rows, false).await
    }

    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.statements(sql, max_rows, true).await
    }

    async fn execute_in_transaction(&self, _statements: &[String], _expected: Option<u64>) -> Result<u64> {
        Err(ClientError::new(
            "ClickHouse has no transactions, so TwinBase does not apply staged edits. Change data with SQL: INSERT, ALTER TABLE … UPDATE / DELETE, or DELETE FROM.",
        ))
    }
}

impl ClickhouseClient {
    async fn statements(&self, sql: &str, max_rows: usize, read_only: bool) -> Result<Vec<QueryResult>> {
        let mut out = Vec::new();
        for (i, stmt) in split_statements(sql, SqlDialect::Clickhouse).iter().enumerate() {
            let started = Instant::now();
            match self.run_with(stmt, max_rows, read_only).await {
                Ok((rs, written)) => {
                    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                    out.push(match rs {
                        Some((columns, rows, truncated)) => {
                            QueryResult { statement: stmt.clone(), columns, rows, truncated, elapsed_ms, ..Default::default() }
                        }
                        None => QueryResult {
                            statement: stmt.clone(),
                            affected: written,
                            elapsed_ms,
                            message: written.is_none().then(|| "Ok.".to_owned()),
                            ..Default::default()
                        },
                    });
                }
                Err(mut e) => {
                    e.statement_index = Some(i);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_message_and_code_name() {
        let mut h = hyper::HeaderMap::new();
        h.insert("x-clickhouse-exception-code", "60".parse().unwrap());
        let e = error_from(404, &h, b"Code: 60. DB::Exception: Unknown table expression identifier 'nope' in scope SELECT * FROM nope. (UNKNOWN_TABLE) (version 25.8.33.6 (official build))\n");
        assert_eq!(e.message, "Unknown table expression identifier 'nope' in scope SELECT * FROM nope");
        assert_eq!(e.code.as_deref(), Some("UNKNOWN_TABLE"));
        assert_eq!(e.detail.as_deref(), Some("ClickHouse error 60"));
    }

    #[test]
    fn endpoints() {
        let mut c = ConnectionConfig {
            kind: crate::DbKind::Clickhouse,
            host: "https://abc.eu-west-1.aws.clickhouse.cloud:8443".into(),
            port: 0,
            user: String::new(),
            password: None,
            database: None,
            path: None,
            tls: TlsMode::Disable,
            timeout_secs: 5,
            ssh: None,
            via: None,
        };
        assert_eq!(endpoint(&c), (true, "abc.eu-west-1.aws.clickhouse.cloud".into(), 8443));
        c.host = "db.local".into();
        assert_eq!(endpoint(&c), (false, "db.local".into(), 8123));
        c.tls = TlsMode::VerifyFull;
        assert_eq!(endpoint(&c), (true, "db.local".into(), 8443));
    }

    #[test]
    fn small_64_bit_integers_become_numbers() {
        assert_eq!(cell(Value::String("5".into()), "Int64"), serde_json::json!(5));
        assert_eq!(cell(Value::String("18446744073709551615".into()), "UInt64"), serde_json::json!("18446744073709551615"));
        assert_eq!(cell(Value::String("1.50".into()), "Decimal(18, 2)"), serde_json::json!("1.50"));
    }
}
