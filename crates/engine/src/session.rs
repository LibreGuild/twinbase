//! A session is one client connection: its own SQLite connection with every schema file
//! attached, session settings, transaction state and a catalog snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::{Mutex, RwLock};
use rusqlite::types::{ToSqlOutput, Value, ValueRef};
use rusqlite::{Connection, ToSql};

use crate::Dialect;
use crate::catalog::{self, Catalog, CatalogTypes, META_DDL, quote_ident, table_oid};
use crate::engine::{Engine, encode_name, list_schema_files};
use crate::error::{EngineError, Result, state};
use crate::functions::{self, FnEnv, SeqState, SessionShared};
use crate::translate::{self, Ctx, Plan, SqlPlan, StmtKind};
use crate::types::SqlType;

/// A bound parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Param::Null => ToSqlOutput::Owned(Value::Null),
            Param::Bool(b) => ToSqlOutput::Owned(Value::Integer(*b as i64)),
            Param::Int(i) => ToSqlOutput::Owned(Value::Integer(*i)),
            Param::Float(f) => ToSqlOutput::Owned(Value::Real(*f)),
            Param::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Param::Bytes(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
        })
    }
}

/// Metadata of a result column.
#[derive(Debug, Clone, PartialEq)]
pub struct OutColumn {
    pub name: String,
    pub ty: SqlType,
    /// Source table (schema, table) for plain column references.
    pub source: Option<(String, String)>,
    pub table_oid: Option<u32>,
    pub column_index: Option<i16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<OutColumn>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEffect {
    None,
    Begin,
    End,
}

/// Result of one statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// Command tag without counts (`SELECT`, `INSERT`, `CREATE TABLE`, …).
    pub tag: String,
    /// Rows returned or affected.
    pub affected: u64,
    pub result: Option<ResultSet>,
    pub last_insert_id: Option<i64>,
    pub tx: TxEffect,
    /// A message for the client (T-SQL PRINT).
    pub message: Option<String>,
}

impl Outcome {
    pub(crate) fn command(tag: &str) -> Self {
        Outcome { tag: tag.to_owned(), affected: 0, result: None, last_insert_id: None, tx: TxEffect::None, message: None }
    }

    /// Full PostgreSQL command tag, e.g. `INSERT 0 3`.
    pub fn pg_tag(&self) -> String {
        match self.tag.as_str() {
            "INSERT" => format!("INSERT 0 {}", self.affected),
            "SELECT" | "UPDATE" | "DELETE" | "MOVE" | "FETCH" | "COPY" => format!("{} {}", self.tag, self.affected),
            other => other.to_owned(),
        }
    }
}

/// Outcomes of a batch; `error` is set when a statement failed (later statements did not run).
#[derive(Debug, Clone)]
pub struct BatchResult {
    pub outcomes: Vec<Outcome>,
    pub error: Option<EngineError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Idle,
    Active,
    /// PostgreSQL: an error happened inside the transaction; only ROLLBACK is accepted.
    Failed,
}

/// A statement prepared through the extended protocol.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub sql: String,
    plan: Plan,
    generation: u64,
    pub param_types: Vec<SqlType>,
    pub columns: Vec<OutColumn>,
}

pub struct Session {
    pub(crate) engine: Engine,
    pub(crate) conn: Connection,
    pub(crate) dialect: Dialect,
    pub(crate) catalog: Arc<RwLock<Arc<Catalog>>>,
    pub(crate) shared: Arc<Mutex<SessionShared>>,
    pub(crate) tx: TxState,
    pub(crate) attached: Vec<String>,
    db_dir: PathBuf,
    pub(crate) database: String,
    seen_catalog_gen: u64,
    seen_layout_gen: u64,
    /// Incremented on every catalog reload; system catalogs are rebuilt when it changes.
    local_gen: u64,
    pg_catalog_gen: u64,
    info_gen: u64,
    catalog_types: CatalogTypes,
    pub(crate) autocommit: bool,
    pub(crate) ddl_in_tx: bool,
    /// The open transaction is read-only (SQLite's `query_only` is on).
    read_only_tx: bool,
    defaults: HashMap<String, String>,
}

