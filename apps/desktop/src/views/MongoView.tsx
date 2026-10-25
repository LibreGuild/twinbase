// MongoDB: a collection browser (documents, aggregation, indexes, schema) and the mongosh-style
// console used by query tabs on MongoDB connections.

import {
  BookmarkPlus, Braces, CheckCircle2, Clock, Copy, Files, History, KeySquare, Layers, ListTree, Pencil, Play, PlayCircle, Plus, RefreshCw, Rows3, Save, Trash2, Workflow, XCircle,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { DocCard, DocList, isContainer, Scalar, useStoredView, ViewToggle, type Doc } from "../components/JsonDocs";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { ConfirmDialog, CopyButton, Dialog, Empty, ErrorBox, Field, Resizer, restoredSize, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { duration, kindLabel, plural, relativeTime } from "../lib/format";
import { PIPELINE_STAGES, shellText, shortText, type Json } from "../lib/mongo";
import { renderMarkdown } from "../lib/markdown";
import { attempt, targetInfo, useStore, useTargets, type Tab } from "../lib/store";
import type { HistoryEntry, MongoField, MongoOutput, MongoPage } from "../lib/types";

type MongoTab = Extract<Tab, { kind: "mongo" }>;
type QueryTab = Extract<Tab, { kind: "query" }>;

// ------------------------------------------------------------------ collection browser

export function MongoCollectionView({ tab }: { tab: MongoTab }) {
  const { connection, database, collection } = tab;
  const target = targetInfo(connection);
  const readOnly = !!target?.readOnly;
  const [section, setSection] = useState<"documents" | "aggregation" | "indexes" | "schema">("documents");
  const [editing, setEditing] = useState<{ doc?: Doc; text: string; copy?: boolean } | null>(null);
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const ns = { connection, database, collection };

  return (
    <div className="split-v">
      <div className="toolbar">
        <div className="title">
          <Layers size={15} className="kind-mongo" />
          <span className="faint">{database}.</span>
          {collection}
        </div>
        {target?.isMock && <span className="badge accent">mock</span>}
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
        <button className="btn sm ghost" onClick={() => useStore.getState().openTab({ kind: "query", connection, sql: `use ${database}\ndb.${/^[A-Za-z_]\w*$/.test(collection) ? collection : `getCollection(${JSON.stringify(collection)})`}.find({}).limit(20)`, title: `${collection} console` })}>
          <Braces size={13} /> Console
        </button>
        {!readOnly && (
          <button className="btn sm primary" onClick={() => setEditing({ text: "{\n  \n}" })}>
            <Plus size={13} /> Document
          </button>
        )}
      </div>
      <div className="subtabs" role="tablist">
        {(
          [
            ["documents", "Documents", <Files size={13} key="d" />],
            ["aggregation", "Aggregation", <Workflow size={13} key="a" />],
            ["indexes", "Indexes", <KeySquare size={13} key="i" />],
            ["schema", "Schema", <ListTree size={13} key="s" />],
          ] as const
        ).map(([id, label, icon]) => (
          <button key={id} className={section === id ? "on" : ""} onClick={() => setSection(id)} role="tab" aria-selected={section === id}>
            {icon} {label}
          </button>
        ))}
      </div>
      <div style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}>
        {section === "documents" && <Documents ns={ns} readOnly={readOnly} dataVersion={dataVersion} onEdit={(doc, copy) => setEditing({ doc, copy, text: shellText(copy ? withoutId(doc) : doc) })} />}
        {section === "aggregation" && <Aggregation ns={ns} />}
        {section === "indexes" && <Indexes ns={ns} readOnly={readOnly} />}
        {section === "schema" && <Schema ns={ns} dataVersion={dataVersion} />}
      </div>
      {editing && <DocumentDialog ns={ns} doc={editing.copy ? undefined : editing.doc} initial={editing.text} onClose={() => setEditing(null)} />}
    </div>
  );
}

function withoutId(d: Doc): Doc {
  const { _id: _ignored, ...rest } = d;
  return rest;
}

interface Ns {
  connection: string;
  database: string;
  collection: string;
}

const LIMITS = [20, 50, 100, 500];

function Documents({ ns, readOnly, dataVersion, onEdit }: { ns: Ns; readOnly: boolean; dataVersion: number; onEdit: (d: Doc, copy: boolean) => void }) {
  const [filter, setFilter] = useState("");
  const [sort, setSort] = useState("");
  const [projection, setProjection] = useState("");
  const [limit, setLimit] = useState(50);
  const [skip, setSkip] = useState(0);
  const [applied, setApplied] = useState({ filter: "", sort: "", projection: "" });
  const [view, setView] = useStoredView("twinbase.mongo.view");
  const [page, setPage] = useState<MongoPage | null>(null);
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(false);
  const [deleting, setDeleting] = useState<Doc | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setPage(await call<MongoPage>("mongo.find", { ...ns, ...applied, skip, limit }));
      setError(undefined);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ns.connection, ns.database, ns.collection, applied, skip, limit]);

  useEffect(() => {
    load();
  }, [load, dataVersion]);

  const submit = (e?: React.FormEvent) => {
    e?.preventDefault();
    setSkip(0);
    setApplied({ filter, sort, projection });
  };
  const docs = (page?.documents ?? []) as Doc[];
  const total = page?.total ?? null;
  // A projection hides fields, so saving the shown document would drop them.
  const projected = !!applied.projection.trim();

  return (
    <>
      <form className="query-bar" onSubmit={submit}>
        <div className="qb-field grow">
          <span className="qb-label">Filter</span>
          <input className="input sm mono grow" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="{ status: 'active', age: { $gte: 18 } }" aria-label="Filter" spellCheck={false} />
        </div>
        <div className="qb-field" style={{ width: 200 }}>
          <span className="qb-label">Sort</span>
          <input className="input sm mono grow" value={sort} onChange={(e) => setSort(e.target.value)} placeholder="{ _id: -1 }" aria-label="Sort" spellCheck={false} />
        </div>
        <div className="qb-field" style={{ width: 200 }}>
          <span className="qb-label">Project</span>
          <input className="input sm mono grow" value={projection} onChange={(e) => setProjection(e.target.value)} placeholder="{ name: 1 }" aria-label="Projection" spellCheck={false} />
        </div>
        <button className="btn sm primary" type="submit" disabled={loading}>
          {loading ? <Spinner size={13} /> : <Play size={13} />} Find
        </button>
        {(filter || sort || projection) && (
          <button
            type="button"
            className="btn sm ghost"
            onClick={() => {
              setFilter("");
              setSort("");
              setProjection("");
              setSkip(0);
              setApplied({ filter: "", sort: "", projection: "" });
            }}
          >
            Reset
          </button>
        )}
      </form>
      {error ? (
        <div style={{ padding: 12 }}>
          <ErrorBox error={error} />
        </div>
      ) : (
        <div className={view === "table" ? "doc-grid-host" : "pane"}>
          {!page && loading ? (
            <Empty icon={<Spinner />} title="Loading…" />
          ) : (
            <DocList
              docs={docs}
              view={view}
              empty={
                <Empty icon={<Files size={22} />} title={applied.filter ? "No documents match" : "This collection is empty"}>
                  {applied.filter ? "Change the filter and press Find." : !readOnly ? "Add a document with the button above." : undefined}
                </Empty>
              }
              actions={(d) => (
                <>
                  <CopyButton text={shellText(d)} small />
                  {!readOnly && !projected && (
                    <>
                      <button className="icon-btn sm" title="Edit" aria-label="Edit document" onClick={() => onEdit(d, false)}>
                        <Pencil size={13} />
                      </button>
                      <button className="icon-btn sm" title="Duplicate" aria-label="Duplicate document" onClick={() => onEdit(d, true)}>
                        <Copy size={13} />
                      </button>
                    </>
                  )}
                  {!readOnly && "_id" in d && (
                    <button className="icon-btn sm danger" title="Delete" aria-label="Delete document" onClick={() => setDeleting(d)}>
                      <Trash2 size={13} />
                    </button>
                  )}
                </>
              )}
            />
          )}
        </div>
      )}
      <div className="grid-footer">
        <span>
          {docs.length ? `${(skip + 1).toLocaleString()}–${(skip + docs.length).toLocaleString()}` : "0"}
          {total !== null ? ` of ${total.toLocaleString()}` : ""}
          {page ? <span className="faint"> · {duration(page.duration_ms)}</span> : null}
          {projected && !readOnly ? <span className="faint"> · editing is off while a projection hides fields</span> : null}
        </span>
        <span className="spacer" />
        <ViewToggle view={view} setView={setView} />
        <select className="select sm" value={limit} onChange={(e) => (setLimit(Number(e.target.value)), setSkip(0))} aria-label="Page size">
          {LIMITS.map((l) => (
            <option key={l} value={l}>
              {l} / page
            </option>
          ))}
        </select>
        <button className="btn sm" disabled={skip === 0 || loading} onClick={() => setSkip(Math.max(0, skip - limit))}>
          Prev
        </button>
        <button className="btn sm" disabled={loading || docs.length < limit || (total !== null && skip + docs.length >= total)} onClick={() => setSkip(skip + limit)}>
          Next
        </button>
        <button className="icon-btn" title="Reload" aria-label="Reload" onClick={load}>
          <RefreshCw size={13} />
        </button>
      </div>
      {deleting && (
        <ConfirmDialog
          title="Delete document?"
          body={shortText(deleting._id ?? null)}
          confirmLabel="Delete"
          danger
          onClose={() => setDeleting(null)}
          onConfirm={async () => {
            const r = await attempt("Could not delete", () => call("mongo.delete", { ...ns, ids: [deleting._id] }));
            if (r) load();
          }}
        />
      )}
    </>
  );
}

