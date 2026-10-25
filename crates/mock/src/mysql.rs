//! MySQL wire-protocol mock server on top of the TwinLite engine.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter, OkResponse, ParamParser,
    QueryResultWriter, RowWriter, StatementMetaWriter, StatusFlags, ValueInner,
};
use tokio::io::AsyncWrite;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use twinbase_engine::catalog::Identity;
use twinbase_engine::value::{self, parse_any_timestamp, parse_time_only};
use twinbase_engine::{Dialect, Engine, EngineError, OutColumn, Outcome, Param, Prepared, Session, SqlType, Value};

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};

struct MyConn {
    engine: Engine,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    id: u64,
    connection_id: u32,
    user: std::sync::Mutex<String>,
    // Mutex only makes the shim Sync; the connection uses it from one task at a time.
    session: std::sync::Mutex<Option<Session>>,
    statements: HashMap<u32, Arc<Prepared>>,
    next_statement: u32,
}

fn column_type(ty: &SqlType) -> ColumnType {
    match ty {
        SqlType::Bool | SqlType::Int1 => ColumnType::MYSQL_TYPE_TINY,
        SqlType::Int2 => ColumnType::MYSQL_TYPE_SHORT,
        SqlType::Int4 => ColumnType::MYSQL_TYPE_LONG,
        SqlType::Int8 => ColumnType::MYSQL_TYPE_LONGLONG,
        SqlType::Year => ColumnType::MYSQL_TYPE_YEAR,
        SqlType::Float4 => ColumnType::MYSQL_TYPE_FLOAT,
        SqlType::Float8 => ColumnType::MYSQL_TYPE_DOUBLE,
        SqlType::Numeric { .. } => ColumnType::MYSQL_TYPE_NEWDECIMAL,
        SqlType::Date => ColumnType::MYSQL_TYPE_DATE,
        SqlType::Time => ColumnType::MYSQL_TYPE_TIME,
        SqlType::Timestamp => ColumnType::MYSQL_TYPE_DATETIME,
        SqlType::TimestampTz => ColumnType::MYSQL_TYPE_TIMESTAMP,
        SqlType::Json | SqlType::Jsonb => ColumnType::MYSQL_TYPE_JSON,
        SqlType::Bytea => ColumnType::MYSQL_TYPE_BLOB,
        SqlType::Enum { .. } => ColumnType::MYSQL_TYPE_STRING,
        _ => ColumnType::MYSQL_TYPE_VAR_STRING,
    }
}

impl MyConn {
    async fn with_session<T, F>(&mut self, f: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> Result<T, EngineError> + Send + 'static,
    {
        let taken = self.session.lock().unwrap_or_else(|p| p.into_inner()).take();
        let mut session = match taken {
            Some(s) => s,
            None => {
                let engine = self.engine.clone();
                let user = self.user.lock().unwrap().clone();
                tokio::task::spawn_blocking(move || engine.session(None, &user))
                    .await
                    .map_err(|e| EngineError::internal(e.to_string()))??
            }
        };
        let (session, result) = tokio::task::spawn_blocking(move || {
            let r = f(&mut session);
            (session, r)
        })
        .await
        .map_err(|e| EngineError::internal(e.to_string()))?;
        *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(session);
        result
    }

    fn columns(&self, cols: &[OutColumn]) -> Vec<Column> {
        let cat = self.session.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|s| s.catalog());
        cols.iter()
            .map(|c| {
                let mut flags = ColumnFlags::empty();
                let mut table = String::new();
                if let (Some((schema, t)), Some(cat)) = (&c.source, &cat) {
                    table = t.clone();
                    if let Some(td) = cat.table(schema, t)
                        && let Some(col) = td.column(&c.name)
                    {
                        if !col.nullable {
                            flags |= ColumnFlags::NOT_NULL_FLAG;
                        }
                        if td.primary_key.as_ref().is_some_and(|pk| pk.columns.contains(&col.name)) {
                            flags |= ColumnFlags::PRI_KEY_FLAG;
                        }
                        if matches!(col.identity, Some(Identity::AutoIncrement { .. })) {
                            flags |= ColumnFlags::AUTO_INCREMENT_FLAG;
                        }
                    }
                }
                if matches!(c.ty, SqlType::Bytea) {
                    flags |= ColumnFlags::BINARY_FLAG | ColumnFlags::BLOB_FLAG;
                }
                Column { table, column: c.name.clone(), coltype: column_type(&c.ty), colflags: flags }
            })
            .collect()
    }

