//! Oracle Database driver (the `oracle` crate on ODPI-C). It loads Oracle Instant Client at run time:
//! the folder set in Settings, `TWINBASE_ORACLE_CLIENT`, or one of the usual install locations.
//!
//! Scripts follow SQL*Plus conventions: `;` ends SQL statements, and PL/SQL blocks
//! (`BEGIN … END;`, `CREATE OR REPLACE PROCEDURE …`) end with a `/` on its own line. Statements
//! autocommit, as in other clients; staged edits share one transaction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use oracle::sql_type::OracleType;
use parking_lot::Mutex;
use serde_json::Value;

use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, TlsMode, float_cell, hex_cell, int_cell,
};

static CLIENT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// The Instant Client folder chosen in Settings (empty to detect one).
pub fn set_client_dir(dir: Option<PathBuf>) {
    *CLIENT_DIR.lock() = dir.filter(|d| !d.as_os_str().is_empty());
}

/// Instant Client folders to try: the setting, `TWINBASE_ORACLE_CLIENT`, then common locations.
fn candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = CLIENT_DIR.lock().clone().into_iter().collect();
    if let Some(d) = std::env::var_os("TWINBASE_ORACLE_CLIENT") {
        out.push(d.into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for base in
        [home.join("Downloads"), home.clone(), PathBuf::from("/opt/oracle"), PathBuf::from("/usr/local/lib"), PathBuf::from("C:\\oracle")]
    {
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("instantclient")))
                .collect();
            found.sort();
            out.extend(found.into_iter().rev());
        }
    }
    if let Ok(entries) = std::fs::read_dir("/usr/lib/oracle") {
        for e in entries.flatten() {
            out.push(e.path().join("client64").join("lib"));
        }
    }
    out
}

fn has_client_library(dir: &std::path::Path) -> bool {
    ["libclntsh.dylib", "libclntsh.so", "oci.dll"].iter().any(|f| dir.join(f).exists())
}

/// Load Instant Client once per process.
fn init() -> Result<()> {
    INIT.get_or_init(|| {
        let mut params = oracle::InitParams::new();
        if let Some(dir) = candidates().into_iter().find(|d| has_client_library(d)) {
            params.oracle_client_lib_dir(dir).map_err(|e| e.to_string())?;
        }
        params.init().map(|_| ()).map_err(|e| e.to_string())
    })
    .clone()
    .map_err(|e| {
        ClientError::new(format!(
            "Oracle Instant Client could not be loaded ({e}). Install it (Oracle Instant Client Basic or Basic Light for your platform) and choose its folder in Settings › Data, or set TWINBASE_ORACLE_CLIENT."
        ))
    })
}

fn ora_err(e: oracle::Error) -> ClientError {
    match e.db_error() {
        Some(db) => {
            let mut err = ClientError::new(db.message().trim().to_owned()).with_code(format!("ORA-{:05}", db.code()));
            if db.offset() > 0 {
                err.detail = Some(format!("at character {}", db.offset() + 1));
            }
            err
        }
        None => ClientError::new(e.to_string()),
    }
}

