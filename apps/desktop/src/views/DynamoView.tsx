// DynamoDB: a table browser (scan / query builder, items, editor, table info) and the request
// console used by query tabs on DynamoDB connections.

import { BookmarkPlus, CheckCircle2, ChevronLeft, ChevronRight, Clock, Filter, History, Info, Play, PlayCircle, Plus, RefreshCw, Rows3, Save, Search, Terminal, Trash2, X, XCircle } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DataGrid } from "../components/DataGrid";
import { DocCard, DocList, useStoredView, ViewToggle, type Doc } from "../components/JsonDocs";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { ConfirmDialog, Dialog, Empty, ErrorBox, Resizer, restoredSize, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { attrType, formatItem, itemColumns, keyNames, keyOf, typed, typeTag, unmarshal, unmarshalItem, type DynItem, type ScalarType, type TableDescription } from "../lib/dynamo";
import { duration, kindLabel, plural, relativeTime } from "../lib/format";
import { renderMarkdown } from "../lib/markdown";
import { shortText, type Json } from "../lib/mongo";
import { attempt, targetInfo, useStore, useTargets, type Tab } from "../lib/store";
import type { HistoryEntry } from "../lib/types";

type DynamoTab = Extract<Tab, { kind: "dynamo" }>;
type QueryTab = Extract<Tab, { kind: "query" }>;

interface FilterRow {
  path: string;
  op: string;
  type: ScalarType;
  value: string;
}

interface ReadResult {
  operation: "Query" | "Scan";
  items: DynItem[];
  count: number;
  scanned: number;
  last_key?: DynItem | null;
  duration_ms: number;
}

const SORT_OPS = [
  ["", "any"],
  ["=", "="],
  ["<", "<"],
  ["<=", "≤"],
  [">", ">"],
  [">=", "≥"],
  ["between", "between"],
  ["begins_with", "begins with"],
] as const;

const FILTER_OPS = [
  ["=", "="],
  ["<>", "≠"],
  ["<", "<"],
  ["<=", "≤"],
  [">", ">"],
  [">=", "≥"],
  ["begins_with", "begins with"],
  ["contains", "contains"],
  ["exists", "exists"],
  ["not_exists", "missing"],
] as const;

function cellOf(v: DynItem[string] | undefined): string | number | boolean | null {
  if (!v) return null;
  const u = unmarshal(v);
  return typeof u === "number" || typeof u === "boolean" || u === null ? u : typeof u === "string" ? u : shortText(u as Json, 200);
}

export function DynamoTableView({ tab }: { tab: DynamoTab }) {
  const { connection, table } = tab;
  const target = targetInfo(connection);
  const readOnly = !!target?.readOnly;
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const desc = useAsync(() => call<TableDescription>("dynamo.describe", { connection, table }), [connection, table, version]);
  const [section, setSection] = useState<"items" | "info">("items");
  const openTab = useStore((s) => s.openTab);
  const keys = keyNames(desc.data?.KeySchema);
  return (
    <div className="split-v">
      <div className="toolbar">
        <div className="title">
          <Rows3 size={15} className="kind-dynamo" /> {table}
        </div>
        {keys.hash && (
          <span className="badge" title="Partition key">
            {keys.hash} <span className="faint">{attrType(desc.data, keys.hash)}</span>
          </span>
        )}
        {keys.range && (
          <span className="badge" title="Sort key">
            {keys.range} <span className="faint">{attrType(desc.data, keys.range)}</span>
          </span>
        )}
        {target?.isMock && <span className="badge accent">mock</span>}
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
        <button className="btn sm ghost" onClick={() => openTab({ kind: "query", connection, sql: `Query {\n  "TableName": ${JSON.stringify(table)},\n  "KeyConditionExpression": "#pk = :pk",\n  "ExpressionAttributeNames": { "#pk": ${JSON.stringify(keys.hash ?? "pk")} },\n  "ExpressionAttributeValues": { ":pk": { "${attrType(desc.data, keys.hash)}": "" } }\n}`, title: `${table} console` })}>
          <Terminal size={13} /> Console
        </button>
      </div>
      <div className="subtabs" role="tablist">
        <button className={section === "items" ? "on" : ""} onClick={() => setSection("items")} role="tab" aria-selected={section === "items"}>
          <Rows3 size={13} /> Items
        </button>
        <button className={section === "info" ? "on" : ""} onClick={() => setSection("info")} role="tab" aria-selected={section === "info"}>
          <Info size={13} /> Table
        </button>
      </div>
      <div style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}>
        {desc.error ? (
          <div style={{ padding: 12 }}>
            <ErrorBox error={desc.error} />
          </div>
        ) : !desc.data ? (
          <Empty icon={<Spinner />} title="Loading table…" />
        ) : section === "items" ? (
          <Items connection={connection} desc={desc.data} readOnly={readOnly} />
        ) : (
          <TableInfo desc={desc.data} />
        )}
      </div>
    </div>
  );
}

