//! Client drivers TwinBase uses to talk to real servers and to its own mocks: PostgreSQL, MySQL,
//! SQLite, Redis, MongoDB, DynamoDB and Firestore behind one interface for introspection, querying, paging and staged edits.

pub mod bigquery;
pub mod cassandra;
pub mod clickhouse;
pub mod dynamo_client;
pub mod firestore_client;
pub mod google_auth;
pub mod http;
mod mongo_client;
pub mod mongo_shell;
mod mssql;
mod mysql;
pub mod oracle_db;
mod pg;
mod redis_client;
pub mod search_client;
pub mod spanner;
pub mod sqlgen;
mod sqlite;
pub mod ssh;
mod tls;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use dynamo_client::DynamoClient;
pub use firestore_client::FirestoreClient;
pub use mongo_client::{
    CollectionInfo, DatabaseInfo, DocumentsPage, FieldStats, FindParams, MongoClient, MongoOutput, SchemaSample, sample_schema,
};
pub use redis_client::{RedisClient, RedisKey, RedisValue, split_command_line};
pub use sqlgen::{ChangeSet, Filter, FilterOp, RowQuery, SortKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Mysql,
    Sqlite,
    Redis,
    Mongodb,
    Dynamodb,
    Firestore,
    Mssql,
    Clickhouse,
    /// Elasticsearch and OpenSearch.
    Elasticsearch,
    /// Apache Cassandra and ScyllaDB.
    Cassandra,
    /// Google Cloud Spanner (GoogleSQL).
    Spanner,
    Bigquery,
    Oracle,
}

impl DbKind {
    pub fn default_port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::Mysql => 3306,
            DbKind::Redis => 6379,
            DbKind::Mongodb => 27017,
            DbKind::Dynamodb => 8000,
            DbKind::Firestore => 8080,
            DbKind::Mssql => 1433,
            DbKind::Clickhouse => 8123,
            DbKind::Elasticsearch => 9200,
            DbKind::Cassandra => 9042,
            DbKind::Spanner => 9020,
            DbKind::Bigquery => 9050,
            DbKind::Oracle => 1521,
            DbKind::Sqlite => 0,
        }
    }
}

/// SQL flavour of a connection (quoting, literals, introspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    Postgres,
    Mysql,
    Sqlite,
    /// SQL Server (T-SQL).
    Mssql,
    Clickhouse,
    /// CQL (Cassandra, ScyllaDB).
    Cassandra,
    /// GoogleSQL as Cloud Spanner runs it.
    Spanner,
    /// GoogleSQL as BigQuery runs it.
    Bigquery,
    Oracle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// Never use TLS.
    #[default]
    Disable,
    /// Encrypt, but do not verify the server certificate (like libpq `sslmode=require`).
    Require,
    /// Encrypt and verify the certificate against public roots (`sslmode=verify-full`).
    VerifyFull,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub kind: DbKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// SQLite file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub tls: TlsMode,
    /// Connection timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Reach the database through an SSH bastion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<ssh::SshTunnel>,
    /// Address to connect to instead of `host:port` (set for tunnels; `host` stays the TLS name).
    #[serde(skip)]
    pub via: Option<std::net::SocketAddr>,
}

fn default_timeout() -> u64 {
    10
}

#[derive(Debug, thiserror::Error, Clone, Serialize)]
#[error("{message}")]
pub struct ClientError {
    pub message: String,
    /// SQLSTATE / MySQL error code / Redis error kind when the server reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Index of the failing statement in a batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement_index: Option<usize>,
}

impl ClientError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None, detail: None, statement_index: None }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResultColumn {
    pub name: String,
    /// Database type name (e.g. `int4`, `varchar`, `DATETIME`).
    pub type_name: String,
}

