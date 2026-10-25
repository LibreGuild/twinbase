// Table tab: paged rows with staged edits (review → preview → apply), filters, value inspector,
// structure and DDL.

import {
  ChevronLeft, ChevronRight, ClipboardCopy, Code2, Copy, Eye, FilterIcon, KeyRound, Link2, Network, PanelRight, Plus, RefreshCw,
  Rows3, Save, Sparkles, Trash2, Undo2, Wand2, X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DataGrid, type GridColumn, type RowState } from "../components/DataGrid";
import { PreviewPanel } from "../components/PreviewPanel";
import { CodeBlock, CopyButton, Dialog, Empty, ErrorBox, Resizer, restoredSize, Spinner, useAsync, useContextMenu } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { cellText, kindLabel, plural, prettyValue, qualifiedName, quoteIdent, sqlLiteral } from "../lib/format";
import { attempt, targetInfo, useStore, type Tab } from "../lib/store";
import { useGraph } from "../lib/schema";
import type { Cell, ChangeSet, Filter, FilterOp, PreviewResult, Row, RowQuery, RowsPage, SortKey, TableSchema } from "../lib/types";

type TableTab = Extract<Tab, { kind: "table" }>;

const OPS: { op: FilterOp; label: string; needsValue: boolean }[] = [
  { op: "eq", label: "=", needsValue: true },
  { op: "ne", label: "≠", needsValue: true },
  { op: "lt", label: "<", needsValue: true },
  { op: "le", label: "≤", needsValue: true },
  { op: "gt", label: ">", needsValue: true },
  { op: "ge", label: "≥", needsValue: true },
  { op: "contains", label: "contains", needsValue: true },
  { op: "starts_with", label: "starts with", needsValue: true },
  { op: "ends_with", label: "ends with", needsValue: true },
  { op: "is_null", label: "is null", needsValue: false },
  { op: "is_not_null", label: "is not null", needsValue: false },
];

interface Staged {
  /** Original row index → column index → new value. */
  updates: Map<number, Map<number, Cell>>;
  /** New rows: column index → value (missing = column default). */
  inserts: Map<number, Cell>[];
  deletes: Set<number>;
}

const emptyStaged = (): Staged => ({ updates: new Map(), inserts: [], deletes: new Set() });