/// Split a script: `;` ends SQL statements (dropped, Oracle rejects it), PL/SQL units run to a `/`
/// line (or the end) and keep their semicolons.
pub fn split_script(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut plsql = String::new();
    let mut in_plsql = false;
    let mut buf = String::new();
    for line in sql.lines() {
        let t = line.trim();
        if in_plsql {
            if t == "/" {
                out.push(plsql.trim().to_owned());
                plsql.clear();
                in_plsql = false;
            } else {
                plsql.push_str(line);
                plsql.push('\n');
            }
            continue;
        }
        if buf.trim().is_empty() && starts_plsql(t) {
            in_plsql = true;
            plsql.push_str(line);
            plsql.push('\n');
            continue;
        }
        if t == "/" {
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        // Plain SQL: split at semicolons outside quotes and comments.
        let parts = twinbase_engine::translate::split_statements(&buf, twinbase_engine::Dialect::Postgres);
        if buf.trim_end().ends_with(';') {
            out.extend(parts.into_iter().map(|p| p.trim().trim_end_matches(';').trim().to_owned()).filter(|p| !p.is_empty()));
            buf.clear();
        }
    }
    if in_plsql && !plsql.trim().is_empty() {
        out.push(plsql.trim().to_owned());
    }
    out.extend(
        twinbase_engine::translate::split_statements(&buf, twinbase_engine::Dialect::Postgres)
            .into_iter()
            .map(|p| p.trim().trim_end_matches(';').trim().to_owned())
            .filter(|p| !p.is_empty()),
    );
    out
}

fn starts_plsql(line: &str) -> bool {
    let u = line.to_ascii_uppercase();
    let w: Vec<&str> = u.split_whitespace().collect();
    match w.as_slice() {
        ["BEGIN", ..] | ["DECLARE", ..] => true,
        ["CREATE", rest @ ..] => {
            let rest: Vec<&str> =
                rest.iter().copied().filter(|x| !matches!(*x, "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE")).collect();
            matches!(rest.first(), Some(&"PROCEDURE" | &"FUNCTION" | &"PACKAGE" | &"TRIGGER" | &"TYPE"))
        }
        _ => false,
    }
}

/// A cell by Oracle column type.
fn cell(row: &oracle::Row, i: usize, t: &OracleType) -> Value {
    let s = |r: oracle::Result<Option<String>>| r.ok().flatten().map(Value::String).unwrap_or(Value::Null);
    match t {
        OracleType::Number(p, 0) if *p > 0 && *p <= 18 => {
            row.get::<usize, Option<i64>>(i).ok().flatten().map(int_cell).unwrap_or(Value::Null)
        }
        OracleType::Number(_, _) | OracleType::Float(_) => match row.get::<usize, Option<String>>(i).ok().flatten() {
            Some(n) => n.parse::<i64>().ok().filter(|v| v.unsigned_abs() <= 1 << 53).map(Value::from).unwrap_or(Value::String(n)),
            None => Value::Null,
        },
        OracleType::BinaryFloat | OracleType::BinaryDouble => {
            row.get::<usize, Option<f64>>(i).ok().flatten().map(float_cell).unwrap_or(Value::Null)
        }
        OracleType::Date | OracleType::Timestamp(_) => row
            .get::<usize, Option<chrono::NaiveDateTime>>(i)
            .ok()
            .flatten()
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
            .unwrap_or(Value::Null),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => row
            .get::<usize, Option<chrono::DateTime<chrono::FixedOffset>>>(i)
            .ok()
            .flatten()
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string()))
            .unwrap_or(Value::Null),
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => {
            row.get::<usize, Option<Vec<u8>>>(i).ok().flatten().map(|b| hex_cell(&b)).unwrap_or(Value::Null)
        }
        OracleType::Boolean => row.get::<usize, Option<bool>>(i).ok().flatten().map(Value::Bool).unwrap_or(Value::Null),
        _ => s(row.get::<usize, Option<String>>(i)),
    }
}

pub struct OracleClient {
    conn: Arc<Mutex<oracle::Connection>>,
    user: String,
}

/// Easy Connect string: `//host:port/service`, `tcps://…` with TLS.
pub fn connect_string(config: &ConnectionConfig) -> String {
    let (host, port) = match config.via {
        Some(v) => (v.ip().to_string(), v.port()),
        None => (
            if config.host.is_empty() { "127.0.0.1".into() } else { config.host.clone() },
            if config.port == 0 { 1521 } else { config.port },
        ),
    };
    let service = config.database.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| "FREEPDB1".into());
    match config.tls {
        TlsMode::Disable => format!("//{host}:{port}/{service}"),
        TlsMode::Require => format!("tcps://{host}:{port}/{service}?ssl_server_dn_match=off"),
        TlsMode::VerifyFull => format!("tcps://{host}:{port}/{service}"),
    }
}

