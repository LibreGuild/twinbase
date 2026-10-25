// Dialogs for saved queries, collections, sample data, bundles and workspaces, plus the host
// that renders whichever dialog the store has open.

import { FileDown, FileUp, FolderOpen, Sparkles } from "lucide-react";
import { useState } from "react";
import { ConfirmDialog, Dialog, ErrorBox, Field, Spinner, useAsync } from "../components/ui";
import { call, errorMessage, pickPath } from "../lib/api";
import { renderMarkdown } from "../lib/markdown";
import { switchWorkspace, useStore } from "../lib/store";
import type { Collection, Expectation, SavedQuery, SnapshotInfo } from "../lib/types";
import { ConnectionDialog, MockDialog } from "./ConnectionDialogs";
import { TableDesigner } from "./TableDesigner";
import { SettingsDialog } from "../views/SettingsView";

function DocsEditor({ value, onChange, rows = 6 }: { value: string; onChange: (v: string) => void; rows?: number }) {
  const [preview, setPreview] = useState(false);
  return (
    <div>
      <div className="seg" style={{ marginBottom: 6 }}>
        <button className={!preview ? "on" : ""} onClick={() => setPreview(false)}>
          Write
        </button>
        <button className={preview ? "on" : ""} onClick={() => setPreview(true)}>
          Preview
        </button>
      </div>
      {preview ? (
        <div className="markdown note-box" style={{ minHeight: rows * 20 }} dangerouslySetInnerHTML={{ __html: renderMarkdown(value) || "<p class='faint'>Nothing to preview</p>" }} />
      ) : (
        <textarea className="textarea" style={{ width: "100%" }} rows={rows} value={value} onChange={(e) => onChange(e.target.value)} placeholder="Markdown: why this query exists, parameters, expected results, caveats…" />
      )}
    </div>
  );
}

function SaveQueryDialog({ tabId, sql, connection, saved, onClose }: { tabId: string; sql: string; connection?: string; saved?: { collection: string; id: string }; onClose: () => void }) {
  const ws = useStore((s) => s.workspace)!;
  const updateTab = useStore((s) => s.updateTab);
  const toast = useStore((s) => s.toast);
  const existing = saved ? ws.queries.find((q) => q.collection === saved.collection && q.id === saved.id) : undefined;
  const [name, setName] = useState(existing?.name ?? "");
  const [collection, setCollection] = useState(existing?.collection ?? ws.collections[0]?.id ?? "");
  const [newCollection, setNewCollection] = useState(ws.collections.length ? "" : "Queries");
  const [docs, setDocs] = useState(existing?.docs ?? "");
  const [tags, setTags] = useState((existing?.tags ?? []).join(", "));
  const [keepConnection, setKeepConnection] = useState(existing ? !!existing.connection : !!connection);
  const [expect, setExpect] = useState<Expectation | undefined>(existing?.expect);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  const save = async () => {
    setBusy(true);
    setError(undefined);
    try {
      let col = collection;
      if (!col || newCollection.trim()) {
        const c = await call<Collection>("collection.save", { collection: { id: "", name: newCollection.trim() || "Queries", docs: "" } });
        col = c.id;
      }
      const query: SavedQuery = {
        id: existing?.id ?? "",
        name: name.trim(),
        collection: col,
        sql: existing ? sql : sql,
        docs,
        tags: tags.split(",").map((t) => t.trim()).filter(Boolean),
        connection: keepConnection ? (connection ?? existing?.connection) : undefined,
        expect,
      };
      const r = await call<SavedQuery>("query.save", { query, previous_collection: existing?.collection });
      updateTab(tabId, { title: r.name, saved: { collection: r.collection, id: r.id }, sql });
      toast("success", "Query saved", r.name);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={existing ? "Query details" : "Save query"}
      onClose={onClose}
      wide
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!name.trim() || busy} onClick={save}>
            {busy && <Spinner />} Save
          </button>
        </>
      }
    >
      <div className="grid-2">
        <Field label="Name">
          <input className="input" autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Active customers by country" />
        </Field>
        <Field label="Collection">
          <div className="row">
            {ws.collections.length > 0 && (
              <select className="select grow" value={newCollection ? "" : collection} onChange={(e) => (setCollection(e.target.value), setNewCollection(""))}>
                {ws.collections.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            )}
            <input className="input grow" value={newCollection} onChange={(e) => setNewCollection(e.target.value)} placeholder={ws.collections.length ? "or new collection" : "Collection name"} />
          </div>
        </Field>
      </div>
      <Field label="Documentation">
        <DocsEditor value={docs} onChange={setDocs} />
      </Field>
      <div className="grid-2">
        <Field label="Tags" hint="Comma separated.">
          <input className="input" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="reports, onboarding" />
        </Field>
        <Field label="Expectation (for scenario runs)">
          <div className="row">
            <select
              className="select"
              value={expect?.kind ?? ""}
              onChange={(e) => {
                const k = e.target.value;
                setExpect(k === "" ? undefined : k === "rows" ? { kind: "rows", count: 1 } : k === "fails" ? { kind: "fails" } : ({ kind: k } as Expectation));
              }}
            >
              <option value="">None</option>
              <option value="succeeds">Succeeds</option>
              <option value="rows">Returns N rows</option>
              <option value="empty">Returns no rows</option>
              <option value="fails">Fails</option>
            </select>
            {expect?.kind === "rows" && <input className="input" style={{ width: 90 }} value={expect.count} onChange={(e) => setExpect({ kind: "rows", count: Number(e.target.value.replace(/\D/g, "")) || 0 })} aria-label="Row count" />}
            {expect?.kind === "fails" && <input className="input" style={{ width: 130 }} value={expect.code ?? ""} placeholder="error code" onChange={(e) => setExpect({ kind: "fails", code: e.target.value || undefined })} aria-label="Error code" />}
          </div>
        </Field>
      </div>
      {(connection || existing?.connection) && (
        <label className="check">
          <input type="checkbox" checked={keepConnection} onChange={(e) => setKeepConnection(e.target.checked)} /> Remember the connection ({connection ?? existing?.connection})
        </label>
      )}
      <ErrorBox error={error} />
    </Dialog>
  );
}

