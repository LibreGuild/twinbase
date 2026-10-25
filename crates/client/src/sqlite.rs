//! SQLite files (rusqlite on a blocking thread).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::types::ValueRef;

use crate::sqlgen::{quote_ident, string_literal};
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, float_cell, hex_cell, int_cell, split_statements,
};

pub struct SqliteClient {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

fn lite_err(e: rusqlite::Error) -> ClientError {
    ClientError::new(e.to_string())
}

impl SqliteClient {
    pub fn open(config: &ConnectionConfig) -> Result<SqliteClient> {
        let path =
            config.path.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| ClientError::new("SQLite connections need a file path"))?;
        let conn = rusqlite::Connection::open(path).map_err(lite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(lite_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON").map_err(lite_err)?;
        Ok(SqliteClient { conn: Arc::new(Mutex::new(conn)) })
    }

    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&conn.lock())).await.map_err(|e| ClientError::new(e.to_string()))?
    }
}

fn text_rows(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut stmt = conn.prepare(sql).map_err(lite_err)?;
    let n = stmt.column_count();
    let rows = stmt
        .query_map([], |r| {
            Ok((0..n)
                .map(|i| match r.get_ref(i) {
                    Ok(ValueRef::Null) | Err(_) => None,
                    Ok(ValueRef::Integer(v)) => Some(v.to_string()),
                    Ok(ValueRef::Real(v)) => Some(v.to_string()),
                    Ok(ValueRef::Text(t)) => Some(String::from_utf8_lossy(t).into_owned()),
                    Ok(ValueRef::Blob(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                })
                .collect())
        })
        .map_err(lite_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(lite_err)
}

fn run_statement(conn: &rusqlite::Connection, sql: &str, max_rows: usize) -> Result<QueryResult> {
    let started = Instant::now();
    let mut stmt = conn.prepare(sql).map_err(lite_err)?;
    let columns: Vec<ResultColumn> = stmt
        .columns()
        .map(|c| ResultColumn { name: c.name().to_owned(), type_name: c.decl_type().unwrap_or("").to_ascii_lowercase() })
        .collect();
    let n = columns.len();
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut affected = None;
    if n > 0 {
        let mut q = stmt.raw_query();
        while let Some(r) = q.next().map_err(lite_err)? {
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            rows.push(
                (0..n)
                    .map(|i| match r.get_ref(i) {
                        Ok(ValueRef::Null) | Err(_) => serde_json::Value::Null,
                        Ok(ValueRef::Integer(v)) => int_cell(v),
                        Ok(ValueRef::Real(v)) => float_cell(v),
                        Ok(ValueRef::Text(t)) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
                        Ok(ValueRef::Blob(b)) => hex_cell(b),
                    })
                    .collect(),
            );
        }
    } else {
        affected = Some(stmt.raw_execute().map_err(lite_err)? as u64);
    }
    Ok(QueryResult {
        statement: sql.to_owned(),
        columns,
        rows,
        affected,
        truncated,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        message: None,
    })
}

#[async_trait]
impl SqlClient for SqliteClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        self.blocking(|c| {
            Ok(text_rows(c, "SELECT name FROM pragma_database_list WHERE name <> 'temp'")?
                .into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .collect())
        })
        .await
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let schema = schema.to_owned();
        self.blocking(move |c| {
            let sql = format!(
                "SELECT name, type FROM {}.sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
                quote_ident(SqlDialect::Sqlite, &schema)
            );
            Ok(text_rows(c, &sql)?
                .into_iter()
                .map(|r| TableInfo {
                    schema: schema.clone(),
                    name: r[0].clone().unwrap_or_default(),
                    kind: r[1].clone().unwrap_or_default(),
                })
                .collect())
        })
        .await
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let tables = self.tables(schema).await?;
        let schema = schema.to_owned();
        self.blocking(move |c| {
            let mut out = Vec::new();
            for t in tables {
                let lit = string_literal(SqlDialect::Sqlite, &t.name);
                let sch = string_literal(SqlDialect::Sqlite, &schema);
                let cols = text_rows(c, &format!("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info({lit}, {sch})"))?;
                let mut pk: Vec<(i64, String)> = Vec::new();
                let mut columns = Vec::new();
                for r in &cols {
                    let name = r[0].clone().unwrap_or_default();
                    let pk_pos: i64 = r[4].as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
                    if pk_pos > 0 {
                        pk.push((pk_pos, name.clone()));
                    }
                    let ty = r[1].clone().unwrap_or_default();
                    columns.push(ColumnInfo {
                        name,
                        auto: pk_pos > 0 && ty.eq_ignore_ascii_case("INTEGER"),
                        data_type: ty.to_ascii_lowercase(),
                        nullable: r[2].as_deref() != Some("1") && pk_pos == 0,
                        default: r[3].clone(),
                        primary_key: pk_pos > 0,
                        unique: false,
                    });
                }
                pk.sort();
                let primary_key: Vec<String> = pk.into_iter().map(|(_, n)| n).collect();
                let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
                for r in text_rows(
                    c,
                    &format!(
                        "SELECT id, \"table\", \"from\", \"to\", on_delete FROM pragma_foreign_key_list({lit}, {sch}) ORDER BY id, seq"
                    ),
                )? {
                    let id = format!("fk_{}_{}", t.name, r[0].clone().unwrap_or_default());
                    let ref_table = r[1].clone().unwrap_or_default();
                    let from = r[2].clone().unwrap_or_default();
                    let to = r[3].clone().unwrap_or_default();
                    match foreign_keys.iter_mut().find(|f| f.name == id) {
                        Some(f) => {
                            f.columns.push(from);
                            f.ref_columns.push(to);
                        }
                        None => foreign_keys.push(ForeignKeyInfo {
                            name: id,
                            columns: vec![from],
                            ref_schema: schema.clone(),
                            ref_table,
                            ref_columns: vec![to],
                            on_delete: r[4].clone().filter(|a| a != "NO ACTION"),
                        }),
                    }
                }
                // Implicit references to the parent's primary key.
                for f in &mut foreign_keys {
                    if f.ref_columns.iter().any(|c| c.is_empty()) {
                        let parent = string_literal(SqlDialect::Sqlite, &f.ref_table);
                        let pks = text_rows(c, &format!("SELECT name FROM pragma_table_info({parent}, {sch}) WHERE pk > 0 ORDER BY pk"))?;
                        f.ref_columns = pks.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
                    }
                }
                let mut indexes = Vec::new();
                for r in text_rows(c, &format!("SELECT name, \"unique\", origin FROM pragma_index_list({lit}, {sch})"))? {
                    let name = r[0].clone().unwrap_or_default();
                    let cols = text_rows(
                        c,
                        &format!("SELECT name FROM pragma_index_info({}, {sch}) ORDER BY seqno", string_literal(SqlDialect::Sqlite, &name)),
                    )?
                    .into_iter()
                    .filter_map(|x| x.into_iter().next().flatten())
                    .collect::<Vec<_>>();
                    let unique = r[1].as_deref() == Some("1");
                    if unique
                        && cols.len() == 1
                        && let Some(col) = columns.iter_mut().find(|cc| cc.name == cols[0])
                    {
                        col.unique = true;
                    }
                    indexes.push(IndexInfo { name, columns: cols, unique, primary: r[2].as_deref() == Some("pk") });
                }
                out.push(TableSchema { schema: schema.clone(), name: t.name, kind: t.kind, columns, primary_key, foreign_keys, indexes });
            }
            Ok(out)
        })
        .await
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let statements = split_statements(sql, SqlDialect::Sqlite);
        self.blocking(move |c| {
            let mut out = Vec::new();
            for (i, s) in statements.iter().enumerate() {
                match run_statement(c, s, max_rows) {
                    Ok(r) => out.push(r),
                    Err(mut e) => {
                        e.statement_index = Some(i);
                        return Err(e);
                    }
                }
            }
            Ok(out)
        })
        .await
    }

    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let statements = split_statements(sql, SqlDialect::Sqlite);
        self.blocking(move |c| {
            // query_only makes SQLite refuse every change to the database file.
            c.pragma_update(None, "query_only", true).map_err(|e| ClientError::new(e.to_string()))?;
            let mut out = Vec::new();
            let mut failure = None;
            for (i, s) in statements.iter().enumerate() {
                match run_statement(c, s, max_rows) {
                    Ok(r) => out.push(r),
                    Err(mut e) => {
                        e.statement_index = Some(i);
                        failure = Some(e);
                        break;
                    }
                }
            }
            c.pragma_update(None, "query_only", false).map_err(|e| ClientError::new(e.to_string()))?;
            match failure {
                Some(e) => Err(e),
                None => Ok(out),
            }
        })
        .await
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let statements = statements.to_vec();
        self.blocking(move |c| {
            c.execute_batch("BEGIN IMMEDIATE").map_err(lite_err)?;
            let mut total = 0u64;
            for (i, s) in statements.iter().enumerate() {
                match c.execute(s, []) {
                    Ok(n) => total += n as u64,
                    Err(e) => {
                        let _ = c.execute_batch("ROLLBACK");
                        let mut err = lite_err(e);
                        err.statement_index = Some(i);
                        return Err(err);
                    }
                }
            }
            if let Some(e) = expected.filter(|e| *e != total) {
                let _ = c.execute_batch("ROLLBACK");
                return Err(ClientError::new(format!(
                    "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
                ))
                .with_code("TB001"));
            }
            c.execute_batch("COMMIT").map_err(lite_err)?;
            Ok(total)
        })
        .await
    }
}
