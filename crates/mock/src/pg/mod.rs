//! PostgreSQL wire-protocol (v3) mock server on top of the TwinLite engine.

mod codec;

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::BytesMut;
use futures::{Sink, stream};
use pgwire::api::auth::{
    AuthSource, LoginInfo, Password, ServerParameterProvider, StartupHandler, cleartext::CleartextPasswordAuthStartupHandler,
    finish_authentication, protocol_negotiation, save_startup_parameters_to_metadata,
};
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{
    ClientInfo, METADATA_DATABASE, METADATA_USER, PgWireConnectionState, PgWireServerHandlers, PidSecretKeyGenerator,
    RandomPidSecretKeyGenerator, Type,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use twinbase_engine::{Engine, EngineError, OutColumn, Outcome, Param, Prepared, Session, TxEffect, TxState};

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};

/// Shared state of one PostgreSQL mock server.
struct Server {
    engine: Engine,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
}

/// Per-connection handler: one engine session per client connection.
struct Conn {
    server: Arc<Server>,
    id: u64,
    session: Arc<std::sync::Mutex<Option<Session>>>,
    prepared: parking_lot::Mutex<HashMap<String, Arc<Prepared>>>,
    parser: Arc<NoopQueryParser>,
}

fn to_error_info(e: &EngineError) -> ErrorInfo {
    let mut info = ErrorInfo::new("ERROR".to_owned(), e.sqlstate.clone(), e.message.clone());
    info.detail = e.detail.clone();
    info.hint = e.hint.clone();
    info
}

fn user_error(e: EngineError) -> PgWireError {
    PgWireError::UserError(Box::new(to_error_info(&e)))
}

/// Outcome of fault injection before a statement: `Ok(None)` to run it, `Ok(Some(error))` to answer
/// with an error, `Err` to terminate the connection (as when a backend is killed).
async fn inject(server: &Server, conn: u64, sql: &str) -> PgWireResult<Option<ErrorInfo>> {
    match server.faults.before(sql).await {
        Decision::Proceed => Ok(None),
        Decision::Fail { code, message } => {
            server.traffic.fault(conn, sql, &message);
            Ok(Some(ErrorInfo::new("ERROR".into(), code.unwrap_or_else(|| "XX000".into()), message)))
        }
        Decision::Drop => {
            server.traffic.fault(conn, sql, "connection dropped");
            Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".into(),
                "57P01".into(),
                "terminating connection due to administrator command".into(),
            ))))
        }
    }
}

impl Conn {
    /// Run blocking engine work for this connection, opening the session on first use.
    async fn with_session<T, F>(&self, client: &(impl ClientInfo + Sync), f: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> Result<T, EngineError> + Send + 'static,
    {
        let user = client.metadata().get(METADATA_USER).cloned().unwrap_or_else(|| "postgres".into());
        let db = client.metadata().get(METADATA_DATABASE).cloned();
        let session = self.session.clone();
        let engine = self.server.engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = session.lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                *guard = Some(engine.session(db.as_deref(), &user)?);
            }
            f(guard.as_mut().expect("session opened"))
        })
        .await
        .map_err(|e| EngineError::internal(format!("worker failed: {e}")))?
    }

    async fn prepare(&self, client: &(impl ClientInfo + Sync), sql: &str) -> PgWireResult<Arc<Prepared>> {
        if let Some(p) = self.prepared.lock().get(sql) {
            return Ok(p.clone());
        }
        let text = sql.to_owned();
        let p = Arc::new(self.with_session(client, move |s| s.prepare(&text)).await.map_err(user_error)?);
        let mut cache = self.prepared.lock();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(sql.to_owned(), p.clone());
        Ok(p)
    }

    fn field_infos(columns: &[OutColumn], format: &Format) -> Vec<FieldInfo> {
        columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                FieldInfo::new(c.name.clone(), c.table_oid.map(|o| o as i32), c.column_index, codec::pg_type(&c.ty), format.format_for(i))
            })
            .collect()
    }

    fn to_response(outcome: Outcome, format: &Format) -> Response {
        match outcome.tx {
            TxEffect::Begin => return Response::TransactionStart(Tag::new(&outcome.tag)),
            TxEffect::End => return Response::TransactionEnd(Tag::new(&outcome.tag)),
            TxEffect::None => {}
        }
        if let Some(rs) = outcome.result {
            let fields = Arc::new(Self::field_infos(&rs.columns, format));
            let binary: Vec<bool> = (0..rs.columns.len()).map(|i| format.format_for(i) == FieldFormat::Binary).collect();
            let ncols = rs.columns.len() as i16;
            let rows: Vec<PgWireResult<DataRow>> = rs
                .rows
                .iter()
                .map(|row| {
                    let mut buf = BytesMut::with_capacity(64);
                    for (i, cell) in row.iter().enumerate() {
                        codec::encode_cell(&mut buf, cell, &rs.columns[i].ty, binary[i]);
                    }
                    Ok(DataRow::new(buf, ncols))
                })
                .collect();
            let mut qr = QueryResponse::new(fields, stream::iter(rows));
            qr.set_command_tag(if outcome.tag == "INSERT" { "INSERT 0" } else { &outcome.tag });
            return Response::Query(qr);
        }
        if outcome.tag.is_empty() {
            return Response::EmptyQuery;
        }
        let tag = match outcome.tag.as_str() {
            "INSERT" => Tag::new("INSERT").with_oid(0).with_rows(outcome.affected as usize),
            "UPDATE" | "DELETE" | "SELECT" | "MOVE" | "FETCH" | "COPY" => Tag::new(&outcome.tag).with_rows(outcome.affected as usize),
            other => Tag::new(other),
        };
        Response::Execution(tag)
    }
}

