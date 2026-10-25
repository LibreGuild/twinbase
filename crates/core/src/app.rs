//! Application state shared by the desktop app, the CLI and the MCP server.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::broadcast;
use twinbase_client::{Client, DbKind};
use twinbase_mock::{Backend, MockConfig, MockHandle, MockKind};

use crate::error::{CoreError, Result};
use crate::events::{Activity, Actor, Event};
use crate::model::{AiAccess, ConnectionDef, MockDef, Workspace, WorkspaceData, mock_connection_id};
use crate::settings::{LocalStore, Settings};

struct RunningMock {
    handle: MockHandle,
    forward: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MockStatus {
    #[serde(flatten)]
    pub def: MockDef,
    pub running: bool,
    pub connection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub at: i64,
    pub sql: String,
    pub ok: bool,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct App {
    pub local: LocalStore,
    settings: RwLock<Settings>,
    workspace: RwLock<Workspace>,
    data: RwLock<WorkspaceData>,
    mocks: tokio::sync::Mutex<HashMap<String, RunningMock>>,
    mock_errors: Mutex<HashMap<String, String>>,
    clients: tokio::sync::Mutex<HashMap<String, Arc<Client>>>,
    /// One connection attempt per connection id at a time (others wait and reuse it).
    connecting: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    events: broadcast::Sender<Event>,
    activity: Mutex<VecDeque<Activity>>,
    history: Mutex<HashMap<String, VecDeque<HistoryEntry>>>,
    next_activity: AtomicU64,
    /// Port the MCP endpoint is listening on in this process (None until it binds).
    mcp_listening: Mutex<Option<u16>>,
    /// True when this run started without a previously chosen workspace.
    pub first_run: bool,
}

/// Secret-store key of a connection's SSH password or key passphrase.
fn ssh_secret_id(id: &str) -> String {
    format!("{id}#ssh")
}

/// Statements that only read data.
pub fn is_read_only_sql(sql: &str, kind: DbKind) -> bool {
    use twinbase_client::{SqlDialect, split_statements};
    let statements: Vec<String> = match kind {
        // ClickHouse and GoogleSQL strings use backslash escapes and backtick names, like MySQL.
        DbKind::Mysql | DbKind::Clickhouse | DbKind::Spanner | DbKind::Bigquery => split_statements(sql, SqlDialect::Mysql),
        // SQL*Plus conventions: PL/SQL blocks stay whole (and count as writes).
        DbKind::Oracle => split_statements(sql, SqlDialect::Oracle),
        DbKind::Sqlite => split_statements(sql, SqlDialect::Sqlite),
        // SQL Server: `GO` batches, then the statements inside each batch.
        DbKind::Mssql => split_statements(sql, SqlDialect::Mssql).iter().flat_map(|b| split_statements(b, SqlDialect::Postgres)).collect(),
        _ => split_statements(sql, SqlDialect::Postgres),
    };
    let lex = Lexing::of(kind);
    statements.iter().all(|s| {
        let words = code_words(s, lex);
        let has = |w: &str| words.iter().any(|x| x == w);
        let modifies = words.iter().any(|w| {
            matches!(
                w.as_str(),
                "INSERT"
                    | "UPDATE"
                    | "DELETE"
                    | "MERGE"
                    | "UPSERT"
                    | "REPLACE"
                    | "TRUNCATE"
                    | "DROP"
                    | "ALTER"
                    | "CREATE"
                    | "GRANT"
                    | "REVOKE"
                    | "EXEC"
                    | "EXECUTE"
                    | "CALL"
            )
        });
        match words.first().map(String::as_str).unwrap_or("") {
            // `SELECT … INTO` creates a table (or writes variables/files).
            "SELECT" | "VALUES" | "TABLE" | "WITH" => !has("INTO") && !modifies,
            "SHOW" | "DESCRIBE" | "DESC" => true,
            "EXPLAIN" => !has("ANALYZE") || (!modifies && !has("INTO")),
            _ => false,
        }
    })
}

/// How a dialect quotes and comments, for [`code_words`]. Getting this wrong in the lenient
/// direction would let a write hide inside what looks like a string or comment.
#[derive(Clone, Copy)]
struct Lexing {
    /// `\'` escapes a quote inside '…' (MySQL family, GoogleSQL); PostgreSQL only in E'…'.
    backslash: bool,
    /// `#` starts a comment (MySQL family, GoogleSQL).
    hash_comments: bool,
    /// `[name]` quotes an identifier (SQL Server, SQLite).
    brackets: bool,
    /// `$tag$…$tag$` strings (PostgreSQL).
    dollar: bool,
    /// `q'[…]'` strings (Oracle).
    q_quotes: bool,
    /// `/*! … */` is executed (MySQL).
    exec_comments: bool,
}

impl Lexing {
    fn of(kind: DbKind) -> Lexing {
        let none = Lexing { backslash: false, hash_comments: false, brackets: false, dollar: false, q_quotes: false, exec_comments: false };
        match kind {
            DbKind::Mysql | DbKind::Clickhouse => Lexing { backslash: true, hash_comments: true, exec_comments: true, ..none },
            DbKind::Spanner | DbKind::Bigquery => Lexing { backslash: true, hash_comments: true, ..none },
            DbKind::Mssql | DbKind::Sqlite => Lexing { brackets: true, ..none },
            DbKind::Oracle => Lexing { q_quotes: true, ..none },
            _ => Lexing { dollar: true, ..none },
        }
    }
}

/// Upper-case words of a statement, without comments, string literals and quoted names (so a
/// value such as `'delete'` is not mistaken for a keyword).
fn code_words(s: &str, lex: Lexing) -> Vec<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    let mut i = 0;
    let flush = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        // Oracle q'<delimiter>…<closing delimiter>'.
        if (lex.q_quotes && word.eq_ignore_ascii_case("q") || lex.q_quotes && word.eq_ignore_ascii_case("nq"))
            && c == '\''
            && let Some(open) = next
        {
            let close = match open {
                '[' => ']',
                '(' => ')',
                '{' => '}',
                '<' => '>',
                other => other,
            };
            word.clear();
            i += 2;
            while i < b.len() && !(b[i] == close && b.get(i + 1) == Some(&'\'')) {
                i += 1;
            }
            i += 2;
            continue;
        }
        // PostgreSQL dollar quoting ($$…$$, $tag$…$tag$; $1 is a parameter).
        if lex.dollar && c == '$' && word.is_empty() {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            let tag: String = b[i..=j.min(b.len() - 1)].iter().collect();
            let starts_digit = b.get(i + 1).is_some_and(|d| d.is_ascii_digit());
            if j < b.len() && b[j] == '$' && !starts_digit {
                let body_start = j + 1;
                let rest: String = b[body_start..].iter().collect();
                i = match rest.find(&tag) {
                    Some(p) => body_start + rest[..p].chars().count() + tag.chars().count(),
                    None => b.len(),
                };
                continue;
            }
        }
        let end = match c {
            '\'' => Some('\''),
            '"' => Some('"'),
            '`' => Some('`'),
            '[' if lex.brackets => Some(']'),
            _ => None,
        };
        if let Some(end) = end {
            // PostgreSQL E'…' strings take backslash escapes.
            let backslash = lex.backslash || (lex.dollar && end == '\'' && word.eq_ignore_ascii_case("e"));
            if backslash && word.eq_ignore_ascii_case("e") {
                word.clear();
            }
            flush(&mut word, &mut out);
            i += 1;
            while i < b.len() {
                if b[i] == end {
                    if b.get(i + 1) == Some(&end) {
                        i += 1;
                    } else {
                        break;
                    }
                } else if backslash && b[i] == '\\' && end == '\'' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == '-' && next == Some('-') || lex.hash_comments && c == '#' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // MySQL runs the text of /*! … */ (and MariaDB's /*M! … */): read it as code.
            let executed = lex.exec_comments && (b.get(i + 2) == Some(&'!') || (b.get(i + 2) == Some(&'M') && b.get(i + 3) == Some(&'!')));
            if executed {
                i += 3;
                continue;
            }
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 1;
        } else if c.is_alphanumeric() || c == '_' {
            word.push(c.to_ascii_uppercase());
            i += 1;
            continue;
        }
        flush(&mut word, &mut out);
        i += 1;
    }
    flush(&mut word, &mut out);
    out
}

/// Remove a folder whose files a stopped mock server is still closing: connections close their
/// database files a moment after the server stops, and Windows refuses to delete open files.
async fn remove_dir_when_closed(dir: &std::path::Path) -> Result<()> {
    let mut attempt = 0;
    while dir.exists() {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => break,
            Err(_) if attempt < 50 => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(CoreError::io(dir, e)),
        }
    }
    Ok(())
}