function CollectionDialog({ collectionId, onClose }: { collectionId?: string; onClose: () => void }) {
  const ws = useStore((s) => s.workspace)!;
  const openTab = useStore((s) => s.openTab);
  const existing = ws.collections.find((c) => c.id === collectionId);
  const [name, setName] = useState(existing?.name ?? "");
  const [docs, setDocs] = useState(existing?.docs ?? "");
  const [error, setError] = useState<string>();
  const save = async () => {
    try {
      const c = await call<Collection>("collection.save", { collection: { id: existing?.id ?? "", name: name.trim(), docs } });
      if (!existing) openTab({ kind: "collection", collectionId: c.id });
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  return (
    <Dialog
      title={existing ? "Edit collection" : "New collection"}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!name.trim()} onClick={save}>
            Save
          </button>
        </>
      }
    >
      <Field label="Name">
        <input className="input" autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Checkout flow" />
      </Field>
      <Field label="Documentation">
        <DocsEditor value={docs} onChange={setDocs} rows={8} />
      </Field>
      <ErrorBox error={error} />
    </Dialog>
  );
}

function GenerateDialog({ connection, schema, table, onClose }: { connection: string; schema: string; table: string; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [count, setCount] = useState(25);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const run = async () => {
    setBusy(true);
    setError(undefined);
    try {
      const r = await call<{ inserted: number }>("db.generate", { connection, schema, table, count });
      toast("success", "Sample data added", `${r.inserted} rows inserted into ${table}.`);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={`Generate rows for ${table}`}
      icon={<Sparkles size={17} className="muted" />}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || count < 1} onClick={run}>
            {busy && <Spinner />} Insert rows
          </button>
        </>
      }
    >
      <p className="muted" style={{ margin: 0 }}>
        Values follow column names and types (emails, names, prices, dates…), respect unique columns, and pick foreign keys from existing parent rows — fill parent tables first.
      </p>
      <Field label="Rows">
        <div className="row">
          {[10, 25, 100, 1000].map((n) => (
            <button key={n} className={`btn sm${count === n ? " primary" : ""}`} onClick={() => setCount(n)}>
              {n}
            </button>
          ))}
          <input className="input" style={{ width: 100 }} value={count} onChange={(e) => setCount(Math.min(10000, Number(e.target.value.replace(/\D/g, "")) || 0))} aria-label="Row count" />
        </div>
      </Field>
      <ErrorBox error={error} />
    </Dialog>
  );
}

