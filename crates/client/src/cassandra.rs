//! Apache Cassandra and ScyllaDB driver (CQL native protocol through the `scylla` crate).
//!
//! Keyspaces are the schemas; a table's primary key is its partition key followed by its clustering
//! columns. CQL has no OFFSET and no transactions: the grid pages by reading ahead, and staged edits
//! run as one logged batch (applied together, though without isolation — Cassandra writes are
//! upserts, so there are no affected-row counts to check).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::value::{CqlValue, Row};
use serde_json::{Value, json};

use crate::{
    ClientError, ColumnInfo, ConnectionConfig, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect, TableInfo, TableSchema,
    TlsMode, float_cell, hex_cell, int_cell, split_statements,
};

pub struct CassandraClient {
    session: Session,
    keyspace: Option<String>,
}

/// `Database returned an error: <kind>, Error message: <message>` → message and a kind name.
fn cql_err(e: impl std::fmt::Display) -> ClientError {
    let text = e.to_string();
    let Some((head, message)) = text.split_once("Error message: ") else {
        return ClientError::new(text);
    };
    let head = head.to_ascii_lowercase();
    let code = [
        ("syntax error", "SyntaxError"),
        ("syntactically correct but invalid", "Invalid"),
        ("already exist", "AlreadyExists"),
        ("not authorized", "Unauthorized"),
        ("unauthorized", "Unauthorized"),
        ("authentication", "AuthenticationError"),
        ("not enough nodes", "Unavailable"),
        ("unavailable", "Unavailable"),
        ("timeout", "Timeout"),
        ("timed out", "Timeout"),
        ("overloaded", "Overloaded"),
        ("truncat", "TruncateError"),
        ("configuration", "ConfigError"),
    ]
    .iter()
    .find(|(k, _)| head.contains(k))
    .map(|(_, c)| c.to_string());
    let mut err = ClientError::new(message.trim().to_owned());
    err.code = code;
    err
}

/// CQL type name of a result column.
fn type_name(t: &ColumnType) -> String {
    match t {
        ColumnType::Native(n) => match n {
            NativeType::Text => "text".into(),
            other => format!("{other:?}").to_ascii_lowercase(),
        },
        ColumnType::Collection { typ, .. } => match typ {
            CollectionType::List(e) => format!("list<{}>", type_name(e)),
            CollectionType::Set(e) => format!("set<{}>", type_name(e)),
            CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
            _ => "collection".into(),
        },
        ColumnType::Vector { typ, dimensions } => format!("vector<{}, {dimensions}>", type_name(typ)),
        ColumnType::UserDefinedType { definition, .. } => definition.name.to_string(),
        ColumnType::Tuple(ts) => format!("tuple<{}>", ts.iter().map(type_name).collect::<Vec<_>>().join(", ")),
        _ => "unknown".into(),
    }
}

