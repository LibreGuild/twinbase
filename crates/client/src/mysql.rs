//! MySQL / MariaDB driver (mysql_async).

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::*;
use tokio::sync::Mutex;

use crate::sqlgen::string_literal;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, TlsMode, float_cell, hex_cell, int_cell, split_statements,
};

pub struct MySqlClient {
    conn: Mutex<mysql_async::Conn>,
}

fn my_err(e: mysql_async::Error) -> ClientError {
    match e {
        mysql_async::Error::Server(s) => ClientError::new(s.message).with_code(s.code.to_string()),
        other => ClientError::new(other.to_string()),
    }
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType::*;
    match t {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB => "blob",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        _ => "varchar",
    }
}

fn cell(v: &mysql_async::Value, t: ColumnType, binary: bool) -> serde_json::Value {
    use mysql_async::Value as V;
    match v {
        V::NULL => serde_json::Value::Null,
        V::Int(i) => int_cell(*i),
        V::UInt(u) => int_cell(*u as i64),
        V::Float(f) => float_cell(*f as f64),
        V::Double(f) => float_cell(*f),
        V::Bytes(b) => {
            if binary {
                return hex_cell(b);
            }
            let s = String::from_utf8_lossy(b).into_owned();
            match t {
                ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_YEAR => s.parse::<i64>().map(int_cell).unwrap_or(serde_json::Value::String(s)),
                ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
                    s.parse::<f64>().map(float_cell).unwrap_or(serde_json::Value::String(s))
                }
                _ => serde_json::Value::String(s),
            }
        }
        other => serde_json::Value::String(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}

/// Run statements on a locked connection; stops at the first error.
async fn run_statements(c: &mut mysql_async::Conn, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut out = Vec::new();
    for (i, stmt) in split_statements(sql, SqlDialect::Mysql).iter().enumerate() {
        let started = Instant::now();
        let run = async {
            let mut result = c.query_iter(stmt.as_str()).await?;
            let cols = result.columns().map(|c| c.to_vec()).unwrap_or_default();
            let columns: Vec<ResultColumn> = cols
                .iter()
                .map(|c| ResultColumn { name: c.name_str().into_owned(), type_name: type_name(c.column_type()).to_owned() })
                .collect();
            let binary: Vec<bool> =
                cols.iter().map(|c| c.character_set() == 63 && matches!(type_name(c.column_type()), "blob" | "varchar" | "bit")).collect();
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(row) = result.next().await? {
                if rows.len() >= max_rows {
                    truncated = true;
                    continue;
                }
                rows.push(
                    (0..row.len())
                        .map(|i| cell(row.as_ref(i).unwrap_or(&mysql_async::Value::NULL), cols[i].column_type(), binary[i]))
                        .collect(),
                );
            }
            let affected = result.affected_rows();
            drop(result);
            Ok::<_, mysql_async::Error>((columns, rows, truncated, affected))
        };
        match run.await {
            Ok((columns, rows, truncated, affected)) => out.push(QueryResult {
                statement: stmt.clone(),
                affected: if columns.is_empty() { Some(affected) } else { None },
                columns,
                rows,
                truncated,
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                message: None,
            }),
            Err(e) => {
                let mut err = my_err(e);
                err.statement_index = Some(i);
                return Err(err);
            }
        }
    }
    Ok(out)
}

/// `information_schema.COLUMNS.COLUMN_DEFAULT` holds string defaults unquoted; turn it into a
/// SQL expression (as PostgreSQL reports them) so DDL export and schema diffs stay correct.
fn mysql_default_sql(raw: &str, column_type: &str, extra: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    let expression = extra.to_ascii_uppercase().contains("DEFAULT_GENERATED")
        || upper.starts_with("CURRENT_TIMESTAMP")
        || upper == "NULL"
        || (raw.starts_with('(') && raw.ends_with(')'));
    let ty = column_type.to_ascii_lowercase();
    let numeric = ["int", "decimal", "numeric", "float", "double", "real", "bit", "bool"].iter().any(|t| ty.contains(t));
    if expression || (numeric && raw.trim().parse::<f64>().is_ok()) {
        raw.to_owned()
    } else {
        format!("'{}'", raw.replace('\\', "\\\\").replace('\'', "''"))
    }
}

impl MySqlClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<MySqlClient> {
        let mut opts = mysql_async::OptsBuilder::default()
            .ip_or_hostname(if config.host.is_empty() { "127.0.0.1".to_owned() } else { config.host.clone() })
            .tcp_port(if config.port == 0 { 3306 } else { config.port })
            .user(Some(if config.user.is_empty() { "root".to_owned() } else { config.user.clone() }))
            .pass(config.password.clone())
            .db_name(config.database.clone().filter(|d| !d.is_empty()))
            .prefer_socket(false)
            // Report matched rather than changed rows, as PostgreSQL and SQLite do, so row-count
            // checks (staged edits, preview commits) are not fooled by no-op updates.
            .client_found_rows(true);
        if let Some(via) = config.via {
            opts = opts.ip_or_hostname(via.ip().to_string()).tcp_port(via.port());
        }
        if config.tls != TlsMode::Disable {
            let mut ssl = mysql_async::SslOpts::default();
            if config.tls == TlsMode::Require {
                ssl = ssl.with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
            } else if config.via.is_some() {
                // Verify the certificate against the real server name, not the tunnel address.
                ssl = ssl.with_danger_tls_hostname_override(Some(if config.host.is_empty() {
                    "localhost".to_owned()
                } else {
                    config.host.clone()
                }));
            }
            opts = opts.ssl_opts(Some(ssl));
        }
        let conn = mysql_async::Conn::new(opts).await.map_err(my_err)?;
        Ok(MySqlClient { conn: Mutex::new(conn) })
    }

    async fn text_rows(conn: &mut mysql_async::Conn, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let rows: Vec<mysql_async::Row> = conn.query(sql).await.map_err(my_err)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (0..r.len())
                    .map(|i| match r.as_ref(i) {
                        Some(mysql_async::Value::NULL) | None => None,
                        Some(mysql_async::Value::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                        Some(v) => Some(v.as_sql(true).trim_matches('\'').to_owned()),
                    })
                    .collect()
            })
            .collect())
    }
}

