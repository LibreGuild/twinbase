// ER diagram: tables as nodes, foreign keys as edges. Drag between columns to add a relationship;
// positions are remembered per schema.

import { Graph, layout } from "@dagrejs/dagre";
import {
  Background, Controls, Handle, MarkerType, MiniMap, Position, ReactFlow, ReactFlowProvider, useEdgesState, useNodesState, useReactFlow,
  type Connection, type Edge, type Node, type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { Code2, FileCode2, GitCompare, KeyRound, LayoutGrid, Link2, Maximize, Plus, RefreshCw, Rows3, Search, Sparkles, Trash2, Wand2, X } from "lucide-react";
import { memo, useCallback, useEffect, useState } from "react";
import { CodeBlock, ConfirmDialog, Dialog, Empty, ErrorBox, Spinner, useAsync } from "../components/ui";
import { call } from "../lib/api";
import { kindLabel, qualifiedName, quoteIdent } from "../lib/format";
import { useGraph } from "../lib/schema";
import { attempt, targetInfo, useStore, type Tab } from "../lib/store";
import { useAppliedTheme } from "../lib/theme";
import type { TableSchema } from "../lib/types";

type DiagramTab = Extract<Tab, { kind: "diagram" }>;

interface TableNodeData extends Record<string, unknown> {
  table: TableSchema;
  fkColumns: Set<string>;
  refColumns: Set<string>;
  dim: boolean;
  keysOnly: boolean;
}

const NODE_W = 260;
const HEAD_H = 34;
const COL_H = 24;
const MAX_COLS = 40;

function visibleColumns(t: TableSchema, keysOnly: boolean, fk: Set<string>, ref: Set<string>) {
  const cols = keysOnly ? t.columns.filter((c) => c.primary_key || fk.has(c.name) || ref.has(c.name)) : t.columns;
  return { shown: cols.slice(0, MAX_COLS), hidden: cols.length - Math.min(cols.length, MAX_COLS) };
}

const TableNode = memo(function TableNode({ data, selected }: NodeProps<Node<TableNodeData>>) {
  const { table, fkColumns, refColumns, keysOnly } = data;
  const { shown, hidden } = visibleColumns(table, keysOnly, fkColumns, refColumns);
  return (
    <div className={`er-node${selected ? " selected" : ""}${data.dim ? " dim" : ""}`} style={{ width: NODE_W }}>
      <div className="er-head">
        <Rows3 size={13} className="muted" />
        <span className="n" title={table.name}>
          {table.name}
        </span>
        {table.kind === "view" && <span className="badge">view</span>}
      </div>
      {shown.map((c) => (
        <div className="er-col" key={c.name}>
          <Handle type="target" position={Position.Left} id={`${c.name}-t`} className={fkColumns.has(c.name) ? "connected" : ""} />
          {c.primary_key ? <KeyRound size={11} className="pk" /> : fkColumns.has(c.name) ? <Link2 size={11} className="fk" /> : <span style={{ width: 11, flex: "none" }} />}
          <span className="cn" title={c.name} style={{ fontWeight: c.primary_key ? 600 : 400 }}>
            {c.name}
          </span>
          <span className="ct" title={c.data_type}>
            {c.data_type}
            {!c.nullable && !c.primary_key ? "" : c.nullable ? "?" : ""}
          </span>
          <Handle type="source" position={Position.Right} id={`${c.name}-s`} className={refColumns.has(c.name) ? "connected" : ""} />
        </div>
      ))}
      {hidden > 0 && <div className="er-col faint">+{hidden} more columns</div>}
    </div>
  );
});

const nodeTypes = { table: TableNode };

function storageKey(connection: string, schema: string) {
  return `twinbase.diagram.${connection}|${schema}`;
}

function loadPositions(connection: string, schema: string): Record<string, { x: number; y: number }> {
  try {
    return JSON.parse(localStorage.getItem(storageKey(connection, schema)) ?? "{}");
  } catch {
    return {};
  }
}

function autoLayout(tables: TableSchema[], heights: Map<string, number>): Record<string, { x: number; y: number }> {
  const g = new Graph();
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 110, marginx: 20, marginy: 20 });
  g.setDefaultEdgeLabel(() => ({}));
  for (const t of tables) g.setNode(t.name, { width: NODE_W, height: heights.get(t.name) ?? 120 });
  for (const t of tables) for (const f of t.foreign_keys) if (g.hasNode(f.ref_table) && f.ref_table !== t.name) g.setEdge(f.ref_table, t.name);
  layout(g);
  const out: Record<string, { x: number; y: number }> = {};
  for (const t of tables) {
    const n = g.node(t.name);
    out[t.name] = { x: n.x - NODE_W / 2, y: n.y - (heights.get(t.name) ?? 120) / 2 };
  }
  return out;
}