/// Built-in PostgreSQL functions that touch server files or run SQL given as a string (which a
/// statement check cannot see into).
const SERVER_FUNCTIONS: &[&str] = &[
    "PG_STAT_FILE",
    "PG_FILE_WRITE",
    "PG_FILE_RENAME",
    "PG_FILE_UNLINK",
    "LO_IMPORT",
    "LO_EXPORT",
    "QUERY_TO_XML",
    "QUERY_TO_XMLSCHEMA",
    "QUERY_TO_XML_AND_XMLSCHEMA",
    "CURSOR_TO_XML",
    "TS_STAT",
];

/// Redis commands that only read.
pub fn is_read_only_redis(line: &str) -> bool {
    let cmd = line.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
    matches!(
        cmd.as_str(),
        "GET"
            | "MGET"
            | "EXISTS"
            | "TYPE"
            | "TTL"
            | "PTTL"
            | "KEYS"
            | "SCAN"
            | "STRLEN"
            | "GETRANGE"
            | "HGET"
            | "HMGET"
            | "HGETALL"
            | "HEXISTS"
            | "HKEYS"
            | "HVALS"
            | "HLEN"
            | "HSCAN"
            | "LRANGE"
            | "LLEN"
            | "LINDEX"
            | "SMEMBERS"
            | "SISMEMBER"
            | "SCARD"
            | "SSCAN"
            | "ZSCORE"
            | "ZCARD"
            | "ZCOUNT"
            | "ZRANK"
            | "ZRANGE"
            | "ZREVRANGE"
            | "ZRANGEBYSCORE"
            | "ZSCAN"
            | "DBSIZE"
            | "INFO"
            | "PING"
            | "ECHO"
            | "TIME"
    )
}