function DocumentDialog({ ns, doc, initial, onClose }: { ns: Ns; doc?: Doc; initial: string; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [text, setText] = useState(initial);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      if (doc) {
        await call("mongo.replace", { ...ns, id: doc._id, document: text });
        toast("success", "Document saved");
      } else {
        const r = await call<{ inserted_ids: Json[] }>("mongo.insert", { ...ns, documents: text });
        toast("success", r.inserted_ids.length === 1 ? "Document inserted" : `${r.inserted_ids.length} documents inserted`);
      }
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={doc ? "Edit document" : "Insert documents"}
      wide
      onClose={onClose}
      footer={
        <>
          <span className="faint xs grow">mongosh syntax: ObjectId("…"), ISODate("…"), NumberLong(…), /regex/i. {doc ? "_id cannot change." : "An array inserts several documents."}</span>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={save}>
            {busy ? <Spinner size={13} /> : <Save size={13} />} {doc ? "Save" : "Insert"}
          </button>
        </>
      }
    >
      <div style={{ height: 380, border: "1px solid var(--border)", borderRadius: "var(--radius)", overflow: "hidden" }}>
        <SqlEditor value={text} kind="mongodb" onChange={setText} onRun={save} onSave={save} />
      </div>
      <ErrorBox error={error} />
    </Dialog>
  );
}