#[async_trait]
impl SqlClient for MySqlClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
    }

    async fn server_version(&self) -> Result<String> {
        let mut c = self.conn.lock().await;
        let rows = Self::text_rows(&mut c, "SELECT VERSION()").await?;
        Ok(rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default())
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let mut c = self.conn.lock().await;
        let rows = Self::text_rows(&mut c, "SHOW DATABASES").await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter(|d| !matches!(d.as_str(), "information_schema" | "mysql" | "performance_schema" | "sys"))
            .collect())
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let mut c = self.conn.lock().await;
        let lit = string_literal(SqlDialect::Mysql, schema);
        let rows = Self::text_rows(
            &mut c,
            &format!("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {lit} ORDER BY TABLE_NAME"),
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| TableInfo {
                schema: schema.to_owned(),
                name: r[0].clone().unwrap_or_default(),
                kind: if r[1].as_deref() == Some("VIEW") { "view".into() } else { "table".into() },
            })
            .collect())
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let mut c = self.conn.lock().await;
        let lit = string_literal(SqlDialect::Mysql, schema);
        let tables = Self::text_rows(
            &mut c,
            &format!("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {lit} ORDER BY TABLE_NAME"),
        )
        .await?;
        let columns = Self::text_rows(
            &mut c,
            &format!(
                "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = {lit} ORDER BY TABLE_NAME, ORDINAL_POSITION"
            ),
        )
        .await?;
        let stats = Self::text_rows(
            &mut c,
            &format!(
                "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {lit} \
                 ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX"
            ),
        )
        .await?;
        let fks = Self::text_rows(
            &mut c,
            &format!(
                "SELECT k.TABLE_NAME, k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
                 r.DELETE_RULE FROM information_schema.KEY_COLUMN_USAGE k LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
                 ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
                 WHERE k.TABLE_SCHEMA = {lit} AND k.REFERENCED_TABLE_NAME IS NOT NULL ORDER BY k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ),
        )
        .await?;
        let g = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().unwrap_or_default();
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
            let key = g(r, 5);
            out[i].columns.push(ColumnInfo {
                name: g(r, 1),
                data_type: g(r, 2),
                nullable: g(r, 3) == "YES",
                default: r.get(4).cloned().flatten().map(|d| mysql_default_sql(&d, &g(r, 2), &g(r, 6))),
                primary_key: key == "PRI",
                unique: key == "UNI",
                auto: g(r, 6).contains("auto_increment"),
            });
        }
        for r in &stats {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let name = g(r, 1);
            let col = g(r, 3);
            if name == "PRIMARY" {
                out[i].primary_key.push(col.clone());
            }
            match out[i].indexes.iter_mut().find(|x| x.name == name) {
                Some(ix) => ix.columns.push(col),
                None => out[i].indexes.push(IndexInfo { primary: name == "PRIMARY", unique: g(r, 2) == "0", name, columns: vec![col] }),
            }
        }
        for r in &fks {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let name = g(r, 1);
            match out[i].foreign_keys.iter_mut().find(|f| f.name == name) {
                Some(f) => {
                    f.columns.push(g(r, 2));
                    f.ref_columns.push(g(r, 5));
                }
                None => out[i].foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![g(r, 2)],
                    ref_schema: g(r, 3),
                    ref_table: g(r, 4),
                    ref_columns: vec![g(r, 5)],
                    on_delete: r.get(6).cloned().flatten().filter(|a| a != "NO ACTION" && a != "RESTRICT"),
                }),
            }
        }
        Ok(out)
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        run_statements(&mut c, sql, max_rows).await
    }

    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        c.query_drop("START TRANSACTION").await.map_err(my_err)?;
        let result = run_statements(&mut c, sql, max_rows).await;
        c.query_drop("ROLLBACK").await.map_err(my_err)?;
        result
    }

    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        c.query_drop("START TRANSACTION READ ONLY").await.map_err(my_err)?;
        let result = run_statements(&mut c, sql, max_rows).await;
        c.query_drop("ROLLBACK").await.map_err(my_err)?;
        result
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let mut c = self.conn.lock().await;
        c.query_drop("START TRANSACTION").await.map_err(my_err)?;
        let mut total = 0;
        for (i, s) in statements.iter().enumerate() {
            match c.query_drop(s.as_str()).await {
                Ok(()) => total += c.affected_rows(),
                Err(e) => {
                    let _ = c.query_drop("ROLLBACK").await;
                    let mut err = my_err(e);
                    err.statement_index = Some(i);
                    return Err(err);
                }
            }
        }
        if let Some(e) = expected.filter(|e| *e != total) {
            let _ = c.query_drop("ROLLBACK").await;
            return Err(ClientError::new(format!(
                "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
            ))
            .with_code("TB001"));
        }
        c.query_drop("COMMIT").await.map_err(my_err)?;
        Ok(total)
    }

    async fn close(&self) {
        let mut c = self.conn.lock().await;
        let _ = c.reset().await;
    }
}