impl App {
    pub fn new(app_dir: PathBuf, workspace_path: Option<PathBuf>) -> Result<Arc<App>> {
        let local = LocalStore::new(app_dir);
        let mut settings = local.load_settings();
        // No workspace was ever chosen: the UI offers its start page.
        let first_run = workspace_path.is_none() && settings.last_workspace.is_none();
        let path = workspace_path
            .or_else(|| settings.last_workspace.clone().map(PathBuf::from))
            .unwrap_or_else(crate::settings::default_workspace_dir);
        let workspace = Workspace::open_or_create(&path, None)?;
        let data = workspace.load()?;
        remember_workspace(&mut settings, &path);
        let _ = local.save_settings(&settings);
        twinbase_client::oracle_db::set_client_dir(Some(PathBuf::from(&settings.oracle_client)));
        let (events, _) = broadcast::channel(2048);
        Ok(Arc::new(App {
            local,
            settings: RwLock::new(settings),
            workspace: RwLock::new(workspace),
            data: RwLock::new(data),
            mocks: tokio::sync::Mutex::new(HashMap::new()),
            mock_errors: Mutex::new(HashMap::new()),
            clients: tokio::sync::Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            events,
            activity: Mutex::new(VecDeque::new()),
            history: Mutex::new(HashMap::new()),
            mcp_listening: Mutex::new(None),
            first_run,
            next_activity: AtomicU64::new(1),
        }))
    }

    pub fn mcp_listening(&self) -> Option<u16> {
        *self.mcp_listening.lock()
    }

    pub fn set_mcp_listening(&self, port: Option<u16>) {
        *self.mcp_listening.lock() = port;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let mut s = self.settings.write();
        f(&mut s);
        self.local.save_settings(&s)?;
        twinbase_client::oracle_db::set_client_dir(Some(PathBuf::from(&s.oracle_client)));
        Ok(s.clone())
    }

    pub fn workspace(&self) -> Workspace {
        self.workspace.read().clone()
    }

    pub fn workspace_key(&self) -> String {
        self.workspace.read().root.to_string_lossy().into_owned()
    }

    pub fn data(&self) -> WorkspaceData {
        self.data.read().clone()
    }

    pub fn reload(&self) -> Result<()> {
        let data = self.workspace().load()?;
        *self.data.write() = data;
        self.emit(Event::WorkspaceChanged);
        Ok(())
    }

