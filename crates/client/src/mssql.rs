//! Microsoft SQL Server / Azure SQL driver (TDS through tiberius).
//!
//! Scripts are split into batches at `GO` lines, as in SSMS and sqlcmd. A batch that declares
//! variables or contains procedural code (`BEGIN … END`, `CREATE PROCEDURE`, …) is sent whole;
//! otherwise each statement is sent on its own, followed by `SELECT @@ROWCOUNT`, so every statement
//! gets its own result and affected-row count. Temporary tables and transactions live in the session
//! either way.

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use futures::TryStreamExt;
use tiberius::{AuthMethod, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::sqlgen::string_literal;
use crate::{
    ClientError, ColumnInfo, ConnectionConfig, ForeignKeyInfo, IndexInfo, QueryResult, Result, ResultColumn, SqlClient, SqlDialect,
    TableInfo, TableSchema, TlsMode, float_cell, hex_cell, int_cell,
};

type Tds = tiberius::Client<Compat<TcpStream>>;

/// A connection plus the session options that change what statements return.
struct Conn {
    tds: Tds,
    /// `SET SHOWPLAN_…`, `STATISTICS PROFILE/XML`, `NOEXEC`, `PARSEONLY`, `FMTONLY` currently ON:
    /// statements then return plans (or nothing), so no row-count probe is added.
    quiet: std::collections::BTreeSet<String>,
}

impl Conn {
    /// Track session options set by a statement.
    fn note_set(&mut self, stmt: &str) {
        let k = keywords(stmt);
        if k.first().map(String::as_str) != Some("SET") || k.len() < 3 {
            return;
        }
        let on = match k.last().map(String::as_str) {
            Some("ON") => true,
            Some("OFF") => false,
            _ => return,
        };
        let names: Vec<&String> = k[1..k.len() - 1].iter().filter(|w| *w != ",").collect();
        let mut i = 0;
        while i < names.len() {
            let mut name = names[i].clone();
            if name == "STATISTICS" {
                i += 1;
                while i < names.len() && matches!(names[i].as_str(), "PROFILE" | "XML" | "IO" | "TIME") {
                    let opt = format!("STATISTICS {}", names[i]);
                    if matches!(names[i].as_str(), "PROFILE" | "XML") {
                        if on {
                            self.quiet.insert(opt);
                        } else {
                            self.quiet.remove(&opt);
                        }
                    }
                    i += 1;
                }
                continue;
            }
            if matches!(name.as_str(), "SHOWPLAN_ALL" | "SHOWPLAN_TEXT" | "SHOWPLAN_XML" | "NOEXEC" | "PARSEONLY" | "FMTONLY") {
                if on {
                    self.quiet.insert(std::mem::take(&mut name));
                } else {
                    self.quiet.remove(&name);
                }
            }
            i += 1;
        }
    }
}

/// Column name of the row-count probe that follows each statement.
const PROBE: &str = "__tb_rowcount";

pub struct MssqlClient {
    conn: Mutex<Conn>,
}

fn ms_err(e: tiberius::error::Error) -> ClientError {
    match e {
        tiberius::error::Error::Server(t) => {
            let mut err = ClientError::new(t.message().to_owned()).with_code(t.code().to_string());
            let mut detail = format!("line {}", t.line());
            if !t.procedure().is_empty() {
                detail = format!("{} in {detail}", t.procedure());
            }
            err.detail = Some(detail);
            err
        }
        other => ClientError::new(other.to_string()),
    }
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType::*;
    match t {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money | Money4 => "money",
        Datetime4 => "smalldatetime",
        Datetime | Datetimen => "datetime",
        Guid => "uniqueidentifier",
        Decimaln => "decimal",
        Numericn => "numeric",
        Daten => "date",
        Timen => "time",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        BigVarBin => "varbinary",
        BigBinary => "binary",
        BigVarChar => "varchar",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Xml => "xml",
        Udt => "udt",
        Text => "text",
        Image => "image",
        NText => "ntext",
        SSVariant => "sql_variant",
    }
}

/// `.5`, `.1234567` (no trailing zeros), or nothing for whole seconds.
fn fraction(nanos: u32) -> String {
    if nanos == 0 {
        return String::new();
    }
    let s = format!(".{nanos:09}");
    s.trim_end_matches('0').to_owned()
}

/// Exact decimal text with the column's scale (`12.50`, `3`).
fn numeric_text(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let sign = if value < 0 { "-" } else { "" };
    let scale = scale as usize;
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{int}.{frac}")
}

fn cell(v: &ColumnData<'static>) -> serde_json::Value {
    use serde_json::Value as J;
    let conv = |r: tiberius::Result<Option<String>>| r.ok().flatten().map(J::String).unwrap_or(J::Null);
    match v {
        ColumnData::U8(x) => x.map(J::from).unwrap_or(J::Null),
        ColumnData::I16(x) => x.map(J::from).unwrap_or(J::Null),
        ColumnData::I32(x) => x.map(J::from).unwrap_or(J::Null),
        ColumnData::I64(x) => x.map(int_cell).unwrap_or(J::Null),
        ColumnData::F32(x) => x.map(|x| float_cell(x as f64)).unwrap_or(J::Null),
        ColumnData::F64(x) => x.map(float_cell).unwrap_or(J::Null),
        ColumnData::Bit(x) => x.map(J::Bool).unwrap_or(J::Null),
        ColumnData::String(x) => x.as_ref().map(|s| J::String(s.to_string())).unwrap_or(J::Null),
        ColumnData::Guid(x) => x.map(|g| J::String(g.to_string().to_uppercase())).unwrap_or(J::Null),
        ColumnData::Binary(x) => x.as_ref().map(|b| hex_cell(b)).unwrap_or(J::Null),
        ColumnData::Numeric(x) => x.map(|n| J::String(numeric_text(n.value(), n.scale()))).unwrap_or(J::Null),
        ColumnData::Xml(x) => x.as_ref().map(|x| J::String(x.to_string())).unwrap_or(J::Null),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => conv(
            chrono::NaiveDateTime::from_sql(v)
                .map(|d| d.map(|d| format!("{}{}", d.format("%Y-%m-%d %H:%M:%S"), fraction(d.and_utc().timestamp_subsec_nanos())))),
        ),
        ColumnData::Date(_) => conv(chrono::NaiveDate::from_sql(v).map(|d| d.map(|d| d.to_string()))),
        ColumnData::Time(_) => {
            use chrono::Timelike;
            conv(chrono::NaiveTime::from_sql(v).map(|d| d.map(|d| format!("{}{}", d.format("%H:%M:%S"), fraction(d.nanosecond())))))
        }
        ColumnData::DateTimeOffset(_) => {
            conv(chrono::DateTime::<chrono::FixedOffset>::from_sql(v).map(|d| {
                d.map(|d| format!("{}{} {}", d.format("%Y-%m-%d %H:%M:%S"), fraction(d.timestamp_subsec_nanos()), d.format("%:z")))
            }))
        }
    }
}

// ------------------------------------------------------------------ script splitting

/// Split a script into batches at `GO` lines (`GO 3` repeats the batch), ignoring `GO` inside
/// strings, bracketed names and block comments.
pub fn split_batches(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_str = false;
    let mut in_bracket = false;
    let mut comment_depth = 0usize;
    for line in sql.split_inclusive('\n') {
        let normal_at_start = !in_str && !in_bracket && comment_depth == 0;
        if normal_at_start {
            let t = line.trim();
            let t = t.split("--").next().unwrap_or("").trim();
            let mut words = t.split_whitespace();
            if words.next().is_some_and(|w| w.eq_ignore_ascii_case("go")) {
                let count = match words.next() {
                    None => Some(1),
                    Some(n) => n.parse::<usize>().ok().filter(|_| words.next().is_none()),
                };
                if let Some(n) = count {
                    let batch = current.trim();
                    if !batch.is_empty() {
                        for _ in 0..n.clamp(1, 1000) {
                            out.push(batch.to_owned());
                        }
                    }
                    current.clear();
                    continue;
                }
            }
        }
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            if comment_depth > 0 {
                if c == b'*' && next == Some(b'/') {
                    comment_depth -= 1;
                    i += 1;
                } else if c == b'/' && next == Some(b'*') {
                    comment_depth += 1;
                    i += 1;
                }
            } else if in_str {
                if c == b'\'' {
                    in_str = false;
                }
            } else if in_bracket {
                if c == b']' {
                    in_bracket = false;
                }
            } else if c == b'-' && next == Some(b'-') {
                break;
            } else if c == b'/' && next == Some(b'*') {
                comment_depth += 1;
                i += 1;
            } else if c == b'\'' {
                in_str = true;
            } else if c == b'[' {
                in_bracket = true;
            }
            i += 1;
        }
        current.push_str(line);
    }
    let batch = current.trim();
    if !batch.is_empty() {
        out.push(batch.to_owned());
    }
    out
}

