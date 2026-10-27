//! Documentation at every level, as Markdown kept in the workspace so it is reviewed in Git:
//!
//! ```text
//! <workspace>/README.md                       the workspace
//! connections/<id>.toml  `docs = """…"""`     a connection
//! mocks/<id>/mock.toml   `docs = """…"""`     a mock server
//! connections/<id>/docs/<object>.toml         a table, collection, index or key pattern of a
//! mocks/<id>/docs/<object>.toml               connection or mock, with per-column docs
//! ```
//!
//! Saved queries and collections keep their docs in their own files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::model::{Workspace, WorkspaceData, check_id, mock_connection_id, write_toml};

/// Docs of one object (table, view, collection, index, key pattern…) and its columns or fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectDocs {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub docs: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, String>,
}

impl ObjectDocs {
    pub fn is_empty(&self) -> bool {
        self.docs.trim().is_empty() && self.columns.values().all(|v| v.trim().is_empty())
    }
}

/// What a docs request is about: a saved connection or a mock server.
#[derive(Debug, Clone, PartialEq)]
pub enum DocsTarget {
    Connection(String),
    Mock(String),
}

/// Resolve a connection id, mock id or mock connection id (`mock:<id>`).
pub fn resolve_target(data: &WorkspaceData, id: &str) -> Result<DocsTarget> {
    if let Some(m) = data.mocks.iter().find(|m| m.id == id || mock_connection_id(&m.id) == id) {
        return Ok(DocsTarget::Mock(m.id.clone()));
    }
    if data.connections.iter().any(|c| c.id == id) {
        return Ok(DocsTarget::Connection(id.to_owned()));
    }
    Err(CoreError::not_found(format!("no connection or mock server '{id}'")))
}

/// Object names become file names: keep letters, digits, `.`, `_` and `-`, percent-encode the rest.
pub fn object_file_name(object: &str) -> Result<String> {
    if object.trim().is_empty() || object.len() > 400 || object.chars().any(char::is_control) {
        return Err(CoreError::invalid("invalid object name"));
    }
    let mut out = String::new();
    for (i, c) in object.chars().enumerate() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || (c == '.' && i > 0) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Ok(out)
}

fn object_name(file_stem: &str) -> String {
    let bytes = file_stem.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&file_stem[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Workspace {
    pub fn readme_path(&self) -> PathBuf {
        self.root.join("README.md")
    }

    /// The workspace README, or "" when there is none.
    pub fn readme(&self) -> String {
        std::fs::read_to_string(self.readme_path()).unwrap_or_default()
    }

    pub fn save_readme(&self, text: &str) -> Result<()> {
        let p = self.readme_path();
        if text.trim().is_empty() {
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| CoreError::io(&p, e))?;
            }
            return Ok(());
        }
        std::fs::write(&p, text).map_err(|e| CoreError::io(&p, e))
    }

    fn object_docs_dir(&self, target: &DocsTarget) -> Result<PathBuf> {
        Ok(match target {
            DocsTarget::Connection(id) => {
                check_id(id)?;
                self.root.join("connections").join(id).join("docs")
            }
            DocsTarget::Mock(id) => {
                check_id(id)?;
                self.mock_dir(id).join("docs")
            }
        })
    }

    pub fn object_docs(&self, target: &DocsTarget, object: &str) -> Result<ObjectDocs> {
        let p = self.object_docs_dir(target)?.join(format!("{}.toml", object_file_name(object)?));
        read_docs(&p)
    }

    /// Save (or, when empty, remove) the docs of one object.
    pub fn save_object_docs(&self, target: &DocsTarget, object: &str, docs: &ObjectDocs) -> Result<()> {
        let dir = self.object_docs_dir(target)?;
        let p = dir.join(format!("{}.toml", object_file_name(object)?));
        if docs.is_empty() {
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| CoreError::io(&p, e))?;
            }
            return Ok(());
        }
        let mut clean = docs.clone();
        clean.columns.retain(|_, v| !v.trim().is_empty());
        write_toml(&p, &clean)
    }

    /// Every documented object of a connection or mock, by object name.
    pub fn list_object_docs(&self, target: &DocsTarget) -> Result<BTreeMap<String, ObjectDocs>> {
        let dir = self.object_docs_dir(target)?;
        let mut out = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "toml")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && let Ok(d) = read_docs(&p)
                {
                    out.insert(object_name(stem), d);
                }
            }
        }
        Ok(out)
    }

    /// Remove the docs folder of a deleted connection.
    pub fn delete_connection_docs(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let d = self.root.join("connections").join(id);
        if d.is_dir() {
            std::fs::remove_dir_all(&d).map_err(|e| CoreError::io(&d, e))?;
        }
        Ok(())
    }
}

fn read_docs(p: &Path) -> Result<ObjectDocs> {
    match std::fs::read_to_string(p) {
        Ok(text) => toml::from_str(&text).map_err(|e| CoreError::invalid(format!("{}: {e}", p.display()))),
        Err(_) => Ok(ObjectDocs::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_round_trip_through_safe_file_names() {
        for name in ["public.users", "Orders", "users/alice/posts", "../etc", ".hidden", "user:*", "表"] {
            let f = object_file_name(name).unwrap();
            assert!(!f.contains('/') && !f.contains('\\') && !f.contains(':') && !f.starts_with('.') && !f.contains(".."), "{f}");
            assert_eq!(object_name(&f), name);
        }
        assert!(object_file_name("").is_err());
    }

    #[test]
    fn saves_lists_and_removes_object_docs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_or_create(dir.path().join("ws"), Some("t")).unwrap();
        let t = DocsTarget::Connection("prod".into());
        let mut d = ObjectDocs { docs: "# Users\nOne row per account.".into(), ..Default::default() };
        d.columns.insert("email".into(), "Unique, lower-case.".into());
        d.columns.insert("blank".into(), " ".into());
        ws.save_object_docs(&t, "public.users", &d).unwrap();
        let back = ws.object_docs(&t, "public.users").unwrap();
        assert_eq!(back.docs, d.docs);
        assert_eq!(back.columns.len(), 1, "empty column docs are dropped");
        assert_eq!(ws.list_object_docs(&t).unwrap().keys().collect::<Vec<_>>(), vec!["public.users"]);
        ws.save_object_docs(&t, "public.users", &ObjectDocs::default()).unwrap();
        assert!(ws.list_object_docs(&t).unwrap().is_empty());
        ws.save_readme("# Team workspace").unwrap();
        assert_eq!(ws.readme(), "# Team workspace");
        // The connection loader still only reads `connections/*.toml`.
        assert!(ws.load().unwrap().connections.is_empty());
    }
}