    /// Switch to another workspace folder (stops running mocks and closes connections).
    pub async fn open_workspace(&self, path: PathBuf, name: Option<&str>) -> Result<()> {
        self.stop_all_mocks().await;
        self.clients.lock().await.clear();
        let ws = Workspace::open_or_create(&path, name)?;
        let data = ws.load()?;
        *self.workspace.write() = ws;
        *self.data.write() = data;
        self.update_settings(|s| remember_workspace(s, &path))?;
        self.emit(Event::WorkspaceChanged);
        self.autostart_mocks().await;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Activity and history
    // -----------------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        actor: &Actor,
        action: &str,
        summary: String,
        connection: Option<String>,
        sql: Option<String>,
        error: Option<String>,
        duration_ms: f64,
    ) {
        let a = Activity {
            id: self.next_activity.fetch_add(1, Ordering::Relaxed),
            at: chrono::Utc::now().timestamp_millis(),
            actor: actor.clone(),
            action: action.into(),
            summary,
            connection,
            sql: sql.map(|s| s.chars().take(2000).collect()),
            ok: error.is_none(),
            error,
            duration_ms,
        };
        {
            let mut log = self.activity.lock();
            if log.len() >= 1000 {
                log.pop_front();
            }
            log.push_back(a.clone());
        }
        self.emit(Event::Activity(a));
    }

    pub fn activity(&self, limit: usize) -> Vec<Activity> {
        let log = self.activity.lock();
        log.iter().rev().take(limit).cloned().collect()
    }

    pub fn add_history(&self, connection: &str, mut entry: HistoryEntry) {
        // Huge scripts (pasted dumps) are not kept: 200 of them per connection would pin a lot of
        // memory, and a cut-off copy must never be run again from the history.
        if entry.sql.len() > 256 * 1024 {
            entry.sql = format!("-- script of {} KB, not kept in history", entry.sql.len() / 1024);
        }
        let mut h = self.history.lock();
        let list = h.entry(connection.to_owned()).or_default();
        if list.len() >= 200 {
            list.pop_back();
        }
        list.push_front(entry);
    }

    pub fn history(&self, connection: &str) -> Vec<HistoryEntry> {
        self.history.lock().get(connection).map(|v| v.iter().cloned().collect()).unwrap_or_default()
    }

    // -----------------------------------------------------------------------------------------
    // Connections
    // -----------------------------------------------------------------------------------------

    /// All connections: saved ones plus one per mock server.
    pub fn connections(&self) -> Vec<ConnectionDef> {
        let d = self.data.read();
        let mut out: Vec<ConnectionDef> = d.mocks.iter().map(|m| m.connection()).collect();
        out.extend(d.connections.iter().cloned());
        out
    }

    pub fn connection_def(&self, id: &str) -> Result<ConnectionDef> {
        self.connections().into_iter().find(|c| c.id == id).ok_or_else(|| CoreError::not_found(format!("connection '{id}' not found")))
    }