function ExportDialog({ onClose }: { onClose: () => void }) {
  const ws = useStore((s) => s.workspace)!;
  const toast = useStore((s) => s.toast);
  const [mocks, setMocks] = useState<string[]>(ws.mocks.map((m) => m.id));
  const [collections, setCollections] = useState<string[]>(ws.collections.map((c) => c.id));
  const [connections, setConnections] = useState<string[]>([]);
  const [snapshotIds, setSnapshotIds] = useState<string[]>([]);
  const snapshotList = useAsync(() => call<SnapshotInfo[]>("schema.snapshot.list"), []);
  const [includeData, setIncludeData] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const toggle = (list: string[], set: (l: string[]) => void, id: string) => set(list.includes(id) ? list.filter((x) => x !== id) : [...list, id]);
  const run = async () => {
    const path = await pickPath({ save: true, title: "Export bundle", defaultPath: `${ws.name.replace(/[^\w.-]+/g, "-")}.twinbase`, extensions: ["twinbase"] });
    if (!path) return;
    setBusy(true);
    setError(undefined);
    try {
      await call("bundle.export", { path, selection: { mocks, collections, connections, snapshots: snapshotIds, include_data: includeData } });
      toast("success", "Bundle exported", path);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const nothing = !mocks.length && !collections.length && !connections.length && !snapshotIds.length;
  return (
    <Dialog
      title="Export bundle"
      icon={<FileDown size={17} className="muted" />}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || nothing} onClick={run}>
            {busy && <Spinner />} Export…
          </button>
        </>
      }
    >
      <p className="muted" style={{ margin: 0 }}>
        A <code>.twinbase</code> file a teammate can import: mock servers (with their data and seed), and query collections with documentation. Passwords are never included.
      </p>
      <div className="grid-2">
        <Field label="Mock servers">
          {ws.mocks.length === 0 && <span className="faint small">None</span>}
          {ws.mocks.map((m) => (
            <label key={m.id} className="check">
              <input type="checkbox" checked={mocks.includes(m.id)} onChange={() => toggle(mocks, setMocks, m.id)} /> {m.name}
            </label>
          ))}
        </Field>
        <Field label="Query collections">
          {ws.collections.length === 0 && <span className="faint small">None</span>}
          {ws.collections.map((c) => (
            <label key={c.id} className="check">
              <input type="checkbox" checked={collections.includes(c.id)} onChange={() => toggle(collections, setCollections, c.id)} /> {c.name}
            </label>
          ))}
        </Field>
      </div>
      {ws.connections.length > 0 && (
        <Field label="Connection definitions (without passwords)">
          {ws.connections.map((c) => (
            <label key={c.id} className="check">
              <input type="checkbox" checked={connections.includes(c.id)} onChange={() => toggle(connections, setConnections, c.id)} /> {c.name}
            </label>
          ))}
        </Field>
      )}
      {(snapshotList.data?.length ?? 0) > 0 && (
        <Field label="Schema snapshots">
          {snapshotList.data!.map((s) => (
            <label key={s.id} className="check">
              <input type="checkbox" checked={snapshotIds.includes(s.id)} onChange={() => toggle(snapshotIds, setSnapshotIds, s.id)} /> {s.label}
            </label>
          ))}
        </Field>
      )}
      <label className="check">
        <input type="checkbox" checked={includeData} onChange={(e) => setIncludeData(e.target.checked)} /> Include mock data (otherwise only definitions and seed scripts)
      </label>
      <div className="faint xs">SQL mock data is snapshotted consistently, even while the server runs. Redis mocks export their last saved snapshot.</div>
      <ErrorBox error={error} />
    </Dialog>
  );
}

function ImportDialog({ onClose }: { onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [path, setPath] = useState("");
  const [manifest, setManifest] = useState<{ mocks: string[]; collections: string[]; connections: string[]; created_at: string; source_workspace: string } | null>(null);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const choose = async () => {
    const p = await pickPath({ title: "Import bundle", extensions: ["twinbase", "zip"] });
    if (!p) return;
    setPath(p);
    setError(undefined);
    try {
      setManifest(await call("bundle.inspect", { path: p }));
    } catch (e) {
      setManifest(null);
      setError(errorMessage(e));
    }
  };
  const run = async () => {
    setBusy(true);
    try {
      const r = await call<{ mocks: string[]; collections: string[]; connections: string[]; snapshots: string[]; notes?: string[] }>("bundle.import", { path });
      toast("success", "Bundle imported", `${r.mocks.length} mocks, ${r.collections.length} collections, ${r.connections.length} connections, ${r.snapshots.length} snapshots`);
      for (const note of r.notes ?? []) toast("info", "Imported with a safety change", note);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="Import bundle"
      icon={<FileUp size={17} className="muted" />}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!manifest || busy} onClick={run}>
            {busy && <Spinner />} Import
          </button>
        </>
      }
    >
      <div className="row">
        <input className="input mono grow" value={path} readOnly placeholder="Choose a .twinbase file" />
        <button className="btn" onClick={choose}>
          <FolderOpen size={14} /> Choose…
        </button>
      </div>
      {manifest && (
        <dl className="kv">
          <dt>From</dt>
          <dd>{manifest.source_workspace}</dd>
          <dt>Created</dt>
          <dd>{manifest.created_at}</dd>
          <dt>Mock servers</dt>
          <dd>{manifest.mocks.join(", ") || "none"}</dd>
          <dt>Collections</dt>
          <dd>{manifest.collections.join(", ") || "none"}</dd>
          <dt>Connections</dt>
          <dd>{manifest.connections.join(", ") || "none"}</dd>
        </dl>
      )}
      {manifest && <div className="faint xs">Items whose ids already exist in this workspace are imported under a new id; nothing is overwritten.</div>}
      <ErrorBox error={error} />
    </Dialog>
  );
}