/// Upper-case words of a batch outside strings, names and comments.
fn keywords(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let b: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let flush = |w: &mut String, out: &mut Vec<String>| {
        if !w.is_empty() {
            out.push(w.to_ascii_uppercase());
            w.clear();
        }
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '-' && next == Some('-') {
            flush(&mut word, &mut out);
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            flush(&mut word, &mut out);
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 1;
                } else if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 1;
                }
                i += 1;
            }
            continue;
        } else if c == '\'' || c == '[' || c == '"' {
            flush(&mut word, &mut out);
            let end = if c == '[' { ']' } else { c };
            i += 1;
            while i < b.len() {
                if b[i] == end {
                    if b.get(i + 1) == Some(&end) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                i += 1;
            }
        } else if c.is_alphanumeric() || c == '_' || c == '@' || c == '#' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            if c == ';' {
                out.push(";".into());
            }
        }
        i += 1;
    }
    flush(&mut word, &mut out);
    out
}

/// Whether a batch must run as one unit (variables, procedural blocks, module definitions).
fn needs_whole_batch(batch: &str) -> bool {
    let k = keywords(batch);
    let first: Vec<&str> = k.iter().take(4).map(String::as_str).collect();
    let module = matches!(first.as_slice(), ["CREATE" | "ALTER", "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER" | "VIEW" | "SCHEMA", ..])
        || matches!(first.as_slice(), ["CREATE", "OR", "ALTER", ..]);
    let block = k.iter().enumerate().any(|(i, w)| {
        (w == "BEGIN" && !matches!(k.get(i + 1).map(String::as_str), Some("TRAN" | "TRANSACTION" | "DISTRIBUTED")))
            || w == "DECLARE"
            || w == "GOTO"
            || (w == "ELSE" && i > 0 && k[i - 1] == ";")
    });
    module || block
}