#[async_trait]
impl SimpleQueryHandler for Conn {
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        if let Some(e) = inject(&self.server, self.id, query).await? {
            return Ok(vec![Response::Error(Box::new(e))]);
        }
        let started = Instant::now();
        let text = query.to_owned();
        let result = self.with_session(client, move |s| Ok(s.execute(&text))).await;
        let batch = match result {
            Ok(b) => b,
            Err(e) => {
                self.server.traffic.query(self.id, query, started, 0, Some(e.message.clone()));
                return Ok(vec![Response::Error(Box::new(to_error_info(&e)))]);
            }
        };
        let rows: u64 = batch.outcomes.iter().map(|o| o.affected).sum();
        self.server.traffic.query(self.id, query, started, rows, batch.error.as_ref().map(|e| e.message.clone()));
        let mut out: Vec<Response> = batch.outcomes.into_iter().map(|o| Self::to_response(o, &Format::UnifiedText)).collect();
        if let Some(e) = batch.error {
            out.push(Response::Error(Box::new(to_error_info(&e))));
        }
        Ok(out)
    }
}

#[async_trait]
impl ExtendedQueryHandler for Conn {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.parser.clone()
    }

    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let p = self.prepare(client, &stmt.statement).await?;
        let count = p.param_types.len().max(stmt.parameter_types.len());
        let params: Vec<Type> = (0..count)
            .map(|i| match stmt.parameter_types.get(i) {
                Some(Some(t)) if t.oid() != 0 => t.clone(),
                _ => p.param_types.get(i).map(codec::pg_type).unwrap_or(Type::TEXT),
            })
            .collect();
        Ok(DescribeStatementResponse::new(params, Self::field_infos(&p.columns, &Format::UnifiedText)))
    }

    async fn do_describe_portal<C>(&self, client: &mut C, portal: &Portal<Self::Statement>) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let p = self.prepare(client, &portal.statement.statement).await?;
        Ok(DescribePortalResponse::new(Self::field_infos(&p.columns, &portal.result_column_format)))
    }

    async fn do_query<C>(&self, client: &mut C, portal: &Portal<Self::Statement>, _max_rows: usize) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let sql = portal.statement.statement.clone();
        if let Some(e) = inject(&self.server, self.id, &sql).await? {
            return Ok(Response::Error(Box::new(e)));
        }
        let started = Instant::now();
        let p = match self.prepare(client, &sql).await {
            Ok(p) => p,
            Err(e) => {
                self.server.traffic.query(self.id, &sql, started, 0, Some(e.to_string()));
                return Err(e);
            }
        };
        let params: Vec<Param> = portal
            .parameters
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                let oid = match portal.statement.parameter_types.get(i) {
                    Some(Some(t)) if t.oid() != 0 => t.oid(),
                    _ => p.param_types.get(i).map(|t| t.pg_oid()).unwrap_or(25),
                };
                let binary = portal.parameter_format.format_for(i) == FieldFormat::Binary;
                codec::decode_param(raw.as_deref(), binary, oid)
            })
            .collect();
        let prepared = p.clone();
        let result = self.with_session(client, move |s| s.execute_prepared(&prepared, &params)).await;
        match result {
            Ok(outcome) => {
                self.server.traffic.query(self.id, &sql, started, outcome.affected, None);
                Ok(Self::to_response(outcome, &portal.result_column_format))
            }
            Err(e) => {
                self.server.traffic.query(self.id, &sql, started, 0, Some(e.message.clone()));
                Ok(Response::Error(Box::new(to_error_info(&e))))
            }
        }
    }
}

/// Reports PostgreSQL-like server parameters at startup.
struct Params;

