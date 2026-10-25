// Start page: pick the default workspace, create a new one, or open an existing folder.

import { ArrowRight, FolderGit2, FolderOpen, FolderPlus, Home, X } from "lucide-react";
import { useState } from "react";
import art from "../assets/workspace.webp";
import { ErrorBox, Spinner } from "../components/ui";
import { errorMessage, pickPath } from "../lib/api";
import { dismissStart, switchWorkspace, useStore } from "../lib/store";

function lastSegment(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

export function StartPage() {
  const info = useStore((s) => s.info);
  const ws = useStore((s) => s.workspace);
  const settings = useStore((s) => s.settings);
  const openDialog = useStore((s) => s.openDialog);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string>();
  const defaultPath = info?.default_workspace ?? "";

  const open = async (key: string, path: string | null) => {
    if (!path) return;
    setBusy(key);
    setError(undefined);
    try {
      await switchWorkspace(path);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const recent = (settings?.recent_workspaces ?? []).filter((p) => p !== defaultPath).slice(0, 5);
  const firstRun = info?.first_run && !recent.length;

  return (
    <div className="start" role="dialog" aria-label="Choose a workspace">
      <div className="start-card">
        <div className="start-side">
          <span className="eyebrow">
            <FolderGit2 size={13} /> Workspaces
          </span>
          <h1>{firstRun ? "Welcome to TwinBase" : "Choose a workspace"}</h1>
          <p>
            A workspace is a plain folder that holds your mock servers, seed data, saved queries with their docs, and schema snapshots — readable TOML, SQL and Markdown files.
          </p>
          <img src={art} alt="" draggable={false} />
          <p className="small">
            <strong>Tip:</strong> keep it inside your project's repository (for example in a <code>.twinbase</code> folder) and commit it to GitHub, so your whole team shares the same mocks and queries. Passwords and tokens are never written
            into it.
          </p>
        </div>
        <div className="start-main">
          <div className="row">
            <strong className="grow" style={{ fontSize: 15 }}>
              How do you want to start?
            </strong>
            {!firstRun && ws && (
              <button className="icon-btn" onClick={dismissStart} aria-label="Close start page" title="Keep the current workspace">
                <X size={16} />
              </button>
            )}
          </div>
          <button className="choice" onClick={() => open("default", defaultPath)} disabled={!!busy}>
            <span className="icon-tile">{busy === "default" ? <Spinner /> : <Home size={18} />}</span>
            <span className="grow">
              <div className="t">Open the default workspace</div>
              <div className="d">The quickest start. You can move to a project folder any time.</div>
              <div className="p">{defaultPath}</div>
            </span>
            <ArrowRight size={16} className="faint" />
          </button>
          <button className="choice" onClick={() => openDialog({ kind: "new-workspace" })} disabled={!!busy}>
            <span className="icon-tile neutral">
              <FolderPlus size={18} />
            </span>
            <span className="grow">
              <div className="t">Create a new workspace</div>
              <div className="d">Name it and pick where it lives — your code repository is a great place.</div>
            </span>
            <ArrowRight size={16} className="faint" />
          </button>
          <button
            className="choice"
            onClick={async () => open("existing", await pickPath({ directory: true, title: "Open an existing workspace folder" }))}
            disabled={!!busy}
          >
            <span className="icon-tile neutral">{busy === "existing" ? <Spinner /> : <FolderOpen size={18} />}</span>
            <span className="grow">
              <div className="t">Open an existing workspace</div>
              <div className="d">For example a teammate's <code>.twinbase</code> folder you cloned from GitHub. A folder without a workspace becomes a new one.</div>
            </span>
            <ArrowRight size={16} className="faint" />
          </button>
          {recent.length > 0 && (
            <>
              <div className="faint xs" style={{ fontWeight: 700, textTransform: "uppercase", letterSpacing: 0.6, marginTop: 6 }}>
                Recent
              </div>
              {recent.map((p) => (
                <button key={p} className="tree-row" style={{ border: 0, background: "none", textAlign: "left", width: "100%", height: 34 }} onClick={() => open(p, p)} title={p}>
                  {busy === p ? <Spinner size={13} /> : <FolderOpen size={14} className="muted" />}
                  <span className="label">
                    <strong style={{ fontWeight: 600 }}>{lastSegment(p)}</strong> <span className="faint xs mono">{p}</span>
                  </span>
                </button>
              ))}
            </>
          )}
          <ErrorBox error={error} />
        </div>
      </div>
    </div>
  );
}
