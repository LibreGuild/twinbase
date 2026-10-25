//! The engine owns a storage directory and engine-wide state shared by all sessions.
//!
//! Layout on disk:
//! * PostgreSQL: `<root>/<database>/<schema>.sqlite` (one directory per database).
//! * MySQL: `<root>/<database>.sqlite` (MySQL databases are schemas).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::Dialect;
use crate::error::{EngineError, Result, state};
use crate::functions::Sequences;
use crate::session::Session;

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// PostgreSQL: create a database on first connection instead of failing.
    pub auto_create_databases: bool,
    /// Version string reported by `version()` / `@@version`.
    pub version: Option<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self { auto_create_databases: true, version: None }
    }
}

pub(crate) struct EngineInner {
    pub root: PathBuf,
    pub dialect: Dialect,
    pub options: EngineOptions,
    pub sequences: Arc<Sequences>,
    pub version: String,
    next_connection_id: AtomicU32,
    /// Bumped whenever committed catalog metadata changes.
    pub catalog_gen: AtomicU64,
    /// Bumped whenever schema files (PostgreSQL schemas / MySQL databases) are added or removed.
    pub layout_gen: AtomicU64,
    _temp: Option<tempdir::TempDir>,
}

/// A database engine instance (one per mock server).
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<EngineInner>,
}

mod tempdir {
    use std::path::{Path, PathBuf};

    /// Minimal self-deleting temporary directory.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(prefix: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path)?;
            Ok(TempDir(path))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Encode a schema/database name into a safe file name.
pub(crate) fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn decode_name(file: &str) -> String {
    let bytes = file.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&file[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `*.sqlite` files in a directory, as (decoded name, path), sorted by name.
pub(crate) fn list_schema_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    let stem = p.file_name()?.to_str()?.strip_suffix(".sqlite")?.to_owned();
                    Some((decode_name(&stem), p))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

impl Engine {
    /// Open (or create) an engine storing its data under `root`.
    pub fn open(root: impl Into<PathBuf>, dialect: Dialect, options: EngineOptions) -> Result<Engine> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| EngineError::internal(format!("cannot create {}: {e}", root.display())))?;
        Ok(Self::build(root, dialect, options, None))
    }

    /// An engine backed by a temporary directory that is removed when the engine is dropped.
    pub fn temporary(dialect: Dialect) -> Result<Engine> {
        let dir = tempdir::TempDir::new("twinbase").map_err(|e| EngineError::internal(e.to_string()))?;
        let root = dir.path().to_path_buf();
        Ok(Self::build(root, dialect, EngineOptions::default(), Some(dir)))
    }

    fn build(root: PathBuf, dialect: Dialect, options: EngineOptions, temp: Option<tempdir::TempDir>) -> Engine {
        let version = options.version.clone().unwrap_or_else(|| match dialect {
            Dialect::MySql => "8.0.36-TwinBase".to_owned(),
            Dialect::MsSql => "Microsoft SQL Server 2022 (RTM) - 16.0.1000.6 (X64) TwinBase mock on TwinLite".to_owned(),
            _ => "PostgreSQL 16.4 (TwinBase mock) on TwinLite, compiled by rustc, 64-bit".to_owned(),
        });
        Engine {
            inner: Arc::new(EngineInner {
                root,
                dialect,
                options,
                sequences: Arc::new(Sequences::default()),
                version,
                next_connection_id: AtomicU32::new(1),
                catalog_gen: AtomicU64::new(1),
                layout_gen: AtomicU64::new(1),
                _temp: temp,
            }),
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.inner.dialect
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub(crate) fn next_connection_id(&self) -> u32 {
        self.inner.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Directory holding the schema files of a session's database.
    pub(crate) fn database_dir(&self, database: &str) -> PathBuf {
        match self.inner.dialect {
            Dialect::MySql => self.inner.root.clone(),
            _ => self.inner.root.join(encode_name(database)),
        }
    }

    /// Databases that exist (PostgreSQL directories / MySQL files).
    pub fn databases(&self) -> Vec<String> {
        match self.inner.dialect {
            Dialect::MySql => list_schema_files(&self.inner.root).into_iter().map(|(n, _)| n).collect(),
            _ => {
                let mut v: Vec<String> = std::fs::read_dir(&self.inner.root)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .filter(|e| e.path().is_dir())
                            .filter_map(|e| e.file_name().to_str().map(decode_name))
                            .collect()
                    })
                    .unwrap_or_default();
                v.sort();
                v
            }
        }
    }

    /// Create a PostgreSQL database (directory with a `public` schema).
    pub fn create_database(&self, name: &str) -> Result<()> {
        if self.inner.dialect == Dialect::MySql {
            return Err(EngineError::internal("use CREATE DATABASE in a session for MySQL"));
        }
        let dir = self.database_dir(name);
        if dir.exists() {
            return Err(EngineError::new(state::DUPLICATE_DATABASE, format!("database \"{name}\" already exists")));
        }
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::internal(e.to_string()))?;
        Ok(())
    }

    pub fn drop_database(&self, name: &str) -> Result<()> {
        let dir = self.database_dir(name);
        match self.inner.dialect {
            Dialect::MySql => remove_schema_file(&dir.join(format!("{}.sqlite", encode_name(name)))),
            _ => {
                if !dir.exists() {
                    return Err(EngineError::new(state::INVALID_CATALOG_NAME, format!("database \"{name}\" does not exist")));
                }
                std::fs::remove_dir_all(&dir).map_err(|e| EngineError::internal(e.to_string()))
            }
        }?;
        self.inner.layout_gen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Open a session (one per client connection).
    pub fn session(&self, database: Option<&str>, user: &str) -> Result<Session> {
        Session::open(self.clone(), database, user)
    }
}

pub(crate) fn remove_schema_file(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| EngineError::internal(format!("cannot remove {}: {e}", p.display())))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for n in ["public", "My Schema", "a/b", "ünï"] {
            assert_eq!(decode_name(&encode_name(n)), n);
        }
    }
}