/// Two's-complement big-endian bytes as a decimal string (exact up to 128 bits).
fn varint_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".into();
    }
    if bytes.len() > 16 {
        return format!("0x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    let mut buf = if bytes[0] & 0x80 != 0 { [0xffu8; 16] } else { [0u8; 16] };
    buf[16 - bytes.len()..].copy_from_slice(bytes);
    i128::from_be_bytes(buf).to_string()
}

fn decimal_text(unscaled: &[u8], scale: i32) -> String {
    let digits = varint_text(unscaled);
    if digits.starts_with("0x") || scale <= 0 {
        return if scale < 0 && !digits.starts_with("0x") { format!("{digits}{}", "0".repeat((-scale) as usize)) } else { digits };
    }
    let (sign, abs) = match digits.strip_prefix('-') {
        Some(a) => ("-", a.to_owned()),
        None => ("", digits),
    };
    let scale = scale as usize;
    let padded = format!("{abs:0>width$}", width = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{int}.{frac}")
}

fn cell(v: &Option<CqlValue>) -> Value {
    let Some(v) = v else { return Value::Null };
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::String(s.clone()),
        CqlValue::Boolean(b) => Value::Bool(*b),
        CqlValue::Blob(b) => hex_cell(b),
        CqlValue::Counter(c) => int_cell(c.0),
        CqlValue::Decimal(d) => {
            let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
            Value::String(decimal_text(bytes, scale))
        }
        CqlValue::Date(d) => {
            // Days with the Unix epoch at 2^31.
            let days = d.0 as i64 - (1i64 << 31);
            i32::try_from(719_163 + days)
                .ok()
                .and_then(chrono::NaiveDate::from_num_days_from_ce_opt)
                .map(|d| Value::String(d.to_string()))
                .unwrap_or_else(|| json!(days))
        }
        CqlValue::Double(f) => float_cell(*f),
        CqlValue::Duration(d) => Value::String(format!("{}mo{}d{}ns", d.months, d.days, d.nanoseconds)),
        CqlValue::Empty => Value::Null,
        CqlValue::Float(f) => float_cell(*f as f64),
        CqlValue::Int(i) => json!(i),
        CqlValue::BigInt(i) => int_cell(*i),
        CqlValue::SmallInt(i) => json!(i),
        CqlValue::TinyInt(i) => json!(i),
        CqlValue::Timestamp(t) => chrono::DateTime::from_timestamp_millis(t.0)
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.3fZ").to_string()))
            .unwrap_or_else(|| int_cell(t.0)),
        CqlValue::Time(t) => {
            let n = t.0;
            let frac = n % 1_000_000_000;
            let base = format!("{:02}:{:02}:{:02}", n / 3_600_000_000_000, n / 60_000_000_000 % 60, n / 1_000_000_000 % 60);
            Value::String(if frac == 0 { base } else { format!("{base}.{}", format!("{frac:09}").trim_end_matches('0')) })
        }
        CqlValue::Inet(a) => Value::String(a.to_string()),
        CqlValue::Uuid(u) => Value::String(u.to_string()),
        CqlValue::Timeuuid(u) => Value::String(u.to_string()),
        CqlValue::Varint(v) => Value::String(varint_text(v.as_signed_bytes_be_slice())),
        CqlValue::List(xs) | CqlValue::Set(xs) | CqlValue::Vector(xs) => Value::Array(xs.iter().map(|x| cell(&Some(x.clone()))).collect()),
        CqlValue::Map(kv) => {
            // Text keys make a JSON object; other keys a list of [key, value] pairs.
            if kv.iter().all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_))) {
                Value::Object(
                    kv.iter().map(|(k, v)| (k.as_text().or(k.as_ascii()).cloned().unwrap_or_default(), cell(&Some(v.clone())))).collect(),
                )
            } else {
                Value::Array(kv.iter().map(|(k, v)| json!([cell(&Some(k.clone())), cell(&Some(v.clone()))])).collect())
            }
        }
        CqlValue::Tuple(xs) => Value::Array(xs.iter().map(cell).collect()),
        CqlValue::UserDefinedType { fields, .. } => Value::Object(fields.iter().map(|(k, v)| (k.clone(), cell(v))).collect()),
        other => Value::String(other.to_string()),
    }
}

/// Statements that return rows.
fn returns_rows(stmt: &str) -> bool {
    let first = stmt.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_ascii_uppercase();
    matches!(first.as_str(), "SELECT" | "DESCRIBE" | "DESC" | "LIST")
}

struct ThroughContactPoint(std::net::SocketAddr);

#[async_trait]
impl scylla::policies::address_translator::AddressTranslator for ThroughContactPoint {
    async fn translate_address(
        &self,
        _peer: &scylla::policies::address_translator::UntranslatedPeer,
    ) -> std::result::Result<std::net::SocketAddr, scylla::errors::TranslationError> {
        Ok(self.0)
    }
}