function WorkspaceDialog({ onClose }: { onClose: () => void }) {
  const settings = useStore((s) => s.settings);
  const [path, setPath] = useState("");
  const [error, setError] = useState<string>();
  const open = async (p: string) => {
    try {
      await switchWorkspace(p);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  return (
    <Dialog
      title="Open workspace"
      icon={<FolderOpen size={17} className="muted" />}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!path} onClick={() => open(path)}>
            Open
          </button>
        </>
      }
    >
      <p className="muted" style={{ margin: 0 }}>
        Pick any folder — for example <code>.twinbase</code> inside your project repository. A new workspace is created there if the folder has none. Open tabs are closed.
      </p>
      <div className="row">
        <input className="input mono grow" value={path} onChange={(e) => setPath(e.target.value)} placeholder="/path/to/project/.twinbase" />
        <button
          className="btn"
          onClick={async () => {
            const p = await pickPath({ directory: true, title: "Choose workspace folder" });
            if (p) setPath(p);
          }}
        >
          Browse…
        </button>
      </div>
      {settings && settings.recent_workspaces.length > 0 && (
        <Field label="Recent">
          {settings.recent_workspaces.map((p) => (
            <div key={p} className="tree-row" onClick={() => open(p)} title={p}>
              <FolderOpen size={13} className="muted" />
              <span className="label mono small">{p}</span>
            </div>
          ))}
        </Field>
      )}
      <ErrorBox error={error} />
    </Dialog>
  );
}

/** Parent folder of the default workspace (e.g. ~/TwinBase). */
function defaultParent(): string {
  const d = useStore.getState().info?.default_workspace ?? "";
  return d.replace(/[\\/][^\\/]*$/, "");
}

function joinPath(parent: string, child: string): string {
  const sep = parent.includes("\\") && !parent.includes("/") ? "\\" : "/";
  return parent ? `${parent.replace(/[\\/]+$/, "")}${sep}${child}` : child;
}

export function NewWorkspaceDialog({ onClose }: { onClose: () => void }) {
  const [name, setName] = useState("");
  const [parent, setParent] = useState(defaultParent());
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const folder = name.trim().replace(/[^\w .-]+/g, "-").replace(/^[.\s]+/, "");
  const path = folder ? joinPath(parent, folder) : "";
  const create = async () => {
    setBusy(true);
    setError(undefined);
    try {
      await switchWorkspace(path, name.trim());
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="New workspace"
      icon={<FolderOpen size={17} className="muted" />}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!path || busy} onClick={create}>
            {busy && <Spinner />} Create workspace
          </button>
        </>
      }
    >
      <Field label="Name">
        <input className="input" autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Checkout service" onKeyDown={(e) => e.key === "Enter" && path && create()} />
      </Field>
      <Field label="Location" hint="Tip: choose your project's repository (for example a .twinbase folder in it) to commit the workspace with your code.">
        <div className="row">
          <input className="input mono grow" value={parent} onChange={(e) => setParent(e.target.value)} />
          <button
            className="btn"
            onClick={async () => {
              const p = await pickPath({ directory: true, title: "Choose where to create the workspace" });
              if (p) setParent(p);
            }}
          >
            Browse…
          </button>
        </div>
      </Field>
      {path && (
        <div className="note-box small">
          Creates <code>{path}</code>
        </div>
      )}
      <ErrorBox error={error} />
    </Dialog>
  );
}

export function DialogHost() {
  const dialog = useStore((s) => s.dialog);
  const close = useStore((s) => s.closeDialog);
  if (!dialog) return null;
  switch (dialog.kind) {
    case "connection":
      return <ConnectionDialog connection={dialog.connection} onClose={close} />;
    case "mock":
      return <MockDialog mock={dialog.mock} mockKind={dialog.mockKind} onClose={close} />;
    case "table-designer":
      return <TableDesigner connection={dialog.connection} schema={dialog.schema} table={dialog.table} onClose={close} />;
    case "save-query":
      return <SaveQueryDialog {...dialog} onClose={close} />;
    case "collection":
      return <CollectionDialog collectionId={dialog.collectionId} onClose={close} />;
    case "generate":
      return <GenerateDialog {...dialog} onClose={close} />;
    case "export":
      return <ExportDialog onClose={close} />;
    case "import":
      return <ImportDialog onClose={close} />;
    case "workspace":
      return <WorkspaceDialog onClose={close} />;
    case "confirm":
      return <ConfirmDialog {...dialog} onClose={close} />;
    case "settings":
      return <SettingsDialog section={dialog.section} onClose={close} />;
    case "new-workspace":
      return <NewWorkspaceDialog onClose={close} />;
  }
}
