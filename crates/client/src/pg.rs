//! PostgreSQL driver (tokio-postgres). Queries use the simple protocol so every type comes
//! back in PostgreSQL's own text form; column types come from a separate Describe.

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio_postgres::{NoTls, SimpleQueryMessage};

use crate::sqlgen::string_literal;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, TlsMode, float_cell, int_cell, split_statements,
};

pub struct PgClient {
    client: tokio_postgres::Client,
    /// Serializes statements so transactions are never interleaved with other requests.
    lock: Mutex<()>,
}

fn pg_err(e: tokio_postgres::Error) -> ClientError {
    match e.as_db_error() {
        Some(db) => {
            let mut err = ClientError::new(db.message().to_owned()).with_code(db.code().code());
            err.detail = db.detail().map(str::to_owned);
            err
        }
        None => ClientError::new(e.to_string()),
    }
}

/// Convert PostgreSQL text output into a JSON cell based on the column type.
fn cell(text: Option<&str>, type_name: &str) -> serde_json::Value {
    let Some(t) = text else { return serde_json::Value::Null };
    match type_name {
        "int2" | "int4" | "int8" | "oid" => t.parse::<i64>().map(int_cell).unwrap_or_else(|_| serde_json::Value::String(t.into())),
        "float4" | "float8" => t.parse::<f64>().map(float_cell).unwrap_or_else(|_| serde_json::Value::String(t.into())),
        "bool" => serde_json::Value::Bool(t == "t" || t == "true"),
        _ => serde_json::Value::String(t.to_owned()),
    }
}

fn returns_rows(stmt: &str) -> bool {
    let first = stmt.trim_start().split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("").to_ascii_uppercase();
    matches!(first.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN") || stmt.to_ascii_uppercase().contains("RETURNING")
}

/// Friendly type name from information_schema fields.
fn display_type(data_type: &str, udt: &str, len: Option<&str>, prec: Option<&str>, scale: Option<&str>) -> String {
    let base = match udt {
        "int2" => "smallint".to_owned(),
        "int4" => "integer".to_owned(),
        "int8" => "bigint".to_owned(),
        "float4" => "real".to_owned(),
        "float8" => "double precision".to_owned(),
        "bool" => "boolean".to_owned(),
        "bpchar" => "char".to_owned(),
        _ if data_type == "ARRAY" => format!("{}[]", udt.trim_start_matches('_')),
        other => other.to_owned(),
    };
    match (udt, len, prec, scale) {
        ("varchar" | "bpchar", Some(l), _, _) => format!("{base}({l})"),
        ("numeric", _, Some(p), Some(s)) => format!("numeric({p},{s})"),
        _ => base,
    }
}

impl PgClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<PgClient> {
        let mut pg = tokio_postgres::Config::new();
        // Through a tunnel: connect to the local port, keep `host` for TLS verification.
        let port = config.via.map(|v| v.port()).unwrap_or(if config.port == 0 { 5432 } else { config.port });
        pg.host(if config.host.is_empty() { "127.0.0.1" } else { &config.host })
            .port(port)
            .user(if config.user.is_empty() { "postgres" } else { &config.user })
            .dbname(config.database.as_deref().filter(|d| !d.is_empty()).unwrap_or("postgres"))
            .application_name("TwinBase")
            .connect_timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)));
        if let Some(p) = &config.password {
            pg.password(p);
        }
        if let Some(via) = config.via {
            pg.hostaddr(via.ip());
        }
        let client = match config.tls {
            TlsMode::Disable => {
                let (client, conn) = pg.connect(NoTls).await.map_err(pg_err)?;
                tokio::spawn(conn);
                client
            }
            mode => {
                pg.ssl_mode(tokio_postgres::config::SslMode::Require);
                let tls = tokio_postgres_rustls::MakeRustlsConnect::new(crate::tls::client_config(mode));
                let (client, conn) = pg.connect(tls).await.map_err(pg_err)?;
                tokio::spawn(conn);
                client
            }
        };
        Ok(PgClient { client, lock: Mutex::new(()) })
    }

    /// Run a simple query and return text rows.
    async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let msgs = self.client.simple_query(sql).await.map_err(pg_err)?;
        Ok(msgs
            .into_iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect()),
                _ => None,
            })
            .collect())
    }

    async fn run_statement(&self, stmt: &str, max_rows: usize) -> Result<QueryResult> {
        let started = Instant::now();
        let types: Option<Vec<String>> = if returns_rows(stmt) {
            self.client.prepare(stmt).await.ok().map(|s| s.columns().iter().map(|c| c.type_().name().to_owned()).collect())
        } else {
            None
        };
        let stream = self.client.simple_query_raw(stmt).await.map_err(pg_err)?;
        futures::pin_mut!(stream);
        let mut columns: Vec<ResultColumn> = Vec::new();
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut affected = None;
        while let Some(msg) = stream.next().await {
            match msg.map_err(pg_err)? {
                SimpleQueryMessage::RowDescription(cols) => {
                    columns = cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ResultColumn {
                            name: c.name().to_owned(),
                            type_name: types.as_ref().and_then(|t| t.get(i).cloned()).unwrap_or_else(|| "text".into()),
                        })
                        .collect();
                }
                SimpleQueryMessage::Row(r) => {
                    if rows.len() >= max_rows {
                        truncated = true;
                        break;
                    }
                    rows.push(
                        (0..r.len()).map(|i| cell(r.get(i), columns.get(i).map(|c| c.type_name.as_str()).unwrap_or("text"))).collect(),
                    );
                }
                SimpleQueryMessage::CommandComplete(n) if columns.is_empty() => {
                    affected = Some(n);
                }
                _ => {}
            }
        }
        Ok(QueryResult {
            statement: stmt.to_owned(),
            columns,
            rows,
            affected,
            truncated,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            message: None,
        })
    }
}

