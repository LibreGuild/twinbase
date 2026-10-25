//! Machine-local application state: settings and secrets. Never stored in workspaces.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `system`, `light` or `dark`.
    pub theme: String,
    pub font_size: u32,
    /// UI typeface: `inter`, `atkinson`, `plex` or `system`.
    pub font_family: String,
    /// Code typeface: `jetbrains-mono`, `fira-code` or `system`.
    pub mono_font: String,
    /// Maximum rows fetched by the SQL editor per statement.
    pub row_limit: u64,
    pub page_size: u64,
    pub recent_workspaces: Vec<String>,
    pub last_workspace: Option<String>,
    pub mcp: McpSettings,
    pub confirm_destructive: bool,
    /// Bin folder of a PostgreSQL installation for native-engine mocks (empty: detect one).
    pub postgres_bin: String,
    /// Oracle Instant Client folder for Oracle connections (empty: detect one).
    pub oracle_client: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpSettings {
    pub enabled: bool,
    pub port: u16,
    /// Bearer token agents must present.
    pub token: String,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self { enabled: true, port: 47800, token: new_token() }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            font_size: 13,
            font_family: "inter".into(),
            mono_font: "jetbrains-mono".into(),
            row_limit: 1000,
            page_size: 200,
            recent_workspaces: vec![],
            last_workspace: None,
            mcp: McpSettings::default(),
            confirm_destructive: true,
            postgres_bin: String::new(),
            oracle_client: String::new(),
        }
    }
}

pub fn new_token() -> String {
    format!("tb_{}", uuid::Uuid::new_v4().simple())
}

/// Default directory for app-local files.
pub fn app_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TWINBASE_HOME") {
        return PathBuf::from(dir);
    }
    default_app_dir().unwrap_or_else(|| PathBuf::from(".").join("TwinBase"))
}

/// The per-user app directory (`~/Library/Application Support/TwinBase`, `%APPDATA%\TwinBase`, …).
fn default_app_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("TwinBase"))
}

pub fn default_workspace_dir() -> PathBuf {
    // An isolated app directory (tests, CI, portable installs) keeps its default workspace inside it.
    if let Ok(dir) = std::env::var("TWINBASE_HOME") {
        return PathBuf::from(dir).join("Default workspace");
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("TwinBase").join("Default")
}

/// Where passwords and SSH secrets live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBackend {
    /// macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
    Keychain,
    /// `secrets.toml` in the app directory (owner-only permissions).
    File,
}

impl SecretBackend {
    pub fn name(self) -> &'static str {
        match self {
            SecretBackend::Keychain => "keychain",
            SecretBackend::File => "file",
        }
    }
}

const KEYCHAIN_SERVICE: &str = "TwinBase";

pub struct LocalStore {
    dir: PathBuf,
    backend: SecretBackend,
}

impl LocalStore {
    /// The OS keychain is used only for the real, per-user app directory; any other directory
    /// (`TWINBASE_HOME`, tests, CI, portable installs) keeps secrets in its own file so it never
    /// touches the user's keychain. `TWINBASE_SECRET_STORE=file|keychain` overrides.
    pub fn new(dir: PathBuf) -> Self {
        let forced = std::env::var("TWINBASE_SECRET_STORE").ok();
        let backend = match forced.as_deref() {
            Some("file") => SecretBackend::File,
            Some("keychain") => SecretBackend::Keychain,
            _ if std::env::var_os("TWINBASE_HOME").is_none() && Some(&dir) == default_app_dir().as_ref() => SecretBackend::Keychain,
            _ => SecretBackend::File,
        };
        Self { dir, backend }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn secret_backend(&self) -> SecretBackend {
        self.backend
    }

    pub fn load_settings(&self) -> Settings {
        let path = self.dir.join("settings.toml");
        let loaded = std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str::<Settings>(&t).ok());
        match loaded {
            Some(s) => s,
            None => {
                let s = Settings::default();
                let _ = self.save_settings(&s);
                s
            }
        }
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        let text = toml::to_string_pretty(s).map_err(|e| CoreError::invalid(e.to_string()))?;
        write_private(&self.dir.join("settings.toml"), &text)
    }

