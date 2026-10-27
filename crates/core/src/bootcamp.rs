//! Bootcamp: the built-in course. It runs in a workspace of its own (inside the app directory) so
//! its lab mock servers never touch the user's projects; progress is kept next to the settings.

use std::path::{Path, PathBuf};

use twinbase_mock::{AuthMode, MockKind};

use crate::error::{CoreError, Result};
use crate::model::{MockDef, MockEngine, Workspace};

/// Lab servers every lesson can rely on: (id, name, kind, first port to try).
pub const LAB_MOCKS: &[(&str, &str, MockKind, u16)] = &[
    ("lab-postgres", "Lab PostgreSQL", MockKind::Postgres, 25432),
    ("lab-redis", "Lab Redis", MockKind::Redis, 26379),
    ("lab-mongodb", "Lab MongoDB", MockKind::Mongodb, 27117),
];

const README: &str = "# Bootcamp\n\nThis workspace belongs to the TwinBase Bootcamp. Its lab servers are reset by the lessons, \
so keep your own work in another workspace.\n\n```mermaid\nflowchart LR\n  L[Lesson] --> Q[Quiz]\n  Q --> X[Lab on a mock server]\n  X --> B[XP and badges]\n```\n";

pub fn workspace_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("Bootcamp")
}

pub fn progress_path(app_dir: &Path) -> PathBuf {
    app_dir.join("bootcamp-progress.json")
}

fn free_port(start: u16, taken: &[u16]) -> u16 {
    (start..start.saturating_add(200)).find(|p| !taken.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(0)
}

/// Create the bootcamp workspace and its lab mocks when missing (idempotent).
pub fn prepare(dir: &Path) -> Result<()> {
    let ws = Workspace::open_or_create(dir, Some("Bootcamp"))?;
    if !ws.readme_path().exists() {
        ws.save_readme(README)?;
    }
    let data = ws.load()?;
    let mut taken: Vec<u16> = data.mocks.iter().map(|m| m.port).collect();
    for (id, name, kind, start) in LAB_MOCKS {
        if data.mocks.iter().any(|m| m.id == *id) {
            continue;
        }
        let port = free_port(*start, &taken);
        if port == 0 {
            return Err(CoreError::conflict(format!("no free port near {start} for the {name} server")));
        }
        taken.push(port);
        ws.save_mock(&MockDef {
            id: (*id).into(),
            name: (*name).into(),
            kind: *kind,
            host: "127.0.0.1".into(),
            port,
            auth: AuthMode::Any,
            database: "lab".into(),
            persist: true,
            auto_start: true,
            description: Some("Used by Bootcamp labs; lessons reset its data.".into()),
            docs: String::new(),
            faults: Default::default(),
            engine: MockEngine::Twinlite,
        })?;
    }
    Ok(())
}

/// Saved progress (opaque JSON owned by the UI), or `{}`.
pub fn load_progress(app_dir: &Path) -> serde_json::Value {
    std::fs::read_to_string(progress_path(app_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn save_progress(app_dir: &Path, progress: &serde_json::Value) -> Result<()> {
    if !progress.is_object() {
        return Err(CoreError::invalid("progress must be an object"));
    }
    let text = serde_json::to_string_pretty(progress)?;
    if text.len() > 2 << 20 {
        return Err(CoreError::invalid("progress is limited to 2 MB"));
    }
    let p = progress_path(app_dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| CoreError::io(&tmp, e))?;
    std::fs::rename(&tmp, &p).map_err(|e| CoreError::io(&p, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_an_isolated_workspace_once() {
        let dir = tempfile::tempdir().unwrap();
        let ws_dir = workspace_dir(dir.path());
        prepare(&ws_dir).unwrap();
        prepare(&ws_dir).unwrap();
        let data = Workspace::open_or_create(&ws_dir, None).unwrap().load().unwrap();
        assert_eq!(data.mocks.len(), LAB_MOCKS.len());
        assert!(data.mocks.iter().all(|m| m.auto_start && m.port != 0));
        save_progress(dir.path(), &serde_json::json!({ "xp": 40 })).unwrap();
        assert_eq!(load_progress(dir.path())["xp"], 40);
        assert!(save_progress(dir.path(), &serde_json::json!([1])).is_err());
    }
}
