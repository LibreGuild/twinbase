// Firestore: a collection browser (documents, filters, subcollections, editor) and the statement
// console used by query tabs on Firestore connections.

import { BookmarkPlus, CheckCircle2, ChevronRight, Clock, Copy, FileText, Filter, FolderTree, History, Pencil, Play, PlayCircle, Plus, RefreshCw, Save, Trash2, X, XCircle } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { DocCard, DocList, Node, useStoredView, ViewToggle, type Doc } from "../components/JsonDocs";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { ConfirmDialog, CopyButton, Dialog, Empty, ErrorBox, Field, Resizer, restoredSize, Spinner } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { duration, kindLabel, plural, relativeTime } from "../lib/format";
import { renderMarkdown } from "../lib/markdown";
import { attempt, targetInfo, useStore, useTargets, type Tab } from "../lib/store";
import type { HistoryEntry } from "../lib/types";
import type { Json } from "../lib/mongo";

type FirestoreTab = Extract<Tab, { kind: "firestore" }>;
type QueryTab = Extract<Tab, { kind: "query" }>;

export interface FsDoc {
  id: string;
  path: string;
  data: Record<string, Json>;
  create_time?: string;
  update_time?: string;
}

interface Condition {
  field: string;
  op: string;
  value: string;
}

const OPS = ["==", "!=", "<", "<=", ">", ">=", "in", "not-in", "array-contains", "array-contains-any"];

/** A filter value: JSON when it parses (numbers, true, ["a"], {"__timestamp": …}), else a string. */
function parseValue(text: string): Json {
  try {
    return JSON.parse(text) as Json;
  } catch {
    return text;
  }
}

function pretty(data: Record<string, Json>): string {
  return JSON.stringify(data, null, 2);
}