    fn ok_response(&self, o: &Outcome) -> OkResponse {
        let mut status = StatusFlags::empty();
        if let Some(s) = self.session.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            if s.in_transaction() {
                status |= StatusFlags::SERVER_STATUS_IN_TRANS;
            }
            if s.setting("autocommit").as_deref() != Some("0") {
                status |= StatusFlags::SERVER_STATUS_AUTOCOMMIT;
            }
        }
        OkResponse {
            affected_rows: o.affected,
            last_insert_id: o.last_insert_id.unwrap_or(0).max(0) as u64,
            status_flags: status,
            ..Default::default()
        }
    }
}

async fn write_rows<W: AsyncWrite + Send + Unpin>(w: &mut RowWriter<'_, W>, outcome: &Outcome) -> io::Result<()> {
    let Some(rs) = &outcome.result else { return Ok(()) };
    for row in &rs.rows {
        for (cell, col) in row.iter().zip(&rs.columns) {
            write_cell(w, cell, &col.ty)?;
        }
        w.end_row().await?;
    }
    Ok(())
}

fn write_cell<W: AsyncWrite + Send + Unpin>(w: &mut RowWriter<'_, W>, v: &Value, ty: &SqlType) -> io::Result<()> {
    if matches!(v, Value::Null) {
        return w.write_col(None::<i64>);
    }
    let int = || match v {
        Value::Integer(i) => *i,
        Value::Real(f) => f.round() as i64,
        Value::Text(t) => t.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let float = || match v {
        Value::Integer(i) => *i as f64,
        Value::Real(f) => *f,
        Value::Text(t) => t.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    let text = || value::format_text(v, ty, Dialect::MySql).unwrap_or_default();
    match column_type(ty) {
        ColumnType::MYSQL_TYPE_TINY => w.write_col(int() as i8),
        ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR => w.write_col(int() as i16),
        ColumnType::MYSQL_TYPE_LONG => w.write_col(int() as i32),
        ColumnType::MYSQL_TYPE_LONGLONG => w.write_col(int()),
        ColumnType::MYSQL_TYPE_FLOAT => w.write_col(float() as f32),
        ColumnType::MYSQL_TYPE_DOUBLE => w.write_col(float()),
        ColumnType::MYSQL_TYPE_DATE => match parse_any_timestamp(&text()) {
            Some(ts) => w.write_col(ts.date()),
            None => w.write_col(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
        },
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            let raw = match v {
                Value::Text(t) => t.clone(),
                _ => text(),
            };
            match parse_any_timestamp(&raw) {
                Some(ts) => w.write_col(ts),
                None => w.write_col(NaiveDateTime::default()),
            }
        }
        ColumnType::MYSQL_TYPE_TIME => {
            let t = parse_time_only(&text()).unwrap_or_default();
            w.write_col(std::time::Duration::new(t.num_seconds_from_midnight() as u64, t.nanosecond()))
        }
        ColumnType::MYSQL_TYPE_BLOB => match v {
            Value::Blob(b) => w.write_col(b.as_slice()),
            _ => w.write_col(text().as_bytes()),
        },
        _ => w.write_col(text().as_str()),
    }
}

async fn send_outcome<W: AsyncWrite + Send + Unpin>(conn: &MyConn, results: QueryResultWriter<'_, W>, outcome: &Outcome) -> io::Result<()> {
    match &outcome.result {
        Some(rs) => {
            let cols = conn.columns(&rs.columns);
            let mut w = results.start(&cols).await?;
            write_rows(&mut w, outcome).await?;
            w.finish().await
        }
        None => results.completed(conn.ok_response(outcome)).await,
    }
}

/// Fault injection before a statement: `Ok(None)` runs it, `Ok(Some(..))` answers with that
/// error, `Err` closes the connection.
async fn inject(conn: &MyConn, sql: &str) -> io::Result<Option<(u16, String)>> {
    match conn.faults.before(sql).await {
        Decision::Proceed => Ok(None),
        Decision::Fail { code, message } => {
            conn.traffic.fault(conn.id, sql, &message);
            Ok(Some((code.and_then(|c| c.trim().parse().ok()).unwrap_or(1105), message)))
        }
        Decision::Drop => {
            conn.traffic.fault(conn.id, sql, "connection dropped");
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connection dropped by fault injection"))
        }
    }
}

async fn send_error<W: AsyncWrite + Send + Unpin>(results: QueryResultWriter<'_, W>, e: &EngineError) -> io::Result<()> {
    results.error(ErrorKind::from(e.mysql_code), e.message.as_bytes()).await
}

fn decode_params(params: ParamParser<'_>) -> Vec<Param> {
    params
        .into_iter()
        .map(|p| match p.value.into_inner() {
            ValueInner::NULL => Param::Null,
            ValueInner::Int(i) => Param::Int(i),
            ValueInner::UInt(u) => Param::Int(u as i64),
            ValueInner::Double(f) => Param::Float(f),
            ValueInner::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => Param::Text(s.to_owned()),
                Err(_) => Param::Bytes(b.to_vec()),
            },
            ValueInner::Date(b) | ValueInner::Datetime(b) => Param::Text(decode_mysql_datetime(b)),
            ValueInner::Time(b) => Param::Text(decode_mysql_time(b)),
        })
        .collect()
}

fn decode_mysql_datetime(b: &[u8]) -> String {
    if b.len() < 4 {
        return "0000-00-00 00:00:00".into();
    }
    let year = u16::from_le_bytes([b[0], b[1]]);
    let (month, day) = (b[2], b[3]);
    let (h, m, s) = if b.len() >= 7 { (b[4], b[5], b[6]) } else { (0, 0, 0) };
    let micros = if b.len() >= 11 { u32::from_le_bytes([b[7], b[8], b[9], b[10]]) } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}.{micros:06}")
}