    pub fn mock_def(&self, id: &str) -> Result<MockDef> {
        self.data
            .read()
            .mocks
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| CoreError::not_found(format!("mock server '{id}' not found")))
    }

    fn password_for(&self, def: &ConnectionDef) -> Option<String> {
        if let Some(mock_id) = def.id.strip_prefix("mock:") {
            return self.mock_def(mock_id).ok().map(|m| m.credentials().1);
        }
        self.local.secret(&self.workspace_key(), &def.id)
    }

    /// Connection settings with stored secrets filled in (database password, SSH secret).
    pub fn config_for(
        &self,
        def: &ConnectionDef,
        password: Option<String>,
        ssh_secret: Option<String>,
    ) -> twinbase_client::ConnectionConfig {
        let mut config = def.to_config(password.or_else(|| self.password_for(def)));
        if let Some(ssh) = config.ssh.as_mut() {
            ssh.secret = ssh_secret.or_else(|| self.local.secret(&self.workspace_key(), &ssh_secret_id(&def.id)));
            ssh.known_hosts = Some(self.local.dir().join("known_hosts"));
        }
        config
    }

    pub fn set_ssh_secret(&self, id: &str, secret: Option<&str>) -> Result<()> {
        self.local.set_secret(&self.workspace_key(), &ssh_secret_id(id), secret)
    }

    pub fn has_ssh_secret(&self, id: &str) -> bool {
        self.local.secret(&self.workspace_key(), &ssh_secret_id(id)).is_some()
    }

    pub fn set_password(&self, id: &str, password: Option<&str>) -> Result<()> {
        self.local.set_secret(&self.workspace_key(), id, password)
    }

    pub fn has_password(&self, id: &str) -> bool {
        self.local.secret(&self.workspace_key(), id).is_some()
    }

    /// Open (or reuse) a connection.
    pub async fn client(&self, id: &str) -> Result<Arc<Client>> {
        if let Some(c) = self.clients.lock().await.get(id) {
            return Ok(c.clone());
        }
        let gate = self.connecting.lock().entry(id.to_owned()).or_default().clone();
        let _connecting = gate.lock().await;
        if let Some(c) = self.clients.lock().await.get(id) {
            return Ok(c.clone());
        }
        let def = self.connection_def(id)?;
        if let Some(mock_id) = id.strip_prefix("mock:")
            && !self.mocks.lock().await.contains_key(mock_id)
        {
            return Err(CoreError::invalid(format!("mock server '{}' is not running; start it first", def.name)));
        }
        let config = self.config_for(&def, None, None);
        let client = Arc::new(Client::connect(&config).await?);
        self.clients.lock().await.insert(id.to_owned(), client.clone());
        self.emit(Event::ConnectionStatus { id: id.to_owned(), connected: true });
        Ok(client)
    }

    pub async fn disconnect(&self, id: &str) {
        if let Some(c) = self.clients.lock().await.remove(id) {
            if let Some(sql) = c.sql() {
                sql.close().await;
            }
            self.emit(Event::ConnectionStatus { id: id.to_owned(), connected: false });
        }
    }

    pub async fn connected_ids(&self) -> Vec<String> {
        self.clients.lock().await.keys().cloned().collect()
    }

    /// Whether `actor` may only read on this connection (then reads also run in a read-only or
    /// rolled-back transaction, in case a write got past the statement check).
    pub fn reads_only(&self, def: &ConnectionDef, actor: &Actor) -> bool {
        def.read_only || (matches!(actor, Actor::Agent { .. }) && def.ai_access == AiAccess::ReadOnly)
    }

    /// Agents on a mock that runs real PostgreSQL work as its superuser, which can reach the
    /// computer itself (`COPY … PROGRAM`, server file functions, untrusted languages). Statements
    /// that could do so are refused for agents; a person can still run them.
    pub fn check_agent_sql(&self, def: &ConnectionDef, actor: &Actor, sql: &str) -> Result<()> {
        if !matches!(actor, Actor::Agent { .. }) {
            return Ok(());
        }
        let native =
            def.id.strip_prefix("mock:").and_then(|m| self.mock_def(m).ok()).is_some_and(|m| m.engine == crate::model::MockEngine::Native);
        if !native {
            return Ok(());
        }
        let lex = Lexing::of(def.kind);
        for s in twinbase_client::split_statements(sql, twinbase_client::SqlDialect::Postgres) {
            let words = code_words(&s, lex);
            let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
            let first = w(0);
            let creates_code = first == "CREATE"
                && words
                    .iter()
                    .take(6)
                    .any(|x| matches!(x.as_str(), "FUNCTION" | "PROCEDURE" | "LANGUAGE" | "EXTENSION" | "TRIGGER" | "RULE"));
            let dangerous = matches!(first, "COPY" | "DO" | "LOAD" | "CALL")
                || creates_code
                || (first == "ALTER" && matches!(w(1), "SYSTEM" | "ROLE" | "USER" | "FUNCTION" | "PROCEDURE" | "EXTENSION" | "LANGUAGE"))
                || words.iter().any(|x| {
                    SERVER_FUNCTIONS.contains(&x.as_str())
                        || x.starts_with("DBLINK")
                        || x.starts_with("PG_LS_")
                        || x.starts_with("PG_READ_")
                });
            if dangerous {
                return Err(CoreError::forbidden(format!(
                    "'{}' runs on real PostgreSQL as its superuser; AI agents may not run COPY, DO, CALL, LOAD, CREATE FUNCTION/PROCEDURE/EXTENSION/LANGUAGE/TRIGGER/RULE, ALTER SYSTEM/ROLE, or server file and query-string functions there. A person can run this statement in TwinBase.",
                    def.name
                )));
            }
        }
        Ok(())
    }

    /// Refuse writes on read-only connections and for agents without write access.
    pub fn check_access(&self, def: &ConnectionDef, actor: &Actor, writes: bool) -> Result<()> {
        if let Actor::Agent { .. } = actor {
            match def.ai_access {
                AiAccess::None => {
                    return Err(CoreError::forbidden(format!(
                        "AI access is disabled for connection '{}'. Change it in the connection settings.",
                        def.name
                    )));
                }
                AiAccess::ReadOnly if writes => {
                    return Err(CoreError::forbidden(format!(
                        "connection '{}' is read-only for AI agents. A person can allow writes in the connection settings.",
                        def.name
                    )));
                }
                _ => {}
            }
        }
        if def.read_only && writes {
            return Err(CoreError::forbidden(format!("connection '{}' is marked read-only", def.name)));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Mock servers
    // -----------------------------------------------------------------------------------------

    pub async fn mock_statuses(&self) -> Vec<MockStatus> {
        let running = self.mocks.lock().await;
        let errors = self.mock_errors.lock();
        self.data
            .read()
            .mocks
            .iter()
            .map(|m| MockStatus {
                def: m.clone(),
                running: running.contains_key(&m.id),
                connection_id: mock_connection_id(&m.id),
                error: errors.get(&m.id).cloned(),
            })
            .collect()
    }

    pub async fn is_mock_running(&self, id: &str) -> bool {
        self.mocks.lock().await.contains_key(id)
    }

    pub async fn mock_backend(&self, id: &str) -> Option<Backend> {
        self.mocks.lock().await.get(id).map(|m| m.handle.backend.clone())
    }

    /// Apply fault rules to a running mock immediately (no restart).
    pub async fn apply_mock_faults(&self, id: &str, faults: &twinbase_mock::FaultConfig) -> Result<()> {
        if let Some(m) = self.mocks.lock().await.get(id) {
            m.handle.faults.set(faults.clone()).map_err(CoreError::invalid)?;
        }
        Ok(())
    }

    pub async fn mock_traffic(&self, id: &str, after: u64, limit: usize) -> Vec<twinbase_mock::TrafficEvent> {
        self.mocks.lock().await.get(id).map(|m| m.handle.traffic.since(after, limit)).unwrap_or_default()
    }

    pub async fn start_mock(&self, id: &str) -> Result<()> {
        let def = self.mock_def(id)?;
        if self.mocks.lock().await.contains_key(id) {
            return Ok(());
        }
        let ws = self.workspace();
        let data_dir = def.persist.then(|| ws.mock_data_dir(id));
        let fresh = data_dir.as_ref().is_none_or(|d| !d.exists());
        let config = MockConfig {
            kind: def.kind,
            host: def.host.clone(),
            port: def.port,
            data_dir,
            auth: def.auth.clone(),
            faults: def.faults.clone(),
            native: (def.kind == MockKind::Postgres && def.engine == crate::model::MockEngine::Native).then(|| {
                twinbase_mock::NativeEngine { bin: Some(self.settings().postgres_bin).filter(|b| !b.trim().is_empty()).map(PathBuf::from) }
            }),
        };
        let handle = match twinbase_mock::start(config).await {
            Ok(h) => h,
            Err(e) => {
                let msg = match &e {
                    twinbase_mock::MockError::Bind { addr, .. } => {
                        format!(
                            "port {} is already in use (another database or mock may be running on {addr}); choose another port",
                            def.port
                        )
                    }
                    other => other.to_string(),
                };
                self.mock_errors.lock().insert(id.to_owned(), msg.clone());
                self.emit(Event::MockStatus { id: id.into(), running: false, port: def.port, error: Some(msg.clone()) });
                return Err(CoreError::new("mock", msg));
            }
        };
        self.mock_errors.lock().remove(id);
        if let Backend::Sql(engine) = &handle.backend {
            let engine = engine.clone();
            let db = def.database.clone();
            let kind = def.kind;
            let seed = fresh.then(|| std::fs::read_to_string(ws.seed_path(id, kind)).ok()).flatten();
            let res = tokio::task::spawn_blocking(move || -> Result<()> {
                if kind == MockKind::Mysql {
                    let mut s = engine.session(None, "root")?;
                    let r = s.execute(&format!("CREATE DATABASE IF NOT EXISTS `{}`", db.replace('`', "``")));
                    if let Some(e) = r.error {
                        return Err(e.into());
                    }
                }
                if let Some(seed) = seed {
                    let mut s = engine.session(Some(&db), "twinbase")?;
                    if let Some(e) = s.execute(&seed).error {
                        return Err(CoreError::invalid(format!("seed script failed: {}", e.message)));
                    }
                }
                Ok(())
            })
            .await
            .map_err(|e| CoreError::new("mock", e.to_string()))?;
            if let Err(e) = res {
                self.mock_errors.lock().insert(id.to_owned(), e.message.clone());
            }
        } else if let Backend::NativePg(server) = &handle.backend {
            let db = def.database.clone();
            let seed = fresh.then(|| std::fs::read_to_string(ws.seed_path(id, def.kind)).ok()).flatten().filter(|s| !s.trim().is_empty());
            let res = match seed {
                Some(sql) => twinbase_mock::native_pg::seed(server, &db, &sql).await,
                None => server.ensure("postgres", &db).await,
            };
            if let Err(e) = res {
                self.mock_errors.lock().insert(id.to_owned(), format!("seed script failed: {e}"));
            }
        } else if let Backend::Firestore(store) = &handle.backend {
            if fresh
                && let Ok(seed) = std::fs::read_to_string(ws.seed_path(id, def.kind))
                && !seed.trim().is_empty()
            {
                let store = store.clone();
                let res = tokio::task::spawn_blocking(move || twinbase_mock::firestore::apply_seed(&store, &seed)).await;
                if let Ok(Err(e)) = res {
                    self.mock_errors.lock().insert(id.to_owned(), format!("seed failed: {e}"));
                }
            }
        } else if let Backend::Dynamo(store) = &handle.backend {
            if fresh
                && let Ok(seed) = std::fs::read_to_string(ws.seed_path(id, def.kind))
                && !seed.trim().is_empty()
            {
                let store = store.clone();
                let res = tokio::task::spawn_blocking(move || twinbase_mock::dynamo::apply_seed(&store, &seed)).await;
                if let Ok(Err(e)) = res {
                    self.mock_errors.lock().insert(id.to_owned(), format!("seed failed: {e}"));
                }
            }
        } else if let Backend::Mongo(store) = &handle.backend {
            if fresh
                && let Ok(seed) = std::fs::read_to_string(ws.seed_path(id, def.kind))
                && !seed.trim().is_empty()
            {
                let (store, db) = (store.clone(), def.database.clone());
                let res = tokio::task::spawn_blocking(move || twinbase_mock::mongo::apply_seed(&store, &db, &seed)).await;
                if let Ok(Err(e)) = res {
                    self.mock_errors.lock().insert(id.to_owned(), format!("seed failed: {e}"));
                }
            }
        } else if let Backend::Redis(store) = &handle.backend
            && fresh
            && let Ok(seed) = std::fs::read_to_string(ws.seed_path(id, def.kind))
        {
            let mut db = 0usize;
            for line in seed.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let args: Vec<Vec<u8>> = twinbase_client::split_command_line(line).into_iter().map(String::into_bytes).collect();
                if !args.is_empty() {
                    store.execute(&mut db, &args);
                }
            }
        }
        let mut rx = handle.traffic.subscribe();
        let events = self.events.clone();
        let mock_id = id.to_owned();
        let forward = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let _ = events.send(Event::MockTraffic { id: mock_id.clone(), event });
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        let port = handle.addr.port();
        self.mocks.lock().await.insert(id.to_owned(), RunningMock { handle, forward });
        self.clients.lock().await.remove(&mock_connection_id(id));
        self.emit(Event::MockStatus { id: id.into(), running: true, port, error: None });
        Ok(())
    }

    pub async fn stop_mock(&self, id: &str) -> Result<()> {
        self.disconnect(&mock_connection_id(id)).await;
        let running = self.mocks.lock().await.remove(id);
        if let Some(r) = running {
            let port = r.handle.addr.port();
            r.forward.abort();
            r.handle.stop().await;
            self.emit(Event::MockStatus { id: id.into(), running: false, port, error: None });
        }
        Ok(())
    }

    /// Stop a mock and delete its folder (definition, seed and data).
    pub async fn delete_mock(&self, id: &str) -> Result<()> {
        crate::model::check_id(id)?;
        self.stop_mock(id).await?;
        remove_dir_when_closed(&self.workspace().mock_dir(id)).await
    }

    /// Delete a mock's data and start it again from its seed script.
    pub async fn reset_mock(&self, id: &str) -> Result<()> {
        let was_running = self.is_mock_running(id).await;
        self.stop_mock(id).await?;
        remove_dir_when_closed(&self.workspace().mock_data_dir(id)).await?;
        let _ = was_running;
        self.start_mock(id).await?;
        self.emit(Event::SchemaChanged { connection: mock_connection_id(id) });
        Ok(())
    }

    pub async fn stop_all_mocks(&self) {
        let ids: Vec<String> = self.mocks.lock().await.keys().cloned().collect();
        for id in ids {
            let _ = self.stop_mock(&id).await;
        }
    }

    pub async fn autostart_mocks(&self) {
        let ids: Vec<String> = self.data.read().mocks.iter().filter(|m| m.auto_start).map(|m| m.id.clone()).collect();
        for id in ids {
            if let Err(e) = self.start_mock(&id).await {
                tracing::warn!("auto-start of mock {id} failed: {e}");
            }
        }
    }

    pub fn save_mock_def(&self, def: &MockDef) -> Result<()> {
        self.workspace().save_mock(def)?;
        self.reload()
    }
}

