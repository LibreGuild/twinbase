//! TwinBase mock servers: real wire protocols backed by lightweight engines.
//!
//! * PostgreSQL, MySQL and SQL Server (TDS, [`tds`]) use the TwinLite engine (`twinbase-engine`).
//! * Redis uses an in-memory store with snapshot persistence ([`redis`]).
//! * MongoDB uses an in-memory document store with BSON snapshot persistence ([`mongo`]).
//! * DynamoDB speaks HTTP (JSON 1.0, SigV4) over an in-memory table store ([`dynamo`]).
//! * Firestore speaks gRPC (`google.firestore.v1`) over an in-memory document store ([`firestore`]).

pub mod dynamo;
pub mod faults;
pub mod firestore;
pub mod mongo;
pub mod mysql;
pub mod native_pg;
pub mod pg;
pub mod redis;
pub mod tds;
mod traffic;

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twinbase_engine::{Dialect, Engine, EngineOptions};

pub use faults::{Decision, FaultAction, FaultConfig, FaultRule, Faults};
pub use traffic::{TrafficEvent, TrafficKind, TrafficLog};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MockKind {
    Postgres,
    Mysql,
    Mssql,
    Redis,
    Mongodb,
    Dynamodb,
    Firestore,
}

impl MockKind {
    pub fn default_port(self) -> u16 {
        match self {
            MockKind::Postgres => 5432,
            MockKind::Mysql => 3306,
            MockKind::Mssql => 1433,
            MockKind::Redis => 6379,
            MockKind::Mongodb => 27017,
            MockKind::Dynamodb => 8000,
            MockKind::Firestore => 8080,
        }
    }
}

/// How clients authenticate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthMode {
    /// Accept any user name and password (so any application config works).
    #[default]
    Any,
    /// Require exactly this user and password.
    Password { user: String, password: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockConfig {
    pub kind: MockKind,
    pub host: String,
    /// 0 picks a free port.
    pub port: u16,
    /// Where data lives; `None` keeps it in a temporary directory removed on stop.
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub auth: AuthMode,
    #[serde(default)]
    pub faults: FaultConfig,
    /// PostgreSQL mocks only: run a real PostgreSQL server instead of TwinLite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeEngine>,
}

/// Where the native PostgreSQL engine finds its binaries (`None`: detect an installation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeEngine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum MockError {
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: String, source: std::io::Error },
    #[error("{0}")]
    Engine(#[from] twinbase_engine::EngineError),
    #[error("{0}")]
    Other(String),
}

/// The backing store of a running mock (for in-process access by TwinBase itself).
#[derive(Clone)]
pub enum Backend {
    Sql(Engine),
    Redis(std::sync::Arc<redis::RedisStore>),
    Mongo(std::sync::Arc<mongo::MongoStore>),
    Dynamo(std::sync::Arc<dynamo::DynamoStore>),
    Firestore(std::sync::Arc<firestore::FirestoreStore>),
    NativePg(std::sync::Arc<native_pg::NativeServer>),
}

/// A running mock server.
pub struct MockHandle {
    pub addr: SocketAddr,
    pub kind: MockKind,
    pub backend: Backend,
    pub traffic: TrafficLog,
    /// Live fault-injection rules; change them with [`Faults::set`].
    pub faults: Faults,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl MockHandle {
    /// Stop accepting connections, close existing ones and wait for shutdown.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Start a mock server.
pub async fn start(config: MockConfig) -> Result<MockHandle, MockError> {
    let addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&addr).await.map_err(|source| MockError::Bind { addr: addr.clone(), source })?;
    let local = listener.local_addr().map_err(|e| MockError::Other(e.to_string()))?;
    let cancel = CancellationToken::new();
    let traffic = TrafficLog::default();
    let faults = Faults::new(config.faults.clone()).map_err(MockError::Other)?;
    let open_engine = |dialect| -> Result<Engine, MockError> {
        Ok(match &config.data_dir {
            Some(dir) => Engine::open(dir, dialect, EngineOptions::default())?,
            None => Engine::temporary(dialect)?,
        })
    };
    let (backend, task) = match config.kind {
        MockKind::Postgres if config.native.is_some() => {
            let wanted = config.native.as_ref().and_then(|n| n.bin.clone());
            let bins = native_pg::detect(wanted.as_deref()).ok_or_else(|| {
                MockError::Other(match &wanted {
                    Some(dir) => format!("no PostgreSQL server binaries (initdb, postgres) in {}", dir.display()),
                    None => "PostgreSQL is not installed: install it (macOS: brew install postgresql@17 or Postgres.app; Debian/Ubuntu: apt install postgresql) or choose its bin folder in Settings".into(),
                })
            })?;
            let server = std::sync::Arc::new(
                native_pg::NativeServer::start(bins, config.data_dir.as_ref().map(|d| d.join("pgdata"))).await.map_err(MockError::Other)?,
            );
            let s = server.clone();
            let (auth, tr, fa, ca) = (config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone());
            let task = tokio::spawn(async move {
                native_pg::serve(listener, s.clone(), auth, tr, fa, ca).await;
                s.stop().await;
            });
            (Backend::NativePg(server), task)
        }
        MockKind::Postgres => {
            let engine = open_engine(Dialect::Postgres)?;
            let task =
                tokio::spawn(pg::serve(listener, engine.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Sql(engine), task)
        }
        MockKind::Mysql => {
            let engine = open_engine(Dialect::MySql)?;
            let task =
                tokio::spawn(mysql::serve(listener, engine.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Sql(engine), task)
        }
        MockKind::Mssql => {
            let engine = open_engine(Dialect::MsSql)?;
            let task =
                tokio::spawn(tds::serve(listener, engine.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Sql(engine), task)
        }
        MockKind::Redis => {
            let path = config.data_dir.as_deref().map(redis::snapshot_path);
            let store = std::sync::Arc::new(redis::RedisStore::new(path));
            let task =
                tokio::spawn(redis::serve(listener, store.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Redis(store), task)
        }
        MockKind::Mongodb => {
            let path = config.data_dir.as_deref().map(mongo::snapshot_path);
            let store = std::sync::Arc::new(mongo::MongoStore::new(path));
            let task =
                tokio::spawn(mongo::serve(listener, store.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Mongo(store), task)
        }
        MockKind::Dynamodb => {
            let path = config.data_dir.as_deref().map(dynamo::snapshot_path);
            let store = std::sync::Arc::new(dynamo::DynamoStore::new(path));
            let task =
                tokio::spawn(dynamo::serve(listener, store.clone(), config.auth.clone(), traffic.clone(), faults.clone(), cancel.clone()));
            (Backend::Dynamo(store), task)
        }
        MockKind::Firestore => {
            let path = config.data_dir.as_deref().map(firestore::snapshot_path);
            let store = std::sync::Arc::new(firestore::FirestoreStore::new(path));
            let task = tokio::spawn(firestore::serve(
                listener,
                store.clone(),
                config.auth.clone(),
                traffic.clone(),
                faults.clone(),
                cancel.clone(),
            ));
            (Backend::Firestore(store), task)
        }
    };
    Ok(MockHandle { addr: local, kind: config.kind, backend, traffic, faults, cancel, task })
}