impl OracleClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<OracleClient> {
        init()?;
        let (user, password, cs) = (config.user.clone(), config.password.clone().unwrap_or_default(), connect_string(config));
        let conn = tokio::task::spawn_blocking(move || -> Result<oracle::Connection> {
            let mut c = oracle::Connection::connect(&user, &password, &cs).map_err(ora_err)?;
            c.set_autocommit(true);
            Ok(c)
        })
        .await
        .map_err(|e| ClientError::new(e.to_string()))??;
        Ok(OracleClient { conn: Arc::new(Mutex::new(conn)), user: config.user.to_ascii_uppercase() })
    }

    /// Run `f` with the connection on a blocking thread.
    async fn with<T: Send + 'static>(&self, f: impl FnOnce(&mut oracle::Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.lock())).await.map_err(|e| ClientError::new(e.to_string()))?
    }

    async fn rows(&self, sql: &str, params: Vec<String>) -> Result<Vec<Vec<Value>>> {
        let sql = sql.to_owned();
        self.with(move |c| {
            let p: Vec<&dyn oracle::sql_type::ToSql> = params.iter().map(|s| s as &dyn oracle::sql_type::ToSql).collect();
            let rs = c.query(&sql, &p).map_err(ora_err)?;
            let types: Vec<OracleType> = rs.column_info().iter().map(|c| c.oracle_type().clone()).collect();
            let mut out = Vec::new();
            for row in rs {
                let row = row.map_err(ora_err)?;
                out.push(types.iter().enumerate().map(|(i, t)| cell(&row, i, t)).collect());
            }
            Ok(out)
        })
        .await
    }
}

/// Run statements on the locked connection.
fn run(c: &mut oracle::Connection, statements: &[String], max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut out = Vec::new();
    for (i, s) in statements.iter().enumerate() {
        let started = Instant::now();
        let res: Result<QueryResult> = (|| {
            let mut stmt = c.statement(s).build().map_err(ora_err)?;
            if stmt.is_query() {
                let rs = stmt.query(&[]).map_err(ora_err)?;
                let info = rs.column_info().to_vec();
                let mut rows = Vec::new();
                let mut truncated = false;
                for row in rs {
                    let row = row.map_err(ora_err)?;
                    if rows.len() >= max_rows {
                        truncated = true;
                        break;
                    }
                    rows.push(info.iter().enumerate().map(|(i, ci)| cell(&row, i, ci.oracle_type())).collect());
                }
                Ok(QueryResult {
                    statement: s.clone(),
                    columns: info
                        .iter()
                        .map(|ci| ResultColumn { name: ci.name().to_owned(), type_name: ci.oracle_type().to_string() })
                        .collect(),
                    rows,
                    truncated,
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    ..Default::default()
                })
            } else {
                let dml = stmt.is_dml();
                stmt.execute(&[]).map_err(ora_err)?;
                let affected = if dml { stmt.row_count().ok() } else { None };
                Ok(QueryResult {
                    statement: s.clone(),
                    affected,
                    message: affected.is_none().then(|| "Done.".to_owned()),
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    ..Default::default()
                })
            }
        })();
        match res {
            Ok(r) => out.push(r),
            Err(mut e) => {
                e.statement_index = Some(i);
                return Err(e);
            }
        }
    }
    Ok(out)
}

/// Declared type text from the data dictionary.
fn type_text(r: &[Value]) -> String {
    let s =
        |i: usize| r.get(i).and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default();
    let n = |i: usize| r.get(i).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let ty = s(2);
    match ty.as_str() {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" => {
            format!("{ty}({}{})", n(4).unwrap_or(n(3).unwrap_or(1)), if s(10) == "C" && !ty.starts_with('N') { " CHAR" } else { "" })
        }
        "RAW" => format!("RAW({})", n(3).unwrap_or(1)),
        "NUMBER" => match (n(5), n(6)) {
            (Some(p), Some(0)) => format!("NUMBER({p})"),
            (Some(p), Some(sc)) => format!("NUMBER({p},{sc})"),
            (None, Some(0)) => "INTEGER".into(),
            _ => "NUMBER".into(),
        },
        _ => ty,
    }
}

