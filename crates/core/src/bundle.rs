//! Export/import bundles (`.twinbase` zip files) to share mocks, queries and connections.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::model::{Workspace, WorkspaceData, check_id, unique_id, write_toml};

/// Most bytes an import writes (a bundle is a zip: guards against archives that expand without end).
const MAX_IMPORT_BYTES: u64 = 4 << 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub mocks: Vec<String>,
    /// Include mock data (not just definitions and seed scripts).
    #[serde(default = "yes")]
    pub include_data: bool,
    #[serde(default)]
    pub collections: Vec<String>,
    #[serde(default)]
    pub connections: Vec<String>,
    #[serde(default)]
    pub snapshots: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub created_at: String,
    pub source_workspace: String,
    pub mocks: Vec<String>,
    pub collections: Vec<String>,
    pub connections: Vec<String>,
    #[serde(default)]
    pub snapshots: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportSummary {
    pub mocks: Vec<String>,
    pub collections: Vec<String>,
    pub connections: Vec<String>,
    pub snapshots: Vec<String>,
    /// Things the import changed for safety, to show the user.
    pub notes: Vec<String>,
}

fn io(e: impl std::fmt::Display) -> CoreError {
    CoreError::new("io", e.to_string())
}

/// A consistent copy of a SQLite database, including changes still in its WAL, even while a
/// mock server is writing to it.
fn sqlite_snapshot(path: &Path) -> Result<Vec<u8>> {
    let tmp = tempfile::Builder::new().prefix("twinbase-export").suffix(".sqlite").tempfile().map_err(io)?;
    let out = tmp.path().with_extension("snapshot");
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| CoreError::new("io", format!("cannot open {}: {e}", path.display())))?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(io)?;
    conn.execute("VACUUM INTO ?1", [out.to_string_lossy()])
        .map_err(|e| CoreError::new("io", format!("cannot snapshot {}: {e}", path.display())))?;
    drop(conn);
    let bytes = std::fs::read(&out).map_err(|e| CoreError::io(&out, e));
    let _ = std::fs::remove_file(&out);
    bytes
}

fn add_dir(zip: &mut zip::ZipWriter<std::fs::File>, src: &Path, prefix: &str) -> Result<()> {
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let Ok(rd) = std::fs::read_dir(src) else { return Ok(()) };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if path.is_dir() {
            add_dir(zip, &path, &name)?;
        } else {
            let fname = entry.file_name().to_string_lossy().into_owned();
            // WAL/SHM are folded into the snapshot below; temp files are never exported.
            if fname.ends_with("-shm") || fname.ends_with("-wal") || fname.ends_with("-journal") || fname.ends_with(".tmp") {
                continue;
            }
            let bytes = if fname.ends_with(".sqlite") {
                sqlite_snapshot(&path)?
            } else {
                match std::fs::read(&path) {
                    Ok(b) => b,
                    // Removed while we were listing (e.g. a mock rewrote its snapshot).
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(CoreError::io(&path, e)),
                }
            };
            zip.start_file(name, opts).map_err(io)?;
            zip.write_all(&bytes).map_err(io)?;
        }
    }
    Ok(())
}

/// Write a bundle. SQL mock data is snapshotted consistently even while the mock runs.
pub fn export(ws: &Workspace, data: &WorkspaceData, sel: &Selection, out: &Path) -> Result<Manifest> {
    let file = std::fs::File::create(out).map_err(|e| CoreError::io(out, e))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for id in &sel.mocks {
        if !data.mocks.iter().any(|m| &m.id == id) {
            return Err(CoreError::not_found(format!("mock server '{id}' not found")));
        }
        let dir = ws.mock_dir(id);
        for f in ["mock.toml", "seed.sql", "seed.json"] {
            let p = dir.join(f);
            if p.exists() {
                zip.start_file(format!("mocks/{id}/{f}"), opts).map_err(io)?;
                zip.write_all(&std::fs::read(&p).map_err(|e| CoreError::io(&p, e))?).map_err(io)?;
            }
        }
        add_dir(&mut zip, &dir.join("docs"), &format!("mocks/{id}/docs"))?;
        if sel.include_data {
            add_dir(&mut zip, &dir.join("data"), &format!("mocks/{id}/data"))?;
        }
    }
    for id in &sel.collections {
        add_dir(&mut zip, &ws.root.join("queries").join(id), &format!("queries/{id}"))?;
    }
    for id in &sel.connections {
        let p = ws.root.join("connections").join(format!("{id}.toml"));
        if p.exists() {
            zip.start_file(format!("connections/{id}.toml"), opts).map_err(io)?;
            zip.write_all(&std::fs::read(&p).map_err(|e| CoreError::io(&p, e))?).map_err(io)?;
        }
        add_dir(&mut zip, &ws.root.join("connections").join(id).join("docs"), &format!("connections/{id}/docs"))?;
    }
    for id in &sel.snapshots {
        let p = ws.snapshot_path(id)?;
        if p.exists() {
            zip.start_file(format!("snapshots/{id}.json"), opts).map_err(io)?;
            zip.write_all(&std::fs::read(&p).map_err(|e| CoreError::io(&p, e))?).map_err(io)?;
        }
    }
    let manifest = Manifest {
        format: "twinbase-bundle".into(),
        version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        source_workspace: data.name.clone(),
        mocks: sel.mocks.clone(),
        collections: sel.collections.clone(),
        connections: sel.connections.clone(),
        snapshots: sel.snapshots.clone(),
    };
    zip.start_file("manifest.json", opts).map_err(io)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes()).map_err(io)?;
    zip.finish().map_err(io)?;
    Ok(manifest)
}