export function TableView({ tab }: { tab: TableTab }) {
  const { connection, schema, table } = tab;
  const target = targetInfo(connection);
  const kind = target?.kind ?? "postgres";
  const pageSize = useStore((s) => s.settings?.page_size ?? 200);
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const schemaVersion = useStore((s) => s.schemaVersion[connection] ?? 0);
  const setDirty = useStore((s) => s.setDirty);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);

  const [section, setSection] = useState<"data" | "structure" | "ddl">("data");
  const [query, setQuery] = useState<RowQuery>({ offset: 0, limit: pageSize, sort: [], filters: tab.filters ?? [] });
  const [page, setPage] = useState<RowsPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>();
  const [staged, setStaged] = useState<Staged>(emptyStaged);
  const [selected, setSelected] = useState<{ row: number; col: number } | null>(null);
  const [inspector, setInspector] = useState(false);
  const [stale, setStale] = useState(false);
  const [review, setReview] = useState(false);
  const [whereDraft, setWhereDraft] = useState("");
  const menu = useContextMenu();
  const drawerRef = useRef<HTMLDivElement>(null);

  const describe = useAsync(() => call<TableSchema>("db.describe", { connection, schema, table }), [connection, schema, table, schemaVersion]);
  const info = describe.data;

  const changeCount = staged.updates.size + staged.inserts.length + staged.deletes.size;
  useEffect(() => {
    setDirty(tab.id, changeCount > 0);
  }, [changeCount, tab.id, setDirty]);

  // Filters pushed from elsewhere (e.g. "open referenced row").
  useEffect(() => {
    if (tab.filters) setQuery((q) => ({ ...q, offset: 0, filters: tab.filters! }));
  }, [tab.filters]);

  const load = useCallback(
    async (q: RowQuery) => {
      setLoading(true);
      try {
        const p = await call<RowsPage>("db.rows", { connection, schema, table, query: q });
        setPage(p);
        setError(undefined);
        setStale(false);
      } catch (e) {
        setError(e);
      } finally {
        setLoading(false);
      }
    },
    [connection, schema, table],
  );

  useEffect(() => {
    load(query);
  }, [load, query, schemaVersion]);

  // Changes made elsewhere (other tabs, agents): reload unless the user has staged edits.
  const firstVersion = useRef(dataVersion);
  useEffect(() => {
    if (dataVersion === firstVersion.current) return;
    firstVersion.current = dataVersion;
    if (changeCount === 0) load(query);
    else setStale(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dataVersion]);

  // Display columns (hide the synthetic row-id column).
  const rowIdIndex = page?.row_id_column ? page.result.columns.findIndex((c) => c.name === `__${page.row_id_column}`) : -1;
  const colIndexes = useMemo(() => (page ? page.result.columns.map((_, i) => i).filter((i) => i !== rowIdIndex) : []), [page, rowIdIndex]);
  const columns: GridColumn[] = useMemo(() => {
    if (!page) return [];
    return colIndexes.map((i) => {
      const c = page.result.columns[i];
      const ci = info?.columns.find((x) => x.name === c.name);
      const fk = info?.foreign_keys.find((f) => f.columns.length === 1 && f.columns[0] === c.name);
      return { name: c.name, type: ci?.data_type ?? c.type_name, pk: ci?.primary_key, fk: fk ? `${fk.ref_table}.${fk.ref_columns[0]}` : undefined };
    });
  }, [page, colIndexes, info]);

  const baseRows = useMemo(() => (page ? page.result.rows.map((r) => colIndexes.map((i) => r[i])) : []), [page, colIndexes]);

  const view = useMemo(() => {
    const rows: Cell[][] = [];
    const states: RowState[] = [];
    const edited = new Set<string>();
    const defaults = new Set<string>();
    baseRows.forEach((r, ri) => {
      const upd = staged.updates.get(ri);
      rows.push(upd ? r.map((v, ci) => (upd.has(ci) ? upd.get(ci)! : v)) : r);
      states.push(staged.deletes.has(ri) ? "deleted" : undefined);
      upd?.forEach((_, ci) => edited.add(`${ri}:${ci}`));
    });
    staged.inserts.forEach((ins) => {
      const ri = rows.length;
      rows.push(columns.map((_, ci) => (ins.has(ci) ? ins.get(ci)! : null)));
      states.push("added");
      columns.forEach((_, ci) => (ins.has(ci) ? edited.add(`${ri}:${ci}`) : defaults.add(`${ri}:${ci}`)));
    });
    return { rows, states, edited, defaults };
  }, [baseRows, staged, columns]);

  const keyOf = useCallback(
    (ri: number): Row => {
      const key: Row = {};
      if (!page) return key;
      for (const k of page.key_columns) {
        if (page.row_id_column === k) key[k] = page.result.rows[ri][rowIdIndex];
        else key[k] = page.result.rows[ri][page.result.columns.findIndex((c) => c.name === k)];
      }
      return key;
    },
    [page, rowIdIndex],
  );

  const changeSet = useCallback((): ChangeSet => {
    const cs: ChangeSet = { updates: [], inserts: [], deletes: [] };
    staged.updates.forEach((cols, ri) => {
      if (staged.deletes.has(ri)) return;
      const values: Row = {};
      cols.forEach((v, ci) => (values[columns[ci].name] = v));
      cs.updates.push({ key: keyOf(ri), values });
    });
    for (const ins of staged.inserts) {
      const row: Row = {};
      ins.forEach((v, ci) => (row[columns[ci].name] = v));
      cs.inserts.push(row);
    }
    staged.deletes.forEach((ri) => cs.deletes.push(keyOf(ri)));
    return cs;
  }, [staged, columns, keyOf]);

  const editCell = (row: number, col: number, value: Cell) => {
    setStaged((s) => {
      const next: Staged = { updates: new Map(s.updates), inserts: s.inserts.slice(), deletes: new Set(s.deletes) };
      if (row >= baseRows.length) {
        const i = row - baseRows.length;
        const m = new Map(next.inserts[i]);
        m.set(col, value);
        next.inserts[i] = m;
      } else {
        const m = new Map(next.updates.get(row) ?? []);
        if (baseRows[row][col] === value) m.delete(col);
        else m.set(col, value);
        if (m.size) next.updates.set(row, m);
        else next.updates.delete(row);
      }
      return next;
    });
  };

  const toggleDelete = (row: number) => {
    setStaged((s) => {
      const next: Staged = { updates: new Map(s.updates), inserts: s.inserts.slice(), deletes: new Set(s.deletes) };
      if (row >= baseRows.length) next.inserts.splice(row - baseRows.length, 1);
      else if (next.deletes.has(row)) next.deletes.delete(row);
      else next.deletes.add(row);
      return next;
    });
  };

  const addRow = (from?: Cell[]) => {
    setStaged((s) => {
      const m = new Map<number, Cell>();
      if (from) from.forEach((v, ci) => !info?.columns.find((c) => c.name === columns[ci].name)?.auto && m.set(ci, v));
      return { ...s, inserts: [...s.inserts, m] };
    });
  };

  const discard = () => {
    setStaged(emptyStaged());
    if (stale) load(query);
  };

  const apply = async () => {
    const r = await attempt("Could not save changes", () => call<{ affected: number }>("db.apply", { connection, schema, table, changes: changeSet() }));
    if (r) {
      toast("success", "Changes saved", `${plural(r.affected, "row")} affected in ${table}.`);
      setStaged(emptyStaged());
      setReview(false);
      load(query);
    }
  };

  const sortBy = (column: string, additive: boolean) => {
    if (target?.kind === "cassandra") {
      toast("info", "Cassandra returns rows in partition order", "Sort with ORDER BY on clustering columns in a query on one partition.");
      return;
    }
    setQuery((q) => {
      const cur = q.sort.find((s) => s.column === column);
      let sort: SortKey[];
      if (!cur) sort = additive ? [...q.sort, { column }] : [{ column }];
      else if (!cur.descending) sort = q.sort.map((s) => (s.column === column ? { column, descending: true } : s));
      else sort = q.sort.filter((s) => s.column !== column);
      if (!additive && cur) sort = sort.filter((s) => s.column === column);
      return { ...q, offset: 0, sort };
    });
  };

  const setFilters = (filters: Filter[]) => setQuery((q) => ({ ...q, offset: 0, filters }));

  const rowAsObject = (r: number): Row => Object.fromEntries(columns.map((c, i) => [c.name, view.rows[r][i]]));

  const onContextMenu = (e: React.MouseEvent, r: number, c: number) => {
    const v = view.rows[r][c];
    const col = columns[c];
    const fk = info?.foreign_keys.find((f) => f.columns.includes(col.name));
    const editable = !target?.readOnly && target?.kind !== "clickhouse" && target?.kind !== "bigquery" && view.states[r] !== "deleted";
    menu.open(e, [
      { label: "Copy value", icon: <Copy size={14} />, onClick: () => navigator.clipboard.writeText(cellText(v)) },
      { label: "Copy row as JSON", icon: <ClipboardCopy size={14} />, onClick: () => navigator.clipboard.writeText(JSON.stringify(rowAsObject(r), null, 2)) },
      {
        label: "Copy row as INSERT",
        icon: <Code2 size={14} />,
        onClick: () =>
          navigator.clipboard.writeText(
            `INSERT INTO ${qualifiedName(kind, schema, table)} (${columns.map((x) => quoteIdent(kind, x.name)).join(", ")}) VALUES (${view.rows[r].map(sqlLiteral).join(", ")});`,
          ),
      },
      "sep",
      { label: `Filter: ${col.name} = this value`, icon: <FilterIcon size={14} />, onClick: () => setFilters([...query.filters, v === null ? { column: col.name, op: "is_null" } : { column: col.name, op: "eq", value: v }]) },
      ...(fk && v !== null
        ? [
            {
              label: `Open ${fk.ref_table} row`,
              icon: <Link2 size={14} />,
              onClick: () => {
                const filter: Filter[] = [{ column: fk.ref_columns[fk.columns.indexOf(col.name)], op: "eq", value: v }];
                openTab({ kind: "table", connection, schema: fk.ref_schema || schema, table: fk.ref_table });
                const st = useStore.getState();
                st.updateTab(st.activeTab, { filters: filter });
              },
            },
          ]
        : []),
      "sep",
      { label: "Set NULL", disabled: !editable, onClick: () => editCell(r, c, null) },
      { label: "Edit in inspector", icon: <PanelRight size={14} />, onClick: () => setInspector(true) },
      { label: "Duplicate row", icon: <Plus size={14} />, disabled: target?.readOnly, onClick: () => addRow(view.rows[r]) },
      view.states[r] === "deleted"
        ? { label: "Undo delete", icon: <Undo2 size={14} />, onClick: () => toggleDelete(r) }
        : { label: view.states[r] === "added" ? "Remove new row" : "Delete row", icon: <Trash2 size={14} />, danger: true, disabled: target?.readOnly, onClick: () => toggleDelete(r) },
    ]);
  };

  const total = page?.total ?? null;
  const from = page ? query.offset + 1 : 0;
  const to = page ? query.offset + page.result.rows.length : 0;
  // ClickHouse has no transactions to apply staged edits safely: change its data with SQL.
  const readOnly = !!target?.readOnly || info?.kind === "view" || target?.kind === "clickhouse" || target?.kind === "bigquery";
  const noKey = !!page && !page.row_id_column && !(info?.primary_key.length ?? 0);

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <Rows3 size={15} className="muted" />
          <span className="ellipsis" style={{ maxWidth: 280 }} title={`${schema}.${table}`}>
            {table}
          </span>
          {info?.kind === "view" && <span className="badge">view</span>}
          {readOnly && (
            <span
              className="badge"
              title={
                target?.kind === "clickhouse"
                  ? "ClickHouse has no transactions; change data with SQL (INSERT, ALTER TABLE … UPDATE/DELETE)"
                  : target?.kind === "bigquery"
                    ? "Change BigQuery tables with DML in the SQL editor. Unfiltered pages are read with tabledata.list (not billed); filters and sorting run billed queries."
                    : "Editing disabled"
              }
            >
              read-only
            </span>
          )}
        </div>
        <div className="sep" />
        <div className="seg" role="tablist">
          {(["data", "structure", "ddl"] as const).map((s) => (
            <button key={s} className={section === s ? "on" : ""} onClick={() => setSection(s)} role="tab" aria-selected={section === s}>
              {s === "data" ? "Data" : s === "structure" ? "Structure" : "DDL"}
            </button>
          ))}
        </div>
        <div className="grow" />
        <button className="btn ghost sm" onClick={() => openTab({ kind: "query", connection, sql: `SELECT *\nFROM ${qualifiedName(kind, schema, table)}\nLIMIT 100;`, title: `${table} query` })} title="New SQL query on this table">
          <Code2 size={14} /> Query
        </button>
        <button className="btn ghost sm" onClick={() => openTab({ kind: "diagram", connection, schema })} title="Open ER diagram">
          <Network size={14} /> Diagram
        </button>
        {!readOnly && (
          <button className="btn ghost sm" onClick={() => openDialog({ kind: "generate", connection, schema, table })} title="Insert generated sample rows">
            <Sparkles size={14} /> Generate
          </button>
        )}
        {!readOnly && info?.kind !== "view" && (
          <button className="btn ghost sm" onClick={() => openDialog({ kind: "table-designer", connection, schema, table })} title="Change columns">
            <Wand2 size={14} /> Edit table
          </button>
        )}
      </div>

      {section === "data" && (
        <>
          <FilterBar columns={columns.map((c) => c.name)} filters={query.filters} onChange={setFilters} whereDraft={whereDraft} setWhereDraft={setWhereDraft} where={query.where_sql} onWhere={(w) => setQuery((q) => ({ ...q, offset: 0, where_sql: w || undefined }))} />
          {stale && (
            <div className="note-box row" style={{ margin: "6px 10px 0", borderColor: "var(--warning-soft)" }}>
              <RefreshCw size={14} />
              <span className="grow">Rows changed elsewhere while you have unsaved edits.</span>
              <button className="btn sm" onClick={discard}>
                Discard edits and reload
              </button>
            </div>
          )}
          <div style={{ display: "flex", flex: 1, minHeight: 0 }}>
            <div className="grid-wrap">
              {error ? (
                <div style={{ padding: 14 }}>
                  <ErrorBox error={error} />
                </div>
              ) : !page ? (
                <Empty icon={<Spinner size={20} />} title="Loading rows…" />
              ) : (
                <DataGrid
                  columns={columns}
                  rows={view.rows}
                  rowStates={view.states}
                  edited={view.edited}
                  defaults={view.defaults}
                  editable={!readOnly && !noKey}
                  rowNumberOffset={query.offset}
                  sort={query.sort}
                  onSort={sortBy}
                  onEdit={editCell}
                  onSelect={setSelected}
                  onContextMenu={onContextMenu}
                  widthKey={`${connection}|${schema}|${table}`}
                />
              )}
              <div className="grid-footer">
                {loading && <Spinner size={12} />}
                <span>
                  {page
                    ? total !== null
                      ? `${from.toLocaleString()}–${to.toLocaleString()} of ${total.toLocaleString()}`
                      : page.total_at_least
                        ? `${from.toLocaleString()}–${to.toLocaleString()} of ${page.total_at_least.toLocaleString()}+`
                        : `${from}–${to}`
                    : ""}
                </span>
                <button className="icon-btn sm" disabled={query.offset === 0} onClick={() => setQuery((q) => ({ ...q, offset: Math.max(0, q.offset - q.limit) }))} aria-label="Previous page">
                  <ChevronLeft size={14} />
                </button>
                <button className="icon-btn sm" disabled={!page || (total !== null ? to >= total : page.result.rows.length < query.limit)} onClick={() => setQuery((q) => ({ ...q, offset: q.offset + q.limit }))} aria-label="Next page">
                  <ChevronRight size={14} />
                </button>
                <button className="icon-btn sm" onClick={() => load(query)} title="Reload" aria-label="Reload">
                  <RefreshCw size={13} />
                </button>
                {noKey && !readOnly && <span className="badge warning" title="Rows cannot be identified safely, so editing is disabled">no primary key</span>}
                <span className="spacer" />
                {!readOnly && !noKey && (
                  <button className="btn sm ghost" onClick={() => addRow()}>
                    <Plus size={13} /> Row
                  </button>
                )}
                {changeCount > 0 && (
                  <>
                    <span className="badge accent">{plural(changeCount, "change")}</span>
                    <button className="btn sm ghost" onClick={discard}>
                      <X size={13} /> Discard
                    </button>
                    <button className="btn sm" onClick={() => setReview(true)}>
                      <Eye size={13} /> Review
                    </button>
                    <button className="btn sm primary" onClick={apply}>
                      <Save size={13} /> Save
                    </button>
                  </>
                )}
                <button className={`icon-btn sm${inspector ? " on" : ""}`} onClick={() => setInspector((v) => !v)} title="Value inspector" aria-label="Toggle value inspector">
                  <PanelRight size={14} />
                </button>
              </div>
            </div>
            {inspector && (
              <>
                <Resizer axis="x" target={() => drawerRef.current} variable="--drawer-w" min={260} max={760} invert />
                <div className="drawer" ref={drawerRef} style={{ ["--drawer-w" as string]: restoredSize("--drawer-w") }}>
                  <Inspector
                    columns={columns}
                    row={selected ? view.rows[selected.row] : undefined}
                    col={selected?.col}
                    editable={!readOnly && !noKey && !!selected && view.states[selected.row] !== "deleted"}
                    onSave={(v) => selected && editCell(selected.row, selected.col, v)}
                    onSelectCol={(c) => selected && setSelected({ row: selected.row, col: c })}
                    onClose={() => setInspector(false)}
                  />
                </div>
              </>
            )}
          </div>
        </>
      )}

      {section === "structure" && <Structure info={info} loading={describe.loading} error={describe.error} connection={connection} schema={schema} />}
      {section === "ddl" && <TableDdl connection={connection} schema={schema} table={table} />}

      {review && <ReviewDialog connection={connection} schema={schema} table={table} changes={changeSet()} onClose={() => setReview(false)} onApply={apply} />}
      {menu.node}
    </>
  );
}