#[async_trait]
impl SqlClient for PgClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    async fn server_version(&self) -> Result<String> {
        let _g = self.lock.lock().await;
        let rows = self.text_rows("SELECT version()").await?;
        Ok(rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default())
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let _g = self.lock.lock().await;
        let rows = self
            .text_rows(
                "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('pg_catalog', 'information_schema') \
                 AND schema_name NOT LIKE 'pg_toast%' AND schema_name NOT LIKE 'pg_temp%' ORDER BY schema_name",
            )
            .await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let _g = self.lock.lock().await;
        let lit = string_literal(SqlDialect::Postgres, schema);
        let rows = self
            .text_rows(&format!(
                "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = {lit} ORDER BY table_name"
            ))
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
        let _g = self.lock.lock().await;
        let lit = string_literal(SqlDialect::Postgres, schema);
        let tables = self
            .text_rows(&format!(
                "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = {lit} ORDER BY table_name"
            ))
            .await?;
        let columns = self
            .text_rows(&format!(
                "SELECT table_name, column_name, data_type, udt_name, is_nullable, column_default, character_maximum_length, \
                 numeric_precision, numeric_scale, is_identity FROM information_schema.columns WHERE table_schema = {lit} \
                 ORDER BY table_name, ordinal_position"
            ))
            .await?;
        let keys = self
            .text_rows(&format!(
                "SELECT tc.table_name, tc.constraint_name, tc.constraint_type, kcu.column_name FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu ON kcu.constraint_schema = tc.constraint_schema \
                 AND kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name \
                 WHERE tc.table_schema = {lit} AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
                 ORDER BY tc.table_name, tc.constraint_name, kcu.ordinal_position"
            ))
            .await?;
        let fks = self
            .text_rows(&format!(
                "SELECT kcu.table_name, kcu.constraint_name, kcu.column_name, rk.table_schema, rk.table_name, rk.column_name, rc.delete_rule \
                 FROM information_schema.key_column_usage kcu \
                 JOIN information_schema.referential_constraints rc ON rc.constraint_schema = kcu.constraint_schema AND rc.constraint_name = kcu.constraint_name \
                 JOIN information_schema.key_column_usage rk ON rk.constraint_schema = rc.unique_constraint_schema \
                 AND rk.constraint_name = rc.unique_constraint_name AND rk.ordinal_position = kcu.position_in_unique_constraint \
                 WHERE kcu.table_schema = {lit} ORDER BY kcu.table_name, kcu.constraint_name, kcu.ordinal_position"
            ))
            .await?;
        let indexes = self
            .text_rows(&format!("SELECT tablename, indexname, indexdef FROM pg_indexes WHERE schemaname = {lit}"))
            .await
            .unwrap_or_default();
        // Extension types (pgvector `halfvec(1536)`, PostGIS `geometry(Point,4326)`…) are only
        // "USER-DEFINED" in information_schema; format_type() has their full declaration.
        let mut columns = columns;
        if columns.iter().any(|r| r.get(2).cloned().flatten().as_deref() == Some("USER-DEFINED")) {
            let full = self
                .text_rows(&format!(
                    "SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
                     JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = {lit} AND a.attnum > 0 AND NOT a.attisdropped"
                ))
                .await
                .unwrap_or_default();
            let full: HashMap<(String, String), String> =
                full.into_iter().filter_map(|r| Some(((r.first()?.clone()?, r.get(1)?.clone()?), r.get(2)?.clone()?))).collect();
            for r in columns.iter_mut() {
                if r.get(2).cloned().flatten().as_deref() == Some("USER-DEFINED")
                    && let (Some(Some(t)), Some(Some(c))) = (r.first(), r.get(1))
                    && let Some(ty) = full.get(&(t.clone(), c.clone()))
                    && r.len() > 3
                {
                    r[3] = Some(ty.clone());
                }
            }
        }
        Ok(assemble(schema, tables, columns, keys, fks, indexes))
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let _g = self.lock.lock().await;
        let mut out = Vec::new();
        for (i, stmt) in split_statements(sql, SqlDialect::Postgres).iter().enumerate() {
            match self.run_statement(stmt, max_rows).await {
                Ok(r) => out.push(r),
                Err(mut e) => {
                    e.statement_index = Some(i);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.in_rolled_back(sql, max_rows, "BEGIN").await
    }

    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.in_rolled_back(sql, max_rows, "BEGIN TRANSACTION READ ONLY").await
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        self.execute_in_transaction_inner(statements, expected).await
    }
}

impl PgClient {
    /// Statements inside a transaction opened with `begin` and always rolled back.
    async fn in_rolled_back(&self, sql: &str, max_rows: usize, begin: &str) -> Result<Vec<QueryResult>> {
        let _g = self.lock.lock().await;
        self.client.batch_execute(begin).await.map_err(pg_err)?;
        let mut out = Vec::new();
        let mut failure = None;
        for (i, stmt) in split_statements(sql, SqlDialect::Postgres).iter().enumerate() {
            match self.run_statement(stmt, max_rows).await {
                Ok(r) => out.push(r),
                Err(mut e) => {
                    e.statement_index = Some(i);
                    failure = Some(e);
                    break;
                }
            }
        }
        self.client.batch_execute("ROLLBACK").await.map_err(pg_err)?;
        match failure {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    async fn execute_in_transaction_inner(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let _g = self.lock.lock().await;
        self.client.batch_execute("BEGIN").await.map_err(pg_err)?;
        let mut total = 0;
        for (i, s) in statements.iter().enumerate() {
            match self.client.execute(s.as_str(), &[]).await {
                Ok(n) => total += n,
                Err(e) => {
                    let _ = self.client.batch_execute("ROLLBACK").await;
                    let mut err = pg_err(e);
                    err.statement_index = Some(i);
                    return Err(err);
                }
            }
        }
        if let Some(e) = expected.filter(|e| *e != total) {
            let _ = self.client.batch_execute("ROLLBACK").await;
            return Err(ClientError::new(format!(
                "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
            ))
            .with_code("TB001"));
        }
        self.client.batch_execute("COMMIT").await.map_err(pg_err)?;
        Ok(total)
    }
}

type Rows = Vec<Vec<Option<String>>>;

fn assemble(schema: &str, tables: Rows, columns: Rows, keys: Rows, fks: Rows, indexes: Rows) -> Vec<TableSchema> {
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
        let default = r.get(5).cloned().flatten();
        let auto = default.as_deref().is_some_and(|d| d.starts_with("nextval(")) || g(r, 9) == "YES";
        out[i].columns.push(ColumnInfo {
            name: g(r, 1),
            data_type: display_type(
                &g(r, 2),
                &g(r, 3),
                r.get(6).and_then(|x| x.as_deref()),
                r.get(7).and_then(|x| x.as_deref()),
                r.get(8).and_then(|x| x.as_deref()),
            ),
            nullable: g(r, 4) == "YES",
            default,
            primary_key: false,
            unique: false,
            auto,
        });
    }
    // Primary keys and unique constraints.
    let mut uniques: HashMap<(usize, String), Vec<String>> = HashMap::new();
    for r in &keys {
        let Some(&i) = pos.get(&g(r, 0)) else { continue };
        let col = g(r, 3);
        if g(r, 2) == "PRIMARY KEY" {
            out[i].primary_key.push(col);
        } else {
            uniques.entry((i, g(r, 1))).or_default().push(col);
        }
    }
    for ((i, name), cols) in uniques {
        if cols.len() == 1
            && let Some(c) = out[i].columns.iter_mut().find(|c| c.name == cols[0])
        {
            c.unique = true;
        }
        out[i].indexes.push(IndexInfo { name, columns: cols, unique: true, primary: false });
    }
    for t in &mut out {
        let pk = t.primary_key.clone();
        for c in &mut t.columns {
            if pk.contains(&c.name) {
                c.primary_key = true;
            }
        }
    }
    // Foreign keys.
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
                on_delete: r.get(6).cloned().flatten().filter(|a| a != "NO ACTION"),
            }),
        }
    }
    // Plain indexes from pg_indexes.indexdef: CREATE [UNIQUE] INDEX name ON tbl USING btree (a, b)
    for r in &indexes {
        let Some(&i) = pos.get(&g(r, 0)) else { continue };
        let name = g(r, 1);
        if out[i].indexes.iter().any(|x| x.name == name) {
            continue;
        }
        let def = g(r, 2);
        let cols = def
            .rfind('(')
            .and_then(|o| def[o + 1..].find(')').map(|c| &def[o + 1..o + 1 + c]))
            .map(|inner| inner.split(',').map(|c| c.trim().trim_matches('"').split_whitespace().next().unwrap_or("").to_owned()).collect())
            .unwrap_or_default();
        let primary = out[i].primary_key == cols && def.contains("UNIQUE");
        out[i].indexes.push(IndexInfo { name, columns: cols, unique: def.contains("UNIQUE"), primary });
    }
    out
}
