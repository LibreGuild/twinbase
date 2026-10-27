// Elasticsearch / OpenSearch: an index browser (search, sort, paging, document editor, fields) and
// the Dev Tools-style request console used by query tabs on these connections.

import { BookmarkPlus, CheckCircle2, Clock, Copy, FileText, History, ListTree, Pencil, Play, PlayCircle, Plus, RefreshCw, Save, Search, Trash2, XCircle } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { DocCard, DocList, Node, useStoredView, ViewToggle, type Doc } from "../components/JsonDocs";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { ConfirmDialog, CopyButton, Dialog, Empty, ErrorBox, Field, Resizer, restoredSize, Spinner } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { duration, kindLabel, plural, relativeTime } from "../lib/format";
import { Markdown } from "../components/Markdown";
import { attempt, targetInfo, useStore, useTargets, type Tab } from "../lib/store";
import type { HistoryEntry } from "../lib/types";
import type { Json } from "../lib/mongo";

type SearchTab = Extract<Tab, { kind: "search" }>;
type QueryTab = Extract<Tab, { kind: "query" }>;

export interface Hit {
  _id: string;
  _index: string;
  _score?: number | null;
  _source?: Record<string, Json>;
  _seq_no?: number;
  _primary_term?: number;
}

interface FieldInfo {
  path: string;
  type: string;
}

const SORTABLE = new Set(["keyword", "long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float", "unsigned_long", "date", "date_nanos", "boolean", "ip", "version"]);

/** The search box: a JSON query DSL object, or a query string (Lucene syntax). */
function parseQuery(text: string): unknown {
  const t = text.trim();
  if (!t) return undefined;
  if (t.startsWith("{")) return JSON.parse(t);
  return t;
}

export function consoleTemplate(index: string): string {
  return `GET /${index}/_search\n{\n  "query": { "match_all": {} },\n  "size": 20\n}`;
}