fn remember_workspace(s: &mut Settings, path: &std::path::Path) {
    let p = path.to_string_lossy().into_owned();
    s.recent_workspaces.retain(|w| w != &p);
    s.recent_workspaces.insert(0, p.clone());
    s.recent_workspaces.truncate(10);
    s.last_workspace = Some(p);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_read_only_sql() {
        assert!(is_read_only_sql("SELECT * FROM t; -- comment\nSHOW tables", DbKind::Postgres));
        assert!(is_read_only_sql("WITH x AS (SELECT 1) SELECT * FROM x", DbKind::Postgres));
        assert!(!is_read_only_sql("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x", DbKind::Postgres));
        assert!(!is_read_only_sql("SELECT 1; DROP TABLE t", DbKind::Postgres));
        assert!(!is_read_only_sql("UPDATE t SET a = 1", DbKind::Mysql));
        assert!(is_read_only_sql("SELECT * FROM t WHERE note = 'please delete me' -- drop", DbKind::Postgres));
        assert!(!is_read_only_sql("SELECT * INTO copy FROM t", DbKind::Postgres));
        assert!(!is_read_only_sql("WITH x AS (SELECT 1 a) SELECT a INTO t2 FROM x", DbKind::Mssql));
        assert!(is_read_only_sql("SELECT TOP 10 * FROM [dbo].[orders]\nGO\nSELECT 1", DbKind::Mssql));
        assert!(!is_read_only_sql("SELECT 1\nGO\nEXEC sp_rename 'a', 'b'", DbKind::Mssql));
        assert!(!is_read_only_sql("SELECT 1; DELETE FROM t", DbKind::Mssql));
        // Strings and comments are read the way each server reads them, so a write cannot hide.
        let hidden = "WITH a AS (SELECT '\\'), d AS (DELETE FROM t RETURNING 1) SELECT 1 --'";
        assert!(!is_read_only_sql(hidden, DbKind::Postgres));
        assert!(is_read_only_sql("SELECT 'it\\'s' AS a", DbKind::Mysql));
        assert!(!is_read_only_sql("WITH a AS (SELECT 1 # 1), d AS (DELETE FROM t RETURNING 1) SELECT 1", DbKind::Postgres));
        assert!(is_read_only_sql("SELECT 1 # DELETE\n", DbKind::Mysql));
        assert!(!is_read_only_sql("WITH a AS (SELECT $$'$$), d AS (DELETE FROM t RETURNING 1) SELECT 1 --'", DbKind::Postgres));
        assert!(is_read_only_sql("SELECT $tag$ DELETE $tag$, $1", DbKind::Postgres));
        assert!(is_read_only_sql("SELECT E'it\\'s' AS a", DbKind::Postgres));
        assert!(!is_read_only_sql("SELECT * FROM t /*! INTO OUTFILE '/tmp/x' */", DbKind::Mysql));
        assert!(!is_read_only_sql("SELECT q'[']' , x FROM dual WHERE 1 = (DELETE FROM t) --'", DbKind::Oracle));
        assert!(is_read_only_sql("SELECT q'[ it's DELETE ]' FROM dual", DbKind::Oracle));
        assert!(is_read_only_sql("SELECT arr[1] FROM t", DbKind::Postgres));
        assert!(is_read_only_redis("hgetall user:1"));
        assert!(!is_read_only_redis("DEL user:1"));
    }
}