/// Statements of a batch (one entry when it must run whole).
fn batch_statements(batch: &str) -> Vec<String> {
    if needs_whole_batch(batch) {
        return vec![batch.to_owned()];
    }
    let parts = twinbase_engine::translate::split_statements(batch, twinbase_engine::Dialect::Postgres);
    if parts.is_empty() { vec![batch.to_owned()] } else { parts }
}

/// `SET SHOWPLAN_…`, `SET PARSEONLY`, … must be alone in their batch: no row-count probe.
fn probe_allowed(stmt: &str) -> bool {
    let k = keywords(stmt);
    !(k.first().is_some_and(|w| w == "SET") || k.first().is_some_and(|w| w == "USE"))
}

// ------------------------------------------------------------------ execution

struct Batch {
    results: Vec<QueryResult>,
    /// `@@ROWCOUNT` after the statement, when probed.
    rowcount: Option<u64>,
}

/// Run one SQL batch and collect its result sets.
async fn run_batch(c: &mut Conn, sql: &str, max_rows: usize, statement: &str) -> Result<Batch> {
    let started = Instant::now();
    let mut stream = c.tds.simple_query(sql).await.map_err(ms_err)?;
    let mut results: Vec<QueryResult> = Vec::new();
    let mut rowcount = None;
    let mut probe = false;
    while let Some(item) = stream.try_next().await.map_err(ms_err)? {
        match item {
            QueryItem::Metadata(meta) => {
                let columns: Vec<ResultColumn> = meta
                    .columns()
                    .iter()
                    .map(|c| ResultColumn { name: c.name().to_owned(), type_name: type_name(c.column_type()).to_owned() })
                    .collect();
                probe = columns.len() == 1 && columns[0].name == PROBE;
                if !probe {
                    results.push(QueryResult { statement: statement.to_owned(), columns, ..Default::default() });
                }
            }
            QueryItem::Row(row) => {
                if probe {
                    rowcount = row.into_iter().next().and_then(|v| match v {
                        ColumnData::I32(Some(n)) => u64::try_from(n).ok(),
                        ColumnData::I64(Some(n)) => u64::try_from(n).ok(),
                        _ => None,
                    });
                    continue;
                }
                let Some(r) = results.last_mut() else { continue };
                if r.rows.len() >= max_rows {
                    r.truncated = true;
                    continue;
                }
                r.rows.push(row.into_iter().map(|v| cell(&v)).collect());
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    for r in &mut results {
        r.elapsed_ms = elapsed;
    }
    Ok(Batch { results, rowcount })
}

/// Run one statement (or whole batch) with a row-count probe when allowed.
async fn run_statement(c: &mut Conn, stmt: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let started = Instant::now();
    let probed = c.quiet.is_empty() && probe_allowed(stmt) && !needs_whole_batch(stmt);
    let sql = if probed { format!("{stmt}\n;SELECT @@ROWCOUNT AS [{PROBE}]") } else { stmt.to_owned() };
    let batch = run_batch(c, &sql, max_rows, stmt).await?;
    c.note_set(stmt);
    if !batch.results.is_empty() {
        return Ok(batch.results);
    }
    Ok(vec![QueryResult {
        statement: stmt.to_owned(),
        affected: batch.rowcount,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        message: (!probed).then(|| "Commands completed successfully.".to_owned()),
        ..Default::default()
    }])
}

async fn run_script(c: &mut Conn, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut out = Vec::new();
    let mut index = 0;
    for batch in split_batches(sql) {
        for stmt in batch_statements(&batch) {
            match run_statement(c, &stmt, max_rows).await {
                Ok(r) => out.extend(r),
                Err(mut e) => {
                    e.statement_index = Some(index);
                    return Err(e);
                }
            }
            index += 1;
        }
    }
    Ok(out)
}

async fn exec(c: &mut Conn, sql: &str) -> Result<()> {
    c.tds.simple_query(sql).await.map_err(ms_err)?.into_results().await.map_err(ms_err)?;
    Ok(())
}

/// Text rows of an introspection query.
async fn text_rows(c: &mut Conn, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let rows = c.tds.simple_query(sql).await.map_err(ms_err)?.into_first_result().await.map_err(ms_err)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match cell(&v) {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s),
                    serde_json::Value::Bool(b) => Some(if b { "1".into() } else { "0".into() }),
                    other => Some(other.to_string()),
                })
                .collect()
        })
        .collect())
}