export function SearchIndexView({ tab }: { tab: SearchTab }) {
  const { connection, index } = tab;
  const target = targetInfo(connection);
  const readOnly = !!target?.readOnly;
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const [text, setText] = useState("");
  const [applied, setApplied] = useState("");
  const [sortField, setSortField] = useState("");
  const [sortDesc, setSortDesc] = useState(false);
  const [size, setSize] = useState(50);
  const [from, setFrom] = useState(0);
  const [hits, setHits] = useState<Hit[] | null>(null);
  const [total, setTotal] = useState<{ value: number; relation: string } | null>(null);
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(false);
  const [ms, setMs] = useState(0);
  const [fields, setFields] = useState<FieldInfo[] | null>(null);
  const [showFields, setShowFields] = useState(false);
  const [editing, setEditing] = useState<{ hit?: Hit; text: string } | null>(null);
  const [deleting, setDeleting] = useState<Hit | null>(null);
  const [view, setView] = useStoredView("twinbase.search.view");

  useEffect(() => {
    call<{ fields: FieldInfo[] }>("search.mapping", { connection, index })
      .then((r) => setFields(r.fields))
      .catch(() => setFields([]));
  }, [connection, index, dataVersion]);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const query = parseQuery(applied);
      const sort = sortField.trim() ? [{ [sortField.trim()]: sortDesc ? "desc" : "asc" }] : undefined;
      const r = await call<{ hits: Hit[]; total: number; relation: string; duration_ms: number }>("search.documents", { connection, index, query, sort, from, size });
      setHits(r.hits);
      setTotal({ value: r.total, relation: r.relation });
      setMs(r.duration_ms);
      setError(undefined);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  }, [connection, index, applied, sortField, sortDesc, from, size]);

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection, index, applied, sortField, sortDesc, from, size, dataVersion]);

  const run = () => {
    try {
      parseQuery(text);
    } catch (e) {
      setError(`The query is not valid JSON: ${errorMessage(e)}`);
      return;
    }
    setFrom(0);
    if (text === applied) load();
    else setApplied(text);
  };
  const list = hits ?? [];
  const sortable = (fields ?? []).filter((f) => SORTABLE.has(f.type));

  return (
    <div className="split-v">
      <div className="toolbar">
        <div className="title">
          <Search size={15} className="kind-es" />
          <span>{index}</span>
        </div>
        {total && <span className="badge">{total.relation === "gte" ? "≥ " : ""}{plural(total.value, "document")}</span>}
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
        <button className={`btn sm ghost${showFields ? " on" : ""}`} onClick={() => setShowFields((v) => !v)}>
          <ListTree size={13} /> Fields
        </button>
        <button className="btn sm ghost" onClick={() => openTab({ kind: "query", connection, sql: consoleTemplate(index), title: `${index} console` })}>
          <FileText size={13} /> Console
        </button>
        {!readOnly && (
          <button className="btn sm primary" onClick={() => setEditing({ text: "{\n  \n}" })}>
            <Plus size={13} /> Document
          </button>
        )}
      </div>
      <form
        className="query-bar"
        onSubmit={(e) => {
          e.preventDefault();
          run();
        }}
      >
        <input
          className="input sm mono grow"
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={'title:rust AND year:>2019 · or query DSL: {"match": {"title": "rust"}}'}
          aria-label="Query"
        />
        <div className="qb-field">
          <span className="qb-label">Sort</span>
          <select className="select sm" value={sortField} onChange={(e) => (setSortField(e.target.value), setFrom(0))} aria-label="Sort field">
            <option value="">relevance</option>
            {sortable.map((f) => (
              <option key={f.path} value={f.path}>
                {f.path}
              </option>
            ))}
          </select>
          <button type="button" className="icon-btn sm" title={sortDesc ? "Descending" : "Ascending"} disabled={!sortField} onClick={() => setSortDesc((d) => !d)}>
            {sortDesc ? "↓" : "↑"}
          </button>
        </div>
        <button className="btn sm primary" type="submit" disabled={loading}>
          {loading ? <Spinner size={13} /> : <Play size={13} />} Search
        </button>
      </form>
      {showFields && (
        <div className="filter-rows" style={{ maxHeight: 180, overflow: "auto" }}>
          {fields === null ? (
            <Spinner size={12} />
          ) : fields.length === 0 ? (
            <span className="faint xs">No mapped fields</span>
          ) : (
            <div className="es-fields">
              {fields.map((f) => (
                <span key={f.path} className="es-field" title={f.type}>
                  <span className="mono">{f.path}</span> <span className="faint xs">{f.type}</span>
                </span>
              ))}
            </div>
          )}
        </div>
      )}
      {error ? (
        <div style={{ padding: 12 }}>
          <ErrorBox error={error} />
        </div>
      ) : (
        <div className={view === "table" ? "doc-grid-host" : "pane"}>
          {!hits && loading ? (
            <Empty icon={<Spinner />} title="Loading…" />
          ) : list.length === 0 ? (
            <Empty icon={<Search size={22} />} title={applied ? "No documents match" : "No documents"}>
              {!readOnly && !applied ? "Add one with + Document." : undefined}
            </Empty>
          ) : view === "table" ? (
            <DocList docs={list.map((h) => ({ _id: h._id, ...(h._source ?? {}) })) as Doc[]} view="table" />
          ) : (
            <div className="doc-list">
              {list.map((h) => (
                <div key={`${h._index}/${h._id}`} className="doc-card">
                  <div className="doc-actions">
                    <CopyButton text={JSON.stringify(h._source ?? {}, null, 2)} small />
                    {!readOnly && (
                      <>
                        <button className="icon-btn sm" title="Edit" aria-label="Edit document" onClick={() => setEditing({ hit: h, text: JSON.stringify(h._source ?? {}, null, 2) })}>
                          <Pencil size={13} />
                        </button>
                        <button className="icon-btn sm" title="Duplicate" aria-label="Duplicate document" onClick={() => setEditing({ text: JSON.stringify(h._source ?? {}, null, 2) })}>
                          <Copy size={13} />
                        </button>
                        <button className="icon-btn sm danger" title="Delete" aria-label="Delete document" onClick={() => setDeleting(h)}>
                          <Trash2 size={13} />
                        </button>
                      </>
                    )}
                  </div>
                  <div className="fs-doc-head">
                    <span className="id selectable">{h._id}</span>
                    {h._index !== index && <span className="badge">{h._index}</span>}
                    {typeof h._score === "number" && <span className="faint xs">score {h._score.toFixed(3)}</span>}
                  </div>
                  <div className="jv selectable">
                    {Object.entries(h._source ?? {}).map(([k, v]) => (
                      <Node key={k} name={k} v={v} depth={0} open={false} />
                    ))}
                    {!h._source && <span className="faint">_source is disabled for this index</span>}
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
      <div className="grid-footer">
        <span>
          {list.length ? `${from + 1}–${from + list.length}` : "0"}
          {total ? ` of ${total.relation === "gte" ? "≥ " : ""}${total.value.toLocaleString()}` : ""} · {duration(ms)}
        </span>
        <span className="spacer" />
        <ViewToggle view={view} setView={setView} />
        <select className="select sm" value={size} onChange={(e) => (setSize(Number(e.target.value)), setFrom(0))} aria-label="Documents per page">
          {[25, 50, 100, 500].map((l) => (
            <option key={l} value={l}>
              {l} / page
            </option>
          ))}
        </select>
        <button className="btn sm" disabled={from === 0 || loading} onClick={() => setFrom((f) => Math.max(0, f - size))}>
          Prev
        </button>
        <button className="btn sm" disabled={!total || from + size >= total.value || from + size >= 10_000 || loading} onClick={() => setFrom((f) => f + size)} title={from + size >= 10_000 ? "Deep paging beyond 10,000 hits needs search_after (use the console)" : undefined}>
          Next
        </button>
        <button className="icon-btn" title="Reload" aria-label="Reload" onClick={load}>
          <RefreshCw size={13} />
        </button>
      </div>
      {editing && <DocDialog connection={connection} index={index} hit={editing.hit} initial={editing.text} onClose={() => setEditing(null)} />}
      {deleting && (
        <ConfirmDialog
          title={`Delete ${deleting._id}?`}
          body={`The document is removed from ${deleting._index}.`}
          confirmLabel="Delete"
          danger
          onClose={() => setDeleting(null)}
          onConfirm={async () => {
            const r = await attempt("Could not delete", () => call("search.delete", { connection, index: deleting._index, ids: [deleting._id] }));
            if (r) load();
          }}
        />
      )}
    </div>
  );
}

function DocDialog({ connection, index, hit, initial, onClose }: { connection: string; index: string; hit?: Hit; initial: string; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [text, setText] = useState(initial);
  const [id, setId] = useState("");
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      const document = JSON.parse(text);
      if (!document || typeof document !== "object" || Array.isArray(document)) throw new Error("a document is a JSON object");
      if (hit) await call("search.put", { connection, index: hit._index, id: hit._id, document, if_seq_no: hit._seq_no, if_primary_term: hit._primary_term });
      else await call("search.put", { connection, index, id: id.trim(), document, create: !!id.trim() });
      toast("success", hit ? "Document saved" : "Document added");
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={hit ? `Edit ${hit._id}` : "New document"}
      wide
      onClose={onClose}
      footer={
        <>
          <span className="faint xs grow">{hit ? "Saving fails if someone changed the document since it was loaded." : "The document is searchable as soon as it is saved."}</span>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={save}>
            {busy ? <Spinner size={13} /> : <Save size={13} />} Save
          </button>
        </>
      }
    >
      {!hit && (
        <Field label="Document id" hint="Leave empty for an automatic id.">
          <input className="input mono" value={id} onChange={(e) => setId(e.target.value)} placeholder="auto" autoFocus />
        </Field>
      )}
      <div style={{ height: 340, border: "1px solid var(--border)", borderRadius: "var(--radius)", overflow: "hidden" }}>
        <SqlEditor value={text} kind="elasticsearch" onChange={setText} onRun={save} onSave={save} />
      </div>
      <ErrorBox error={error} />
    </Dialog>
  );
}

// ------------------------------------------------------------------ console

interface SearchOutput {
  request: string;
  status: number;
  response: Json;
  elapsed_ms: number;
  error?: { message: string; code?: string };
}

/** Search hits in a response, if it is one. */
function hitsOf(r: Json): Hit[] | undefined {
  if (!r || typeof r !== "object" || Array.isArray(r)) return undefined;
  const h = (r as Record<string, Json>).hits;
  if (!h || typeof h !== "object" || Array.isArray(h)) return undefined;
  const list = (h as Record<string, Json>).hits;
  return Array.isArray(list) ? (list as unknown as Hit[]) : undefined;
}

export function SearchQueryView({ tab }: { tab: QueryTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const ws = useStore((s) => s.workspace);
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const editor = useRef<SqlEditorHandle>(null);
  const editorBox = useRef<HTMLDivElement>(null);
  const [text, setText] = useState(tab.sql);
  const [outputs, setOutputs] = useState<SearchOutput[] | null>(null);
  const [error, setError] = useState<unknown>();
  const [running, setRunning] = useState(false);
  const [panel, setPanel] = useState("o0");
  const [raw, setRaw] = useState(false);
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const [showDocs, setShowDocs] = useState(false);
  const [view, setView] = useStoredView("twinbase.search.consoleview");
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
      const r = await call<{ outputs: SearchOutput[] }>("search.run", { connection, command: script });
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
  const hits = current && !current.error ? hitsOf(current.response) : undefined;

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
        <button className="btn sm" disabled={!connection || running} onClick={() => execute(editor.current?.currentStatement() ?? text)} title="Run the request at the cursor (Shift+Enter)">
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
      </div>
      {showDocs && saved?.docs && <Markdown source={saved.docs} className="note-box" style={{ margin: "8px 10px 0", maxHeight: 260, overflow: "auto" }} />}
      <div className="editor-host" ref={editorBox} style={{ height: "var(--editor-h, 40%)", flex: "none", ["--editor-h" as string]: restoredSize("--editor-h") }}>
        <SqlEditor
          ref={editor}
          value={text}
          kind="elasticsearch"
          onChange={setText}
          onRun={() => execute(editor.current?.selectionOrAll() ?? text)}
          onRunCurrent={() => execute(editor.current?.currentStatement() ?? text)}
          onSave={save}
          placeholder={'GET /products/_search\n{ "query": { "match": { "name": "lamp" } } }\n\nPOST /products/_doc\n{ "name": "Desk lamp" }'}
        />
      </div>
      <Resizer axis="y" target={() => editorBox.current} variable="--editor-h" min={80} max={2000} />
      <div className="result-tabs" role="tablist">
        {(outputs ?? []).map((o, i) => (
          <button key={i} className={panel === `o${i}` ? "on" : ""} onClick={() => setPanel(`o${i}`)} title={o.request}>
            {o.error ? <XCircle size={13} style={{ color: "var(--danger)" }} /> : null}
            <span className="ellipsis mono" style={{ maxWidth: 240 }}>{o.request}</span>
            <span className="faint xs">{o.status}</span>
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
              <span className="mono ellipsis grow">{current.request}</span>
              <span className={`badge${current.status >= 400 ? " danger" : ""}`}>{current.status}</span>
              <span className="faint xs">{duration(current.elapsed_ms)}</span>
              {hits && (
                <button className={`btn sm ghost${raw ? " on" : ""}`} onClick={() => setRaw((r) => !r)}>
                  Raw
                </button>
              )}
              {hits && !raw && <ViewToggle view={view} setView={setView} />}
            </div>
            {current.error ? (
              <div style={{ padding: 12 }}>
                <ErrorBox error={`${current.error.message}${current.error.code ? ` (${current.error.code})` : ""}`} />
                <div className="faint small" style={{ marginTop: 8 }}>Requests after this one did not run.</div>
                <div className="doc-list" style={{ marginTop: 8 }}>
                  <DocCard doc={{ response: current.response } as Doc} />
                </div>
              </div>
            ) : hits && !raw ? (
              <div className={view === "table" ? "doc-grid-host" : "pane"}>
                <div className="messages" style={{ flex: "none" }}>
                  <span className="ok">✓</span> {plural(hits.length, "hit")} shown
                  {(() => {
                    const t = ((current.response as Record<string, Json>).hits as Record<string, Json>).total as Record<string, Json> | number | undefined;
                    const n = typeof t === "number" ? t : (t?.value as number | undefined);
                    return n !== undefined ? ` of ${n.toLocaleString()}` : "";
                  })()}
                </div>
                <DocList docs={hits.map((h) => ({ _id: h._id, ...(h._source ?? {}) })) as Doc[]} view={view} />
              </div>
            ) : typeof current.response === "string" ? (
              <pre className="pane mono selectable" style={{ margin: 0, padding: 12 }}>{current.response}</pre>
            ) : (
              <div className="pane">
                <div className="doc-list">
                  <DocCard doc={(current.response && typeof current.response === "object" && !Array.isArray(current.response) ? current.response : { response: current.response }) as Doc} />
                </div>
              </div>
            )}
          </>
        ) : (
          <div className="messages">
            <span className="faint">Run requests to see responses here. Each request gets its own result tab.</span>
          </div>
        )}
      </div>
    </div>
  );
}