/// Read a bundle's manifest without importing it.
pub fn inspect(path: &Path) -> Result<Manifest> {
    let file = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(io)?;
    let mut text = String::new();
    zip.by_name("manifest.json")
        .map_err(|_| CoreError::invalid("not a TwinBase bundle (manifest.json missing)"))?
        .read_to_string(&mut text)
        .map_err(io)?;
    let m: Manifest = serde_json::from_str(&text)?;
    if m.format != "twinbase-bundle" {
        return Err(CoreError::invalid("not a TwinBase bundle"));
    }
    for id in m.mocks.iter().chain(&m.collections).chain(&m.connections).chain(&m.snapshots) {
        check_id(id)?;
    }
    Ok(m)
}

/// Import a bundle. Items whose ids already exist are imported under new ids, never overwritten.
pub fn import(ws: &Workspace, data: &WorkspaceData, path: &Path) -> Result<ImportSummary> {
    let manifest = inspect(path)?;
    let file = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(io)?;
    let mut taken_mocks: Vec<String> = data.mocks.iter().map(|m| m.id.clone()).collect();
    let mut taken_cols: Vec<String> = data.collections.iter().map(|c| c.id.clone()).collect();
    let mut taken_conns: Vec<String> = data.connections.iter().map(|c| c.id.clone()).collect();
    let mut taken_snaps: Vec<String> = ws.list_snapshots().into_iter().map(|s| s.id).collect();
    let mut rename = |kind: &str, old: &str| -> String {
        let taken = match kind {
            "mocks" => &mut taken_mocks,
            "queries" => &mut taken_cols,
            "snapshots" => &mut taken_snaps,
            _ => &mut taken_conns,
        };
        let new = unique_id(old, taken);
        taken.push(new.clone());
        new
    };
    let mut map: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();
    let mut summary = ImportSummary { mocks: vec![], collections: vec![], connections: vec![], snapshots: vec![], notes: vec![] };
    for id in &manifest.mocks {
        let new = rename("mocks", id);
        summary.mocks.push(new.clone());
        map.insert(("mocks".into(), id.clone()), new);
    }
    for id in &manifest.collections {
        let new = rename("queries", id);
        summary.collections.push(new.clone());
        map.insert(("queries".into(), id.clone()), new);
    }
    for id in &manifest.connections {
        let new = rename("connections", id);
        summary.connections.push(new.clone());
        map.insert(("connections".into(), id.clone()), new);
    }
    for id in &manifest.snapshots {
        let new = rename("snapshots", id);
        summary.snapshots.push(new.clone());
        map.insert(("snapshots".into(), id.clone()), new);
    }
    let mut written: u64 = 0;
    let mut skipped_cluster = false;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(io)?;
        // Names that aren't valid UTF-8 can't be ours; skip them.
        let Ok(name) = entry.name().map(|n| n.into_owned()) else { continue };
        if name == "manifest.json" || entry.is_dir() || entry.enclosed_name().is_none() {
            continue;
        }
        // Every part must be a plain name: no traversal, drive letters or Windows separators.
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() < 2
            || parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':']) || p.chars().any(char::is_control))
        {
            continue;
        }
        // A real PostgreSQL cluster (native engine) is never imported: its configuration can run
        // programs when the server starts. The mock is rebuilt from its seed instead.
        if parts[0] == "mocks" && parts.get(2) == Some(&"data") && parts.get(3) == Some(&"pgdata") {
            skipped_cluster = true;
            continue;
        }
        let kind = parts[0];
        let (old_id, rest): (String, Vec<&str>) = if kind == "connections" && parts.len() == 2 {
            (parts[1].trim_end_matches(".toml").to_owned(), vec![])
        } else if kind == "snapshots" {
            (parts[1].trim_end_matches(".json").to_owned(), vec![])
        } else {
            (parts[1].to_owned(), parts[2..].to_vec())
        };
        let Some(new_id) = map.get(&(kind.to_owned(), old_id.clone())) else { continue };
        let target: PathBuf = match kind {
            "connections" if rest.is_empty() => ws.root.join("connections").join(format!("{new_id}.toml")),
            // Object docs of a connection: only `connections/<id>/docs/<object>.toml`.
            "connections" if rest.len() == 2 && rest[0] == "docs" && rest[1].ends_with(".toml") => {
                ws.root.join("connections").join(new_id).join("docs").join(rest[1])
            }
            "snapshots" => ws.root.join("snapshots").join(format!("{new_id}.json")),
            "mocks" | "queries" => {
                let mut p = ws.root.join(kind).join(new_id);
                for r in &rest {
                    p = p.join(r);
                }
                p
            }
            _ => continue,
        };
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| CoreError::io(&target, e))?;
        let left = MAX_IMPORT_BYTES - written;
        let n = std::io::copy(&mut (&mut entry).take(left + 1), &mut out).map_err(io)?;
        written += n;
        if n > left {
            drop(out);
            let _ = std::fs::remove_file(&target);
            return Err(CoreError::invalid(format!("the bundle expands to more than {} GB; import stopped", MAX_IMPORT_BYTES >> 30)));
        }
    }
    if skipped_cluster {
        summary.notes.push("PostgreSQL server data in the bundle was not imported; those mocks start from their seed.".into());
    }
    // Rewrite ids inside definitions, and move mocks off ports this workspace already uses.
    let mut used_ports: Vec<u16> = data.mocks.iter().map(|m| m.port).collect();
    for (new, old) in summary.mocks.iter().zip(&manifest.mocks) {
        let p = ws.mock_dir(new).join("mock.toml");
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(mut def) = toml::from_str::<crate::model::MockDef>(&text)
        {
            def.id = new.clone();
            if new != old {
                def.name = format!("{} (imported)", def.name);
            }
            // A seed runs with full rights on a real PostgreSQL server (COPY … PROGRAM runs
            // commands), so imported mocks start on TwinLite until the user chooses otherwise.
            if def.engine == crate::model::MockEngine::Native {
                def.engine = crate::model::MockEngine::Twinlite;
                summary.notes.push(format!(
                    "Mock '{}' uses the TwinLite engine after import; switch it to real PostgreSQL in its settings if you trust its seed.",
                    def.name
                ));
            }
            if used_ports.contains(&def.port) {
                let mut port = def.port.saturating_add(1).max(1024);
                while used_ports.contains(&port) || std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
                    port = port.checked_add(1).unwrap_or(1024);
                }
                def.port = port;
            }
            used_ports.push(def.port);
            write_toml(&p, &def)?;
        }
    }
    for new in &summary.connections {
        let p = ws.root.join("connections").join(format!("{new}.toml"));
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(mut def) = toml::from_str::<crate::model::ConnectionDef>(&text)
        {
            def.id = new.clone();
            write_toml(&p, &def)?;
        }
    }
    for new in &summary.snapshots {
        if let Ok(mut s) = ws.load_snapshot(new) {
            s.id = new.clone();
            ws.save_snapshot(&s)?;
        }
    }
    for new in &summary.collections {
        let dir = ws.root.join("queries").join(new);
        let col = dir.join("_collection.toml");
        if let Ok(text) = std::fs::read_to_string(&col)
            && let Ok(mut c) = toml::from_str::<crate::model::Collection>(&text)
        {
            c.id = new.clone();
            write_toml(&col, &c)?;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(dir: &Path, manifest_ids: &[&str], files: &[(&str, &str)]) -> PathBuf {
        let path = dir.join("b.twinbase");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        let manifest = serde_json::json!({
            "format": "twinbase-bundle", "version": 1, "created_at": "", "source_workspace": "evil",
            "mocks": manifest_ids, "collections": ["q"], "connections": [], "snapshots": []
        });
        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();
        for (name, body) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn imports_refuse_traversal_and_untrusted_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_or_create(dir.path().join("ws"), Some("t")).unwrap();
        let data = ws.load().unwrap();
        // Ids that could leave the workspace are refused outright.
        let bad = bundle(dir.path(), &["../x"], &[]);
        assert!(import(&ws, &data, &bad).is_err());
        let mock = "id = \"m\"\nname = \"Native\"\nkind = \"postgres\"\nhost = \"127.0.0.1\"\nport = 15999\nengine = \"native\"\n[auth]\nmode = \"any\"\n";
        let path = bundle(
            dir.path(),
            &["m"],
            &[
                ("mocks/m/mock.toml", mock),
                ("mocks/m/seed.sql", "SELECT 1;"),
                ("mocks/m/data/pgdata/postgresql.conf", "archive_command = 'touch /tmp/pwned'"),
                ("queries/q/..\\..\\..\\evil.txt", "x"),
                ("queries/q/_collection.toml", "id = \"q\"\nname = \"Q\"\n"),
            ],
        );
        let summary = import(&ws, &data, &path).unwrap();
        assert_eq!(summary.mocks, vec!["m"]);
        assert!(!ws.mock_dir("m").join("data").exists(), "the PostgreSQL cluster is not imported");
        let def: crate::model::MockDef = toml::from_str(&std::fs::read_to_string(ws.mock_dir("m").join("mock.toml")).unwrap()).unwrap();
        assert_eq!(def.engine, crate::model::MockEngine::Twinlite);
        assert_eq!(summary.notes.len(), 2, "{:?}", summary.notes);
        assert!(!dir.path().join("evil.txt").exists() && !ws.root.join("queries").join("q").join("..\\..\\..\\evil.txt").exists());
    }
}