impl ServerParameterProvider for Params {
    fn server_parameters<C>(&self, client: &C) -> Option<HashMap<String, String>>
    where
        C: ClientInfo,
    {
        let mut p = HashMap::new();
        for (k, v) in [
            ("server_version", "16.4"),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("IntervalStyle", "postgres"),
            ("TimeZone", "UTC"),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"),
            ("is_superuser", "on"),
            ("in_hot_standby", "off"),
            ("default_transaction_read_only", "off"),
            ("search_path", "\"$user\", public"),
        ] {
            p.insert(k.to_owned(), v.to_owned());
        }
        p.insert("session_authorization".into(), client.metadata().get(METADATA_USER).cloned().unwrap_or_default());
        p.insert("application_name".into(), client.metadata().get("application_name").cloned().unwrap_or_default());
        Some(p)
    }
}

/// Accept any credentials (the default for mocks).
struct TrustStartup {
    traffic: TrafficLog,
    id: u64,
}

#[async_trait]
impl StartupHandler for TrustStartup {
    async fn on_startup<C>(&self, client: &mut C, message: PgWireFrontendMessage) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if let PgWireFrontendMessage::Startup(ref startup) = message {
            protocol_negotiation(client, startup).await?;
            save_startup_parameters_to_metadata(client, startup);
            let (pid, key) = RandomPidSecretKeyGenerator::default().generate(client);
            client.set_pid_and_secret_key(pid, key);
            self.traffic.record(
                self.id,
                TrafficKind::Connect {
                    peer: client.socket_addr().to_string(),
                    user: client.metadata().get(METADATA_USER).cloned(),
                    database: client.metadata().get(METADATA_DATABASE).cloned(),
                },
            );
            finish_authentication(client, &Params).await?;
        }
        if matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
            return Ok(());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FixedPassword {
    user: String,
    password: String,
}

#[async_trait]
impl AuthSource for FixedPassword {
    async fn get_password(&self, login: &LoginInfo) -> PgWireResult<Password> {
        if login.user() != Some(self.user.as_str()) {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".into(),
                "28P01".into(),
                format!("password authentication failed for user \"{}\"", login.user().unwrap_or_default()),
            ))));
        }
        Ok(Password::new(None, self.password.as_bytes().to_vec()))
    }
}

struct Factory {
    conn: Arc<Conn>,
}

impl PgWireServerHandlers for Factory {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.conn.clone()
    }

    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.conn.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        Arc::new(Startup {
            trust: TrustStartup { traffic: self.conn.server.traffic.clone(), id: self.conn.id },
            password: match &self.conn.server.auth {
                AuthMode::Any => None,
                AuthMode::Password { user, password } => {
                    Some(CleartextPasswordAuthStartupHandler::new(FixedPassword { user: user.clone(), password: password.clone() }, Params))
                }
            },
        })
    }
}

struct Startup {
    trust: TrustStartup,
    password: Option<CleartextPasswordAuthStartupHandler<FixedPassword, Params>>,
}

#[async_trait]
impl StartupHandler for Startup {
    async fn on_startup<C>(&self, client: &mut C, message: PgWireFrontendMessage) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match &self.password {
            Some(p) => p.on_startup(client, message).await,
            None => self.trust.on_startup(client, message).await,
        }
    }
}

/// Serve PostgreSQL clients until `cancel` fires.
pub async fn serve(listener: TcpListener, engine: Engine, auth: AuthMode, traffic: TrafficLog, faults: Faults, cancel: CancellationToken) {
    let server = Arc::new(Server { engine, auth, traffic, faults });
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            a = listener.accept() => a,
        };
        let Ok((socket, _peer)) = accepted else { continue };
        let id = server.traffic.new_connection_id();
        if server.faults.refuse_connections() {
            server.traffic.fault(id, "(new connection)", "connection refused");
            drop(socket);
            continue;
        }
        let _ = socket.set_nodelay(true);
        let conn = Arc::new(Conn {
            server: server.clone(),
            id,
            session: Arc::new(std::sync::Mutex::new(None)),
            prepared: parking_lot::Mutex::new(HashMap::new()),
            parser: Arc::new(NoopQueryParser::new()),
        });
        let factory = Arc::new(Factory { conn: conn.clone() });
        let cancel = cancel.clone();
        let traffic = server.traffic.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = pgwire::tokio::process_socket(socket, None, factory) => {}
            }
            // Roll back any open transaction and release the SQLite connection off the runtime.
            let session = conn.session.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut guard = session.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(s) = guard.as_ref() {
                    let _ = s.tx_state() == TxState::Idle;
                }
                guard.take();
            })
            .await;
            traffic.record(id, TrafficKind::Disconnect);
        });
    }
}