    fn secrets_path(&self) -> PathBuf {
        self.dir.join("secrets.toml")
    }

    fn load_file_secrets(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(self.secrets_path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_file_secrets(&self, all: &BTreeMap<String, String>) -> Result<()> {
        let text = toml::to_string(all).map_err(|e| CoreError::invalid(e.to_string()))?;
        write_private(&self.secrets_path(), &text)
    }

    fn file_secret(&self, key: &str) -> Option<String> {
        self.load_file_secrets().get(key).cloned()
    }

    fn set_file_secret(&self, key: &str, value: Option<&str>) -> Result<()> {
        let mut all = self.load_file_secrets();
        let changed = match value {
            Some(v) => all.insert(key.to_owned(), v.to_owned()).as_deref() != Some(v),
            None => all.remove(key).is_some(),
        };
        if changed { self.save_file_secrets(&all) } else { Ok(()) }
    }

    fn entry(key: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN_SERVICE, key).ok()
    }

    /// Secrets are keyed by workspace path and connection id.
    pub fn secret(&self, workspace: &str, id: &str) -> Option<String> {
        let key = format!("{workspace}::{id}");
        if self.backend == SecretBackend::Keychain
            && let Some(entry) = Self::entry(&key)
        {
            match entry.get_password() {
                Ok(v) => return Some(v),
                Err(keyring::Error::NoEntry) => {
                    // Move secrets saved before the keychain was used.
                    let legacy = self.file_secret(&key)?;
                    if entry.set_password(&legacy).is_ok() {
                        let _ = self.set_file_secret(&key, None);
                    }
                    return Some(legacy);
                }
                Err(e) => tracing::warn!("keychain unavailable ({e}); using the secrets file"),
            }
        }
        self.file_secret(&key)
    }

    pub fn set_secret(&self, workspace: &str, id: &str, value: Option<&str>) -> Result<()> {
        let key = format!("{workspace}::{id}");
        if self.backend == SecretBackend::Keychain
            && let Some(entry) = Self::entry(&key)
        {
            let stored = match value {
                Some(v) => entry.set_password(v),
                None => match entry.delete_credential() {
                    Err(keyring::Error::NoEntry) => Ok(()),
                    other => other,
                },
            };
            match stored {
                Ok(()) => {
                    // Never leave a stale copy in the file.
                    return self.set_file_secret(&key, None);
                }
                Err(e) => tracing::warn!("keychain unavailable ({e}); using the secrets file"),
            }
        }
        self.set_file_secret(&key, value)
    }
}

/// Write a file readable only by its owner, atomically (no moment with wider permissions).
fn write_private(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| CoreError::io(&tmp, e))?;
        f.write_all(text.as_bytes()).map_err(|e| CoreError::io(&tmp, e))?;
        f.sync_all().map_err(|e| CoreError::io(&tmp, e))?;
    }
    restrict(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))
}

/// Owner-only permissions for files holding secrets.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_directories_never_use_the_keychain_and_files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        assert_eq!(store.secret_backend(), SecretBackend::File);
        store.set_secret("/ws", "db", Some("s3cret")).unwrap();
        assert_eq!(store.secret("/ws", "db").as_deref(), Some("s3cret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets.toml")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        store.set_secret("/ws", "db", None).unwrap();
        assert_eq!(store.secret("/ws", "db"), None);
    }

    /// Round trip through the real OS keychain. Ignored by default: it writes to the user's
    /// keychain. Run with `cargo test -p twinbase-core -- --ignored keychain`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore { dir: dir.path().to_path_buf(), backend: SecretBackend::Keychain };
        let ws = format!("/twinbase-keychain-test-{}", uuid::Uuid::new_v4());
        store.set_secret(&ws, "db", Some("from-keychain")).unwrap();
        assert_eq!(store.secret(&ws, "db").as_deref(), Some("from-keychain"));
        assert!(
            !dir.path().join("secrets.toml").exists()
                || !std::fs::read_to_string(dir.path().join("secrets.toml")).unwrap().contains("from-keychain")
        );
        store.set_secret(&ws, "db", None).unwrap();
        assert_eq!(store.secret(&ws, "db"), None);
    }
}