fn decode_mysql_time(b: &[u8]) -> String {
    if b.len() < 8 {
        return "00:00:00".into();
    }
    let days = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
    let (h, m, s) = (b[5] as u32 + days * 24, b[6], b[7]);
    format!("{}{h:02}:{m:02}:{s:02}", if b[0] == 1 { "-" } else { "" })
}

/// `mysql_native_password`: SHA1(password) XOR SHA1(salt + SHA1(SHA1(password))).
fn native_password_ok(password: &str, salt: &[u8], auth: &[u8]) -> bool {
    use sha1::{Digest, Sha1};
    if password.is_empty() {
        return auth.is_empty();
    }
    let h1 = Sha1::digest(password.as_bytes());
    let h2 = Sha1::digest(h1);
    let mut hasher = Sha1::new();
    hasher.update(&salt[..salt.len().min(20)]);
    hasher.update(h2);
    let h3 = hasher.finalize();
    let expected: Vec<u8> = h1.iter().zip(h3.iter()).map(|(a, b)| a ^ b).collect();
    expected == auth
}

#[async_trait]
impl<W: AsyncWrite + Send + Unpin> AsyncMysqlShim<W> for MyConn {
    type Error = io::Error;

    fn version(&self) -> String {
        "8.0.36-TwinBase".into()
    }

    fn connect_id(&self) -> u32 {
        self.connection_id
    }

    async fn authenticate(&self, _plugin: &str, username: &[u8], salt: &[u8], auth_data: &[u8]) -> bool {
        let user = String::from_utf8_lossy(username).into_owned();
        *self.user.lock().unwrap() = user.clone();
        let ok = match &self.auth {
            AuthMode::Any => true,
            AuthMode::Password { user: u, password } => *u == user && native_password_ok(password, salt, auth_data),
        };
        if ok {
            self.traffic.record(self.id, TrafficKind::Connect { peer: String::new(), user: Some(user), database: None });
        }
        ok
    }