#[async_trait]
impl SqlClient for OracleClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Oracle
    }

    async fn server_version(&self) -> Result<String> {
        self.with(|c| {
            let (v, banner) = c.server_version().map_err(ora_err)?;
            Ok(if banner.is_empty() { format!("Oracle Database {v}") } else { banner.lines().next().unwrap_or("").to_owned() })
        })
        .await
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let rows = self.rows("SELECT username FROM all_users WHERE oracle_maintained = 'N' ORDER BY username", vec![]).await?;
        let mut out: Vec<String> = rows.into_iter().filter_map(|r| r.first().and_then(|v| v.as_str()).map(str::to_owned)).collect();
        if let Some(p) = out.iter().position(|x| *x == self.user) {
            let me = out.remove(p);
            out.insert(0, me);
        }
        Ok(out)
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let rows = self
            .rows("SELECT table_name, 'table' FROM all_tables WHERE owner = :1 AND nested = 'NO' UNION ALL SELECT view_name, 'view' FROM all_views WHERE owner = :1 ORDER BY 1", vec![schema.to_owned()])
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| TableInfo {
                schema: schema.to_owned(),
                name: r[0].as_str().unwrap_or("").to_owned(),
                kind: r[1].as_str().unwrap_or("table").to_owned(),
            })
            .collect())
    }

    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        let tables = self.tables(schema).await?;
        let columns = self
            .rows(
                "SELECT table_name, column_name, data_type, data_length, char_length, data_precision, data_scale, nullable, data_default, identity_column, char_used \
                 FROM all_tab_columns WHERE owner = :1 ORDER BY table_name, column_id",
                vec![schema.to_owned()],
            )
            .await?;
        let constraints = self
            .rows(
                "SELECT c.table_name, c.constraint_name, c.constraint_type, cc.column_name, r.table_name, rc.column_name, c.delete_rule, r.owner \
                 FROM all_constraints c JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 LEFT JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
                 LEFT JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
                 WHERE c.owner = :1 AND c.constraint_type IN ('P', 'U', 'R') ORDER BY c.table_name, c.constraint_name, cc.position",
                vec![schema.to_owned()],
            )
            .await?;
        let indexes = self
            .rows(
                "SELECT i.table_name, i.index_name, i.uniqueness, ic.column_name FROM all_indexes i JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
                 WHERE i.owner = :1 AND i.index_type NOT LIKE 'LOB%' ORDER BY i.table_name, i.index_name, ic.column_position",
                vec![schema.to_owned()],
            )
            .await?;
        let g = |r: &Vec<Value>, i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned();
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
        for r in &columns {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            out[i].columns.push(ColumnInfo {
                name: g(r, 1),
                data_type: type_text(r),
                nullable: g(r, 7) == "Y",
                default: r
                    .get(8)
                    .and_then(|v| v.as_str())
                    .map(|d| d.trim().to_owned())
                    .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("null")),
                primary_key: false,
                unique: false,
                auto: g(r, 9) == "YES",
            });
        }
        for r in &constraints {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let (name, kind, col) = (g(r, 1), g(r, 2), g(r, 3));
            match kind.as_str() {
                "P" => out[i].primary_key.push(col),
                "U" => {
                    if let Some(c) = out[i].columns.iter_mut().find(|c| c.name == col) {
                        c.unique = true;
                    }
                }
                _ => match out[i].foreign_keys.iter_mut().find(|f| f.name == name) {
                    Some(f) => {
                        f.columns.push(col);
                        f.ref_columns.push(g(r, 5));
                    }
                    None => out[i].foreign_keys.push(ForeignKeyInfo {
                        name,
                        columns: vec![col],
                        ref_schema: g(r, 7),
                        ref_table: g(r, 4),
                        ref_columns: vec![g(r, 5)],
                        on_delete: Some(g(r, 6)).filter(|a| a != "NO ACTION" && !a.is_empty()),
                    }),
                },
            }
        }
        for r in &indexes {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let name = g(r, 1);
            match out[i].indexes.iter_mut().find(|x| x.name == name) {
                Some(ix) => ix.columns.push(g(r, 3)),
                None => out[i].indexes.push(IndexInfo { unique: g(r, 2) == "UNIQUE", primary: false, name, columns: vec![g(r, 3)] }),
            }
        }
        for t in &mut out {
            let pk = t.primary_key.clone();
            for ix in &mut t.indexes {
                ix.primary = ix.unique && ix.columns == pk;
            }
            for c in &mut t.columns {
                c.primary_key = pk.contains(&c.name);
                if c.primary_key {
                    c.unique = false;
                }
            }
        }
        Ok(out)
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let statements = split_script(sql);
        self.with(move |c| run(c, &statements, max_rows)).await
    }

    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let statements = split_script(sql);
        self.with(move |c| {
            c.set_autocommit(false);
            let r = run(c, &statements, max_rows);
            let _ = c.rollback();
            c.set_autocommit(true);
            r
        })
        .await
    }

    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let statements = split_script(sql);
        self.with(move |c| {
            c.set_autocommit(false);
            let r = c.execute("SET TRANSACTION READ ONLY", &[]).map_err(ora_err).and_then(|_| run(c, &statements, max_rows));
            let _ = c.rollback();
            c.set_autocommit(true);
            r
        })
        .await
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let statements = statements.to_vec();
        self.with(move |c| {
            c.set_autocommit(false);
            let outcome = (|| -> Result<u64> {
                let mut total = 0;
                for (i, s) in statements.iter().enumerate() {
                    let stmt = c.execute(s, &[]).map_err(|e| {
                        let mut err = ora_err(e);
                        err.statement_index = Some(i);
                        err
                    })?;
                    total += stmt.row_count().unwrap_or(0);
                }
                if let Some(e) = expected.filter(|e| *e != total) {
                    return Err(ClientError::new(format!(
                        "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
                    ))
                    .with_code("TB001"));
                }
                Ok(total)
            })();
            let r = match outcome {
                Ok(n) => c.commit().map(|_| n).map_err(ora_err),
                Err(e) => {
                    let _ = c.rollback();
                    Err(e)
                }
            };
            c.set_autocommit(true);
            r
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_split_like_sqlplus() {
        let s = "CREATE TABLE t (a NUMBER);\nINSERT INTO t VALUES (1); INSERT INTO t VALUES ('x;y');\nCREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  UPDATE t SET a = 2;\nEND;\n/\nBEGIN\n  p;\nEND;\n/\nSELECT * FROM t";
        let parts = split_script(s);
        assert_eq!(parts.len(), 6, "{parts:?}");
        assert_eq!(parts[0], "CREATE TABLE t (a NUMBER)");
        assert_eq!(parts[2], "INSERT INTO t VALUES ('x;y')");
        assert!(parts[3].starts_with("CREATE OR REPLACE PROCEDURE p AS") && parts[3].ends_with("END;"));
        assert_eq!(parts[4], "BEGIN\n  p;\nEND;");
        assert_eq!(parts[5], "SELECT * FROM t");
    }

    #[test]
    fn connect_strings_and_types() {
        let mut c = ConnectionConfig {
            kind: crate::DbKind::Oracle,
            host: "db".into(),
            port: 0,
            user: "app".into(),
            password: None,
            database: Some("ORCLPDB1".into()),
            path: None,
            tls: TlsMode::Disable,
            timeout_secs: 5,
            ssh: None,
            via: None,
        };
        assert_eq!(connect_string(&c), "//db:1521/ORCLPDB1");
        c.tls = TlsMode::Require;
        assert_eq!(connect_string(&c), "tcps://db:1521/ORCLPDB1?ssl_server_dn_match=off");
        let row = |ty: &str, len: i64, chars: i64, p: Option<i64>, s: Option<i64>| {
            vec![
                Value::Null,
                Value::Null,
                Value::from(ty),
                Value::from(len),
                Value::from(chars),
                p.map(Value::from).unwrap_or(Value::Null),
                s.map(Value::from).unwrap_or(Value::Null),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::from("C"),
            ]
        };
        assert_eq!(type_text(&row("VARCHAR2", 400, 100, None, None)), "VARCHAR2(100 CHAR)");
        assert_eq!(type_text(&row("NUMBER", 22, 0, Some(10), Some(2))), "NUMBER(10,2)");
        assert_eq!(type_text(&row("NUMBER", 22, 0, None, Some(0))), "INTEGER");
        assert_eq!(type_text(&row("DATE", 7, 0, None, None)), "DATE");
    }
}