/// One statement's result. Cells are JSON values: null, bool, number, string
/// (binary data as `\x...` hex strings).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueryResult {
    pub statement: String,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Rows affected for INSERT/UPDATE/DELETE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected: Option<u64>,
    /// More rows existed than `max_rows`.
    pub truncated: bool,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    /// `table` or `view`.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub primary_key: bool,
    pub unique: bool,
    /// Generated by the database (serial, identity, auto_increment).
    pub auto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableSchema {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub indexes: Vec<IndexInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowsPage {
    pub result: QueryResult,
    /// Total matching rows (None when counting was skipped, failed or stopped at the cap).
    pub total: Option<u64>,
    /// When counting stopped early: at least this many rows match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_at_least: Option<u64>,
    /// Columns that identify a row for edits (primary key, or a fallback such as `ctid`).
    pub key_columns: Vec<String>,
    /// Name of the synthetic row-id column included in results, if any.
    pub row_id_column: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyResult {
    pub statements: Vec<String>,
    pub affected: u64,
}

#[async_trait]
pub trait SqlClient: Send + Sync {
    fn dialect(&self) -> SqlDialect;
    async fn server_version(&self) -> Result<String>;
    /// PostgreSQL schemas, MySQL databases, or `main` for SQLite.
    async fn schemas(&self) -> Result<Vec<String>>;
    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>>;
    /// Full structure of every table in a schema (columns, keys, indexes).
    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>>;
    async fn describe(&self, schema: &str, table: &str) -> Result<TableSchema> {
        self.schema_graph(schema)
            .await?
            .into_iter()
            .find(|t| t.name == table)
            .ok_or_else(|| ClientError::new(format!("table {schema}.{table} not found")))
    }
    /// Run one or more statements; stops at the first error.
    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>>;
    /// Run statements inside a transaction (all or nothing). With `expected_affected`, the
    /// transaction is rolled back when the total affected row count differs.
    async fn execute_in_transaction(&self, statements: &[String], expected_affected: Option<u64>) -> Result<u64>;
    /// Run statements that should only read, for someone who may only read: the server refuses
    /// writes where it can (a read-only transaction or setting) and anything else is rolled back.
    /// Without either, it runs as usual (the caller has already checked the statements).
    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.query(sql, max_rows).await
    }
    /// Run statements inside a transaction that is always rolled back (used for `EXPLAIN ANALYZE`
    /// of writes). The connection stays locked for the whole sequence.
    async fn query_rolled_back(&self, _sql: &str, _max_rows: usize) -> Result<Vec<QueryResult>> {
        Err(ClientError::new("this database does not support running statements in a rolled-back transaction"))
    }
    /// Run `sql` between session settings: `setup` first, `teardown` always afterwards (even when
    /// `sql` fails), all on the locked connection; optionally inside a rolled-back transaction.
    /// Used for SQL Server plans (`SET SHOWPLAN_ALL ON` … `OFF`).
    async fn query_wrapped(
        &self,
        _setup: &str,
        _sql: &str,
        _teardown: &str,
        _max_rows: usize,
        _rolled_back: bool,
    ) -> Result<Vec<QueryResult>> {
        Err(ClientError::new("not supported for this database"))
    }
    /// A page of a table read without SQL (BigQuery's free `tabledata.list`); `None` falls back to
    /// a paged SELECT.
    async fn fetch_page(&self, _schema: &str, _table: &str, _query: &sqlgen::RowQuery) -> Result<Option<RowsPage>> {
        Ok(None)
    }
    /// The server's own plan for a statement (Spanner's query plan), when it has one.
    async fn native_plan(&self, _sql: &str, _analyze: bool) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
    async fn close(&self) {}
}

/// A SQL client whose connection runs through an SSH tunnel that must stay open with it.
struct Tunneled {
    inner: Box<dyn SqlClient>,
    _tunnel: Arc<ssh::Tunnel>,
}

#[async_trait]
impl SqlClient for Tunneled {
    fn dialect(&self) -> SqlDialect {
        self.inner.dialect()
    }
    async fn server_version(&self) -> Result<String> {
        self.inner.server_version().await
    }
    async fn schemas(&self) -> Result<Vec<String>> {
        self.inner.schemas().await
    }
    async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        self.inner.tables(schema).await
    }
    async fn schema_graph(&self, schema: &str) -> Result<Vec<TableSchema>> {
        self.inner.schema_graph(schema).await
    }
    async fn describe(&self, schema: &str, table: &str) -> Result<TableSchema> {
        self.inner.describe(schema, table).await
    }
    async fn query(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.inner.query(sql, max_rows).await
    }
    async fn execute_in_transaction(&self, statements: &[String], expected_affected: Option<u64>) -> Result<u64> {
        self.inner.execute_in_transaction(statements, expected_affected).await
    }
    async fn query_rolled_back(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.inner.query_rolled_back(sql, max_rows).await
    }
    async fn query_read_only(&self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        self.inner.query_read_only(sql, max_rows).await
    }
    async fn query_wrapped(&self, setup: &str, sql: &str, teardown: &str, max_rows: usize, rolled_back: bool) -> Result<Vec<QueryResult>> {
        self.inner.query_wrapped(setup, sql, teardown, max_rows, rolled_back).await
    }
    async fn native_plan(&self, sql: &str, analyze: bool) -> Result<Option<serde_json::Value>> {
        self.inner.native_plan(sql, analyze).await
    }
    async fn fetch_page(&self, schema: &str, table: &str, query: &sqlgen::RowQuery) -> Result<Option<RowsPage>> {
        self.inner.fetch_page(schema, table, query).await
    }
    async fn close(&self) {
        self.inner.close().await
    }
}