    async fn on_prepare<'a>(&'a mut self, query: &'a str, info: StatementMetaWriter<'a, W>) -> io::Result<()> {
        let sql = query.to_owned();
        match self.with_session(move |s| s.prepare(&sql)).await {
            Ok(p) => {
                let id = self.next_statement;
                self.next_statement += 1;
                let params: Vec<Column> = p
                    .param_types
                    .iter()
                    .map(|_| Column {
                        table: String::new(),
                        column: "?".into(),
                        coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
                        colflags: ColumnFlags::empty(),
                    })
                    .collect();
                let cols = self.columns(&p.columns);
                self.statements.insert(id, Arc::new(p));
                info.reply(id, &params, &cols).await
            }
            Err(e) => info.error(ErrorKind::from(e.mysql_code), e.message.as_bytes()).await,
        }
    }

    async fn on_execute<'a>(&'a mut self, id: u32, params: ParamParser<'a>, results: QueryResultWriter<'a, W>) -> io::Result<()> {
        let Some(p) = self.statements.get(&id).cloned() else {
            return results.error(ErrorKind::ER_UNKNOWN_STMT_HANDLER, b"Unknown prepared statement handler").await;
        };
        if let Some((code, message)) = inject(self, &p.sql).await? {
            return results.error(ErrorKind::from(code), message.as_bytes()).await;
        }
        let started = Instant::now();
        let params = decode_params(params);
        let prepared = p.clone();
        match self.with_session(move |s| s.execute_prepared(&prepared, &params)).await {
            Ok(o) => {
                self.traffic.query(self.id, &p.sql, started, o.affected, None);
                send_outcome(self, results, &o).await
            }
            Err(e) => {
                self.traffic.query(self.id, &p.sql, started, 0, Some(e.message.clone()));
                send_error(results, &e).await
            }
        }
    }

    async fn on_close<'a>(&'a mut self, id: u32)
    where
        W: 'async_trait,
    {
        self.statements.remove(&id);
    }

    async fn on_query<'a>(&'a mut self, query: &'a str, results: QueryResultWriter<'a, W>) -> io::Result<()> {
        if let Some((code, message)) = inject(self, query).await? {
            return results.error(ErrorKind::from(code), message.as_bytes()).await;
        }
        let started = Instant::now();
        let sql = query.to_owned();
        let batch = match self.with_session(move |s| Ok(s.execute(&sql))).await {
            Ok(b) => b,
            Err(e) => return send_error(results, &e).await,
        };
        let rows: u64 = batch.outcomes.iter().map(|o| o.affected).sum();
        self.traffic.query(self.id, query, started, rows, batch.error.as_ref().map(|e| e.message.clone()));
        if let Some(e) = &batch.error {
            return send_error(results, e).await;
        }
        // Multiple result sets are only produced for multi-statement batches; send the last.
        match batch.outcomes.last() {
            Some(o) => send_outcome(self, results, o).await,
            None => results.completed(OkResponse::default()).await,
        }
    }

    async fn on_init<'a>(&'a mut self, db: &'a str, writer: InitWriter<'a, W>) -> io::Result<()> {
        let name = db.trim().trim_matches('`').to_owned();
        let sql = format!("USE `{}`", name.replace('`', "``"));
        match self.with_session(move |s| s.execute_single(&sql, &[])).await {
            Ok(_) => writer.ok().await,
            Err(e) => writer.error(ErrorKind::from(e.mysql_code), e.message.as_bytes()).await,
        }
    }
}

/// Serve MySQL clients until `cancel` fires.
pub async fn serve(listener: TcpListener, engine: Engine, auth: AuthMode, traffic: TrafficLog, faults: Faults, cancel: CancellationToken) {
    let next_id = std::sync::atomic::AtomicU32::new(1);
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            a = listener.accept() => a,
        };
        let Ok((socket, _)) = accepted else { continue };
        let id = traffic.new_connection_id();
        if faults.refuse_connections() {
            traffic.fault(id, "(new connection)", "connection refused");
            drop(socket);
            continue;
        }
        let _ = socket.set_nodelay(true);
        let conn = MyConn {
            engine: engine.clone(),
            auth: auth.clone(),
            traffic: traffic.clone(),
            faults: faults.clone(),
            id,
            connection_id: next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            user: std::sync::Mutex::new(String::new()),
            session: std::sync::Mutex::new(None),
            statements: HashMap::new(),
            next_statement: 1,
        };
        let cancel = cancel.clone();
        let traffic = traffic.clone();
        tokio::spawn(async move {
            let (r, w) = socket.into_split();
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = AsyncMysqlIntermediary::run_on(conn, r, w) => {}
            }
            traffic.record(id, TrafficKind::Disconnect);
        });
    }
}