export function FirestoreCollectionView({ tab }: { tab: FirestoreTab }) {
  const { connection, collection } = tab;
  const target = targetInfo(connection);
  const readOnly = !!target?.readOnly;
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const [conditions, setConditions] = useState<Condition[]>([]);
  const [orderField, setOrderField] = useState("");
  const [orderDesc, setOrderDesc] = useState(false);
  const [limit, setLimit] = useState(50);
  const [pages, setPages] = useState<(string | null)[]>([null]);
  const [docs, setDocs] = useState<FsDoc[] | null>(null);
  const [count, setCount] = useState<number | null>(null);
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(false);
  const [ms, setMs] = useState(0);
  const [applied, setApplied] = useState(0);
  const [editing, setEditing] = useState<{ doc?: FsDoc; text: string; id?: string } | null>(null);
  const [deleting, setDeleting] = useState<FsDoc | null>(null);
  const [subs, setSubs] = useState<Record<string, string[] | "loading">>({});
  const [view, setView] = useStoredView("twinbase.firestore.view");
  const segments = collection.split("/");

  const buildQuery = useCallback(
    (startAfter: string | null) => ({
      collection,
      conditions: conditions.filter((c) => c.field.trim()).map((c) => ({ field: c.field.trim(), op: c.op, value: parseValue(c.value) })),
      order: orderField.trim() ? [[orderField.trim(), orderDesc]] : [],
      limit,
      start_after: orderField.trim() ? undefined : (startAfter ?? undefined),
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [collection, applied, limit, orderDesc],
  );

  const load = useCallback(
    async (startAfter: string | null) => {
      setLoading(true);
      try {
        const q = buildQuery(startAfter);
        const [r, c] = await Promise.all([
          call<{ documents: FsDoc[]; duration_ms: number }>("firestore.documents", { connection, query: q }),
          call<{ count: number }>("firestore.count", { connection, query: { ...q, limit: undefined, start_after: undefined } }).catch(() => null),
        ]);
        setDocs(r.documents);
        setMs(r.duration_ms);
        setCount(c?.count ?? null);
        setError(undefined);
      } catch (e) {
        setError(e);
      } finally {
        setLoading(false);
      }
    },
    [buildQuery, connection],
  );

  useEffect(() => {
    setPages([null]);
    setSubs({});
    load(null);
  }, [load, dataVersion]);

  const loadSubs = async (d: FsDoc) => {
    setSubs((s) => ({ ...s, [d.path]: "loading" }));
    const r = await attempt("Could not list subcollections", () => call<{ collections: string[] }>("firestore.collections", { connection, parent: d.path }));
    setSubs((s) => ({ ...s, [d.path]: r?.collections ?? [] }));
  };
  const list = docs ?? [];
  const lastPath = list[list.length - 1]?.path ?? null;
  const ordered = !!orderField.trim();

  return (
    <div className="split-v">
      <div className="toolbar">
        <div className="title">
          <FolderTree size={15} className="kind-firestore" />
          {segments.map((s, i) => (
            <span key={i} className={i < segments.length - 1 ? "faint" : undefined}>
              {i > 0 && <ChevronRight size={11} className="faint" style={{ margin: "0 2px" }} />}
              {s}
            </span>
          ))}
        </div>
        {count !== null && <span className="badge">{plural(count, "document")}</span>}
        {target?.isMock && <span className="badge accent">mock</span>}
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
        <button className="btn sm ghost" onClick={() => openTab({ kind: "query", connection, sql: `query ${collection} limit 20`, title: `${segments[segments.length - 1]} console` })}>
          <FileText size={13} /> Console
        </button>
        {!readOnly && (
          <button className="btn sm primary" onClick={() => setEditing({ text: "{\n  \n}", id: "" })}>
            <Plus size={13} /> Document
          </button>
        )}
      </div>
      <form
        className="query-bar"
        onSubmit={(e) => {
          e.preventDefault();
          setApplied((n) => n + 1);
        }}
      >
        <button type="button" className="btn sm ghost" onClick={() => setConditions((c) => [...c, { field: "", op: "==", value: "" }])}>
          <Filter size={13} /> Where
        </button>
        <div className="qb-field">
          <span className="qb-label">Order by</span>
          <input className="input sm mono" style={{ width: 140 }} value={orderField} onChange={(e) => setOrderField(e.target.value)} placeholder="document id" aria-label="Order by field" />
          <button type="button" className="icon-btn sm" title={orderDesc ? "Descending" : "Ascending"} onClick={() => setOrderDesc((d) => !d)}>
            {orderDesc ? "↓" : "↑"}
          </button>
        </div>
        <span className="grow" />
        <button className="btn sm primary" type="submit" disabled={loading}>
          {loading ? <Spinner size={13} /> : <Play size={13} />} Run
        </button>
      </form>
      {conditions.length > 0 && (
        <div className="filter-rows">
          {conditions.map((c, i) => {
            const set = (patch: Partial<Condition>) => setConditions((cs) => cs.map((x, j) => (j === i ? { ...x, ...patch } : x)));
            return (
              <div key={i} className="row">
                <input className="input sm mono" style={{ width: 160 }} value={c.field} onChange={(e) => set({ field: e.target.value })} placeholder="field or a.b" aria-label="Field" />
                <select className="select sm" value={c.op} onChange={(e) => set({ op: e.target.value })} aria-label="Operator">
                  {OPS.map((o) => (
                    <option key={o}>{o}</option>
                  ))}
                </select>
                <input className="input sm mono" style={{ width: 220 }} value={c.value} onChange={(e) => set({ value: e.target.value })} placeholder={'30, "text", true, ["a","b"]'} aria-label="Value" onKeyDown={(e) => e.key === "Enter" && setApplied((n) => n + 1)} />
                <button className="icon-btn sm" aria-label="Remove condition" onClick={() => setConditions((cs) => cs.filter((_, j) => j !== i))}>
                  <X size={12} />
                </button>
              </div>
            );
          })}
          <div className="faint xs">Values are JSON (unquoted text counts as a string). Range filters on a field need it in “Order by” first, as in Firestore.</div>
        </div>
      )}
      {error ? (
        <div style={{ padding: 12 }}>
          <ErrorBox error={error} />
        </div>
      ) : (
        <div className={view === "table" ? "doc-grid-host" : "pane"}>
          {!docs && loading ? (
            <Empty icon={<Spinner />} title="Loading…" />
          ) : list.length === 0 ? (
            <Empty icon={<FileText size={22} />} title={conditions.length ? "No documents match" : "No documents"}>
              {!readOnly && !conditions.length ? "Add one with + Document." : undefined}
            </Empty>
          ) : view === "table" ? (
            <DocList docs={list.map((d) => ({ __id__: d.id, ...d.data })) as Doc[]} view="table" />
          ) : (
            <div className="doc-list">
              {list.map((d) => {
                const sub = subs[d.path];
                return (
                  <div key={d.path} className="doc-card">
                    <div className="doc-actions">
                      <button className="icon-btn sm" title="Subcollections" aria-label="Subcollections" onClick={() => loadSubs(d)}>
                        <FolderTree size={13} />
                      </button>
                      <CopyButton text={pretty(d.data)} small />
                      {!readOnly && (
                        <>
                          <button className="icon-btn sm" title="Edit" aria-label="Edit document" onClick={() => setEditing({ doc: d, text: pretty(d.data) })}>
                            <Pencil size={13} />
                          </button>
                          <button className="icon-btn sm" title="Duplicate" aria-label="Duplicate document" onClick={() => setEditing({ text: pretty(d.data), id: "" })}>
                            <Copy size={13} />
                          </button>
                          <button className="icon-btn sm danger" title="Delete" aria-label="Delete document" onClick={() => setDeleting(d)}>
                            <Trash2 size={13} />
                          </button>
                        </>
                      )}
                    </div>
                    <div className="fs-doc-head">
                      <span className="id selectable">{d.id}</span>
                      {d.update_time && <span className="faint xs" title={`created ${d.create_time}\nupdated ${d.update_time}`}>updated {relativeTime(Date.parse(d.update_time))}</span>}
                    </div>
                    <div className="jv selectable">
                      {Object.entries(d.data).map(([k, v]) => (
                        <Node key={k} name={k} v={v} depth={0} open={false} />
                      ))}
                      {Object.keys(d.data).length === 0 && <span className="faint">{"{ }"}</span>}
                    </div>
                    {sub && (
                      <div className="fs-subs">
                        {sub === "loading" ? (
                          <Spinner size={12} />
                        ) : sub.length === 0 ? (
                          <span className="faint xs">No subcollections</span>
                        ) : (
                          sub.map((c) => (
                            <button key={c} className="btn sm" onClick={() => openTab({ kind: "firestore", connection, collection: `${d.path}/${c}` })}>
                              <FolderTree size={12} /> {c}
                            </button>
                          ))
                        )}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}
      <div className="grid-footer">
        <span>
          {plural(list.length, "document")} · page {pages.length} · {duration(ms)}
          {ordered ? <span className="faint"> · paging works in document-id order</span> : null}
        </span>
        <span className="spacer" />
        <ViewToggle view={view} setView={setView} />
        <select className="select sm" value={limit} onChange={(e) => setLimit(Number(e.target.value))} aria-label="Documents per page">
          {[25, 50, 100, 500].map((l) => (
            <option key={l} value={l}>
              {l} / page
            </option>
          ))}
        </select>
        <button className="btn sm" disabled={pages.length <= 1 || loading || ordered} onClick={() => { const prev = pages.slice(0, -1); setPages(prev); load(prev[prev.length - 1]); }}>
          Prev
        </button>
        <button className="btn sm" disabled={list.length < limit || loading || ordered} onClick={() => { setPages((p) => [...p, lastPath]); load(lastPath); }}>
          Next
        </button>
        <button className="icon-btn" title="Reload" aria-label="Reload" onClick={() => load(pages[pages.length - 1])}>
          <RefreshCw size={13} />
        </button>
      </div>
      {editing && <DocDialog connection={connection} collection={collection} doc={editing.doc} initial={editing.text} onClose={() => setEditing(null)} />}
      {deleting && (
        <ConfirmDialog
          title={`Delete ${deleting.id}?`}
          body={`${deleting.path}. Subcollections are not deleted (as in Firestore).`}
          confirmLabel="Delete"
          danger
          onClose={() => setDeleting(null)}
          onConfirm={async () => {
            const r = await attempt("Could not delete", () => call("firestore.delete", { connection, paths: [deleting.path] }));
            if (r) load(pages[pages.length - 1]);
          }}
        />
      )}
    </div>
  );
}

function DocDialog({ connection, collection, doc, initial, onClose }: { connection: string; collection: string; doc?: FsDoc; initial: string; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [text, setText] = useState(initial);
  const [id, setId] = useState("");
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      const data = JSON.parse(text);
      if (!data || typeof data !== "object" || Array.isArray(data)) throw new Error("a document is a JSON object of fields");
      if (doc) await call("firestore.set", { connection, path: doc.path, data, expect: "exists" });
      else if (id.trim()) await call("firestore.set", { connection, path: `${collection}/${id.trim()}`, data, expect: "new" });
      else await call("firestore.add", { connection, collection, data });
      toast("success", doc ? "Document saved" : "Document added");
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={doc ? `Edit ${doc.id}` : "New document"}
      wide
      onClose={onClose}
      footer={
        <>
          <span className="faint xs grow">JSON. Special values: {'{"__timestamp": "2024-01-01T00:00:00Z"}'}, {'{"__ref": "users/bob"}'}, {'{"__geo": [lat, lng]}'}, {'{"__bytes": "base64"}'}</span>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={save}>
            {busy ? <Spinner size={13} /> : <Save size={13} />} Save
          </button>
        </>
      }
    >
      {!doc && (
        <Field label="Document id" hint="Leave empty for an automatic id.">
          <input className="input mono" value={id} onChange={(e) => setId(e.target.value)} placeholder="auto" autoFocus />
        </Field>
      )}
      <div style={{ height: 340, border: "1px solid var(--border)", borderRadius: "var(--radius)", overflow: "hidden" }}>
        <SqlEditor value={text} kind="firestore" onChange={setText} onRun={save} onSave={save} />
      </div>
      <ErrorBox error={error} />
    </Dialog>
  );
}

// ------------------------------------------------------------------ console

interface FsOutput {
  statement: string;
  documents?: FsDoc[];
  value?: Json;
  message?: string;
  truncated: boolean;
  duration_ms: number;
  error?: { message: string; code?: string };
}

export function FirestoreQueryView({ tab }: { tab: QueryTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const ws = useStore((s) => s.workspace);
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const editor = useRef<SqlEditorHandle>(null);
  const editorBox = useRef<HTMLDivElement>(null);
  const [text, setText] = useState(tab.sql);
  const [outputs, setOutputs] = useState<FsOutput[] | null>(null);
  const [error, setError] = useState<unknown>();
  const [running, setRunning] = useState(false);
  const [panel, setPanel] = useState("o0");
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const [showDocs, setShowDocs] = useState(false);
  const [view, setView] = useStoredView("twinbase.firestore.consoleview");
  const connection = tab.connection;
  const target = targetInfo(connection);
  const saved = tab.saved ? ws?.queries.find((q) => q.collection === tab.saved!.collection && q.id === tab.saved!.id) : undefined;
  const dirty = saved ? saved.sql !== text : false;

  useEffect(() => setDirty(tab.id, dirty), [dirty, tab.id, setDirty]);
  useEffect(() => {
    const t = setTimeout(() => text !== tab.sql && updateTab(tab.id, { sql: text }), 300);
    return () => clearTimeout(t);
  }, [text, tab.id, tab.sql, updateTab]);
  useEffect(() => {
    if (tab.sql !== text && !dirty) setText(tab.sql);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.sql]);

  const execute = async (script: string) => {
    if (!connection || !script.trim()) return;
    setRunning(true);
    try {
      const r = await call<{ outputs: FsOutput[] }>("firestore.run", { connection, command: script, max_documents: 500 });
      setOutputs(r.outputs);
      setError(undefined);
      const failed = r.outputs.findIndex((o) => o.error);
      setPanel(`o${failed >= 0 ? failed : r.outputs.length - 1}`);
    } catch (e) {
      setOutputs(null);
      setError(e);
      setPanel("error");
    } finally {
      setRunning(false);
    }
  };
  const save = async () => {
    if (saved) {
      const r = await attempt("Could not save query", () => call("query.save", { query: { ...saved, sql: text } }));
      if (r) toast("success", "Query saved", saved.name);
    } else openDialog({ kind: "save-query", tabId: tab.id, sql: text, connection });
  };
  const loadHistory = async () => {
    if (!connection) return setHistory([]);
    setHistory((await attempt("Could not load history", () => call<HistoryEntry[]>("db.history", { connection }))) ?? []);
  };
  const current = panel.startsWith("o") ? outputs?.[Number(panel.slice(1))] : undefined;

  return (
    <div className="split-v">
      <div className="toolbar">
        <select className="select sm" style={{ maxWidth: 240 }} value={connection ?? ""} onChange={(e) => updateTab(tab.id, { connection: e.target.value || undefined })} aria-label="Connection">
          <option value="">Choose connection…</option>
          {targets.map((t) => (
            <option key={t.id} value={t.id}>
              {t.isMock ? "◆ " : ""}
              {t.name} · {kindLabel(t.kind)}
            </option>
          ))}
        </select>
        <button className="btn sm primary" disabled={!connection || running} onClick={() => execute(editor.current?.selectionOrAll() ?? text)} title="Run all (⌘/Ctrl+Enter)">
          {running ? <Spinner size={13} /> : <Play size={13} />} Run
        </button>
        <button className="btn sm" disabled={!connection || running} onClick={() => execute(editor.current?.currentStatement() ?? text)} title="Run the statement at the cursor (Shift+Enter)">
          <PlayCircle size={13} /> Current
        </button>
        <div className="sep" />
        <button className="btn sm ghost" onClick={save} title="Save (⌘/Ctrl+S)">
          {saved ? <Save size={13} /> : <BookmarkPlus size={13} />} {saved ? "Save" : "Save as…"}
        </button>
        {saved?.docs && (
          <button className={`btn sm ghost${showDocs ? " on" : ""}`} onClick={() => setShowDocs((v) => !v)}>
            Docs
          </button>
        )}
        <div className="grow" />
        {target?.readOnly && <span className="badge warning">read-only connection</span>}
        {target?.isMock && <span className="badge accent">mock</span>}
      </div>
      {showDocs && saved?.docs && <div className="markdown note-box" style={{ margin: "8px 10px 0", maxHeight: 180, overflow: "auto" }} dangerouslySetInnerHTML={{ __html: renderMarkdown(saved.docs) }} />}
      <div className="editor-host" ref={editorBox} style={{ height: "var(--editor-h, 40%)", flex: "none", ["--editor-h" as string]: restoredSize("--editor-h") }}>
        <SqlEditor
          ref={editor}
          value={text}
          kind="firestore"
          onChange={setText}
          onRun={() => execute(editor.current?.selectionOrAll() ?? text)}
          onRunCurrent={() => execute(editor.current?.currentStatement() ?? text)}
          onSave={save}
          placeholder={'query users where age >= 30 order by age desc limit 10\nget users/alice · collections · count users where active == true\nset users/alice {"name": "Alice"} · update users/alice {"age": 32} · add users {…} · delete users/alice'}
        />
      </div>
      <Resizer axis="y" target={() => editorBox.current} variable="--editor-h" min={80} max={2000} />
      <div className="result-tabs" role="tablist">
        {(outputs ?? []).map((o, i) => (
          <button key={i} className={panel === `o${i}` ? "on" : ""} onClick={() => setPanel(`o${i}`)} title={o.statement}>
            {o.error ? <XCircle size={13} style={{ color: "var(--danger)" }} /> : null}
            <span className="ellipsis mono" style={{ maxWidth: 220 }}>{o.statement.replace(/\s+/g, " ")}</span>
            {o.documents && <span className="faint xs">{o.documents.length}{o.truncated ? "+" : ""}</span>}
          </button>
        ))}
        {error ? (
          <button className={panel === "error" ? "on" : ""} onClick={() => setPanel("error")}>
            <XCircle size={13} style={{ color: "var(--danger)" }} /> Error
          </button>
        ) : null}
        <button className={panel === "history" ? "on" : ""} onClick={() => (setPanel("history"), loadHistory())}>
          <History size={13} /> History
        </button>
      </div>
      <div style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}>
        {panel === "history" ? (
          <div className="pane">
            {history === null ? (
              <Empty icon={<Spinner />} title="Loading…" />
            ) : history.length === 0 ? (
              <Empty icon={<Clock size={22} />} title="No history yet" />
            ) : (
              <table className="simple-table">
                <tbody>
                  {history.slice().reverse().map((h, i) => (
                    <tr key={i} style={{ cursor: "pointer" }} onClick={() => setText(h.sql)} title="Load into editor">
                      <td style={{ width: 22 }}>{h.ok ? <CheckCircle2 size={13} style={{ color: "var(--success)" }} /> : <XCircle size={13} style={{ color: "var(--danger)" }} />}</td>
                      <td className="mono" style={{ maxWidth: 600 }}>
                        <div className="ellipsis">{h.sql.replace(/\s+/g, " ")}</div>
                        {h.error && <div className="xs" style={{ color: "var(--danger)" }}>{h.error}</div>}
                      </td>
                      <td className="faint xs" style={{ whiteSpace: "nowrap" }}>{duration(h.elapsed_ms)}</td>
                      <td className="faint xs" style={{ whiteSpace: "nowrap" }}>{relativeTime(h.at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        ) : panel === "error" && error ? (
          <div style={{ padding: 12 }}>
            <ErrorBox error={error} />
          </div>
        ) : current ? (
          <>
            <div className="output-head">
              <span className="mono ellipsis grow">{current.statement.replace(/\s+/g, " ")}</span>
              <span className="faint xs">{duration(current.duration_ms)}</span>
              {current.documents && <ViewToggle view={view} setView={setView} />}
            </div>
            {current.error ? (
              <div style={{ padding: 12 }}>
                <ErrorBox error={`${current.error.message}${current.error.code ? ` (${current.error.code})` : ""}`} />
                <div className="faint small" style={{ marginTop: 8 }}>Statements after this one did not run.</div>
              </div>
            ) : (
              <div className={current.documents && view === "table" ? "doc-grid-host" : "pane"}>
                {current.message && (
                  <div className="messages" style={{ flex: "none" }}>
                    <span className="ok">✓</span> {current.message}
                  </div>
                )}
                {current.documents && <DocList docs={current.documents.map((d) => ({ __path__: d.path, ...d.data })) as Doc[]} view={view} />}
                {current.value !== undefined && (
                  <div className="doc-list">
                    <DocCard doc={{ result: current.value } as Doc} />
                  </div>
                )}
              </div>
            )}
            {current.truncated && <div className="grid-footer">Showing the first {current.documents?.length} documents — add limit N to fetch fewer.</div>}
          </>
        ) : (
          <div className="messages">
            <span className="faint">Run statements to see results here. Each statement gets its own result tab.</span>
          </div>
        )}
      </div>
    </div>
  );
}