impl CassandraClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<CassandraClient> {
        let host = if config.host.is_empty() { "127.0.0.1".to_owned() } else { config.host.clone() };
        let port = if config.port == 0 { 9042 } else { config.port };
        // Every node is reached through the address entered (or the tunnel): nodes often advertise
        // addresses that are private to their network (Docker, Kubernetes, NAT, a bastion), and the
        // node TwinBase talks to coordinates requests for the others.
        let contact = match config.via {
            Some(via) => via,
            None => tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| ClientError::new(format!("cannot resolve {host}: {e}")))?
                .next()
                .ok_or_else(|| ClientError::new(format!("cannot resolve {host}")))?,
        };
        let mut b = SessionBuilder::new()
            .connection_timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .known_node_addr(contact)
            .address_translator(std::sync::Arc::new(ThroughContactPoint(contact)));
        // The driver binds its sockets before connecting; bound to the wildcard address, macOS can
        // refuse to reach loopback ("Can't assign requested address"). Loopback contacts bind there.
        if contact.ip().is_loopback() {
            b = b.local_ip_address(Some(contact.ip()));
        }
        if !config.user.is_empty() {
            b = b.user(config.user.clone(), config.password.clone().unwrap_or_default());
        }
        if config.tls != TlsMode::Disable {
            let mut tls = crate::tls::client_config(config.tls);
            // Cassandra and ScyllaDB do not negotiate ALPN.
            tls.alpn_protocols.clear();
            b = b.tls_context(Some(std::sync::Arc::new(tls)));
        }
        let keyspace = config.database.clone().filter(|k| !k.is_empty());
        if let Some(ks) = &keyspace {
            b = b.use_keyspace(ks.clone(), true);
        }
        let session = b.build().await.map_err(cql_err)?;
        Ok(CassandraClient { session, keyspace })
    }

    /// Run one statement and collect up to `max_rows` rows.
    async fn run(&self, stmt: &str, max_rows: usize) -> Result<QueryResult> {
        let started = Instant::now();
        if returns_rows(stmt) {
            let pager = self.session.query_iter(stmt, ()).await.map_err(cql_err)?;
            let columns: Vec<ResultColumn> =
                pager.column_specs().iter().map(|c| ResultColumn { name: c.name().to_owned(), type_name: type_name(c.typ()) }).collect();
            let mut stream = pager.rows_stream::<Row>().map_err(cql_err)?;
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(row) = stream.next().await {
                let row = row.map_err(cql_err)?;
                if rows.len() >= max_rows {
                    truncated = true;
                    break;
                }
                rows.push(row.columns.iter().map(cell).collect());
            }
            return Ok(QueryResult {
                statement: stmt.to_owned(),
                columns,
                rows,
                truncated,
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                ..Default::default()
            });
        }
        let r = self.session.query_unpaged(stmt, ()).await.map_err(cql_err)?;
        // Lightweight transactions (IF …) answer with an [applied] row.
        if let Ok(rows) = r.into_rows_result() {
            let columns: Vec<ResultColumn> =
                rows.column_specs().iter().map(|c| ResultColumn { name: c.name().to_owned(), type_name: type_name(c.typ()) }).collect();
            let data: Vec<Vec<Value>> =
                rows.rows::<Row>().map_err(cql_err)?.filter_map(|r| r.ok()).map(|r| r.columns.iter().map(cell).collect()).collect();
            return Ok(QueryResult {
                statement: stmt.to_owned(),
                columns,
                rows: data,
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                ..Default::default()
            });
        }
        Ok(QueryResult {
            statement: stmt.to_owned(),
            message: Some("OK".into()),
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            ..Default::default()
        })
    }

    async fn text_rows(&self, cql: &str, keyspace: &str) -> Result<Vec<Vec<Value>>> {
        let pager = self.session.query_iter(cql, (keyspace,)).await.map_err(cql_err)?;
        let mut stream = pager.rows_stream::<Row>().map_err(cql_err)?;
        let mut out = Vec::new();
        while let Some(row) = stream.next().await {
            out.push(row.map_err(cql_err)?.columns.iter().map(cell).collect());
        }
        Ok(out)
    }
}