function Aggregation({ ns }: { ns: Ns }) {
  const [text, setText] = useState(() => {
    try {
      return localStorage.getItem(`twinbase.pipeline.${ns.connection}.${ns.database}.${ns.collection}`) ?? "[\n  { $match: {} },\n  { $limit: 20 }\n]";
    } catch {
      return "[\n  { $match: {} },\n  { $limit: 20 }\n]";
    }
  });
  const [result, setResult] = useState<{ documents: Doc[]; truncated: boolean; ms: number } | null>(null);
  const [error, setError] = useState<unknown>();
  const [running, setRunning] = useState(false);
  const [view, setView] = useStoredView("twinbase.mongo.aggview");
  const editor = useRef<SqlEditorHandle>(null);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    try {
      localStorage.setItem(`twinbase.pipeline.${ns.connection}.${ns.database}.${ns.collection}`, text);
    } catch {
      /* storage unavailable */
    }
  }, [text, ns.connection, ns.database, ns.collection]);
  const run = async () => {
    setRunning(true);
    const started = performance.now();
    try {
      const r = await call<{ documents: Doc[]; truncated: boolean }>("mongo.aggregate", { ...ns, pipeline: text, max_documents: 500 });
      setResult({ ...r, ms: performance.now() - started });
      setError(undefined);
    } catch (e) {
      setError(e);
    } finally {
      setRunning(false);
    }
  };
  const addStage = (stage: string) => {
    const t = text.trimEnd();
    const next = t.endsWith("]") ? `${t.slice(0, -1).trimEnd().replace(/([^[,\s])$/, "$1,")}\n  ${stage}\n]` : `[\n  ${stage}\n]`;
    setText(next);
    editor.current?.setValue(next);
  };
  return (
    <>
      <div className="query-bar">
        <span className="qb-label">Add stage</span>
        {Object.entries(PIPELINE_STAGES).map(([name, snippet]) => (
          <button key={name} className="btn sm ghost mono" onClick={() => addStage(snippet)}>
            {name}
          </button>
        ))}
        <span className="grow" />
        <button className="btn sm primary" disabled={running} onClick={run} title="Run (⌘/Ctrl+Enter)">
          {running ? <Spinner size={13} /> : <Play size={13} />} Run
        </button>
      </div>
      <div ref={box} className="editor-host" style={{ height: "var(--pipe-h, 38%)", flex: "none", ["--pipe-h" as string]: restoredSize("--pipe-h") }}>
        <SqlEditor ref={editor} value={text} kind="mongodb" onChange={setText} onRun={run} placeholder="[ { $match: { … } }, { $group: { … } } ]" />
      </div>
      <Resizer axis="y" target={() => box.current} variable="--pipe-h" min={80} max={2000} />
      {error ? (
        <div style={{ padding: 12 }}>
          <ErrorBox error={error} />
        </div>
      ) : (
        <div className={view === "table" ? "doc-grid-host" : "pane"}>
          {result ? <DocList docs={result.documents} view={view} empty={<Empty icon={<Workflow size={22} />} title="The pipeline returned no documents" />} /> : <Empty icon={<Workflow size={22} />} title="Build a pipeline">Stages run in order on the server. ⌘/Ctrl+Enter runs it.</Empty>}
        </div>
      )}
      <div className="grid-footer">
        <span>{result ? `${plural(result.documents.length, "document")}${result.truncated ? " (first 500)" : ""} · ${duration(result.ms)}` : ""}</span>
        <span className="spacer" />
        <ViewToggle view={view} setView={setView} />
      </div>
    </>
  );
}