function FilterBar(props: {
  columns: string[];
  filters: Filter[];
  onChange: (f: Filter[]) => void;
  where?: string;
  whereDraft: string;
  setWhereDraft: (s: string) => void;
  onWhere: (w: string) => void;
}) {
  const [showWhere, setShowWhere] = useState(!!props.where);
  const update = (i: number, patch: Partial<Filter>) => props.onChange(props.filters.map((f, j) => (j === i ? { ...f, ...patch } : f)));
  return (
    <div className="filter-bar">
      {props.filters.map((f, i) => {
        const needsValue = OPS.find((o) => o.op === f.op)?.needsValue;
        return (
          <div className="filter-chip" key={i}>
            <select className="select sm" value={f.column} onChange={(e) => update(i, { column: e.target.value })} aria-label="Filter column">
              {props.columns.map((c) => (
                <option key={c}>{c}</option>
              ))}
            </select>
            <select className="select sm" value={f.op} onChange={(e) => update(i, { op: e.target.value as FilterOp })} aria-label="Filter operator">
              {OPS.map((o) => (
                <option key={o.op} value={o.op}>
                  {o.label}
                </option>
              ))}
            </select>
            {needsValue && <FilterValue value={f.value} onCommit={(v) => update(i, { value: v })} />}
            <button className="icon-btn sm" onClick={() => props.onChange(props.filters.filter((_, j) => j !== i))} aria-label="Remove filter">
              <X size={12} />
            </button>
          </div>
        );
      })}
      <button className="btn sm ghost" disabled={!props.columns.length} onClick={() => props.onChange([...props.filters, { column: props.columns[0], op: "eq", value: "" }])}>
        <FilterIcon size={13} /> Filter
      </button>
      <button className="btn sm ghost" onClick={() => setShowWhere((v) => !v)} title="Raw SQL WHERE clause">
        WHERE
      </button>
      {showWhere && (
        <form
          className="row grow"
          onSubmit={(e) => {
            e.preventDefault();
            props.onWhere(props.whereDraft);
          }}
        >
          <input className="input sm mono grow" placeholder="e.g. created_at > now() - interval '7 days'" value={props.whereDraft} onChange={(e) => props.setWhereDraft(e.target.value)} aria-label="WHERE clause" />
          <button className="btn sm" type="submit">
            Apply
          </button>
          {props.where && (
            <button
              className="btn sm ghost"
              type="button"
              onClick={() => {
                props.setWhereDraft("");
                props.onWhere("");
              }}
            >
              Clear
            </button>
          )}
        </form>
      )}
    </div>
  );
}

