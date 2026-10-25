//! Workspace definitions and their on-disk format.
//!
//! ```text
//! <workspace>/
//!   twinbase.toml                    workspace manifest
//!   connections/<id>.toml            saved servers (no passwords)
//!   mocks/<id>/mock.toml             mock server definition
//!   mocks/<id>/seed.sql              optional seed script
//!   mocks/<id>/data/                 mock data (when persisted)
//!   queries/<collection>/_collection.toml
//!   queries/<collection>/<id>.toml   saved queries with docs
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use twinbase_client::{ConnectionConfig, DbKind, TlsMode};
use twinbase_mock::{AuthMode, FaultConfig, MockKind};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AiAccess {
    None,
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceManifest {
    pub name: String,
    #[serde(default = "one")]
    pub format: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionDef {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub tls: TlsMode,
    /// UI accent colour (e.g. red for production).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Block writes from the UI and from agents.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub ai_access: AiAccess,
    /// Folder/group label in the explorer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Reach the database through an SSH bastion (its password/passphrase is stored like passwords).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<twinbase_client::ssh::SshTunnel>,
}

impl ConnectionDef {
    pub fn to_config(&self, password: Option<String>) -> ConnectionConfig {
        ConnectionConfig {
            kind: self.kind,
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            password,
            database: self.database.clone(),
            path: self.path.clone(),
            tls: self.tls,
            timeout_secs: 10,
            ssh: self.ssh.clone(),
            via: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MockDef {
    pub id: String,
    pub name: String,
    pub kind: MockKind,
    #[serde(default = "localhost")]
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub auth: AuthMode,
    /// Database the matching connection opens (PostgreSQL/MySQL).
    #[serde(default = "default_db")]
    pub database: String,
    /// Keep data in the workspace; otherwise data is temporary and lost on stop.
    #[serde(default = "yes")]
    pub persist: bool,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Fault injection (latency, errors, dropped connections); applied live while running.
    #[serde(default, skip_serializing_if = "FaultConfig::is_default")]
    pub faults: FaultConfig,
    /// PostgreSQL mocks: `twinlite` (built in, default) or `native` (a real PostgreSQL server).
    #[serde(default, skip_serializing_if = "MockEngine::is_default")]
    pub engine: MockEngine,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MockEngine {
    #[default]
    Twinlite,
    Native,
}

impl MockEngine {
    pub fn is_default(&self) -> bool {
        *self == MockEngine::Twinlite
    }
}

fn localhost() -> String {
    "127.0.0.1".into()
}

fn default_db() -> String {
    "app".into()
}

fn yes() -> bool {
    true
}

impl MockDef {
    pub fn db_kind(&self) -> DbKind {
        match self.kind {
            MockKind::Postgres => DbKind::Postgres,
            MockKind::Mysql => DbKind::Mysql,
            MockKind::Mssql => DbKind::Mssql,
            MockKind::Redis => DbKind::Redis,
            MockKind::Mongodb => DbKind::Mongodb,
            MockKind::Dynamodb => DbKind::Dynamodb,
            MockKind::Firestore => DbKind::Firestore,
        }
    }

    /// Credentials clients use for this mock. A MongoDB mock without a password takes no
    /// credentials at all (SCRAM cannot accept any password).
    pub fn credentials(&self) -> (String, String) {
        match &self.auth {
            AuthMode::Password { user, password } => (user.clone(), password.clone()),
            AuthMode::Any if matches!(self.kind, MockKind::Mongodb | MockKind::Firestore) => (String::new(), String::new()),
            AuthMode::Any => (
                match self.kind {
                    MockKind::Postgres => "postgres".into(),
                    MockKind::Mysql => "root".into(),
                    MockKind::Mssql => "sa".into(),
                    MockKind::Redis => "default".into(),
                    MockKind::Mongodb => String::new(),
                    MockKind::Dynamodb => "twinbase".into(),
                    MockKind::Firestore => String::new(),
                },
                "twinbase".into(),
            ),
        }
    }

    /// The connection TwinBase itself uses to browse this mock.
    pub fn connection(&self) -> ConnectionDef {
        let (user, _) = self.credentials();
        ConnectionDef {
            id: mock_connection_id(&self.id),
            name: self.name.clone(),
            kind: self.db_kind(),
            host: if self.host == "0.0.0.0" { "127.0.0.1".into() } else { self.host.clone() },
            port: self.port,
            user,
            database: match self.kind {
                MockKind::Redis => Some("0".into()),
                // DynamoDB keeps the region in the database field.
                MockKind::Dynamodb => Some("us-east-1".into()),
                // Firestore keeps the project id there; a mock serves every project id.
                MockKind::Firestore => Some("demo-twinbase".into()),
                _ => Some(self.database.clone()),
            },
            path: None,
            tls: TlsMode::Disable,
            color: None,
            read_only: false,
            ai_access: AiAccess::ReadWrite,
            group: None,
            ssh: None,
        }
    }
}

pub fn mock_connection_id(mock_id: &str) -> String {
    format!("mock:{mock_id}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expectation {
    /// Statement succeeds and returns exactly this many rows.
    Rows { count: u64 },
    /// Statement succeeds and returns no rows.
    Empty,
    /// Statement succeeds (any result).
    Succeeds,
    /// Statement fails, optionally with this error code (SQLSTATE / MySQL number).
    Fails {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub id: String,
    pub name: String,
    pub collection: String,
    /// Default connection id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Markdown documentation.
    #[serde(default)]
    pub docs: String,
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub docs: String,
}

/// Everything defined in a workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceData {
    pub name: String,
    pub path: String,
    pub connections: Vec<ConnectionDef>,
    pub mocks: Vec<MockDef>,
    pub collections: Vec<Collection>,
    pub queries: Vec<SavedQuery>,
}

/// Create a filesystem-safe id from a name.
pub fn slugify(name: &str) -> String {
    let mut s: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_owned();
    if s.is_empty() { "item".into() } else { s.chars().take(48).collect() }
}

/// An id based on `name` that is not in `taken`.
pub fn unique_id(name: &str, taken: &[String]) -> String {
    let base = slugify(name);
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(c)).unwrap()
}

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| CoreError::io(path, e))?;
    toml::from_str(&text).map_err(|e| CoreError::invalid(format!("{}: {e}", path.display())))
}

/// Ids become file and directory names; reject anything that could escape the workspace.
pub fn check_id(id: &str) -> Result<()> {
    let ok = !id.trim().is_empty()
        && id.len() <= 200
        && !id.starts_with('.')
        && !id.contains("..")
        && !id.chars().any(|c| c == '/' || c == '\\' || c == ':' || c.is_control());
    if ok { Ok(()) } else { Err(CoreError::invalid(format!("invalid id '{id}'"))) }
}

pub(crate) fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    }
    let text = toml::to_string_pretty(value).map_err(|e| CoreError::invalid(e.to_string()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| CoreError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// A workspace folder on disk.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    /// Open a workspace folder, creating its layout when missing.
    pub fn open_or_create(root: impl Into<PathBuf>, name: Option<&str>) -> Result<Workspace> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| CoreError::io(&root, e))?;
        let ws = Workspace { root };
        let manifest = ws.root.join("twinbase.toml");
        if !manifest.exists() {
            let default_name = ws.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Workspace".into());
            write_toml(&manifest, &WorkspaceManifest { name: name.unwrap_or(&default_name).to_owned(), format: 1 })?;
            std::fs::write(
                ws.root.join(".gitignore"),
                "# Mock data is binary and machine-local; share it with export bundles.\nmocks/*/data/\n",
            )
            .map_err(|e| CoreError::io(&ws.root, e))?;
        }
        for d in ["connections", "mocks", "queries"] {
            std::fs::create_dir_all(ws.root.join(d)).map_err(|e| CoreError::io(&ws.root, e))?;
        }
        Ok(ws)
    }

    pub fn manifest(&self) -> Result<WorkspaceManifest> {
        read_toml(&self.root.join("twinbase.toml"))
    }

    pub fn load(&self) -> Result<WorkspaceData> {
        let manifest = self.manifest()?;
        let mut data = WorkspaceData { name: manifest.name, path: self.root.to_string_lossy().into_owned(), ..Default::default() };
        for f in toml_files(&self.root.join("connections")) {
            match read_toml::<ConnectionDef>(&f) {
                Ok(c) => data.connections.push(c),
                Err(e) => tracing::warn!("skipping {}: {e}", f.display()),
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.root.join("mocks")) {
            let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("mock.toml").exists()).collect();
            dirs.sort();
            for d in dirs {
                match read_toml::<MockDef>(&d.join("mock.toml")) {
                    Ok(m) => data.mocks.push(m),
                    Err(e) => tracing::warn!("skipping {}: {e}", d.display()),
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.root.join("queries")) {
            let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
            for d in dirs {
                let id = d.file_name().unwrap().to_string_lossy().into_owned();
                let col = read_toml::<Collection>(&d.join("_collection.toml")).unwrap_or(Collection {
                    id: id.clone(),
                    name: id.clone(),
                    docs: String::new(),
                });
                for f in toml_files(&d) {
                    if f.file_name().is_some_and(|n| n == "_collection.toml") {
                        continue;
                    }
                    match read_toml::<SavedQuery>(&f) {
                        Ok(mut q) => {
                            q.collection = col.id.clone();
                            data.queries.push(q);
                        }
                        Err(e) => tracing::warn!("skipping {}: {e}", f.display()),
                    }
                }
                data.collections.push(Collection { id, ..col });
            }
        }
        Ok(data)
    }

    pub fn save_connection(&self, c: &ConnectionDef) -> Result<()> {
        check_id(&c.id)?;
        write_toml(&self.root.join("connections").join(format!("{}.toml", c.id)), c)
    }

    pub fn delete_connection(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let p = self.root.join("connections").join(format!("{id}.toml"));
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| CoreError::io(&p, e))?;
        }
        Ok(())
    }

    pub fn mock_dir(&self, id: &str) -> PathBuf {
        self.root.join("mocks").join(id)
    }

    pub fn mock_data_dir(&self, id: &str) -> PathBuf {
        self.mock_dir(id).join("data")
    }

    pub fn save_mock(&self, m: &MockDef) -> Result<()> {
        check_id(&m.id)?;
        write_toml(&self.mock_dir(&m.id).join("mock.toml"), m)
    }

    /// The seed a mock starts from: SQL or Redis commands in `seed.sql`; MongoDB Extended JSON,
    /// DynamoDB tables or Firestore documents in `seed.json`.
    pub fn seed_path(&self, id: &str, kind: MockKind) -> PathBuf {
        self.mock_dir(id).join(if matches!(kind, MockKind::Mongodb | MockKind::Dynamodb | MockKind::Firestore) {
            "seed.json"
        } else {
            "seed.sql"
        })
    }

    pub fn save_collection(&self, c: &Collection) -> Result<()> {
        check_id(&c.id)?;
        write_toml(&self.root.join("queries").join(&c.id).join("_collection.toml"), c)
    }

    pub fn delete_collection(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let d = self.root.join("queries").join(id);
        if d.exists() {
            std::fs::remove_dir_all(&d).map_err(|e| CoreError::io(&d, e))?;
        }
        Ok(())
    }

    pub fn save_query(&self, q: &SavedQuery) -> Result<()> {
        check_id(&q.collection)?;
        check_id(&q.id)?;
        let dir = self.root.join("queries").join(&q.collection);
        if !dir.join("_collection.toml").exists() {
            self.save_collection(&Collection { id: q.collection.clone(), name: q.collection.clone(), docs: String::new() })?;
        }
        write_toml(&dir.join(format!("{}.toml", q.id)), q)
    }

    pub fn snapshot_path(&self, id: &str) -> Result<PathBuf> {
        check_id(id)?;
        Ok(self.root.join("snapshots").join(format!("{id}.json")))
    }

    pub fn save_snapshot(&self, s: &crate::schema_diff::Snapshot) -> Result<()> {
        let path = self.snapshot_path(&s.id)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        let text = serde_json::to_string_pretty(s).map_err(|e| CoreError::invalid(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| CoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| CoreError::io(&path, e))
    }

    pub fn load_snapshot(&self, id: &str) -> Result<crate::schema_diff::Snapshot> {
        let path = self.snapshot_path(id)?;
        let text = std::fs::read_to_string(&path).map_err(|_| CoreError::not_found(format!("snapshot '{id}' not found")))?;
        serde_json::from_str(&text).map_err(|e| CoreError::invalid(format!("{}: {e}", path.display())))
    }

    /// Snapshots, newest first.
    pub fn list_snapshots(&self) -> Vec<crate::schema_diff::SnapshotInfo> {
        let mut out: Vec<crate::schema_diff::SnapshotInfo> = std::fs::read_dir(self.root.join("snapshots"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .filter_map(|t| serde_json::from_str::<crate::schema_diff::Snapshot>(&t).ok())
                    .map(|s| s.info())
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
        out
    }

    pub fn delete_snapshot(&self, id: &str) -> Result<()> {
        let p = self.snapshot_path(id)?;
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| CoreError::io(&p, e))?;
        }
        Ok(())
    }

    pub fn delete_query(&self, collection: &str, id: &str) -> Result<()> {
        check_id(collection)?;
        check_id(id)?;
        let p = self.root.join("queries").join(collection).join(format!("{id}.toml"));
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| CoreError::io(&p, e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_or_create(dir.path().join("ws"), Some("Demo")).unwrap();
        ws.save_connection(&ConnectionDef {
            id: "prod".into(),
            name: "Prod".into(),
            kind: DbKind::Postgres,
            host: "db".into(),
            port: 5432,
            user: "app".into(),
            database: Some("app".into()),
            path: None,
            tls: TlsMode::Require,
            color: Some("#e5484d".into()),
            read_only: true,
            ai_access: AiAccess::ReadOnly,
            group: None,
            ssh: None,
        })
        .unwrap();
        ws.save_mock(&MockDef {
            id: "local-pg".into(),
            name: "Local PG".into(),
            kind: MockKind::Postgres,
            host: "127.0.0.1".into(),
            port: 15432,
            auth: AuthMode::Any,
            database: "app".into(),
            persist: true,
            auto_start: false,
            description: None,
            faults: Default::default(),
            engine: Default::default(),
        })
        .unwrap();
        ws.save_query(&SavedQuery {
            id: "active-users".into(),
            name: "Active users".into(),
            collection: "reports".into(),
            connection: Some("prod".into()),
            tags: vec!["users".into()],
            docs: "# Active users\nUsers seen in 30 days.".into(),
            sql: "SELECT *\nFROM users\nWHERE last_seen > now() - interval '30 days'".into(),
            expect: Some(Expectation::Succeeds),
        })
        .unwrap();
        let data = ws.load().unwrap();
        assert_eq!(data.name, "Demo");
        assert_eq!(data.connections[0].tls, TlsMode::Require);
        assert_eq!(data.mocks[0].port, 15432);
        assert_eq!(data.collections[0].id, "reports");
        assert!(data.queries[0].sql.contains("interval '30 days'"));
        assert_eq!(unique_id("Reports", &["reports".into()]), "reports-2");
    }
}