function Indexes({ ns, readOnly }: { ns: Ns; readOnly: boolean }) {
  const indexes = useAsync(() => call<Doc[]>("mongo.indexes", ns), [ns.connection, ns.database, ns.collection]);
  const [keys, setKeys] = useState("");
  const [unique, setUnique] = useState(false);
  const [name, setName] = useState("");
  const [dropping, setDropping] = useState<string | null>(null);
  const toast = useStore((s) => s.toast);
  const create = async () => {
    const r = await attempt("Could not create the index", () => call<{ name: string }>("mongo.index.create", { ...ns, keys, unique, name: name || undefined }));
    if (r) {
      toast("success", "Index created", r.name);
      setKeys("");
      setName("");
      setUnique(false);
      indexes.reload();
    }
  };
  return (
    <div className="pane" style={{ padding: 14, display: "flex", flexDirection: "column", gap: 14 }}>
      {indexes.error ? <ErrorBox error={indexes.error} /> : null}
      <table className="simple-table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Keys</th>
            <th>Options</th>
            <th style={{ width: 40 }} />
          </tr>
        </thead>
        <tbody>
          {(indexes.data ?? []).map((ix) => {
            const n = String(ix.name);
            const { v: _v, key: _k, name: _n, ...opts } = ix;
            return (
              <tr key={n}>
                <td className="mono">{n}</td>
                <td className="mono">{shellText(ix.key, "", "")}</td>
                <td>
                  {Object.entries(opts).map(([k, v]) => (
                    <span key={k} className="badge" style={{ marginRight: 4 }}>
                      {v === true ? k : `${k}: ${shortText(v, 40)}`}
                    </span>
                  ))}
                </td>
                <td>
                  {!readOnly && n !== "_id_" && (
                    <button className="icon-btn sm danger" title="Drop index" aria-label={`Drop index ${n}`} onClick={() => setDropping(n)}>
                      <Trash2 size={13} />
                    </button>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {indexes.loading && !indexes.data && <Spinner />}
      {!readOnly && (
        <div className="card" style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          <strong>New index</strong>
          <div className="grid-2">
            <Field label="Keys" hint="1 ascending, -1 descending, 'text' for text search.">
              <input className="input mono" value={keys} onChange={(e) => setKeys(e.target.value)} placeholder="{ email: 1 }" spellCheck={false} />
            </Field>
            <Field label="Name (optional)">
              <input className="input mono" value={name} onChange={(e) => setName(e.target.value)} placeholder="generated from the keys" />
            </Field>
          </div>
          <div className="row">
            <label className="check">
              <input type="checkbox" checked={unique} onChange={(e) => setUnique(e.target.checked)} /> Unique
            </label>
            <span className="grow" />
            <button className="btn primary sm" disabled={!keys.trim()} onClick={create}>
              <Plus size={13} /> Create index
            </button>
          </div>
        </div>
      )}
      {dropping && (
        <ConfirmDialog
          title={`Drop index ${dropping}?`}
          body="Queries that use it get slower; unique constraints it enforced stop applying."
          confirmLabel="Drop index"
          danger
          onClose={() => setDropping(null)}
          onConfirm={async () => {
            const r = await attempt("Could not drop the index", () => call("mongo.index.drop", { ...ns, name: dropping }));
            if (r) indexes.reload();
          }}
        />
      )}
    </div>
  );
}

function Schema({ ns, dataVersion }: { ns: Ns; dataVersion: number }) {
  const schema = useAsync(() => call<{ schema: { sampled: number; fields: MongoField[] } }>("mongo.schema", { ...ns, sample: 300 }), [ns.connection, ns.database, ns.collection, dataVersion]);
  if (schema.error) return <div style={{ padding: 12 }}><ErrorBox error={schema.error} /></div>;
  if (!schema.data) return <Empty icon={<Spinner />} title="Sampling documents…" />;
  const { sampled, fields } = schema.data.schema;
  if (!sampled) return <Empty icon={<ListTree size={22} />} title="No documents to sample" />;
  return (
    <div className="pane" style={{ padding: 14 }}>
      <div className="faint small" style={{ marginBottom: 10 }}>
        Field paths and BSON types in {plural(sampled, "sampled document")}. <span className="mono">[]</span> marks array elements.
      </div>
      <table className="simple-table">
        <thead>
          <tr>
            <th>Field</th>
            <th>Types</th>
            <th style={{ width: 200 }}>Present in</th>
          </tr>
        </thead>
        <tbody>
          {fields.map((f) => {
            const depth = f.path.split(".").length - 1;
            const pct = Math.round((f.present / sampled) * 100);
            return (
              <tr key={f.path}>
                <td className="mono" style={{ paddingLeft: 8 + depth * 14 }}>
                  {f.path.split(".").pop()}
                </td>
                <td>
                  {f.types.map(([t, n]) => (
                    <span key={t} className={`badge type-${t}`} style={{ marginRight: 4 }} title={`${n} of ${f.present}`}>
                      {t}
                      {f.types.length > 1 ? ` ${Math.round((n / Math.max(1, f.types.reduce((a, [, x]) => a + x, 0))) * 100)}%` : ""}
                    </span>
                  ))}
                </td>
                <td>
                  <div className="presence" title={`${f.present} of ${sampled}`}>
                    <span style={{ width: `${pct}%` }} />
                  </div>
                  <span className="faint xs">{pct}%</span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ------------------------------------------------------------------ console

export function MongoQueryView({ tab }: { tab: QueryTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const ws = useStore((s) => s.workspace);
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const editor = useRef<SqlEditorHandle>(null);
  const editorBox = useRef<HTMLDivElement>(null);
  const [text, setText] = useState(tab.sql);
  const [outputs, setOutputs] = useState<MongoOutput[] | null>(null);
  const [error, setError] = useState<unknown>();
  const [running, setRunning] = useState(false);
  const [panel, setPanel] = useState<string>("o0");
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const [showDocs, setShowDocs] = useState(false);
  const [view, setView] = useStoredView("twinbase.mongo.consoleview");
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
      const r = await call<{ outputs: MongoOutput[] }>("mongo.run", { connection, command: script, max_documents: 500 });
      setOutputs(r.outputs);
      setError(undefined);
      const failed = r.outputs.findIndex((o) => o.error);
      setPanel(`o${failed >= 0 ? failed : r.outputs.length - 1}`);
    } catch (e) {
      setOutputs(null);
      setError(e);
      setPanel("messages");
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
        <button className="btn sm primary" disabled={!connection || running} onClick={() => execute(editor.current?.selectionOrAll() ?? text)} title="Run all or selection (⌘/Ctrl+Enter)">
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
          kind="mongodb"
          onChange={setText}
          onRun={() => execute(editor.current?.selectionOrAll() ?? text)}
          onRunCurrent={() => execute(editor.current?.currentStatement() ?? text)}
          onSave={save}
          placeholder={"db.users.find({ age: { $gt: 30 } }).sort({ age: -1 }).limit(10)\nshow collections · use otherdb · db.orders.aggregate([...])\n⌘/Ctrl+Enter runs everything, Shift+Enter the statement at the cursor."}
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
          <button className={panel === "messages" ? "on" : ""} onClick={() => setPanel("messages")}>
            <XCircle size={13} style={{ color: "var(--danger)" }} /> Error
          </button>
        ) : null}
        <button
          className={panel === "history" ? "on" : ""}
          onClick={() => {
            setPanel("history");
            loadHistory();
          }}
        >
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
        ) : error && panel === "messages" ? (
          <div style={{ padding: 12 }}>
            <ErrorBox error={error} />
          </div>
        ) : current ? (
          <OutputPanel o={current} view={view} setView={setView} />
        ) : (
          <div className="messages">
            <span className="faint">Run statements to see results here. Each statement gets its own result tab.</span>
          </div>
        )}
      </div>
    </div>
  );
}

function OutputPanel({ o, view, setView }: { o: MongoOutput; view: "list" | "table"; setView: (v: "list" | "table") => void }) {
  const head = (
    <div className="output-head">
      <Rows3 size={13} className="faint" />
      <span className="mono ellipsis grow">{o.statement.replace(/\s+/g, " ")}</span>
      <span className="badge">{o.database}</span>
      <span className="faint xs">{duration(o.duration_ms)}</span>
      {o.documents && <ViewToggle view={view} setView={setView} />}
    </div>
  );
  if (o.error) {
    return (
      <div className="pane">
        {head}
        <div style={{ padding: 12 }}>
          <ErrorBox error={`${o.error.message}${o.error.code ? ` (code ${o.error.code})` : ""}`} />
          <div className="faint small" style={{ marginTop: 8 }}>Statements after this one did not run.</div>
        </div>
      </div>
    );
  }
  return (
    <>
      {head}
      <div className={o.documents && view === "table" ? "doc-grid-host" : "pane"}>
        {o.message && <div className="messages" style={{ flex: "none" }}><span className="ok">✓</span> {o.message}</div>}
        {o.documents && <DocList docs={o.documents as Doc[]} view={view} />}
        {o.value !== undefined && !o.message && (
          <div className="doc-list">
            {isContainer(o.value as Json) ? <DocCard doc={(Array.isArray(o.value) ? { result: o.value } : o.value) as Doc} /> : <div className="doc-card jv"><Scalar v={o.value as Json} /></div>}
          </div>
        )}
        {o.value !== undefined && o.message && (
          <div className="doc-list">
            <DocCard doc={o.value as Doc} />
          </div>
        )}
      </div>
      {o.truncated && <div className="grid-footer">Showing the first {o.documents?.length} documents — add .limit() to fetch fewer.</div>}
    </>
  );
}