fn pg_defaults(user: &str, database: &str) -> HashMap<String, String> {
    [
        ("server_version", "16.4"),
        ("server_version_num", "160004"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("datestyle", "ISO, MDY"),
        ("intervalstyle", "postgres"),
        ("timezone", "UTC"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("application_name", ""),
        ("search_path", "\"$user\", public"),
        ("default_transaction_isolation", "read committed"),
        ("transaction_isolation", "read committed"),
        ("transaction_read_only", "off"),
        ("default_transaction_read_only", "off"),
        ("max_identifier_length", "63"),
        ("lc_collate", "en_US.UTF-8"),
        ("lc_ctype", "en_US.UTF-8"),
        ("lc_messages", "en_US.UTF-8"),
        ("is_superuser", "on"),
        ("statement_timeout", "0"),
        ("lock_timeout", "0"),
        ("idle_in_transaction_session_timeout", "0"),
        ("extra_float_digits", "1"),
        ("bytea_output", "hex"),
        ("max_connections", "100"),
        ("block_size", "8192"),
        ("row_security", "on"),
        ("synchronous_commit", "on"),
        ("client_min_messages", "notice"),
        ("default_text_search_config", "pg_catalog.english"),
        ("escape_string_warning", "on"),
        ("server_version_display", "16.4 (TwinBase)"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .chain([("session_authorization".to_owned(), user.to_owned()), ("current_database".to_owned(), database.to_owned())])
    .collect()
}

fn mysql_defaults(version: &str, connection_id: u32) -> HashMap<String, String> {
    [
        ("version", version),
        ("version_comment", "TwinBase mock server"),
        ("version_compile_os", "Linux"),
        ("version_compile_machine", "x86_64"),
        ("auto_increment_increment", "1"),
        ("auto_increment_offset", "1"),
        ("autocommit", "1"),
        ("character_set_client", "utf8mb4"),
        ("character_set_connection", "utf8mb4"),
        ("character_set_results", "utf8mb4"),
        ("character_set_server", "utf8mb4"),
        ("character_set_database", "utf8mb4"),
        ("character_set_system", "utf8mb3"),
        ("collation_server", "utf8mb4_0900_ai_ci"),
        ("collation_connection", "utf8mb4_0900_ai_ci"),
        ("collation_database", "utf8mb4_0900_ai_ci"),
        ("init_connect", ""),
        ("interactive_timeout", "28800"),
        ("license", "GPL"),
        ("lower_case_table_names", "0"),
        ("max_allowed_packet", "67108864"),
        ("net_buffer_length", "16384"),
        ("net_write_timeout", "60"),
        ("performance_schema", "0"),
        ("query_cache_size", "0"),
        ("query_cache_type", "OFF"),
        (
            "sql_mode",
            "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION",
        ),
        ("system_time_zone", "UTC"),
        ("time_zone", "SYSTEM"),
        ("transaction_isolation", "REPEATABLE-READ"),
        ("tx_isolation", "REPEATABLE-READ"),
        ("transaction_read_only", "0"),
        ("tx_read_only", "0"),
        ("wait_timeout", "28800"),
        ("foreign_key_checks", "1"),
        ("unique_checks", "1"),
        ("sql_auto_is_null", "0"),
        ("sql_select_limit", "18446744073709551615"),
        ("max_execution_time", "0"),
        ("innodb_lock_wait_timeout", "50"),
        ("lock_wait_timeout", "31536000"),
        ("have_ssl", "DISABLED"),
        ("hostname", "twinbase"),
        ("port", "3306"),
        ("socket", ""),
        ("datadir", "/var/lib/mysql/"),
        ("default_storage_engine", "InnoDB"),
        ("default_authentication_plugin", "mysql_native_password"),
        ("lc_messages", "en_US"),
        ("explicit_defaults_for_timestamp", "1"),
        ("group_concat_max_len", "1024"),
        ("warning_count", "0"),
        ("error_count", "0"),
        ("session_track_schema", "1"),
        ("session_track_system_variables", "time_zone,autocommit,character_set_client,character_set_results,character_set_connection"),
        ("session_track_state_change", "0"),
        ("session_track_transaction_info", "OFF"),
        ("default_week_format", "0"),
        ("div_precision_increment", "4"),
        ("max_connections", "151"),
        ("read_only", "0"),
        ("super_read_only", "0"),
        ("tls_version", ""),
        ("ssl_cipher", ""),
        ("last_insert_id", "0"),
        ("identity", "0"),
        ("event_scheduler", "OFF"),
        ("binlog_format", "ROW"),
        ("log_bin", "0"),
        ("server_id", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .chain([("pseudo_thread_id".to_owned(), connection_id.to_string())])
    .collect()
}

pub(crate) fn sqlite_err(e: rusqlite::Error) -> EngineError {
    if let rusqlite::Error::SqliteFailure(_, Some(msg)) = &e
        && let Some(inner) = EngineError::from_function_message(msg)
    {
        return inner;
    }
    if let rusqlite::Error::UserFunctionError(inner) = &e
        && let Some(x) = EngineError::from_function_message(&inner.to_string())
    {
        return x;
    }
    EngineError::internal(e.to_string())
}

impl Session {
    pub(crate) fn open(engine: Engine, database: Option<&str>, user: &str) -> Result<Session> {
        let dialect = engine.dialect();
        let conn = Connection::open_in_memory().map_err(sqlite_err)?;
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML, false).map_err(sqlite_err)?;
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DDL, false).map_err(sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(sqlite_err)?;
        let mut pragmas = String::from("PRAGMA foreign_keys = ON; PRAGMA recursive_triggers = ON;");
        if dialect == Dialect::Postgres {
            pragmas.push_str("PRAGMA case_sensitive_like = ON;");
        }
        conn.execute_batch(&pragmas).map_err(sqlite_err)?;
        let connection_id = engine.next_connection_id();
        let (database, db_dir) = match dialect {
            Dialect::MySql => {
                let db = database.unwrap_or("").to_owned();
                if !db.is_empty() {
                    let exists = list_schema_files(engine.root()).iter().any(|(n, _)| n.eq_ignore_ascii_case(&db));
                    if !exists {
                        return Err(EngineError::new(state::INVALID_CATALOG_NAME, format!("Unknown database '{db}'")).with_mysql(1049));
                    }
                }
                (db, engine.root().to_path_buf())
            }
            _ => {
                let db = database.filter(|d| !d.is_empty()).unwrap_or(user).to_owned();
                // A SQL Server mock serves one database whatever name the client asks for (USE
                // changes nothing); the name is still what DB_NAME() reports.
                let dir = engine.database_dir(if dialect == Dialect::MsSql { "sqlserver" } else { &db });
                if !dir.exists() {
                    if engine.inner.options.auto_create_databases {
                        std::fs::create_dir_all(&dir).map_err(|e| EngineError::internal(e.to_string()))?;
                    } else {
                        return Err(EngineError::new(state::INVALID_CATALOG_NAME, format!("database \"{db}\" does not exist")));
                    }
                }
                (db, dir)
            }
        };
        let mut settings = match dialect {
            Dialect::MySql => mysql_defaults(&engine.inner.version, connection_id),
            _ => pg_defaults(user, &database),
        };
        settings.insert("server_version_full".into(), engine.inner.version.clone());
        let defaults = settings.clone();
        let shared = Arc::new(Mutex::new(SessionShared {
            settings,
            last_insert_id: 0,
            lastval: None,
            currvals: HashMap::new(),
            search_path: Vec::new(),
            database: database.clone(),
            user: user.to_owned(),
            connection_id,
            found_rows: 0,
            row_count: -1,
        }));
        let catalog = Arc::new(RwLock::new(Arc::new(Catalog::empty(dialect, &database))));
        let env = Arc::new(FnEnv {
            dialect,
            catalog: catalog.clone(),
            sequences: engine.inner.sequences.clone(),
            shared: shared.clone(),
            version: engine.inner.version.clone(),
        });
        functions::register(&conn, env).map_err(sqlite_err)?;
        rusqlite::vtab::series::load_module(&conn).map_err(sqlite_err)?;
        if dialect == Dialect::Postgres {
            conn.execute_batch("ATTACH DATABASE ':memory:' AS \"pg_catalog\"").map_err(sqlite_err)?;
        }
        conn.execute_batch("ATTACH DATABASE ':memory:' AS \"information_schema\"").map_err(sqlite_err)?;
        if dialect == Dialect::MsSql {
            conn.execute_batch("ATTACH DATABASE ':memory:' AS \"sys\"").map_err(sqlite_err)?;
        }
        if dialect == Dialect::MySql {
            conn.execute_batch("CREATE TEMP VIEW dual AS SELECT 'X' AS dummy").map_err(sqlite_err)?;
        }
        let mut session = Session {
            engine,
            conn,
            dialect,
            catalog,
            shared,
            tx: TxState::Idle,
            attached: Vec::new(),
            db_dir,
            database,
            seen_catalog_gen: 0,
            seen_layout_gen: 0,
            local_gen: 0,
            pg_catalog_gen: u64::MAX,
            info_gen: u64::MAX,
            catalog_types: CatalogTypes::new(),
            autocommit: true,
            ddl_in_tx: false,
            read_only_tx: false,
            defaults,
        };
        if dialect != Dialect::MySql {
            session.ensure_schema_file(dialect.default_schema())?;
        }
        session.sync_layout()?;
        session.reload_catalog()?;
        session.update_search_path();
        session.seen_catalog_gen = session.engine.inner.catalog_gen.load(Ordering::SeqCst);
        Ok(session)
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    pub fn connection_id(&self) -> u32 {
        self.shared.lock().connection_id
    }

    pub fn tx_state(&self) -> TxState {
        self.tx
    }

    pub fn in_transaction(&self) -> bool {
        self.tx != TxState::Idle
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().clone()
    }

    pub fn setting(&self, name: &str) -> Option<String> {
        self.shared.lock().settings.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn search_path(&self) -> Vec<String> {
        self.shared.lock().search_path.clone()
    }

    pub fn last_insert_id(&self) -> i64 {
        self.shared.lock().last_insert_id
    }

    // -----------------------------------------------------------------------------------------
    // Layout and catalog
    // -----------------------------------------------------------------------------------------

    fn schema_path(&self, name: &str) -> PathBuf {
        self.db_dir.join(format!("{}.sqlite", encode_name(name)))
    }

    /// Create (if needed) and attach a schema file.
    pub(crate) fn ensure_schema_file(&mut self, name: &str) -> Result<()> {
        if self.attached.iter().any(|a| a == name) {
            return Ok(());
        }
        let path = self.schema_path(name);
        let created = !path.exists();
        self.attach(name, &path)?;
        if created {
            self.engine.inner.layout_gen.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn attach(&mut self, name: &str, path: &std::path::Path) -> Result<()> {
        let q = quote_ident(name);
        self.conn
            .execute(&format!("ATTACH DATABASE ?1 AS {q}"), [path.to_string_lossy().as_ref()])
            .map_err(|e| EngineError::internal(format!("cannot attach {name}: {e}")))?;
        // WAL lets other connections read while one writes. It cannot be changed inside a transaction.
        if self.tx == TxState::Idle {
            let _ = self.conn.query_row(&format!("PRAGMA {q}.journal_mode = WAL"), [], |_| Ok(()));
        }
        self.conn.execute_batch(&META_DDL.replace("{s}", &q)).map_err(sqlite_err)?;
        self.attached.push(name.to_owned());
        Ok(())
    }

    /// Attach schema files created by other sessions and detach removed ones.
    fn sync_layout(&mut self) -> Result<()> {
        let layout_now = self.engine.inner.layout_gen.load(Ordering::SeqCst);
        if layout_now == self.seen_layout_gen {
            return Ok(());
        }
        let files = list_schema_files(&self.db_dir);
        let mut changed = false;
        for (name, path) in &files {
            if !self.attached.contains(name) {
                self.attach(name, path)?;
                changed = true;
            }
        }
        if self.tx == TxState::Idle {
            let gone: Vec<String> = self.attached.iter().filter(|a| !files.iter().any(|(n, _)| n == *a)).cloned().collect();
            for g in gone {
                let _ = self.conn.execute_batch(&format!("DETACH DATABASE {}", quote_ident(&g)));
                self.attached.retain(|a| a != &g);
                changed = true;
            }
        }
        self.seen_layout_gen = layout_now;
        if changed {
            self.reload_catalog()?;
        }
        Ok(())
    }

    pub(crate) fn reload_catalog(&mut self) -> Result<()> {
        let cat = Catalog::load(&self.conn, self.dialect, &self.database, &self.attached)?;
        for sd in cat.schemas.values() {
            for seq in sd.sequences.values() {
                self.engine.inner.sequences.ensure(
                    &format!("{}.{}", sd.name, seq.name),
                    SeqState {
                        last_value: seq.last_value,
                        is_called: seq.is_called,
                        increment: seq.increment,
                        start_value: seq.start_value,
                        min_value: seq.min_value,
                        max_value: seq.max_value,
                    },
                );
            }
        }
        *self.catalog.write() = Arc::new(cat);
        self.local_gen += 1;
        self.update_search_path();
        Ok(())
    }

    /// Bring the session up to date with changes committed by other sessions.
    fn sync_state(&mut self) -> Result<()> {
        self.sync_layout()?;
        let catalog_now = self.engine.inner.catalog_gen.load(Ordering::SeqCst);
        if catalog_now != self.seen_catalog_gen {
            self.reload_catalog()?;
            self.seen_catalog_gen = catalog_now;
        }
        Ok(())
    }

    /// Tell other sessions the committed catalog changed.
    pub(crate) fn publish_catalog_change(&mut self) {
        let new = self.engine.inner.catalog_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen_catalog_gen = new;
    }

    pub(crate) fn update_search_path(&self) {
        let mut sh = self.shared.lock();
        let path: Vec<String> = match self.dialect {
            Dialect::MySql => {
                if self.database.is_empty() {
                    vec![]
                } else {
                    let actual = self
                        .attached
                        .iter()
                        .find(|a| a.eq_ignore_ascii_case(&self.database))
                        .cloned()
                        .unwrap_or_else(|| self.database.clone());
                    vec![actual]
                }
            }
            // T-SQL resolves unqualified names in the user's default schema.
            Dialect::MsSql => vec!["dbo".to_owned()].into_iter().filter(|p| self.attached.contains(p)).collect(),
            _ => {
                let raw = sh.settings.get("search_path").cloned().unwrap_or_default();
                let user = sh.user.clone();
                raw.split(',')
                    .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_owned())
                    .map(|p| if p == "$user" { user.clone() } else { p })
                    .filter(|p| self.attached.contains(p))
                    .collect()
            }
        };
        sh.search_path = path;
    }

    fn ensure_system_catalogs(&mut self, pg: bool, info: bool) -> Result<()> {
        // The emulated catalogs are rebuilt in memory, also inside read-only transactions.
        if self.read_only_tx {
            self.set_query_only(false)?;
            let r = self.ensure_system_catalogs_inner(pg, info);
            self.set_query_only(true)?;
            return r;
        }
        self.ensure_system_catalogs_inner(pg, info)
    }

    fn ensure_system_catalogs_inner(&mut self, pg: bool, info: bool) -> Result<()> {
        if pg && self.dialect == Dialect::Postgres && self.pg_catalog_gen != self.local_gen {
            let cat = self.catalog();
            let settings: Vec<(String, String)> = {
                let sh = self.shared.lock();
                let mut v: Vec<(String, String)> =
                    sh.settings.iter().filter(|(k, _)| !k.starts_with('@')).map(|(k, v)| (k.clone(), v.clone())).collect();
                v.sort();
                v
            };
            catalog::populate_pg_catalog(&self.conn, &cat, &settings, &mut self.catalog_types)?;
            self.pg_catalog_gen = self.local_gen;
        }
        if info && self.info_gen != self.local_gen {
            let cat = self.catalog();
            catalog::populate_information_schema(&self.conn, &cat, &mut self.catalog_types)?;
            if self.dialect == Dialect::MsSql {
                catalog::populate_mssql_sys(&self.conn, &cat, &mut self.catalog_types)?;
            }
            self.info_gen = self.local_gen;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Sequences
    // -----------------------------------------------------------------------------------------

    pub(crate) fn flush_sequences(&mut self) -> Result<()> {
        // Inside a read-only transaction the rows are written later (sequences are not transactional).
        if self.read_only_tx {
            return Ok(());
        }
        for (key, s) in self.engine.inner.sequences.take_dirty() {
            let Some((schema, name)) = key.split_once('.') else { continue };
            if !self.attached.iter().any(|a| a == schema) {
                continue;
            }
            self.conn
                .execute(
                    &format!(
                        "INSERT INTO {}._tb_seq (name, last_value, is_called, increment, start_value, min_value, max_value) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                         ON CONFLICT(name) DO UPDATE SET last_value = excluded.last_value, is_called = excluded.is_called, increment = excluded.increment, \
                         start_value = excluded.start_value, min_value = excluded.min_value, max_value = excluded.max_value",
                        quote_ident(schema)
                    ),
                    rusqlite::params![name, s.last_value, s.is_called as i64, s.increment, s.start_value, s.min_value, s.max_value],
                )
                .map_err(sqlite_err)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------------------------------

    fn ctx_parts(&self) -> (Arc<Catalog>, Vec<String>, HashMap<String, String>) {
        let sh = self.shared.lock();
        (self.catalog(), sh.search_path.clone(), sh.settings.clone())
    }

    fn translate(&self, sql: &str) -> Result<Plan> {
        let (cat, path, settings) = self.ctx_parts();
        let ctx = Ctx { dialect: self.dialect, catalog: &cat, search_path: &path, settings: &settings };
        translate::translate(sql, &ctx)
    }

    /// Execute a batch of statements (simple query protocol / `COM_QUERY`).
    pub fn execute(&mut self, sql: &str) -> BatchResult {
        if self.dialect == Dialect::MsSql {
            return self.execute_tsql(sql);
        }
        let statements = translate::split_statements(sql, self.dialect);
        let mut outcomes = Vec::new();
        let tx_keywords = ["BEGIN", "START", "COMMIT", "END", "ROLLBACK", "ABORT", "SAVEPOINT", "RELEASE"];
        let implicit = self.dialect == Dialect::Postgres
            && self.tx == TxState::Idle
            && statements.len() > 1
            && !statements.iter().any(|s| {
                let first = s.trim_start().split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_ascii_uppercase();
                tx_keywords.contains(&first.as_str())
            });
        if implicit && let Err(e) = self.begin_internal() {
            return BatchResult { outcomes, error: Some(e) };
        }
        for stmt in statements {
            match self.execute_one(&stmt, &[], true) {
                Ok(o) => outcomes.push(o),
                Err(e) => {
                    if implicit {
                        let _ = self.rollback_internal();
                    }
                    return BatchResult { outcomes, error: Some(e) };
                }
            }
        }
        if implicit && let Err(e) = self.commit_internal() {
            return BatchResult { outcomes, error: Some(e) };
        }
        BatchResult { outcomes, error: None }
    }

    /// Execute exactly one statement and return its outcome (errors abort a PostgreSQL transaction).
    pub fn execute_single(&mut self, sql: &str, params: &[Param]) -> Result<Outcome> {
        let statements = translate::split_statements(sql, self.dialect);
        match statements.len() {
            0 => Ok(Outcome::command("")),
            1 => self.execute_one(&statements[0], params, true),
            _ => {
                let mut last = Outcome::command("");
                for s in statements {
                    last = self.execute_one(&s, params, true)?;
                }
                Ok(last)
            }
        }
    }

    pub(crate) fn execute_one(&mut self, sql: &str, params: &[Param], runtime_typing: bool) -> Result<Outcome> {
        let r = self.execute_one_inner(sql, params, runtime_typing);
        self.after_statement(&r);
        r
    }

    fn after_statement<T>(&mut self, r: &Result<T>) {
        if r.is_err() && self.dialect == Dialect::Postgres && self.tx == TxState::Active {
            self.tx = TxState::Failed;
        }
    }

    fn execute_one_inner(&mut self, sql: &str, params: &[Param], runtime_typing: bool) -> Result<Outcome> {
        self.sync_state()?;
        let plan = self.translate(sql)?;
        self.run_plan(plan, params, runtime_typing, None)
    }

    /// Prepare a single statement (extended protocol / `COM_STMT_PREPARE`).
    pub fn prepare(&mut self, sql: &str) -> Result<Prepared> {
        let r = self.prepare_inner(sql);
        self.after_statement(&r);
        r
    }

    fn prepare_inner(&mut self, sql: &str) -> Result<Prepared> {
        self.sync_state()?;
        let statements = translate::split_statements(sql, self.dialect);
        if statements.len() > 1 {
            return Err(EngineError::syntax("cannot insert multiple commands into a prepared statement"));
        }
        let text = statements.into_iter().next().unwrap_or_default();
        if text.is_empty() {
            return Ok(Prepared {
                sql: String::new(),
                plan: Plan::Noop { tag: String::new() },
                generation: self.local_gen,
                param_types: vec![],
                columns: vec![],
            });
        }
        if self.tx == TxState::Failed {
            return Err(failed_tx());
        }
        let plan = self.translate(&text)?;
        let (param_types, columns) = self.describe_plan(&plan)?;
        Ok(Prepared { sql: text, plan, generation: self.local_gen, param_types, columns })
    }

    fn describe_plan(&mut self, plan: &Plan) -> Result<(Vec<SqlType>, Vec<OutColumn>)> {
        match plan {
            Plan::Sql(p) => {
                self.ensure_system_catalogs(p.needs_pg_catalog, p.needs_info_schema)?;
                let stmt = self.conn.prepare(&p.sql).map_err(|e| self.map_sqlite(e, Some(p)))?;
                let count = stmt.parameter_count().max(p.param_count);
                let params = (1..=count).map(|i| p.params.get(&i).cloned().unwrap_or(SqlType::Text)).collect();
                let columns =
                    self.describe_stmt(&stmt, p).into_iter().map(|(c, t)| OutColumn { ty: t.unwrap_or(SqlType::Text), ..c }).collect();
                Ok((params, columns))
            }
            Plan::Rows(r) => Ok((vec![], r.columns.iter().map(|(n, t)| simple_column(n, t.clone())).collect())),
            Plan::Show { name } => Ok((vec![], vec![simple_column(name, SqlType::Text)])),
            Plan::Explain(_) => Ok((vec![], self.explain_columns())),
            _ => Ok((vec![], vec![])),
        }
    }

    /// Execute a prepared statement with bound parameters.
    pub fn execute_prepared(&mut self, prepared: &Prepared, params: &[Param]) -> Result<Outcome> {
        let r = self.execute_prepared_inner(prepared, params);
        self.after_statement(&r);
        r
    }

    fn execute_prepared_inner(&mut self, prepared: &Prepared, params: &[Param]) -> Result<Outcome> {
        self.sync_state()?;
        let plan = if prepared.generation == self.local_gen { prepared.plan.clone() } else { self.translate(&prepared.sql)? };
        let columns = if prepared.columns.is_empty() { None } else { Some(prepared.columns.clone()) };
        self.run_plan(plan, params, false, columns)
    }

    fn explain_columns(&self) -> Vec<OutColumn> {
        match self.dialect {
            Dialect::MySql => {
                ["id", "select_type", "table", "partitions", "type", "possible_keys", "key", "key_len", "ref", "rows", "filtered", "Extra"]
                    .iter()
                    .map(|n| simple_column(n, SqlType::Text))
                    .collect()
            }
            _ => vec![simple_column("QUERY PLAN", SqlType::Text)],
        }
    }

    fn run_plan(&mut self, plan: Plan, params: &[Param], runtime_typing: bool, described: Option<Vec<OutColumn>>) -> Result<Outcome> {
        if self.tx == TxState::Failed && !matches!(plan, Plan::Rollback | Plan::Commit | Plan::RollbackTo(_)) {
            return Err(failed_tx());
        }
        // MySQL with autocommit off starts transactions implicitly.
        if self.dialect == Dialect::MySql && !self.autocommit && self.tx == TxState::Idle && matches!(plan, Plan::Sql(_) | Plan::Raw { .. })
        {
            self.begin_internal()?;
        }
        match plan {
            Plan::Sql(p) => self.run_sql(&p, params, runtime_typing, described),
            Plan::Raw { sql, parse_error } => {
                if self.conn.prepare(&sql).is_err() {
                    return Err(parse_error);
                }
                let p = SqlPlan::new(sql, StmtKind::Other, "SELECT");
                self.run_sql(&p, params, runtime_typing, described)
            }
            Plan::Begin { read_only } => {
                match self.tx {
                    TxState::Idle => self.begin_internal()?,
                    _ if self.dialect == Dialect::MySql => {
                        self.commit_internal()?;
                        self.begin_internal()?;
                    }
                    _ => {}
                }
                if read_only && self.tx == TxState::Active {
                    self.set_query_only(true)?;
                    self.read_only_tx = true;
                }
                Ok(Outcome { tx: TxEffect::Begin, ..Outcome::command("BEGIN") })
            }
            Plan::Commit => {
                let tag = if self.tx == TxState::Failed { "ROLLBACK" } else { "COMMIT" };
                match self.tx {
                    TxState::Failed => self.rollback_internal()?,
                    TxState::Active => self.commit_internal()?,
                    TxState::Idle => {}
                }
                Ok(Outcome { tx: TxEffect::End, ..Outcome::command(tag) })
            }
            Plan::Rollback => {
                if self.tx != TxState::Idle {
                    self.rollback_internal()?;
                }
                Ok(Outcome { tx: TxEffect::End, ..Outcome::command("ROLLBACK") })
            }
            Plan::Savepoint(name) => {
                if self.tx == TxState::Idle {
                    if self.dialect == Dialect::Postgres {
                        return Err(EngineError::new(state::NO_ACTIVE_TRANSACTION, "SAVEPOINT can only be used in transaction blocks"));
                    }
                    self.begin_internal()?;
                }
                self.conn.execute_batch(&format!("SAVEPOINT {}", quote_ident(&name))).map_err(sqlite_err)?;
                Ok(Outcome::command("SAVEPOINT"))
            }
            Plan::Release(name) => {
                if self.tx == TxState::Idle {
                    return Err(EngineError::new(state::NO_ACTIVE_TRANSACTION, "RELEASE SAVEPOINT can only be used in transaction blocks"));
                }
                self.conn.execute_batch(&format!("RELEASE {}", quote_ident(&name))).map_err(|_| {
                    EngineError::new(state::INVALID_SAVEPOINT, format!("savepoint \"{name}\" does not exist")).with_mysql(1305)
                })?;
                Ok(Outcome::command("RELEASE"))
            }
            Plan::RollbackTo(name) => {
                if self.tx == TxState::Idle {
                    return Err(EngineError::new(
                        state::NO_ACTIVE_TRANSACTION,
                        "ROLLBACK TO SAVEPOINT can only be used in transaction blocks",
                    ));
                }
                self.conn.execute_batch(&format!("ROLLBACK TO {}", quote_ident(&name))).map_err(|_| {
                    EngineError::new(state::INVALID_SAVEPOINT, format!("savepoint \"{name}\" does not exist")).with_mysql(1305)
                })?;
                self.tx = TxState::Active;
                self.reload_catalog()?;
                Ok(Outcome::command("ROLLBACK"))
            }
            Plan::Set { vars } => {
                for (name, value) in vars {
                    self.apply_setting(&name, value)?;
                }
                Ok(Outcome::command("SET"))
            }
            Plan::Show { name } => self.show(&name),
            Plan::Use { database } => {
                let found = self.attached.iter().find(|a| a.eq_ignore_ascii_case(&database)).cloned();
                match found {
                    Some(db) => {
                        self.database = db.clone();
                        self.shared.lock().database = db;
                        self.update_search_path();
                        Ok(Outcome::command("USE"))
                    }
                    None => Err(EngineError::new(state::INVALID_CATALOG_NAME, format!("Unknown database '{database}'")).with_mysql(1049)),
                }
            }
            Plan::Explain(p) => self.explain(&p, params),
            Plan::Rows(r) => {
                let n = r.rows.len() as u64;
                let columns = r.columns.into_iter().map(|(name, ty)| simple_column(&name, ty)).collect();
                Ok(Outcome {
                    tag: "SELECT".into(),
                    affected: n,
                    result: Some(ResultSet { columns, rows: r.rows }),
                    last_insert_id: None,
                    tx: TxEffect::None,
                    message: None,
                })
            }
            Plan::Noop { tag } => Ok(Outcome::command(&tag)),
            other => self.run_ddl(other),
        }
    }

    fn apply_setting(&mut self, name: &str, value: Option<String>) -> Result<()> {
        let key = if name.starts_with('@') { name.to_owned() } else { name.to_ascii_lowercase() };
        let value = match value {
            Some(v) => Some(v),
            None => self.defaults.get(&key).cloned(),
        };
        if self.dialect == Dialect::MySql {
            match key.as_str() {
                "autocommit" => {
                    let on = value.as_deref().is_none_or(|v| v == "1" || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true"));
                    if on && !self.autocommit && self.tx == TxState::Active {
                        self.commit_internal()?;
                    }
                    self.autocommit = on;
                }
                "foreign_key_checks" => {
                    let on = value.as_deref().is_none_or(|v| v != "0" && !v.eq_ignore_ascii_case("off"));
                    let _ = self.conn.execute_batch(if on { "PRAGMA foreign_keys = ON" } else { "PRAGMA foreign_keys = OFF" });
                }
                _ => {}
            }
        }
        let mut sh = self.shared.lock();
        match value {
            Some(v) => {
                let v = v.trim_matches('\'').to_owned();
                if key == "transaction_isolation" || key == "tx_isolation" {
                    sh.settings.insert("transaction_isolation".into(), v.clone());
                    sh.settings.insert("tx_isolation".into(), v);
                } else {
                    sh.settings.insert(key.clone(), v);
                }
            }
            None => {
                sh.settings.remove(&key);
            }
        }
        drop(sh);
        if key == "search_path" {
            self.update_search_path();
        }
        Ok(())
    }

    fn show(&mut self, name: &str) -> Result<Outcome> {
        let sh = self.shared.lock();
        let (columns, rows) = if name == "all" {
            let mut items: Vec<(&String, &String)> = sh.settings.iter().filter(|(k, _)| !k.starts_with('@')).collect();
            items.sort();
            (
                vec![
                    simple_column("name", SqlType::Text),
                    simple_column("setting", SqlType::Text),
                    simple_column("description", SqlType::Text),
                ],
                items.into_iter().map(|(k, v)| vec![Value::Text(k.clone()), Value::Text(v.clone()), Value::Text(String::new())]).collect(),
            )
        } else {
            let value =
                sh.settings.get(&name.to_ascii_lowercase()).cloned().ok_or_else(|| {
                    EngineError::new(state::UNDEFINED_PARAMETER, format!("unrecognized configuration parameter \"{name}\""))
                })?;
            (vec![simple_column(name, SqlType::Text)], vec![vec![Value::Text(value)]])
        };
        let n = rows.len() as u64;
        Ok(Outcome {
            tag: "SHOW".into(),
            affected: n,
            result: Some(ResultSet { columns, rows }),
            last_insert_id: None,
            tx: TxEffect::None,
            message: None,
        })
    }

    fn explain(&mut self, p: &SqlPlan, params: &[Param]) -> Result<Outcome> {
        self.ensure_system_catalogs(p.needs_pg_catalog, p.needs_info_schema)?;
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {}", p.sql)).map_err(|e| self.map_sqlite(e, Some(p)))?;
        let count = stmt.parameter_count();
        for i in 1..=count {
            stmt.raw_bind_parameter(i, params.get(i - 1).cloned().unwrap_or(Param::Null)).map_err(sqlite_err)?;
        }
        let mut entries: Vec<(i64, i64, String)> = Vec::new();
        let mut rows = stmt.raw_query();
        while let Some(r) = rows.next().map_err(sqlite_err)? {
            entries.push((r.get(0).map_err(sqlite_err)?, r.get(1).map_err(sqlite_err)?, r.get(3).map_err(sqlite_err)?));
        }
        let depth_of = |parent: i64, entries: &[(i64, i64, String)]| {
            let mut d = 0;
            let mut cur = parent;
            while cur != 0 {
                d += 1;
                cur = entries.iter().find(|e| e.0 == cur).map(|e| e.1).unwrap_or(0);
            }
            d
        };
        let columns = self.explain_columns();
        let out_rows: Vec<Vec<Value>> = match self.dialect {
            Dialect::MySql => entries
                .iter()
                .enumerate()
                .map(|(i, (_, _, detail))| {
                    let mut row = vec![Value::Text((i + 1).to_string()), Value::Text("SIMPLE".into())];
                    row.extend(std::iter::repeat_n(Value::Null, 9));
                    row.push(Value::Text(detail.clone()));
                    row
                })
                .collect(),
            _ => entries
                .iter()
                .map(|(_, parent, detail)| {
                    let d = depth_of(*parent, &entries);
                    let prefix = if d == 0 { String::new() } else { format!("{}->  ", "  ".repeat(d * 2 - 1)) };
                    vec![Value::Text(format!("{prefix}{detail}"))]
                })
                .collect(),
        };
        let n = out_rows.len() as u64;
        Ok(Outcome {
            tag: "EXPLAIN".into(),
            affected: n,
            result: Some(ResultSet { columns, rows: out_rows }),
            last_insert_id: None,
            tx: TxEffect::None,
            message: None,
        })
    }

    /// Column names and types for a prepared SQLite statement.
    fn describe_stmt(&self, stmt: &rusqlite::Statement<'_>, plan: &SqlPlan) -> Vec<(OutColumn, Option<SqlType>)> {
        let cat = self.catalog();
        let mut out = Vec::new();
        for (i, meta) in stmt.columns_with_metadata().enumerate() {
            let name = meta.name().to_owned();
            let hint = plan.projection.get(i).cloned().flatten();
            let mut ty = None;
            let mut source = None;
            let mut table_oid_v = None;
            let mut column_index = None;
            if let (Some(db), Some(table), Some(origin)) = (meta.database_name(), meta.table_name(), meta.origin_name()) {
                if db == "pg_catalog" || db == "information_schema" || db == "sys" {
                    ty = self.catalog_types.get(&(table.to_ascii_lowercase(), origin.to_ascii_lowercase())).cloned();
                } else if let Some(td) = cat.table(db, table)
                    && let Some(idx) = td.column_index(origin)
                {
                    ty = Some(td.columns[idx].ty.clone());
                    source = Some((db.to_owned(), td.name.clone()));
                    table_oid_v = Some(table_oid(db, &td.name));
                    column_index = Some((idx + 1) as i16);
                }
            }
            let ty = match (ty, hint) {
                (Some(t), _) => Some(t),
                (None, h) => h,
            };
            out.push((OutColumn { name, ty: SqlType::Text, source, table_oid: table_oid_v, column_index }, ty));
        }
        out
    }

    pub(crate) fn run_sql(
        &mut self,
        p: &SqlPlan,
        params: &[Param],
        runtime_typing: bool,
        described: Option<Vec<OutColumn>>,
    ) -> Result<Outcome> {
        self.ensure_system_catalogs(p.needs_pg_catalog, p.needs_info_schema)?;
        let result = self.run_sql_inner(p, params, runtime_typing, described);
        // Sequences are non-transactional; persist them even when the statement failed.
        let flushed = self.flush_sequences();
        let outcome = result?;
        flushed?;
        Ok(outcome)
    }

    fn run_sql_inner(&mut self, p: &SqlPlan, params: &[Param], runtime_typing: bool, described: Option<Vec<OutColumn>>) -> Result<Outcome> {
        let mut stmt = self.conn.prepare(&p.sql).map_err(|e| self.map_sqlite(e, Some(p)))?;
        let count = stmt.parameter_count();
        for i in 1..=count {
            let param = params.get(i - 1).cloned().unwrap_or(Param::Null);
            let param = coerce_param(param, p.params.get(&i));
            stmt.raw_bind_parameter(i, param).map_err(sqlite_err)?;
        }
        let ncols = stmt.column_count();
        let mut last_insert_id = None;
        if ncols > 0 {
            let described_cols = self.describe_stmt(&stmt, p);
            let mut rows: Vec<Vec<Value>> = Vec::new();
            {
                let mut q = stmt.raw_query();
                loop {
                    match q.next() {
                        Ok(Some(r)) => {
                            let mut row = Vec::with_capacity(ncols);
                            for c in 0..ncols {
                                row.push(r.get_ref(c).map(owned).unwrap_or(Value::Null));
                            }
                            rows.push(row);
                        }
                        Ok(None) => break,
                        Err(e) => return Err(self.map_sqlite(e, Some(p))),
                    }
                }
            }
            let columns: Vec<OutColumn> = match described {
                Some(cols) if cols.len() == ncols => cols,
                _ => described_cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, (c, t))| {
                        let ty =
                            t.unwrap_or_else(|| if runtime_typing { infer_runtime(rows.iter().map(|r| &r[i])) } else { SqlType::Text });
                        OutColumn { ty, ..c }
                    })
                    .collect(),
            };
            let n = rows.len() as u64;
            drop(stmt);
            self.render_regclass(&columns, &mut rows);
            if p.kind == StmtKind::Insert {
                last_insert_id = self.after_insert(p)?;
            }
            let tag = match p.kind {
                StmtKind::Insert => "INSERT",
                StmtKind::Update => "UPDATE",
                StmtKind::Delete => "DELETE",
                _ => "SELECT",
            };
            {
                let mut sh = self.shared.lock();
                sh.found_rows = n as i64;
                sh.row_count = if p.kind == StmtKind::Select { -1 } else { n as i64 };
            }
            return Ok(Outcome {
                tag: tag.into(),
                affected: n,
                result: Some(ResultSet { columns, rows }),
                last_insert_id,
                tx: TxEffect::None,
                message: None,
            });
        }
        let changed = stmt.raw_execute().map_err(|e| self.map_sqlite(e, Some(p)))?;
        drop(stmt);
        if p.kind == StmtKind::Insert {
            last_insert_id = self.after_insert(p)?;
        }
        let affected = match p.kind {
            StmtKind::Insert | StmtKind::Update | StmtKind::Delete => changed as u64,
            _ => 0,
        };
        self.shared.lock().row_count = affected as i64;
        let tag = match p.kind {
            StmtKind::Insert => "INSERT",
            StmtKind::Update => "UPDATE",
            StmtKind::Delete => "DELETE",
            StmtKind::Select => "SELECT",
            StmtKind::Other => p.tag.as_str(),
        };
        Ok(Outcome { tag: tag.to_owned(), affected, result: None, last_insert_id, tx: TxEffect::None, message: None })
    }

    /// PostgreSQL prints `regclass` values as relation names.
    fn render_regclass(&self, columns: &[OutColumn], rows: &mut [Vec<Value>]) {
        let idx: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(&c.ty, SqlType::Other { name } if name == "regclass"))
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            return;
        }
        let cat = self.catalog();
        let path = self.search_path();
        let mut names = HashMap::new();
        for sd in cat.schemas.values() {
            for t in sd.tables.keys().chain(sd.views.keys()) {
                let shown = if path.first() == Some(&sd.name) { t.clone() } else { format!("{}.{t}", sd.name) };
                names.insert(table_oid(&sd.name, t) as i64, shown);
            }
        }
        for row in rows.iter_mut() {
            for &i in &idx {
                if let Value::Integer(oid) = row[i]
                    && let Some(n) = names.get(&oid)
                {
                    row[i] = Value::Text(n.clone());
                }
            }
        }
    }

    /// MySQL: move AUTO_INCREMENT past explicitly inserted ids; report LAST_INSERT_ID.
    fn after_insert(&mut self, p: &SqlPlan) -> Result<Option<i64>> {
        if let Some((key, schema, table, column)) = &p.autoinc {
            let max: Option<i64> = self
                .conn
                .query_row(&format!("SELECT max({}) FROM {}.{}", quote_ident(column), quote_ident(schema), quote_ident(table)), [], |r| {
                    r.get(0)
                })
                .map_err(sqlite_err)?;
            if let Some(m) = max {
                self.engine.inner.sequences.bump_to(key, m);
            }
            return Ok(Some(self.shared.lock().last_insert_id));
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------------------------
    // Transactions
    // -----------------------------------------------------------------------------------------

    pub(crate) fn begin_internal(&mut self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| self.map_sqlite(e, None))?;
        self.tx = TxState::Active;
        self.ddl_in_tx = false;
        Ok(())
    }

    /// SQLite refuses every change while `query_only` is on.
    fn set_query_only(&self, on: bool) -> Result<()> {
        self.conn.pragma_update(None, "query_only", on).map_err(sqlite_err)
    }

    /// Leave a read-only transaction's mode (on commit or rollback).
    fn end_read_only(&mut self) -> Result<()> {
        if self.read_only_tx {
            self.read_only_tx = false;
            self.set_query_only(false)?;
        }
        Ok(())
    }

    pub(crate) fn commit_internal(&mut self) -> Result<()> {
        self.end_read_only()?;
        self.flush_sequences()?;
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("COMMIT").map_err(|e| self.map_sqlite(e, None))?;
        }
        self.tx = TxState::Idle;
        if self.ddl_in_tx {
            self.ddl_in_tx = false;
            self.publish_catalog_change();
        }
        Ok(())
    }

    pub(crate) fn rollback_internal(&mut self) -> Result<()> {
        self.end_read_only()?;
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK").map_err(sqlite_err)?;
        }
        self.tx = TxState::Idle;
        let had_ddl = self.ddl_in_tx;
        self.ddl_in_tx = false;
        // Rolled-back sequence rows are rewritten on the next flush.
        let cat = self.catalog();
        for sd in cat.schemas.values() {
            for name in sd.sequences.keys() {
                let key = format!("{}.{name}", sd.name);
                if let Some(s) = self.engine.inner.sequences.get(&key) {
                    self.engine.inner.sequences.replace(&key, s);
                }
            }
        }
        if had_ddl {
            self.reload_catalog()?;
        } else {
            // System catalogs live in attached memory databases and were rolled back too.
            self.local_gen += 1;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Error mapping
    // -----------------------------------------------------------------------------------------

    pub(crate) fn map_sqlite(&self, e: rusqlite::Error, plan: Option<&SqlPlan>) -> EngineError {
        let my = self.dialect == Dialect::MySql;
        let msg = match &e {
            rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
            other => other.to_string(),
        };
        if let Some(inner) = EngineError::from_function_message(&msg) {
            return inner;
        }
        if self.read_only_tx && matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ReadOnly) {
            let what = plan.map(|p| p.tag.as_str()).filter(|t| !t.is_empty()).unwrap_or("this statement");
            return if my {
                EngineError::new("25006", "Cannot execute statement in a READ ONLY transaction.").with_mysql(1792)
            } else {
                EngineError::new("25006", format!("cannot execute {what} in a read-only transaction"))
            };
        }
        // Recent SQLite versions append " in <sql> at offset N" to some messages.
        let msg = match msg.find(" in ") {
            Some(pos) if msg.contains(" at offset ") && (msg.starts_with("no such") || msg.starts_with("ambiguous")) => {
                msg[..pos].to_owned()
            }
            _ => msg,
        };
        let cat = self.catalog();
        let find_table = |name: &str| -> Option<(String, crate::catalog::TableDef)> {
            if let Some((s, t)) = plan.and_then(|p| p.target.clone())
                && t.eq_ignore_ascii_case(name)
                && let Some(td) = cat.table(&s, &t)
            {
                return Some((s, td.clone()));
            }
            for sd in cat.schemas.values() {
                if let Some(td) = cat.table(&sd.name, name) {
                    return Some((sd.name.clone(), td.clone()));
                }
            }
            None
        };
        if let Some(rest) = msg.strip_prefix("UNIQUE constraint failed: ") {
            let cols: Vec<String> = rest.split(", ").map(|c| c.split('.').next_back().unwrap_or(c).to_owned()).collect();
            let table = rest.split('.').next().unwrap_or("").to_owned();
            let td = find_table(&table);
            let name = td
                .as_ref()
                .and_then(|(_, td)| {
                    td.primary_key
                        .iter()
                        .chain(td.uniques.iter())
                        .find(|k| k.columns == cols)
                        .map(|k| k.name.clone())
                        .or_else(|| td.indexes.iter().find(|i| i.unique && i.columns == cols).map(|i| i.name.clone()))
                })
                .unwrap_or_else(|| format!("{table}_{}_key", cols.join("_")));
            return if my {
                EngineError::new(state::UNIQUE_VIOLATION, format!("Duplicate entry for key '{table}.{name}'")).with_mysql(1062)
            } else {
                EngineError::new(state::UNIQUE_VIOLATION, format!("duplicate key value violates unique constraint \"{name}\""))
                    .with_detail(format!("Key ({}) already exists.", cols.join(", ")))
            };
        }
        if let Some(rest) = msg.strip_prefix("NOT NULL constraint failed: ") {
            let (table, col) = rest.split_once('.').unwrap_or(("", rest));
            return if my {
                EngineError::new(state::NOT_NULL_VIOLATION, format!("Column '{col}' cannot be null")).with_mysql(1048)
            } else {
                EngineError::new(
                    state::NOT_NULL_VIOLATION,
                    format!("null value in column \"{col}\" of relation \"{table}\" violates not-null constraint"),
                )
            };
        }
        if msg.starts_with("FOREIGN KEY constraint failed") {
            let target = plan.and_then(|p| p.target.clone());
            let kind = plan.map(|p| p.kind);
            if let (Some((s, t)), Some(StmtKind::Delete)) = (&target, kind) {
                let child = cat.referencing(s, t).into_iter().next();
                let (child_table, fk) = child.map(|(_, ct, fk)| (ct, fk.name)).unwrap_or_default();
                return if my {
                    EngineError::new(state::FOREIGN_KEY_VIOLATION, format!("Cannot delete or update a parent row: a foreign key constraint fails (`{s}`.`{child_table}`, CONSTRAINT `{fk}`)")).with_mysql(1451)
                } else {
                    EngineError::new(
                        state::FOREIGN_KEY_VIOLATION,
                        format!("update or delete on table \"{t}\" violates foreign key constraint \"{fk}\" on table \"{child_table}\""),
                    )
                };
            }
            let (table, fk) = match &target {
                Some((s, t)) => {
                    (t.clone(), cat.table(s, t).and_then(|td| td.foreign_keys.first().map(|f| f.name.clone())).unwrap_or_default())
                }
                None => (String::new(), String::new()),
            };
            return if my {
                EngineError::new(
                    state::FOREIGN_KEY_VIOLATION,
                    format!("Cannot add or update a child row: a foreign key constraint fails (CONSTRAINT `{fk}`)"),
                )
                .with_mysql(1452)
            } else {
                EngineError::new(
                    state::FOREIGN_KEY_VIOLATION,
                    format!("insert or update on table \"{table}\" violates foreign key constraint \"{fk}\""),
                )
            };
        }
        if let Some(name) = msg.strip_prefix("CHECK constraint failed: ") {
            let table = plan.and_then(|p| p.target.clone()).map(|(_, t)| t).unwrap_or_default();
            return if my {
                EngineError::new(state::CHECK_VIOLATION, format!("Check constraint '{name}' is violated.")).with_mysql(3819)
            } else {
                EngineError::new(state::CHECK_VIOLATION, format!("new row for relation \"{table}\" violates check constraint \"{name}\""))
            };
        }
        if let Some(t) = msg.strip_prefix("no such table: ") {
            let t = t.trim();
            let (schema, table) = match t.split_once('.') {
                Some((s, t)) if s != "main" => (Some(s.to_owned()), t.to_owned()),
                Some((_, t)) => (None, t.to_owned()),
                None => (None, t.to_owned()),
            };
            return if my {
                let db = schema.unwrap_or_else(|| self.database.clone());
                EngineError::new(state::UNDEFINED_TABLE, format!("Table '{db}.{table}' doesn't exist")).with_mysql(1146)
            } else {
                EngineError::new(
                    state::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", schema.map(|s| format!("{s}.{table}")).unwrap_or(table)),
                )
            };
        }
        if let Some(c) = msg.strip_prefix("no such column: ") {
            return if my {
                EngineError::new(state::UNDEFINED_COLUMN, format!("Unknown column '{c}' in 'field list'")).with_mysql(1054)
            } else {
                EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{}\" does not exist", c.trim_matches('"')))
            };
        }
        if let Some(f) = msg.strip_prefix("no such function: ") {
            let f = f.to_ascii_lowercase();
            return if my {
                EngineError::new(state::UNDEFINED_FUNCTION, format!("FUNCTION {}.{f} does not exist", self.database)).with_mysql(1305)
            } else {
                EngineError::new(state::UNDEFINED_FUNCTION, format!("function {f}() does not exist"))
                    .with_hint("No function matches the given name. The TwinLite engine emulates common PostgreSQL functions only.")
            };
        }
        if let Some(c) = msg.strip_prefix("ambiguous column name: ") {
            return EngineError::new(
                state::AMBIGUOUS_COLUMN,
                if my { format!("Column '{c}' in field list is ambiguous") } else { format!("column reference \"{c}\" is ambiguous") },
            );
        }
        if msg.contains("syntax error") || msg.starts_with("near ") || msg.contains("incomplete input") {
            let near = msg.split('"').nth(1).unwrap_or("");
            return if my {
                EngineError::syntax(format!(
                    "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near '{near}'"
                ))
            } else if near.is_empty() {
                EngineError::syntax("syntax error at end of input")
            } else {
                EngineError::syntax(format!("syntax error at or near \"{near}\""))
            };
        }
        if msg.contains("database is locked") || msg.contains("database table is locked") {
            return EngineError::new(
                state::LOCK_NOT_AVAILABLE,
                if my {
                    "Lock wait timeout exceeded; try restarting transaction".to_owned()
                } else {
                    "could not obtain lock: another transaction is writing".to_owned()
                },
            )
            .with_mysql(1205);
        }
        if msg.contains("cannot reference objects in database") {
            return EngineError::unsupported("views cannot reference objects in other schemas in the TwinLite engine");
        }
        if msg.contains("integer overflow") {
            return EngineError::new(
                state::NUMERIC_VALUE_OUT_OF_RANGE,
                if my { "BIGINT value is out of range" } else { "bigint out of range" },
            );
        }
        if msg.contains("datatype mismatch") {
            return EngineError::new(state::DATATYPE_MISMATCH, "datatype mismatch");
        }
        if msg.contains("too many attached databases") {
            return EngineError::unsupported("too many schemas/databases in one mock server (limit 120)");
        }
        let mut err = EngineError::internal(msg);
        if my {
            err = err.with_mysql(1105);
        }
        err
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.tx != TxState::Idle {
            let _ = self.rollback_internal();
        }
        let _ = self.flush_sequences();
    }
}

fn failed_tx() -> EngineError {
    EngineError::new(state::IN_FAILED_TRANSACTION, "current transaction is aborted, commands ignored until end of transaction block")
}

pub(crate) fn simple_column(name: &str, ty: SqlType) -> OutColumn {
    OutColumn { name: name.to_owned(), ty, source: None, table_oid: None, column_index: None }
}

fn infer_runtime<'a>(values: impl Iterator<Item = &'a Value>) -> SqlType {
    for v in values {
        match v {
            Value::Null => continue,
            Value::Integer(_) => return SqlType::Int8,
            Value::Real(_) => return SqlType::Float8,
            Value::Text(_) => return SqlType::Text,
            Value::Blob(_) => return SqlType::Bytea,
        }
    }
    SqlType::Text
}

/// Convert text parameters to the type the statement expects (e.g. LIMIT needs an integer).
fn coerce_param(p: Param, hint: Option<&SqlType>) -> Param {
    match (p, hint) {
        (Param::Text(s), Some(t)) if t.is_integer() => s.trim().parse().map(Param::Int).unwrap_or(Param::Text(s)),
        (Param::Text(s), Some(SqlType::Float4 | SqlType::Float8 | SqlType::Numeric { .. })) => {
            s.trim().parse().map(Param::Float).unwrap_or(Param::Text(s))
        }
        (Param::Text(s), Some(SqlType::Bool)) => match s.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Param::Bool(true),
            "f" | "false" | "0" | "no" | "off" => Param::Bool(false),
            _ => Param::Text(s),
        },
        (p, _) => p,
    }
}

pub(crate) fn owned(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}