function FilterValue({ value, onCommit }: { value?: Cell; onCommit: (v: Cell) => void }) {
  const [text, setText] = useState(value === null || value === undefined ? "" : String(value));
  return (
    <input
      className="input sm mono"
      style={{ width: 140 }}
      value={text}
      placeholder="value"
      aria-label="Filter value"
      onChange={(e) => setText(e.target.value)}
      onBlur={() => onCommit(text)}
      onKeyDown={(e) => e.key === "Enter" && onCommit(text)}
    />
  );
}

function Inspector(props: { columns: GridColumn[]; row?: Cell[]; col?: number; editable: boolean; onSave: (v: Cell) => void; onSelectCol: (c: number) => void; onClose: () => void }) {
  const { row, col } = props;
  const value = row && col !== undefined ? row[col] : undefined;
  const [text, setText] = useState("");
  useEffect(() => {
    setText(value === undefined || value === null ? "" : prettyValue(value));
  }, [value, col]);
  const column = col !== undefined ? props.columns[col] : undefined;
  return (
    <>
      <div className="drawer-head">
        <strong className="grow ellipsis">{column ? column.name : "Value inspector"}</strong>
        {column && <span className="faint xs mono">{column.type}</span>}
        <button className="icon-btn" onClick={props.onClose} aria-label="Close inspector">
          <X size={14} />
        </button>
      </div>
      <div className="drawer-body">
        {value === undefined ? (
          <div className="faint">Select a cell to see its full value.</div>
        ) : (
          <>
            {props.editable ? (
              <textarea className="textarea mono" style={{ width: "100%", minHeight: 160 }} value={text} onChange={(e) => setText(e.target.value)} aria-label="Value" />
            ) : (
              <pre>{value === null ? "NULL" : prettyValue(value)}</pre>
            )}
            <div className="row" style={{ marginTop: 8 }}>
              <CopyButton text={value === null ? "" : cellText(value)} label="Copy" small />
              <span className="grow" />
              {props.editable && (
                <>
                  <button className="btn sm ghost" onClick={() => props.onSave(null)}>
                    Set NULL
                  </button>
                  <button
                    className="btn sm primary"
                    onClick={() => {
                      // Keep JSON compact-equivalent when the user did not change it.
                      if (value !== null && prettyValue(value) === text) return;
                      props.onSave(typeof value === "number" && /^-?\d+(\.\d+)?$/.test(text.trim()) ? Number(text) : text);
                    }}
                  >
                    Stage edit
                  </button>
                </>
              )}
            </div>
            <h4 style={{ margin: "18px 0 6px", fontSize: 12 }} className="muted">
              Row
            </h4>
            <dl className="kv">
              {props.columns.map((c, i) => (
                <div key={c.name} style={{ display: "contents" }}>
                  <dt style={{ cursor: "pointer", color: i === col ? "var(--accent)" : undefined }} onClick={() => props.onSelectCol(i)}>
                    {c.name}
                  </dt>
                  <dd className="mono ellipsis" title={row ? cellText(row[i]) : ""}>
                    {row && (row[i] === null ? <i className="faint">NULL</i> : cellText(row[i]).slice(0, 200))}
                  </dd>
                </div>
              ))}
            </dl>
          </>
        )}
      </div>
    </>
  );
}