export function DiagramView({ tab }: { tab: DiagramTab }) {
  return (
    <ReactFlowProvider>
      <Diagram tab={tab} />
    </ReactFlowProvider>
  );
}

function Diagram({ tab }: { tab: DiagramTab }) {
  const { connection, schema } = tab;
  const target = targetInfo(connection);
  const kind = target?.kind ?? "postgres";
  const { tables, error } = useGraph(connection, schema);
  const theme = useAppliedTheme();
  const flow = useReactFlow();
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const [nodes, setNodes, onNodesChange] = useNodesState<Node<TableNodeData>>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);
  const [search, setSearch] = useState("");
  const [keysOnly, setKeysOnly] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [pendingFk, setPendingFk] = useState<{ sql: string; label: string } | null>(null);
  const [ddlOpen, setDdlOpen] = useState(false);
  const [drop, setDrop] = useState<string | null>(null);
  const [miniMap, setMiniMap] = useState(true);

  const build = useCallback(
    (relayout: boolean) => {
      if (!tables) return;
      const fkCols = new Map<string, Set<string>>();
      const refCols = new Map<string, Set<string>>();
      for (const t of tables) {
        fkCols.set(t.name, new Set(t.foreign_keys.flatMap((f) => f.columns)));
        for (const f of t.foreign_keys) {
          if (!refCols.has(f.ref_table)) refCols.set(f.ref_table, new Set());
          f.ref_columns.forEach((c) => refCols.get(f.ref_table)!.add(c));
        }
      }
      const heights = new Map(
        tables.map((t) => {
          const v = visibleColumns(t, keysOnly, fkCols.get(t.name)!, refCols.get(t.name) ?? new Set());
          return [t.name, HEAD_H + (v.shown.length + (v.hidden ? 1 : 0)) * COL_H + 2];
        }),
      );
      let saved = relayout ? {} : loadPositions(connection, schema);
      const missing = tables.filter((t) => !saved[t.name]);
      // Mostly new schema: lay everything out. A few new tables: put them below the existing ones.
      if (missing.length * 2 > tables.length) saved = {};
      const auto: Record<string, { x: number; y: number }> = Object.keys(saved).length ? {} : autoLayout(tables, heights);
      if (Object.keys(saved).length && missing.length) {
        const placed = tables.filter((t) => saved[t.name]);
        const minX = Math.min(...placed.map((t) => saved[t.name].x));
        let y = Math.max(...placed.map((t) => saved[t.name].y + (heights.get(t.name) ?? 120))) + 80;
        let x = minX;
        let rowH = 0;
        const width = Math.max(...placed.map((t) => saved[t.name].x + NODE_W)) - minX;
        for (const t of missing) {
          if (x > minX && x + NODE_W > minX + Math.max(width, NODE_W * 3)) {
            x = minX;
            y += rowH + 50;
            rowH = 0;
          }
          auto[t.name] = { x, y };
          x += NODE_W + 50;
          rowH = Math.max(rowH, heights.get(t.name) ?? 120);
        }
        localStorage.setItem(storageKey(connection, schema), JSON.stringify({ ...saved, ...auto }));
      }
      if (relayout) localStorage.setItem(storageKey(connection, schema), JSON.stringify(auto));
      const q = search.trim().toLowerCase();
      setNodes(
        tables.map((t) => ({
          id: t.name,
          type: "table",
          position: saved[t.name] ?? auto[t.name] ?? { x: 0, y: 0 },
          data: {
            table: t,
            fkColumns: fkCols.get(t.name)!,
            refColumns: refCols.get(t.name) ?? new Set(),
            keysOnly,
            dim: !!q && !t.name.toLowerCase().includes(q) && !t.columns.some((c) => c.name.toLowerCase().includes(q)),
          },
          selected: t.name === selected,
        })),
      );
      setEdges(
        tables.flatMap((t) =>
          t.foreign_keys
            .filter((f) => tables.some((x) => x.name === f.ref_table))
            .map((f) => ({
              id: `${t.name}.${f.name}`,
              // Parent (referenced) → child, matching the left-to-right layout.
              source: f.ref_table,
              sourceHandle: `${f.ref_columns[0]}-s`,
              target: t.name,
              targetHandle: `${f.columns[0]}-t`,
              type: "smoothstep",
              animated: selected === t.name || selected === f.ref_table,
              label: f.on_delete && f.on_delete !== "NO ACTION" ? f.on_delete.toLowerCase() : undefined,
              labelStyle: { fontSize: 10, fill: "var(--text-faint)" },
              labelBgStyle: { fill: "var(--bg)" },
              markerEnd: { type: MarkerType.ArrowClosed, width: 14, height: 14 },
              style: { strokeWidth: selected === t.name || selected === f.ref_table ? 2 : 1.3 },
            })),
        ),
      );
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [tables, keysOnly, search, selected, connection, schema],
  );

  useEffect(() => {
    build(false);
  }, [build]);

  // Fit once when the first nodes arrive.
  const [fitted, setFitted] = useState(false);
  useEffect(() => {
    if (!fitted && nodes.length) {
      setFitted(true);
      requestAnimationFrame(() => flow.fitView({ padding: 0.15, maxZoom: 1 }));
    }
  }, [nodes.length, fitted, flow]);

  const savePositions = () => {
    const pos: Record<string, { x: number; y: number }> = {};
    flow.getNodes().forEach((n) => (pos[n.id] = n.position));
    localStorage.setItem(storageKey(connection, schema), JSON.stringify(pos));
  };

  const onConnect = (c: Connection) => {
    if (!c.source || !c.target || !c.sourceHandle || !c.targetHandle) return;
    // Dragged from a referenced column (right dot) onto the column that will hold the foreign key.
    const refCol = c.sourceHandle.replace(/-s$/, "");
    const col = c.targetHandle.replace(/-t$/, "");
    const child = c.target;
    const parent = c.source;
    if (kind === "sqlite") {
      toast("error", "SQLite cannot add foreign keys to existing tables", "Recreate the table with a REFERENCES clause instead.");
      return;
    }
    const q = (n: string) => quoteIdent(kind, n);
    const sql = `ALTER TABLE ${qualifiedName(kind, schema, child)} ADD CONSTRAINT ${q(`${child}_${col}_fkey`)} FOREIGN KEY (${q(col)}) REFERENCES ${qualifiedName(kind, schema, parent)} (${q(refCol)});`;
    setPendingFk({ sql, label: `${child}.${col} → ${parent}.${refCol}` });
  };

  const selectedTable = tables?.find((t) => t.name === selected);
  const readOnly = !!target?.readOnly;

  if (error) return <div style={{ padding: 14 }}><ErrorBox error={error} /></div>;

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <LayoutGrid size={15} className="muted" /> Diagram <span className="faint" style={{ fontWeight: 400 }}>{schema}</span>
        </div>
        <div className="sep" />
        <div className="row" style={{ position: "relative" }}>
          <Search size={13} className="faint" style={{ position: "absolute", left: 8 }} />
          <input className="input sm" style={{ paddingLeft: 26, width: 200 }} placeholder="Find table or column" value={search} onChange={(e) => setSearch(e.target.value)} aria-label="Find table or column" />
        </div>
        <label className="check small">
          <input type="checkbox" checked={keysOnly} onChange={(e) => setKeysOnly(e.target.checked)} /> Keys only
        </label>
        <label className="check small">
          <input type="checkbox" checked={miniMap} onChange={(e) => setMiniMap(e.target.checked)} /> Mini-map
        </label>
        <div className="grow" />
        <button className="btn sm ghost" onClick={() => build(true)} title="Arrange automatically">
          <LayoutGrid size={13} /> Auto-layout
        </button>
        <button className="btn sm ghost" onClick={() => flow.fitView({ padding: 0.15, duration: 250 })} title="Fit to screen">
          <Maximize size={13} /> Fit
        </button>
        <button className="btn sm ghost" onClick={() => openTab({ kind: "schema", from: { connection } })} title="Snapshot this schema or compare it with another">
          <GitCompare size={13} /> Compare
        </button>
        <button className="btn sm ghost" onClick={() => setDdlOpen(true)} title="CREATE statements for this schema">
          <FileCode2 size={13} /> DDL
        </button>
        {!readOnly && (
          <button className="btn sm primary" onClick={() => openDialog({ kind: "table-designer", connection, schema })}>
            <Plus size={13} /> Table
          </button>
        )}
      </div>
      <div style={{ display: "flex", flex: 1, minHeight: 0 }}>
        <div className="diagram">
          {!tables ? (
            <Empty icon={<Spinner size={20} />} title="Loading schema…" />
          ) : tables.length === 0 ? (
            <Empty icon={<LayoutGrid size={28} />} title="No tables yet">
              {!readOnly && (
                <button className="btn primary" onClick={() => openDialog({ kind: "table-designer", connection, schema })}>
                  <Plus size={14} /> Design a table
                </button>
              )}
            </Empty>
          ) : (
            <ReactFlow
              nodes={nodes}
              edges={edges}
              nodeTypes={nodeTypes}
              onNodesChange={onNodesChange}
              onEdgesChange={onEdgesChange}
              onNodeDragStop={savePositions}
              onNodeClick={(_, n) => setSelected(n.id)}
              onPaneClick={() => setSelected(null)}
              onConnect={readOnly ? undefined : onConnect}
              nodesConnectable={!readOnly}
              colorMode={theme}
              minZoom={0.1}
              maxZoom={2}
              proOptions={{ hideAttribution: true }}
              deleteKeyCode={null}
            >
              <Background gap={20} size={1} />
              <Controls showInteractive={false} />
              {miniMap && <MiniMap pannable zoomable nodeStrokeWidth={2} />}
            </ReactFlow>
          )}
          {tables && tables.length > 0 && !readOnly && (
            <div className="faint xs" style={{ position: "absolute", left: 60, bottom: 12, pointerEvents: "none" }}>
              Drag from a key's right dot onto another table's column to make that column reference it.
            </div>
          )}
        </div>
        {selectedTable && (
          <div className="designer">
            <div className="drawer-head">
              <Rows3 size={14} className="muted" />
              <strong className="grow ellipsis">{selectedTable.name}</strong>
              <button className="icon-btn" onClick={() => setSelected(null)} aria-label="Close details">
                <X size={14} />
              </button>
            </div>
            <div className="drawer-body" style={{ display: "flex", flexDirection: "column", gap: 12 }}>
              <div className="row wrap">
                <button className="btn sm" onClick={() => openTab({ kind: "table", connection, schema, table: selectedTable.name })}>
                  <Rows3 size={13} /> Data
                </button>
                <button className="btn sm" onClick={() => openTab({ kind: "query", connection, sql: `SELECT *\nFROM ${qualifiedName(kind, schema, selectedTable.name)}\nLIMIT 100;`, title: `${selectedTable.name} query` })}>
                  <Code2 size={13} /> Query
                </button>
                {!readOnly && selectedTable.kind !== "view" && (
                  <>
                    <button className="btn sm" onClick={() => openDialog({ kind: "table-designer", connection, schema, table: selectedTable.name })}>
                      <Wand2 size={13} /> Edit
                    </button>
                    <button className="btn sm" onClick={() => openDialog({ kind: "generate", connection, schema, table: selectedTable.name })}>
                      <Sparkles size={13} /> Generate
                    </button>
                    <button className="btn sm danger" onClick={() => setDrop(selectedTable.name)}>
                      <Trash2 size={13} /> Drop
                    </button>
                  </>
                )}
              </div>
              <table className="simple-table">
                <tbody>
                  {selectedTable.columns.map((c) => (
                    <tr key={c.name}>
                      <td className="mono">
                        {c.primary_key && <KeyRound size={11} style={{ color: "var(--warning)", marginRight: 4 }} />}
                        {c.name}
                      </td>
                      <td className="mono faint">{c.data_type}</td>
                      <td className="xs faint">{c.nullable ? "" : "not null"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <div>
                <div className="muted small" style={{ fontWeight: 600, marginBottom: 6 }}>
                  Relationships
                </div>
                {selectedTable.foreign_keys.map((f) => (
                  <div key={f.name} className="small mono" style={{ marginBottom: 4 }}>
                    {f.columns.join(", ")} → <a href="#" onClick={(e) => (e.preventDefault(), setSelected(f.ref_table))}>{f.ref_table}</a>.{f.ref_columns.join(", ")}
                  </div>
                ))}
                {(tables ?? []).flatMap((t) =>
                  t.foreign_keys
                    .filter((f) => f.ref_table === selectedTable.name)
                    .map((f) => (
                      <div key={`${t.name}.${f.name}`} className="small mono" style={{ marginBottom: 4 }}>
                        ← <a href="#" onClick={(e) => (e.preventDefault(), setSelected(t.name))}>{t.name}</a>.{f.columns.join(", ")}
                      </div>
                    )),
                )}
                {!selectedTable.foreign_keys.length && !(tables ?? []).some((t) => t.foreign_keys.some((f) => f.ref_table === selectedTable.name)) && <div className="faint small">None</div>}
              </div>
            </div>
          </div>
        )}
      </div>
      {pendingFk && (
        <ConfirmDialog
          title="Add relationship"
          body={`${pendingFk.label}\n\n${pendingFk.sql}`}
          confirmLabel="Add foreign key"
          onClose={() => setPendingFk(null)}
          onConfirm={async () => {
            const r = await attempt("Could not add the foreign key", () => call("db.query", { connection, sql: pendingFk.sql }));
            if (r) toast("success", "Relationship added", pendingFk.label);
          }}
        />
      )}
      {drop && (
        <ConfirmDialog
          title={`Drop table ${drop}?`}
          body={`This deletes the table and all its rows${target?.isMock ? " from the mock server" : ""}. Tables that reference it may block the drop.`}
          confirmLabel="Drop table"
          danger
          onClose={() => setDrop(null)}
          onConfirm={async () => {
            const r = await attempt("Could not drop the table", () => call("db.query", { connection, sql: `DROP TABLE ${qualifiedName(kind, schema, drop)}` }));
            if (r) {
              setSelected(null);
              toast("success", "Table dropped", drop);
            }
          }}
        />
      )}
      {ddlOpen && <SchemaDdlDialog connection={connection} schema={schema} onClose={() => setDdlOpen(false)} />}
    </>
  );
}

function SchemaDdlDialog({ connection, schema, onClose }: { connection: string; schema: string; onClose: () => void }) {
  const kind = targetInfo(connection)?.kind ?? "postgres";
  const [dialect, setDialect] = useState(kind === "redis" ? "postgres" : kind);
  const ddl = useAsync(() => call<{ sql: string }>("db.ddl", { connection, schema, dialect }), [connection, schema, dialect]);
  const openTab = useStore((s) => s.openTab);
  const sql = ddl.data?.sql ?? "";
  return (
    <Dialog
      wide
      title="Schema DDL"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={() => ddl.reload()}>
            <RefreshCw size={14} /> Refresh
          </button>
          <span className="grow" />
          <button
            className="btn primary"
            disabled={!sql}
            onClick={() => {
              openTab({ kind: "query", sql, title: `DDL (${dialect})` });
              onClose();
            }}
          >
            <Code2 size={14} /> Open in editor
          </button>
        </>
      }
    >
      <div className="row">
        <span className="muted small">Convert to</span>
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
      {sql && <CodeBlock code={sql} maxHeight={480} />}
    </Dialog>
  );
}