/// An open connection.
pub enum Client {
    Sql(Box<dyn SqlClient>),
    Redis(RedisClient),
    Mongo(MongoClient),
    Dynamo(DynamoClient),
    Firestore(FirestoreClient),
    Search(search_client::SearchClient),
}

impl Client {
    pub async fn connect(config: &ConnectionConfig) -> Result<Client> {
        tls::ensure_crypto_provider();
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        // Open the SSH tunnel first; the database connection then goes through its local port.
        let (config, tunnel) = match (&config.ssh, config.kind) {
            (Some(t), kind) if kind != DbKind::Sqlite => {
                let (host, port) = match kind {
                    DbKind::Dynamodb => {
                        let (_, h, p, _) = dynamo_client::endpoint(config);
                        (h, p)
                    }
                    DbKind::Firestore if config.host.trim().is_empty() => ("firestore.googleapis.com".to_owned(), 443),
                    DbKind::Clickhouse => {
                        let (_, h, p) = clickhouse::endpoint(config);
                        (h, p)
                    }
                    DbKind::Elasticsearch => {
                        let (_, h, p) = search_client::endpoint(config);
                        (h, p)
                    }
                    DbKind::Spanner => {
                        let (_, h, p) = spanner::endpoint(config);
                        (h, p)
                    }
                    DbKind::Bigquery => {
                        let (_, h, p) = bigquery::endpoint(config);
                        (h, p)
                    }
                    _ => (
                        if config.host.is_empty() { "127.0.0.1".to_owned() } else { config.host.clone() },
                        if config.port == 0 { kind.default_port() } else { config.port },
                    ),
                };
                let tunnel = ssh::open(t, &host, port, timeout).await?;
                let mut c = config.clone();
                c.via = Some(tunnel.local);
                (std::borrow::Cow::Owned(c), Some(Arc::new(tunnel)))
            }
            _ => (std::borrow::Cow::Borrowed(config), None),
        };
        let config = config.as_ref();
        let fut = async {
            Ok(match config.kind {
                // Driver handshakes are large futures: box them so callers' stacks stay small.
                DbKind::Postgres => Client::Sql(Box::new(Box::pin(pg::PgClient::connect(config)).await?)),
                DbKind::Mysql => Client::Sql(Box::new(Box::pin(mysql::MySqlClient::connect(config)).await?)),
                DbKind::Sqlite => Client::Sql(Box::new(sqlite::SqliteClient::open(config)?)),
                DbKind::Redis => Client::Redis(Box::pin(RedisClient::connect(config)).await?),
                DbKind::Mongodb => Client::Mongo(Box::pin(MongoClient::connect(config)).await?),
                DbKind::Dynamodb => Client::Dynamo(Box::pin(DynamoClient::connect(config)).await?),
                DbKind::Firestore => Client::Firestore(Box::pin(FirestoreClient::connect(config)).await?),
                DbKind::Mssql => Client::Sql(Box::new(Box::pin(mssql::MssqlClient::connect(config)).await?)),
                DbKind::Clickhouse => Client::Sql(Box::new(Box::pin(clickhouse::ClickhouseClient::connect(config)).await?)),
                DbKind::Oracle => Client::Sql(Box::new(Box::pin(oracle_db::OracleClient::connect(config)).await?)),
                DbKind::Bigquery => Client::Sql(Box::new(Box::pin(bigquery::BigQueryClient::connect(config)).await?)),
                DbKind::Spanner => Client::Sql(Box::new(Box::pin(spanner::SpannerClient::connect(config)).await?)),
                DbKind::Cassandra => Client::Sql(Box::new(Box::pin(cassandra::CassandraClient::connect(config)).await?)),
                DbKind::Elasticsearch => Client::Search(Box::pin(search_client::SearchClient::connect(config)).await?),
            })
        };
        let client = match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r?,
            Err(_) => return Err(ClientError::new(format!("connection timed out after {}s", timeout.as_secs()))),
        };
        Ok(match (client, tunnel) {
            (Client::Sql(inner), Some(t)) => Client::Sql(Box::new(Tunneled { inner, _tunnel: t })),
            (Client::Redis(r), Some(t)) => Client::Redis(r.with_tunnel(t)),
            (Client::Mongo(m), Some(t)) => Client::Mongo(m.with_tunnel(t)),
            (Client::Dynamo(d), Some(t)) => Client::Dynamo(d.with_tunnel(t)),
            (Client::Firestore(f), Some(t)) => Client::Firestore(f.with_tunnel(t)),
            (Client::Search(s), Some(t)) => Client::Search(s.with_tunnel(t)),
            (c, None) => c,
        })
    }

    pub fn sql(&self) -> Option<&dyn SqlClient> {
        match self {
            Client::Sql(c) => Some(c.as_ref()),
            _ => None,
        }
    }

    pub fn redis(&self) -> Option<&RedisClient> {
        match self {
            Client::Redis(r) => Some(r),
            _ => None,
        }
    }

    pub fn mongo(&self) -> Option<&MongoClient> {
        match self {
            Client::Mongo(m) => Some(m),
            _ => None,
        }
    }

    pub fn firestore(&self) -> Option<&FirestoreClient> {
        match self {
            Client::Firestore(f) => Some(f),
            _ => None,
        }
    }

    pub fn search(&self) -> Option<&search_client::SearchClient> {
        match self {
            Client::Search(s) => Some(s),
            _ => None,
        }
    }

    pub fn dynamo(&self) -> Option<&DynamoClient> {
        match self {
            Client::Dynamo(d) => Some(d),
            _ => None,
        }
    }
}