/// `((0))` → `0`, `(getdate())` → `getdate()`: SQL Server wraps stored defaults in parentheses.
fn strip_parens(s: &str) -> String {
    let mut t = s.trim();
    while t.starts_with('(') && t.ends_with(')') {
        let inner = &t[1..t.len() - 1];
        let mut depth = 0i32;
        let balanced = inner.chars().all(|c| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth >= 0
        });
        if !balanced || depth != 0 {
            break;
        }
        t = inner.trim();
    }
    t.to_owned()
}

/// Declared type text, e.g. `nvarchar(100)`, `decimal(10,2)`, `varchar(max)`.
fn type_text(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    match name {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length == -1 {
                format!("{name}(max)")
            } else {
                format!("{name}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length == -1 {
                format!("{name}(max)")
            } else {
                format!("{name}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" if scale != 7 => format!("{name}({scale})"),
        _ => name.to_owned(),
    }
}

impl MssqlClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<MssqlClient> {
        let mut cfg = Config::new();
        let host = if config.host.is_empty() { "127.0.0.1".to_owned() } else { config.host.clone() };
        cfg.host(&host);
        cfg.port(if config.port == 0 { 1433 } else { config.port });
        if let Some(db) = config.database.as_ref().filter(|d| !d.is_empty()) {
            cfg.database(db);
        }
        cfg.application_name("TwinBase");
        cfg.authentication(if config.user.is_empty() {
            // No user: the password field holds a Microsoft Entra (Azure AD) access token.
            AuthMethod::aad_token(config.password.clone().unwrap_or_default())
        } else {
            AuthMethod::sql_server(&config.user, config.password.as_deref().unwrap_or(""))
        });
        match config.tls {
            // Only the login packet is encrypted and the certificate is not checked, as with
            // `Encrypt=no` in Microsoft's drivers.
            TlsMode::Disable => {
                cfg.encryption(EncryptionLevel::Off);
                cfg.trust_cert();
            }
            TlsMode::Require => {
                cfg.encryption(EncryptionLevel::Required);
                cfg.trust_cert();
            }
            // Verified against the operating system's trusted roots.
            TlsMode::VerifyFull => cfg.encryption(EncryptionLevel::Required),
        }
        cfg.lossy_utf16_decoding(true);
        // Through a tunnel, verify the certificate against the real server name.
        let addr = match config.via {
            Some(via) => {
                cfg.hostname_in_certificate(&host);
                via.to_string()
            }
            None => cfg.get_addr(),
        };
        crate::tls::ensure_crypto_provider();
        let tcp = TcpStream::connect(&addr).await.map_err(|e| ClientError::new(format!("cannot connect to {addr}: {e}")))?;
        tcp.set_nodelay(true).ok();
        let conn = match tiberius::Client::connect(cfg.clone(), tcp.compat_write()).await {
            Ok(c) => c,
            // Azure SQL gateways redirect to the node that serves the database.
            Err(tiberius::error::Error::Routing { host, port }) if config.via.is_none() => {
                cfg.host(&host);
                cfg.port(port);
                let tcp = TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|e| ClientError::new(format!("cannot connect to {host}:{port}: {e}")))?;
                tcp.set_nodelay(true).ok();
                tiberius::Client::connect(cfg, tcp.compat_write()).await.map_err(ms_err)?
            }
            Err(e) => return Err(ms_err(e)),
        };
        Ok(MssqlClient { conn: Mutex::new(Conn { tds: conn, quiet: Default::default() }) })
    }
}

#[async_trait]
impl SqlClient for MssqlClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mssql
    }

    async fn server_version(&self) -> Result<String> {
        let mut c = self.conn.lock().await;
        let rows = text_rows(
            &mut c,
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)), CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), \
             CAST(SERVERPROPERTY('EngineEdition') AS int)",
        )
        .await?;
        let r = rows.into_iter().next().unwrap_or_default();
        let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let product = if g(2) == "5" { "Azure SQL Database" } else { "SQL Server" };
        Ok(format!("{product} {} ({})", g(0), g(1)))
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let mut c = self.conn.lock().await;
        let rows = text_rows(
            &mut c,
            "SELECT name FROM sys.schemas WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') AND name NOT LIKE 'db[_]%' \
             ORDER BY CASE WHEN name = SCHEMA_NAME() THEN 0 ELSE 1 END, name",
        )
        .await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
    }

    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let mut c = self.conn.lock().await;
        let lit = string_literal(SqlDialect::Mssql, schema);
        let rows = text_rows(
            &mut c,
            &format!("SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = {lit} ORDER BY TABLE_NAME"),
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
        let lit = string_literal(SqlDialect::Mssql, schema);
        let tables = text_rows(
            &mut c,
            &format!(
                "SELECT o.name, o.type FROM sys.objects o WHERE o.schema_id = SCHEMA_ID({lit}) AND o.type IN ('U', 'V') ORDER BY o.name"
            ),
        )
        .await?;
        let columns = text_rows(
            &mut c,
            &format!(
                "SELECT o.name, c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, dc.definition, c.is_identity, c.is_computed \
                 FROM sys.objects o JOIN sys.columns c ON c.object_id = o.object_id JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
                 LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                 WHERE o.schema_id = SCHEMA_ID({lit}) AND o.type IN ('U', 'V') ORDER BY o.name, c.column_id"
            ),
        )
        .await?;
        let indexes = text_rows(
            &mut c,
            &format!(
                "SELECT o.name, i.name, i.is_unique, i.is_primary_key, col.name FROM sys.indexes i JOIN sys.objects o ON o.object_id = i.object_id \
                 JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0 \
                 JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                 WHERE o.schema_id = SCHEMA_ID({lit}) AND o.type IN ('U', 'V') AND i.name IS NOT NULL \
                 ORDER BY o.name, i.name, ic.key_ordinal"
            ),
        )
        .await?;
        let fks = text_rows(
            &mut c,
            &format!(
                "SELECT o.name, fk.name, pc.name, SCHEMA_NAME(ro.schema_id), ro.name, rc.name, fk.delete_referential_action_desc \
                 FROM sys.foreign_keys fk JOIN sys.objects o ON o.object_id = fk.parent_object_id \
                 JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 WHERE o.schema_id = SCHEMA_ID({lit}) ORDER BY o.name, fk.name, fkc.constraint_column_id"
            ),
        )
        .await?;
        let g = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let n = |r: &Vec<Option<String>>, i: usize| g(r, i).parse::<i64>().unwrap_or(0);
        let mut out: Vec<TableSchema> = tables
            .iter()
            .map(|r| TableSchema {
                schema: schema.to_owned(),
                name: g(r, 0),
                kind: if g(r, 1).trim() == "V" { "view".into() } else { "table".into() },
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
                data_type: type_text(&g(r, 2), n(r, 3), n(r, 4), n(r, 5)),
                nullable: g(r, 6) == "1",
                default: r.get(7).cloned().flatten().map(|d| strip_parens(&d)),
                primary_key: false,
                unique: false,
                auto: g(r, 8) == "1" || g(r, 9) == "1",
            });
        }
        for r in &indexes {
            let Some(&i) = pos.get(&g(r, 0)) else { continue };
            let name = g(r, 1);
            let col = g(r, 4);
            let primary = g(r, 3) == "1";
            if primary {
                out[i].primary_key.push(col.clone());
            }
            match out[i].indexes.iter_mut().find(|x| x.name == name) {
                Some(ix) => ix.columns.push(col),
                None => out[i].indexes.push(IndexInfo { primary, unique: g(r, 2) == "1", name, columns: vec![col] }),
            }
        }
        for t in &mut out {
            let single_unique: Vec<String> =
                t.indexes.iter().filter(|x| x.unique && !x.primary && x.columns.len() == 1).map(|x| x.columns[0].clone()).collect();
            for c in &mut t.columns {
                c.primary_key = t.primary_key.contains(&c.name);
                c.unique = single_unique.contains(&c.name);
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
                    on_delete: match g(r, 6).as_str() {
                        "CASCADE" => Some("CASCADE".into()),
                        "SET_NULL" => Some("SET NULL".into()),
                        "SET_DEFAULT" => Some("SET DEFAULT".into()),
                        _ => None,
                    },
                }),
            }
        }
        Ok(out)
    }

    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        run_script(&mut c, sql, max_rows).await
    }

    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        exec(&mut c, "BEGIN TRANSACTION").await?;
        let result = run_script(&mut c, sql, max_rows).await;
        exec(&mut c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await?;
        result
    }

    /// SQL Server has no read-only transactions: the statements run in one that is rolled back.
    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.query_rolled_back(sql, max_rows).await
    }

    async fn query_wrapped(&self, setup: &str, sql: &str, teardown: &str, max_rows: usize, rolled_back: bool) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.lock().await;
        if rolled_back {
            exec(&mut c, "BEGIN TRANSACTION").await?;
        }
        let result = match run_script(&mut c, setup, 0).await {
            Ok(_) => run_script(&mut c, sql, max_rows).await,
            Err(e) => Err(e),
        };
        let cleanup = run_script(&mut c, teardown, 0).await;
        if rolled_back {
            exec(&mut c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await?;
        }
        let result = result?;
        cleanup?;
        Ok(result)
    }

    async fn execute_in_transaction(&self, statements: &[String], expected: Option<u64>) -> Result<u64> {
        let mut c = self.conn.lock().await;
        exec(&mut c, "BEGIN TRANSACTION").await?;
        let mut total = 0;
        for (i, s) in statements.iter().enumerate() {
            match run_statement(&mut c, s, 0).await {
                Ok(results) => total += results.iter().filter_map(|r| r.affected).sum::<u64>(),
                Err(mut e) => {
                    let _ = exec(&mut c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
                    e.statement_index = Some(i);
                    return Err(e);
                }
            }
        }
        if let Some(e) = expected.filter(|e| *e != total) {
            let _ = exec(&mut c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
            return Err(ClientError::new(format!(
                "expected {e} affected rows but the server reported {total}; the data changed since it was loaded or previewed. Nothing was committed."
            ))
            .with_code("TB001"));
        }
        exec(&mut c, "COMMIT TRANSACTION").await?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_batches_at_go_lines() {
        let s = "CREATE TABLE t (a int)\nGO\nINSERT t VALUES (1) -- go\n/* GO\n*/\nGO 2\nSELECT 'x\nGO\ny'\ngo";
        let b = split_batches(s);
        assert_eq!(b.len(), 4);
        assert_eq!(b[0], "CREATE TABLE t (a int)");
        assert_eq!(b[1], b[2]);
        assert!(b[1].starts_with("INSERT t VALUES (1)"));
        assert_eq!(b[3], "SELECT 'x\nGO\ny'");
        assert_eq!(split_batches("SELECT 1; SELECT 2"), vec!["SELECT 1; SELECT 2"]);
    }

    #[test]
    fn batches_with_variables_or_blocks_run_whole() {
        assert_eq!(batch_statements("SELECT 1; SELECT 2;").len(), 2);
        assert_eq!(batch_statements("DECLARE @x int = 1; SELECT @x;").len(), 1);
        assert_eq!(batch_statements("CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END").len(), 1);
        assert_eq!(batch_statements("IF 1 = 1 BEGIN PRINT 'a'; PRINT 'b'; END").len(), 1);
        assert_eq!(batch_statements("BEGIN TRAN; UPDATE t SET a = 1; COMMIT").len(), 3);
        assert_eq!(batch_statements("SELECT 'DECLARE'; SELECT [begin] FROM t").len(), 2);
        assert!(!probe_allowed("SET SHOWPLAN_ALL ON"));
        assert!(probe_allowed("UPDATE t SET a = 1"));
    }

    #[test]
    fn numeric_text_keeps_scale() {
        assert_eq!(numeric_text(1250, 2), "12.50");
        assert_eq!(numeric_text(3, 0), "3");
        assert_eq!(numeric_text(-5, 3), "-0.005");
    }

    #[test]
    fn type_text_and_defaults() {
        assert_eq!(type_text("nvarchar", 200, 0, 0), "nvarchar(100)");
        assert_eq!(type_text("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(type_text("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(type_text("datetime2", 8, 27, 7), "datetime2");
        assert_eq!(strip_parens("((0))"), "0");
        assert_eq!(strip_parens("(getdate())"), "getdate()");
        assert_eq!(strip_parens("(N'x')"), "N'x'");
        assert_eq!(strip_parens("((1)+(2))"), "(1)+(2)");
    }
}