function Structure({ info, loading, error, connection, schema }: { info?: TableSchema; loading: boolean; error: unknown; connection: string; schema: string }) {
  const { tables } = useGraph(connection, schema);
  const openTab = useStore((s) => s.openTab);
  if (error) return <div style={{ padding: 14 }}><ErrorBox error={error} /></div>;
  if (!info) return <Empty icon={loading ? <Spinner size={20} /> : undefined} title="Loading structure…" />;
  const referencedBy = (tables ?? []).flatMap((t) => t.foreign_keys.filter((f) => f.ref_table === info.name).map((f) => ({ table: t.name, fk: f })));
  return (
    <div className="pane">
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 18 }}>
        <section>
          <h4 style={{ margin: "0 0 8px" }}>Columns</h4>
          <table className="simple-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Type</th>
                <th>Nullable</th>
                <th>Default</th>
                <th>Keys</th>
              </tr>
            </thead>
            <tbody>
              {info.columns.map((c) => (
                <tr key={c.name}>
                  <td className="mono">
                    {c.primary_key && <KeyRound size={12} style={{ color: "var(--warning)", marginRight: 5, verticalAlign: -1 }} />}
                    {c.name}
                  </td>
                  <td className="mono">{c.data_type}</td>
                  <td>{c.nullable ? "yes" : <span className="badge">NOT NULL</span>}</td>
                  <td className="mono faint" title={c.default}>
                    {c.default ?? ""}
                  </td>
                  <td>
                    <div className="row wrap" style={{ gap: 4 }}>
                      {c.primary_key && <span className="badge warning">PK</span>}
                      {c.unique && !c.primary_key && <span className="badge info">unique</span>}
                      {c.auto && <span className="badge">auto</span>}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
        <section>
          <h4 style={{ margin: "0 0 8px" }}>Foreign keys</h4>
          {info.foreign_keys.length === 0 ? (
            <div className="faint small">None</div>
          ) : (
            <table className="simple-table">
              <tbody>
                {info.foreign_keys.map((f) => (
                  <tr key={f.name}>
                    <td className="mono">{f.columns.join(", ")}</td>
                    <td>
                      →{" "}
                      <a href="#" onClick={(e) => (e.preventDefault(), openTab({ kind: "table", connection, schema: f.ref_schema || schema, table: f.ref_table }))}>
                        {f.ref_table}
                      </a>
                      <span className="mono faint"> ({f.ref_columns.join(", ")})</span>
                    </td>
                    <td className="faint">{f.on_delete ? `ON DELETE ${f.on_delete}` : ""}</td>
                    <td className="mono faint">{f.name}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
        <section>
          <h4 style={{ margin: "0 0 8px" }}>Referenced by</h4>
          {referencedBy.length === 0 ? (
            <div className="faint small">No other table points to this one.</div>
          ) : (
            <table className="simple-table">
              <tbody>
                {referencedBy.map(({ table, fk }) => (
                  <tr key={`${table}.${fk.name}`}>
                    <td>
                      <a href="#" onClick={(e) => (e.preventDefault(), openTab({ kind: "table", connection, schema, table }))}>
                        {table}
                      </a>
                      <span className="mono faint"> ({fk.columns.join(", ")})</span>
                    </td>
                    <td className="faint">{fk.on_delete ? `ON DELETE ${fk.on_delete}` : ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
        <section>
          <h4 style={{ margin: "0 0 8px" }}>Indexes</h4>
          {info.indexes.length === 0 ? (
            <div className="faint small">None</div>
          ) : (
            <table className="simple-table">
              <tbody>
                {info.indexes.map((ix) => (
                  <tr key={ix.name}>
                    <td className="mono">{ix.name}</td>
                    <td className="mono">{ix.columns.join(", ")}</td>
                    <td>
                      {ix.primary ? <span className="badge warning">primary</span> : ix.unique ? <span className="badge info">unique</span> : <span className="badge">index</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      </div>
    </div>
  );
}

function TableDdl({ connection, schema, table }: { connection: string; schema: string; table: string }) {
  const kind = targetInfo(connection)?.kind ?? "postgres";
  const [dialect, setDialect] = useState(kind === "redis" ? "postgres" : kind);
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const ddl = useAsync(() => call<{ sql: string }>("db.ddl", { connection, schema, table, dialect }), [connection, schema, table, dialect, version]);
  return (
    <div className="pane">
      <div style={{ padding: "12px 14px" }}>
        <div className="row" style={{ marginBottom: 10 }}>
          <span className="muted small">Dialect</span>
          <div className="seg">
            {(["postgres", "mysql", "sqlite", "mssql", "clickhouse", "cassandra", "spanner", "bigquery", "oracle"] as const).map((d) => (
              <button key={d} className={dialect === d ? "on" : ""} onClick={() => setDialect(d)}>
                {kindLabel(d)}
              </button>
            ))}
          </div>
          {ddl.loading && <Spinner />}
        </div>
        <ErrorBox error={ddl.error} />
        {ddl.data && <CodeBlock code={ddl.data.sql} maxHeight={10000} />}
      </div>
    </div>
  );
}

function ReviewDialog(props: { connection: string; schema: string; table: string; changes: ChangeSet; onClose: () => void; onApply: () => Promise<void> }) {
  const sql = useAsync(() => call<string[]>("db.changes_sql", { connection: props.connection, schema: props.schema, table: props.table, changes: props.changes }), []);
  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [previewError, setPreviewError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const runPreview = async () => {
    if (!sql.data) return;
    setBusy(true);
    try {
      setPreview(await call<PreviewResult>("db.preview", { connection: props.connection, schema: props.schema, sql: sql.data.map((s) => s.replace(/;\s*$/, "")).join(";\n") }));
      setPreviewError(undefined);
    } catch (e) {
      setPreviewError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      wide
      title="Review changes"
      onClose={props.onClose}
      footer={
        <>
          <button className="btn" onClick={runPreview} disabled={busy || !sql.data}>
            {busy ? <Spinner /> : <Eye size={14} />} Preview on twin
          </button>
          <span className="grow" />
          <button className="btn" onClick={props.onClose}>
            Keep editing
          </button>
          <button className="btn primary" onClick={props.onApply}>
            <Save size={14} /> Save to database
          </button>
        </>
      }
    >
      <div className="muted small">These statements run in one transaction. If any of them fails or matches an unexpected number of rows, nothing is saved.</div>
      <ErrorBox error={sql.error} />
      {sql.data && <CodeBlock code={sql.data.join("\n")} maxHeight={220} />}
      <ErrorBox error={previewError} />
      {preview && (
        <div style={{ border: "1px solid var(--border)", borderRadius: 8 }}>
          <PreviewPanel preview={preview} />
        </div>
      )}
    </Dialog>
  );
}