/// Fetch a page of rows plus the total count, for the data grid.
pub async fn fetch_rows(client: &dyn SqlClient, schema: &str, table: &str, query: &RowQuery) -> Result<RowsPage> {
    if let Some(page) = client.fetch_page(schema, table, query).await? {
        return Ok(page);
    }
    let info = client.describe(schema, table).await?;
    let dialect = client.dialect();
    let (row_id, key_columns) = if info.primary_key.is_empty() {
        match dialect {
            SqlDialect::Sqlite if info.kind == "table" => (Some("rowid".to_owned()), vec!["rowid".to_owned()]),
            _ => (None, info.columns.iter().map(|c| c.name.clone()).collect()),
        }
    } else {
        (None, info.primary_key.clone())
    };
    let sql = sqlgen::select_page(dialect, schema, table, query, row_id.as_deref(), &info);
    if dialect == SqlDialect::Cassandra {
        // CQL has no OFFSET: read ahead and skip; the total is unknown (counting scans every node).
        let offset = query.offset.min(10_000) as usize;
        let mut results = client.query(&sql, offset + query.limit.max(1) as usize).await?;
        let mut result = results.pop().ok_or_else(|| ClientError::new("no result"))?;
        result.rows.drain(..offset.min(result.rows.len()));
        return Ok(RowsPage { result, total: None, total_at_least: None, key_columns, row_id_column: None });
    }
    let mut results = client.query(&sql, query.limit.max(1) as usize).await?;
    let result = results.pop().ok_or_else(|| ClientError::new("no result"))?;
    let count_sql = sqlgen::count_rows(dialect, schema, table, query);
    let counted = match client.query(&count_sql, 1).await {
        Ok(mut r) => r.pop().and_then(|q| q.rows.first().and_then(|row| row.first()).and_then(json_u64)),
        Err(_) => None,
    };
    let (total, total_at_least) = match counted {
        Some(n) if n > sqlgen::COUNT_CAP => (None, Some(sqlgen::COUNT_CAP)),
        other => (other, None),
    };
    Ok(RowsPage { result, total, total_at_least, key_columns, row_id_column: row_id })
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Apply staged grid edits in one transaction. Every statement targets one row, so the total
/// affected count must equal the number of statements; otherwise the rows changed (or a key matched
/// several rows) and everything is rolled back.
pub async fn apply_changes(client: &dyn SqlClient, schema: &str, table: &str, changes: &ChangeSet) -> Result<ApplyResult> {
    let d = client.dialect();
    let statements = if matches!(d, SqlDialect::Cassandra | SqlDialect::Spanner) {
        // Literals depend on the column type there (uuid, bytes, numeric, collections are not strings).
        let info = client.describe(schema, table).await?;
        let types: HashMap<String, String> = info.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect();
        changes.to_sql_typed(d, schema, table, &types)
    } else {
        changes.to_sql(client.dialect(), schema, table)
    };
    if statements.is_empty() {
        return Ok(ApplyResult { statements, affected: 0 });
    }
    let affected = client.execute_in_transaction(&statements, Some(statements.len() as u64)).await?;
    Ok(ApplyResult { statements, affected })
}

/// JSON cell for binary data.
pub(crate) fn hex_cell(b: &[u8]) -> serde_json::Value {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    serde_json::Value::String(s)
}

/// Integers beyond JavaScript's safe range are sent as strings.
pub(crate) fn int_cell(i: i64) -> serde_json::Value {
    if i.unsigned_abs() > (1u64 << 53) { serde_json::Value::String(i.to_string()) } else { serde_json::Value::from(i) }
}

pub(crate) fn float_cell(f: f64) -> serde_json::Value {
    serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or_else(|| serde_json::Value::String(f.to_string()))
}

/// Split a batch into statements using the dialect's quoting rules. SQL Server scripts split into
/// batches at `GO` lines instead (statements in a batch share variables and run together).
pub fn split_statements(sql: &str, dialect: SqlDialect) -> Vec<String> {
    if dialect == SqlDialect::Mssql {
        return mssql::split_batches(sql);
    }
    if dialect == SqlDialect::Oracle {
        return oracle_db::split_script(sql);
    }
    let d = match dialect {
        // ClickHouse strings use backslash escapes and backtick names, like MySQL.
        SqlDialect::Mysql | SqlDialect::Clickhouse | SqlDialect::Spanner | SqlDialect::Bigquery => twinbase_engine::Dialect::MySql,
        _ => twinbase_engine::Dialect::Postgres,
    };
    twinbase_engine::translate::split_statements(sql, d)
}