#[async_trait]
impl SqlClient for CassandraClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Cassandra
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.run("SELECT release_version, cluster_name FROM system.local", 1).await?;
        let row = r.rows.first().cloned().unwrap_or_default();
        let s = |i: usize| row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        // ScyllaDB reports a Cassandra-compatible release_version; its own version is in system.versions.
        let scylla = self
            .run("SELECT version FROM system.versions", 1)
            .await
            .ok()
            .and_then(|r| r.rows.first().and_then(|row| row.first()).and_then(|v| v.as_str()).map(str::to_owned));
        Ok(match scylla {
            Some(v) => format!("ScyllaDB {v} · cluster {}", s(1)),
            None => format!("Cassandra {} · cluster {}", s(0), s(1)),
        })
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let r = self.run("SELECT keyspace_name FROM system_schema.keyspaces", 10_000).await?;
        let mut out: Vec<String> = r
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(|v| v.as_str()).map(str::to_owned))
            .filter(|k| !k.starts_with("system") && !matches!(k.as_str(), "dse_system" | "dse_security" | "solr_admin"))
            .collect();
        out.sort();
        if let Some(k) = &self.keyspace
            && let Some(p) = out.iter().position(|x| x == k)
        {
            let k = out.remove(p);
            out.insert(0, k);
        }
        Ok(out)
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let mut out: Vec<TableInfo> = self
            .text_rows("SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?", schema)
            .await?
            .into_iter()
            .filter_map(|r| {
                r.first().and_then(|v| v.as_str()).map(|n| TableInfo {
                    schema: schema.to_owned(),
                    name: n.to_owned(),
                    kind: "table".into(),
                })
            })
            .collect();
        out.extend(
            self.text_rows("SELECT view_name FROM system_schema.views WHERE keyspace_name = ?", schema)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    r.first().and_then(|v| v.as_str()).map(|n| TableInfo {
                        schema: schema.to_owned(),
                        name: n.to_owned(),
                        kind: "view".into(),
                    })
                }),
        );
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let tables = self.tables(schema).await?;
        let columns = self
            .text_rows(
                "SELECT table_name, column_name, kind, position, type, clustering_order FROM system_schema.columns WHERE keyspace_name = ?",
                schema,
            )
            .await?;
        let indexes = self
            .text_rows("SELECT table_name, index_name, options FROM system_schema.indexes WHERE keyspace_name = ?", schema)
            .await
            .unwrap_or_default();
        let s = |r: &Vec<Value>, i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let mut out: Vec<TableSchema> = tables
            .iter()
            .map(|t| TableSchema {
                schema: schema.to_owned(),
                name: t.name.clone(),
                kind: t.kind.clone(),
                columns: vec![],
                primary_key: vec![],
                foreign_keys: vec![],
                indexes: vec![],
            })
            .collect();
        let pos: HashMap<String, usize> = out.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        // (kind order, position, name) per table, to order key columns.
        let mut keys: HashMap<usize, Vec<(u8, i64, String)>> = HashMap::new();
        for r in &columns {
            let Some(&i) = pos.get(&s(r, 0)) else { continue };
            let kind = s(r, 2);
            let position = r.get(3).and_then(Value::as_i64).unwrap_or(0);
            let name = s(r, 1);
            let rank = match kind.as_str() {
                "partition_key" => Some(0),
                "clustering" => Some(1),
                _ => None,
            };
            if let Some(rank) = rank {
                keys.entry(i).or_default().push((rank, position, name.clone()));
            }
            out[i].columns.push(ColumnInfo {
                name,
                data_type: s(r, 4),
                nullable: rank.is_none(),
                default: None,
                primary_key: rank.is_some(),
                unique: false,
                auto: false,
            });
        }
        for (i, mut k) in keys {
            k.sort();
            let partition: Vec<String> = k.iter().filter(|x| x.0 == 0).map(|x| x.2.clone()).collect();
            let clustering: Vec<String> = k.iter().filter(|x| x.0 == 1).map(|x| x.2.clone()).collect();
            out[i].primary_key = k.into_iter().map(|x| x.2).collect();
            // The partition key is kept as a separate index so DDL can rebuild PRIMARY KEY ((…), …).
            out[i].indexes.push(IndexInfo { name: "partition key".into(), columns: partition, unique: false, primary: true });
            if !clustering.is_empty() {
                out[i].indexes.push(IndexInfo { name: "clustering key".into(), columns: clustering, unique: false, primary: true });
            }
        }
        for r in &indexes {
            let Some(&i) = pos.get(&s(r, 0)) else { continue };
            let target = r.get(2).and_then(|o| o.get("target")).and_then(Value::as_str).unwrap_or("").to_owned();
            out[i].indexes.push(IndexInfo { name: s(r, 1), columns: vec![target], unique: false, primary: false });
        }
        // Column order: key columns first, in key order, then the rest alphabetically (as cqlsh shows).
        for t in &mut out {
            let pk = t.primary_key.clone();
            t.columns.sort_by_key(|c| (pk.iter().position(|k| k == &c.name).unwrap_or(usize::MAX), c.name.clone()));
        }
        Ok(out)
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut out = Vec::new();
        for (i, stmt) in split_statements(sql, SqlDialect::Cassandra).iter().enumerate() {
            match self.run(stmt, max_rows).await {
                Ok(r) => out.push(r),
                Err(mut e) => {
                    e.statement_index = Some(i);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// Staged edits as one logged batch: all of them are applied, or none.
    async fn execute_in_transaction(&self, statements: &[String], _expected: Option<u64>) -> Result<u64> {
        if statements.is_empty() {
            return Ok(0);
        }
        let batch = format!("BEGIN BATCH\n{};\nAPPLY BATCH", statements.join(";\n"));
        self.session.query_unpaged(batch, ()).await.map_err(cql_err)?;
        Ok(statements.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_exact() {
        assert_eq!(varint_text(&[0x01, 0x00]), "256");
        assert_eq!(varint_text(&[0xff]), "-1");
        assert_eq!(decimal_text(&[0x04, 0xe2], 2), "12.50");
        assert_eq!(decimal_text(&[0xfb], 3), "-0.005");
        assert_eq!(decimal_text(&[0x07], -2), "700");
    }

    #[test]
    fn errors_keep_message_and_kind() {
        let e = cql_err(
            "Database returned an error: The submitted query has a syntax error, Error message: line 1:0 no viable alternative at input 'SELEC'",
        );
        assert_eq!(e.message, "line 1:0 no viable alternative at input 'SELEC'");
        assert_eq!(e.code.as_deref(), Some("SyntaxError"));
    }
}