function Items({ connection, desc, readOnly }: { connection: string; desc: TableDescription; readOnly: boolean }) {
  const table = desc.TableName;
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const indexes = [...(desc.GlobalSecondaryIndexes ?? []), ...(desc.LocalSecondaryIndexes ?? [])];
  const [index, setIndex] = useState("");
  const schema = index ? indexes.find((i) => i.IndexName === index)?.KeySchema : desc.KeySchema;
  const keys = keyNames(schema);
  const tableKeys = keyNames(desc.KeySchema);
  const keyList = [tableKeys.hash, tableKeys.range].filter(Boolean) as string[];
  const [partition, setPartition] = useState("");
  const [sortOp, setSortOp] = useState("");
  const [sortValue, setSortValue] = useState("");
  const [sortValue2, setSortValue2] = useState("");
  const [forward, setForward] = useState(true);
  const [filters, setFilters] = useState<FilterRow[]>([]);
  const [limit, setLimit] = useState(50);
  const [pages, setPages] = useState<(DynItem | null)[]>([null]);
  const [result, setResult] = useState<ReadResult | null>(null);
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(false);
  const [editing, setEditing] = useState<{ item?: DynItem; text: string } | null>(null);
  const [view, setView] = useStoredView("twinbase.dynamo.view");
  const [applied, setApplied] = useState(0);

  const run = useCallback(
    async (start: DynItem | null) => {
      setLoading(true);
      try {
        const params: Record<string, unknown> = { connection, table, index: index || undefined, limit, start_key: start, forward };
        if (partition.trim()) {
          params.partition = typed(attrType(desc, keys.hash), partition);
          if (sortOp && keys.range) {
            const t = attrType(desc, keys.range);
            params.sort = { path: keys.range, op: sortOp, value: typed(t, sortValue) };
            if (sortOp === "between") params.sort_value2 = typed(t, sortValue2);
          }
        }
        params.filters = filters.filter((f) => f.path.trim()).map((f) => ({ path: f.path.trim(), op: f.op, value: f.op === "exists" || f.op === "not_exists" ? undefined : typed(f.type, f.value) }));
        setResult(await call<ReadResult>("dynamo.read", params));
        setError(undefined);
      } catch (e) {
        setError(e);
      } finally {
        setLoading(false);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [connection, table, index, limit, forward, applied],
  );

  useEffect(() => {
    setPages([null]);
    run(null);
  }, [run, dataVersion]);

  const submit = (e?: React.FormEvent) => {
    e?.preventDefault();
    setApplied((n) => n + 1);
  };
  const items = result?.items ?? [];
  const cols = useMemo(() => itemColumns(items, keyList), [items, keyList.join("|")]);
  const columns = useMemo(() => cols.map((c) => ({ name: c, type: [...new Set(items.filter((i) => c in i).map((i) => typeTag(i[c])))].join(" | ") || "—", pk: keyList.includes(c) })), [cols, items]);
  const rows = useMemo(() => items.map((it) => cols.map((c) => cellOf(it[c]))), [items, cols]);
  const page = pages.length;
  const newItemText = () => formatItem(Object.fromEntries(keyList.map((k) => [k, { [attrType(desc, k)]: "" }])));

  return (
    <>
      <form className="query-bar" onSubmit={submit}>
        <select className="select sm" value={index} onChange={(e) => (setIndex(e.target.value), setSortOp(""))} aria-label="Index">
          <option value="">Table</option>
          {indexes.map((i) => (
            <option key={i.IndexName} value={i.IndexName}>
              Index: {i.IndexName}
            </option>
          ))}
        </select>
        <div className="qb-field">
          <span className="qb-label" title="Partition key — set it to Query, leave it empty to Scan">
            {keys.hash}
          </span>
          <input className="input sm mono" style={{ width: 160 }} value={partition} onChange={(e) => setPartition(e.target.value)} placeholder="any (scan)" aria-label="Partition key value" />
        </div>
        {keys.range && partition.trim() && (
          <div className="qb-field">
            <span className="qb-label">{keys.range}</span>
            <select className="select sm" value={sortOp} onChange={(e) => setSortOp(e.target.value)} aria-label="Sort key condition">
              {SORT_OPS.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
            {sortOp && <input className="input sm mono" style={{ width: 120 }} value={sortValue} onChange={(e) => setSortValue(e.target.value)} aria-label="Sort key value" />}
            {sortOp === "between" && (
              <>
                <span className="faint xs">and</span>
                <input className="input sm mono" style={{ width: 120 }} value={sortValue2} onChange={(e) => setSortValue2(e.target.value)} aria-label="Sort key upper value" />
              </>
            )}
            <button type="button" className="icon-btn sm" title={forward ? "Ascending" : "Descending"} onClick={() => setForward((f) => !f)}>
              {forward ? "↑" : "↓"}
            </button>
          </div>
        )}
        <button type="button" className="btn sm ghost" onClick={() => setFilters((f) => [...f, { path: "", op: "=", type: "S", value: "" }])}>
          <Filter size={13} /> Filter
        </button>
        <span className="grow" />
        <button className="btn sm primary" type="submit" disabled={loading}>
          {loading ? <Spinner size={13} /> : partition.trim() ? <Search size={13} /> : <Play size={13} />} {partition.trim() ? "Query" : "Scan"}
        </button>
        {!readOnly && (
          <button type="button" className="btn sm" onClick={() => setEditing({ text: newItemText() })}>
            <Plus size={13} /> Item
          </button>
        )}
      </form>
      {filters.length > 0 && (
        <div className="filter-rows">
          {filters.map((f, i) => {
            const set = (patch: Partial<FilterRow>) => setFilters((fs) => fs.map((x, j) => (j === i ? { ...x, ...patch } : x)));
            const needsValue = f.op !== "exists" && f.op !== "not_exists";
            return (
              <div key={i} className="row">
                <input className="input sm mono" style={{ width: 160 }} value={f.path} onChange={(e) => set({ path: e.target.value })} placeholder="attribute or a.b[0]" aria-label="Attribute" />
                <select className="select sm" value={f.op} onChange={(e) => set({ op: e.target.value })} aria-label="Operator">
                  {FILTER_OPS.map(([v, l]) => (
                    <option key={v} value={v}>
                      {l}
                    </option>
                  ))}
                </select>
                {needsValue && (
                  <>
                    <select className="select sm" value={f.type} onChange={(e) => set({ type: e.target.value as ScalarType })} aria-label="Value type">
                      {(["S", "N", "BOOL", "NULL"] as const).map((t) => (
                        <option key={t}>{t}</option>
                      ))}
                    </select>
                    {f.type !== "NULL" && <input className="input sm mono" style={{ width: 160 }} value={f.value} onChange={(e) => set({ value: e.target.value })} aria-label="Value" onKeyDown={(e) => e.key === "Enter" && submit()} />}
                  </>
                )}
                <button className="icon-btn sm" aria-label="Remove filter" onClick={() => setFilters((fs) => fs.filter((_, j) => j !== i))}>
                  <X size={12} />
                </button>
              </div>
            );
          })}
          <div className="faint xs">Filters apply after items are read: a page can hold fewer items than the limit.</div>
        </div>
      )}
      {error ? (
        <div style={{ padding: 12 }}>
          <ErrorBox error={error} />
        </div>
      ) : (
        <div className={view === "table" ? "doc-grid-host" : "pane"}>
          {!result && loading ? (
            <Empty icon={<Spinner />} title="Reading…" />
          ) : items.length === 0 ? (
            <Empty icon={<Rows3 size={22} />} title={result?.scanned ? "No items match the filter on this page" : "No items"}>
              {result?.last_key ? "More items follow — use Next." : !readOnly ? "Add one with + Item." : undefined}
            </Empty>
          ) : view === "table" ? (
            <div className="grid-wrap">
              <DataGrid columns={columns} rows={rows} widthKey={`dynamo-${table}`} onContextMenu={(_, row) => setEditing({ item: items[row], text: formatItem(items[row]) })} />
            </div>
          ) : (
            <DocList
              docs={items.map((i) => unmarshalItem(Object.fromEntries([...keyList.filter((k) => k in i).map((k) => [k, i[k]]), ...Object.entries(i).filter(([k]) => !keyList.includes(k))]))) as Doc[]}
              view="list"
              actions={(_, i) => (
                <button className="icon-btn sm" title={readOnly ? "View typed JSON" : "Edit"} aria-label="Edit item" onClick={() => setEditing({ item: items[i], text: formatItem(items[i]) })}>
                  <Rows3 size={13} />
                </button>
              )}
            />
          )}
        </div>
      )}
      <div className="grid-footer">
        <span>
          {result ? `${result.operation} · ${plural(result.count, "item")}${result.scanned !== result.count ? ` of ${result.scanned} read` : ""} · page ${page} · ${duration(result.duration_ms)}` : ""}
          {view === "table" && items.length ? <span className="faint"> · right-click a row to edit</span> : null}
        </span>
        <span className="spacer" />
        <ViewToggle view={view} setView={setView} />
        <select className="select sm" value={limit} onChange={(e) => setLimit(Number(e.target.value))} aria-label="Items per page">
          {[25, 50, 100, 500].map((l) => (
            <option key={l} value={l}>
              {l} / page
            </option>
          ))}
        </select>
        <button className="btn sm" disabled={page <= 1 || loading} onClick={() => { const prev = pages.slice(0, -1); setPages(prev); run(prev[prev.length - 1]); }}>
          <ChevronLeft size={13} /> Prev
        </button>
        <button className="btn sm" disabled={!result?.last_key || loading} onClick={() => { setPages((p) => [...p, result!.last_key!]); run(result!.last_key!); }}>
          Next <ChevronRight size={13} />
        </button>
        <button className="icon-btn" title="Reload" aria-label="Reload" onClick={() => run(pages[pages.length - 1])}>
          <RefreshCw size={13} />
        </button>
      </div>
      {editing && <ItemDialog connection={connection} table={table} keyNames={keyList} item={editing.item} initial={editing.text} readOnly={readOnly} onClose={() => setEditing(null)} />}
    </>
  );
}

function ItemDialog({ connection, table, keyNames: keys, item, initial, readOnly, onClose }: { connection: string; table: string; keyNames: string[]; item?: DynItem; initial: string; readOnly: boolean; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [text, setText] = useState(initial);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      const parsed = JSON.parse(text) as DynItem;
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("the item must be a JSON object of typed attributes");
      if (item && JSON.stringify(keyOf(parsed, keys)) !== JSON.stringify(keyOf(item, keys))) throw new Error(`the key (${keys.join(", ")}) cannot change; add a new item and delete this one instead`);
      await call("dynamo.put", { connection, table, item: parsed, expect: item ? "exists" : "new" });
      toast("success", item ? "Item saved" : "Item added");
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={item ? (readOnly ? "Item" : "Edit item") : "New item"}
      wide
      onClose={onClose}
      footer={
        <>
          {item && !readOnly && (
            <button className="btn danger" onClick={() => setConfirmDelete(true)}>
              <Trash2 size={13} /> Delete
            </button>
          )}
          <span className="faint xs grow">DynamoDB JSON: {`{"S": "text"}`}, {`{"N": "1.5"}`}, {`{"BOOL": true}`}, {`{"SS": [...]}`}, {`{"M": {...}}`}, {`{"L": [...]}`}</span>
          <button className="btn" onClick={onClose}>
            {readOnly ? "Close" : "Cancel"}
          </button>
          {!readOnly && (
            <button className="btn primary" disabled={busy} onClick={save}>
              {busy ? <Spinner size={13} /> : <Save size={13} />} Save
            </button>
          )}
        </>
      }
    >
      <div style={{ height: 380, border: "1px solid var(--border)", borderRadius: "var(--radius)", overflow: "hidden" }}>
        <SqlEditor value={text} kind="mongodb" onChange={setText} onRun={save} onSave={save} readOnly={readOnly} />
      </div>
      <ErrorBox error={error} />
      {confirmDelete && item && (
        <ConfirmDialog
          title="Delete item?"
          body={keys.map((k) => `${k} = ${shortText(unmarshal(item[k]))}`).join(", ")}
          confirmLabel="Delete"
          danger
          onClose={() => setConfirmDelete(false)}
          onConfirm={async () => {
            const r = await attempt("Could not delete", () => call("dynamo.delete", { connection, table, keys: [keyOf(item, keys)] }));
            if (r) onClose();
          }}
        />
      )}
    </Dialog>
  );
}

function TableInfo({ desc }: { desc: TableDescription }) {
  const keys = keyNames(desc.KeySchema);
  const indexes = [...(desc.GlobalSecondaryIndexes ?? []).map((i) => ({ ...i, kind: "Global" })), ...(desc.LocalSecondaryIndexes ?? []).map((i) => ({ ...i, kind: "Local" }))];
  return (
    <div className="pane" style={{ padding: 16, display: "flex", flexDirection: "column", gap: 16 }}>
      <div className="stat-row">
        <div className="card">
          <div className="faint xs">Items</div>
          <strong>{(desc.ItemCount ?? 0).toLocaleString()}</strong>
        </div>
        <div className="card">
          <div className="faint xs">Size</div>
          <strong>{((desc.TableSizeBytes ?? 0) / 1024).toFixed(1)} KB</strong>
        </div>
        <div className="card">
          <div className="faint xs">Billing</div>
          <strong>{desc.BillingModeSummary?.BillingMode ?? "PROVISIONED"}</strong>
        </div>
        <div className="card">
          <div className="faint xs">TTL</div>
          <strong>{desc.TimeToLiveDescription?.TimeToLiveStatus === "ENABLED" ? desc.TimeToLiveDescription.AttributeName : "off"}</strong>
        </div>
      </div>
      <table className="simple-table">
        <thead>
          <tr>
            <th>Key</th>
            <th>Attribute</th>
            <th>Type</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <td>Partition</td>
            <td className="mono">{keys.hash}</td>
            <td>{attrType(desc, keys.hash)}</td>
          </tr>
          {keys.range && (
            <tr>
              <td>Sort</td>
              <td className="mono">{keys.range}</td>
              <td>{attrType(desc, keys.range)}</td>
            </tr>
          )}
        </tbody>
      </table>
      {indexes.length > 0 && (
        <table className="simple-table">
          <thead>
            <tr>
              <th>Index</th>
              <th>Kind</th>
              <th>Partition</th>
              <th>Sort</th>
              <th>Projection</th>
              <th>Items</th>
            </tr>
          </thead>
          <tbody>
            {indexes.map((i) => {
              const k = keyNames(i.KeySchema);
              return (
                <tr key={i.IndexName}>
                  <td className="mono">{i.IndexName}</td>
                  <td>{i.kind}</td>
                  <td className="mono">{k.hash}</td>
                  <td className="mono">{k.range ?? "—"}</td>
                  <td>
                    {i.Projection.ProjectionType}
                    {i.Projection.NonKeyAttributes?.length ? <span className="faint"> ({i.Projection.NonKeyAttributes.join(", ")})</span> : null}
                  </td>
                  <td>{i.ItemCount ?? "—"}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      <details>
        <summary className="faint small" style={{ cursor: "pointer" }}>
          DescribeTable response
        </summary>
        <div className="doc-list" style={{ padding: "8px 0" }}>
          <DocCard doc={desc as unknown as Doc} />
        </div>
      </details>
    </div>
  );
}

// ------------------------------------------------------------------ console

interface DynOutput {
  operation: string;
  request: Record<string, unknown>;
  response?: Record<string, unknown>;
  error?: { message: string; code?: string; detail?: string };
  duration_ms: number;
}

export function DynamoQueryView({ tab }: { tab: QueryTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const ws = useStore((s) => s.workspace);
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const editor = useRef<SqlEditorHandle>(null);
  const editorBox = useRef<HTMLDivElement>(null);
  const [text, setText] = useState(tab.sql);
  const [outputs, setOutputs] = useState<DynOutput[] | null>(null);
  const [error, setError] = useState<unknown>();
  const [running, setRunning] = useState(false);
  const [panel, setPanel] = useState("o0");
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const [showDocs, setShowDocs] = useState(false);
  const [view, setView] = useStoredView("twinbase.dynamo.consoleview");
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
      const r = await call<{ outputs: DynOutput[] }>("dynamo.run", { connection, command: script });
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
  const currentItems = (current?.response?.Items ?? (current?.response?.Item ? [current.response.Item] : undefined)) as DynItem[] | undefined;

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
        <button className="btn sm primary" disabled={!connection || running} onClick={() => execute(editor.current?.selectionOrAll() ?? text)} title="Run all requests (⌘/Ctrl+Enter)">
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
        {target?.isMock && <span className="badge accent">mock</span>}
      </div>
      {showDocs && saved?.docs && <div className="markdown note-box" style={{ margin: "8px 10px 0", maxHeight: 180, overflow: "auto" }} dangerouslySetInnerHTML={{ __html: renderMarkdown(saved.docs) }} />}
      <div className="editor-host" ref={editorBox} style={{ height: "var(--editor-h, 40%)", flex: "none", ["--editor-h" as string]: restoredSize("--editor-h") }}>
        <SqlEditor
          ref={editor}
          value={text}
          kind="dynamodb"
          onChange={setText}
          onRun={() => execute(editor.current?.selectionOrAll() ?? text)}
          onRunCurrent={() => execute(editor.current?.currentStatement() ?? text)}
          onSave={save}
          placeholder={'Scan { "TableName": "orders", "Limit": 20 }\n\nEach request starts with an operation name (Query, GetItem, PutItem, UpdateItem, TransactWriteItems, CreateTable…) followed by its JSON.'}
        />
      </div>
      <Resizer axis="y" target={() => editorBox.current} variable="--editor-h" min={80} max={2000} />
      <div className="result-tabs" role="tablist">
        {(outputs ?? []).map((o, i) => (
          <button key={i} className={panel === `o${i}` ? "on" : ""} onClick={() => setPanel(`o${i}`)}>
            {o.error ? <XCircle size={13} style={{ color: "var(--danger)" }} /> : null}
            <span className="mono">{o.operation}</span>
            {typeof o.request.TableName === "string" && <span className="faint xs">{o.request.TableName}</span>}
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
              <span className="mono">{current.operation}</span>
              <span className="faint xs grow">{typeof current.request.TableName === "string" ? current.request.TableName : ""}</span>
              {current.response && typeof current.response.Count === "number" && (
                <span className="badge">
                  {plural(current.response.Count as number, "item")}
                  {current.response.ScannedCount !== current.response.Count ? ` / ${current.response.ScannedCount} read` : ""}
                </span>
              )}
              {current.response?.LastEvaluatedKey ? <span className="badge warning">more pages</span> : null}
              <span className="faint xs">{duration(current.duration_ms)}</span>
              {currentItems && <ViewToggle view={view} setView={setView} />}
            </div>
            {current.error ? (
              <div style={{ padding: 12 }}>
                <ErrorBox error={`${current.error.message}${current.error.detail ? `\n${current.error.detail}` : ""}`} />
                <div className="faint small" style={{ marginTop: 8 }}>Requests after this one did not run.</div>
              </div>
            ) : currentItems ? (
              <div className={view === "table" ? "doc-grid-host" : "pane"}>
                <DocList docs={currentItems.map((i) => unmarshalItem(i)) as Doc[]} view={view} />
              </div>
            ) : (
              <div className="pane">
                <div className="doc-list">
                  <DocCard doc={(current.response ?? {}) as Doc} />
                </div>
              </div>
            )}
          </>
        ) : (
          <div className="messages">
            <span className="faint">Run requests to see responses here. Items are shown as plain values; open the response for typed JSON.</span>
          </div>
        )}
      </div>
    </div>
  );
}
