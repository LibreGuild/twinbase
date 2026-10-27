// Explorer: mock servers, saved connections (schemas → tables) and query collections.

import { consoleTemplate } from "../views/SearchView";
import { BookOpen, Braces, ChevronDown, ChevronRight, Code2, Copy, Eye, FileText, FolderOpen, FolderTree, GitCompare, KeyRound, Layers, Network, Pencil, Play, Plug, Plus, RefreshCw, RotateCcw, Rows3, Search, Server, Sparkles, Square, Table2, Terminal, Trash2, Unplug, Upload, Wand2 } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { call } from "../lib/api";
import { TABLE_TEMPLATE } from "../lib/dynamo";
import { CONSOLE_KINDS, consoleStart, kindLabel, qualifiedName } from "../lib/format";
import { attempt, defaultSchema, targetInfo, useStore, type RailPanel } from "../lib/store";
import type { ConnectionDef, DbKind, MockStatus, MongoCollection, MongoDatabase } from "../lib/types";
import { KindIcon, Spinner, useAsync, useContextMenu, type MenuEntry } from "./ui";

function useExpanded() {
  const [open, setOpen] = useState<Set<string>>(() => {
    try {
      return new Set(JSON.parse(localStorage.getItem("twinbase.explorer") ?? '["sec:mocks","sec:connections","sec:queries"]'));
    } catch {
      return new Set(["sec:mocks", "sec:connections", "sec:queries"]);
    }
  });
  const toggle = (key: string, value?: boolean) =>
    setOpen((s) => {
      const n = new Set(s);
      if (value ?? !n.has(key)) n.add(key);
      else n.delete(key);
      try {
        localStorage.setItem("twinbase.explorer", JSON.stringify([...n]));
      } catch {
        /* ignore */
      }
      return n;
    });
  return { open, toggle };
}

function Row(props: {
  depth?: number;
  icon?: ReactNode;
  label: ReactNode;
  meta?: ReactNode;
  expanded?: boolean;
  onToggle?: () => void;
  onClick?: () => void;
  onDoubleClick?: () => void;
  onContextMenu?: (e: React.MouseEvent) => void;
  actions?: ReactNode;
  active?: boolean;
  title?: string;
}) {
  return (
    <div
      className={`tree-row${props.active ? " active" : ""}`}
      style={{ paddingLeft: 6 + (props.depth ?? 0) * 12 }}
      onClick={props.onClick ?? props.onToggle}
      onDoubleClick={props.onDoubleClick}
      onContextMenu={props.onContextMenu}
      title={props.title}
      role="treeitem"
      aria-expanded={props.onToggle ? !!props.expanded : undefined}
    >
      <span
        className="chev"
        onClick={(e) => {
          if (props.onToggle) {
            e.stopPropagation();
            props.onToggle();
          }
        }}
      >
        {props.onToggle ? props.expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} /> : null}
      </span>
      {props.icon}
      <span className="label">{props.label}</span>
      {props.actions && <span className="row-actions" onClick={(e) => e.stopPropagation()}>{props.actions}</span>}
      {props.meta && <span className="meta">{props.meta}</span>}
    </div>
  );
}

function matches(q: string, ...texts: (string | undefined)[]) {
  return !q || texts.some((t) => t?.toLowerCase().includes(q));
}

export function Sidebar({ panel }: { panel: RailPanel }) {
  const ws = useStore((s) => s.workspace);
  const openDialog = useStore((s) => s.openDialog);
  const openTab = useStore((s) => s.openTab);
  const { open, toggle } = useExpanded();
  const [query, setQuery] = useState("");
  const menu = useContextMenu();
  if (!ws) return null;
  const q = query.trim().toLowerCase();

  const section = (key: string, title: string, count: number, actions: ReactNode, body: ReactNode) => (
    <div className="tree-section">
      <div className="tree-section-head" onClick={() => toggle(`sec:${key}`)} style={{ cursor: "pointer" }}>
        {open.has(`sec:${key}`) || q ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        <span className="grow">
          {title} <span style={{ fontWeight: 400 }}>{count ? `· ${count}` : ""}</span>
        </span>
        <span onClick={(e) => e.stopPropagation()} className="row" style={{ gap: 2 }}>
          {actions}
        </span>
      </div>
      {(open.has(`sec:${key}`) || q) && body}
    </div>
  );

  const mocks = ws.mocks.filter((m) => matches(q, m.name, m.kind) || open.has(`mock:${m.id}`));
  const connections = ws.connections;

  return (
    <>
      <div className="sidebar-head">
        <span className="sidebar-title">{panel === "queries" ? "Saved queries" : "Databases"}</span>
        <button
          className="icon-btn"
          title="New…"
          aria-label="New"
          onClick={(e) =>
            menu.open(e, [
              { label: "Mock PostgreSQL server", icon: <Server size={14} className="kind-pg" />, onClick: () => openDialog({ kind: "mock", mockKind: "postgres" }) },
              { label: "Mock MySQL server", icon: <Server size={14} className="kind-mysql" />, onClick: () => openDialog({ kind: "mock", mockKind: "mysql" }) },
              { label: "Mock SQL Server", icon: <Server size={14} className="kind-mssql" />, onClick: () => openDialog({ kind: "mock", mockKind: "mssql" }) },
              { label: "Mock Redis server", icon: <Server size={14} className="kind-redis" />, onClick: () => openDialog({ kind: "mock", mockKind: "redis" }) },
              { label: "Mock MongoDB server", icon: <Server size={14} className="kind-mongo" />, onClick: () => openDialog({ kind: "mock", mockKind: "mongodb" }) },
              { label: "Mock DynamoDB server", icon: <Server size={14} className="kind-dynamo" />, onClick: () => openDialog({ kind: "mock", mockKind: "dynamodb" }) },
              { label: "Mock Firestore server", icon: <Server size={14} className="kind-firestore" />, onClick: () => openDialog({ kind: "mock", mockKind: "firestore" }) },
              "sep",
              { label: "Connection…", icon: <Plug size={14} />, onClick: () => openDialog({ kind: "connection" }) },
              { label: "SQL query", icon: <Code2 size={14} />, onClick: () => openTab({ kind: "query", sql: "", title: "New query" }) },
              { label: "Query collection", icon: <FileText size={14} />, onClick: () => openDialog({ kind: "collection" }) },
              { label: "Schema snapshot / diff", icon: <GitCompare size={14} />, onClick: () => openTab({ kind: "schema" }) },
              "sep",
              { label: "Import bundle…", icon: <Upload size={14} />, onClick: () => openDialog({ kind: "import" }) },
            ], { anchor: e.currentTarget as HTMLElement })
          }
        >
          <Plus size={16} />
        </button>
      </div>
      <div className="sidebar-search row" style={{ position: "relative" }}>
        <Search size={13} className="faint" style={{ position: "absolute", left: 8 }} />
        <input className="input sm grow" style={{ paddingLeft: 26 }} placeholder="Filter" value={query} onChange={(e) => setQuery(e.target.value)} aria-label="Filter explorer" />
      </div>
      <div className="tree" role="tree">
        {panel === "explorer" && section(
          "mocks",
          "Mock servers",
          ws.mocks.length,
          <button className="icon-btn sm" title="New mock server" aria-label="New mock server" onClick={() => openDialog({ kind: "mock" })}>
            <Plus size={13} />
          </button>,
          mocks.length === 0 ? (
            <div className="tree-empty">
              <a href="#" onClick={(e) => (e.preventDefault(), openDialog({ kind: "mock" }))}>
                Create a mock server
              </a>
            </div>
          ) : (
            mocks.map((m) => <MockNode key={m.id} mock={m} q={q} open={open} toggle={toggle} menu={menu.open} />)
          ),
        )}
        {panel === "explorer" && section(
          "connections",
          "Connections",
          connections.length,
          <button className="icon-btn sm" title="New connection" aria-label="New connection" onClick={() => openDialog({ kind: "connection" })}>
            <Plus size={13} />
          </button>,
          connections.length === 0 ? (
            <div className="tree-empty">
              <a href="#" onClick={(e) => (e.preventDefault(), openDialog({ kind: "connection" }))}>
                Add a connection
              </a>
            </div>
          ) : (
            connections.map((c) => <ConnectionNode key={c.id} conn={c} q={q} open={open} toggle={toggle} menu={menu.open} />)
          ),
        )}
        {panel === "queries" && section(
          "queries",
          "Saved queries",
          ws.queries.length,
          <button className="icon-btn sm" title="New collection" aria-label="New collection" onClick={() => openDialog({ kind: "collection" })}>
            <Plus size={13} />
          </button>,
          ws.collections.length === 0 ? (
            <div className="tree-empty">Save a query from the SQL editor to start a collection.</div>
          ) : (
            ws.collections.map((c) => {
              const queries = ws.queries.filter((x) => x.collection === c.id && matches(q, x.name, x.sql, c.name));
              if (q && !queries.length && !matches(q, c.name)) return null;
              const key = `col:${c.id}`;
              return (
                <div key={c.id}>
                  <Row
                    icon={<FileText size={14} className="muted" />}
                    label={c.name}
                    meta={queries.length}
                    expanded={open.has(key) || !!q}
                    onToggle={() => toggle(key)}
                    onClick={() => openTab({ kind: "collection", collectionId: c.id })}
                    onContextMenu={(e) =>
                      menu.open(e, [
                        { label: "Open", icon: <FileText size={14} />, onClick: () => openTab({ kind: "collection", collectionId: c.id }) },
                        { label: "Edit", icon: <Pencil size={14} />, onClick: () => openDialog({ kind: "collection", collectionId: c.id }) },
                      ])
                    }
                  />
                  {(open.has(key) || q) &&
                    queries.map((x) => (
                      <Row
                        key={x.id}
                        depth={1}
                        icon={<Code2 size={13} className="muted" />}
                        label={x.name}
                        title={x.docs ? `${x.name}\n\n${x.docs.slice(0, 300)}` : x.sql.slice(0, 300)}
                        onClick={() => openTab({ kind: "query", sql: x.sql, title: x.name, connection: x.connection, saved: { collection: x.collection, id: x.id } })}
                      />
                    ))}
                </div>
              );
            })
          ),
        )}
      </div>
      {menu.node}
    </>
  );
}

type MenuOpener = (e: React.MouseEvent, items: MenuEntry[]) => void;

function MockNode({ mock, q, open, toggle, menu }: { mock: MockStatus; q: string; open: Set<string>; toggle: (k: string, v?: boolean) => void; menu: MenuOpener }) {
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const key = `mock:${mock.id}`;
  const conn = mock.connection_id;
  const isSql = mock.kind !== "redis" && mock.kind !== "mongodb" && mock.kind !== "dynamodb" && mock.kind !== "firestore";
  const schema = defaultSchema(mock.kind, conn);
  const start = () => attempt("Could not start the mock", () => call("mock.start", { id: mock.id }));
  const stop = () => attempt("Could not stop the mock", () => call("mock.stop", { id: mock.id }));
  const items: MenuEntry[] = [
    { label: "Open", icon: <Server size={14} />, onClick: () => openTab({ kind: "mock", mockId: mock.id }) },
    mock.running ? { label: "Stop", icon: <Square size={14} />, onClick: stop } : { label: "Start", icon: <Play size={14} />, onClick: start },
    "sep",
    ...(isSql
      ? [
          { label: "New SQL query", icon: <Code2 size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "query", connection: conn, sql: "", title: `${mock.name} SQL` }) },
          { label: "Diagram", icon: <Network size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "diagram", connection: conn, schema }) },
          { label: "New table…", icon: <Wand2 size={14} />, disabled: !mock.running, onClick: () => openDialog({ kind: "table-designer", connection: conn, schema }) },
        ]
      : mock.kind === "mongodb"
        ? [{ label: "Open console", icon: <Braces size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "query", connection: conn, sql: "show collections", title: `${mock.name} console` }) }]
        : mock.kind === "dynamodb"
        ? [{ label: "Open console", icon: <Terminal size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "query", connection: conn, sql: "ListTables {}", title: `${mock.name} console` }) }]
        : mock.kind === "firestore"
        ? [{ label: "Open console", icon: <FileText size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "query", connection: conn, sql: "collections", title: `${mock.name} console` }) }]
        : [
            { label: "Browse keys", icon: <KeyRound size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "redis", connection: conn }) },
            { label: "Open console", icon: <Terminal size={14} />, disabled: !mock.running, onClick: () => openTab({ kind: "query", connection: conn, sql: "", title: `${mock.name} console` }) },
          ]),
    "sep",
    { label: "Settings…", icon: <Pencil size={14} />, onClick: () => openDialog({ kind: "mock", mock }) },
    { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: conn }) },
    {
      label: "Reset to seed",
      icon: <RotateCcw size={14} />,
      onClick: () => openDialog({ kind: "confirm", title: "Reset mock data?", body: "All current data is deleted and the seed script runs again.", confirmLabel: "Reset", danger: true, onConfirm: () => attempt("Reset failed", () => call("mock.reset", { id: mock.id })).then(() => undefined) }),
    },
    {
      label: "Delete",
      icon: <Trash2 size={14} />,
      danger: true,
      onClick: () => openDialog({ kind: "confirm", title: `Delete ${mock.name}?`, body: "The server stops and its definition, data and seed are removed.", confirmLabel: "Delete", danger: true, onConfirm: () => attempt("Delete failed", () => call("mock.delete", { id: mock.id })).then(() => undefined) }),
    },
  ];
  return (
    <div>
      <Row
        icon={
          <>
            <span className={`status-dot${mock.running ? " on" : mock.error ? " err" : ""}`} title={mock.running ? "Running" : mock.error ?? "Stopped"} />
            <KindIcon kind={mock.kind} />
          </>
        }
        label={mock.name}
        meta={mock.faults?.enabled ? <span title="Fault injection is active" style={{ color: "var(--warning)" }}>⚡ :{mock.port}</span> : `:${mock.port}`}
        title={`${kindLabel(mock.kind)} mock on ${mock.host}:${mock.port}${mock.error ? `\n${mock.error}` : ""}`}
        active={activeTab?.kind === "mock" && activeTab.mockId === mock.id}
        expanded={open.has(key) && mock.running}
        onToggle={mock.running ? () => toggle(key) : undefined}
        onClick={() => openTab({ kind: "mock", mockId: mock.id })}
        onContextMenu={(e) => menu(e, items)}
        actions={
          mock.running ? (
            <button className="icon-btn sm" title="Stop" aria-label="Stop mock" onClick={stop}>
              <Square size={11} />
            </button>
          ) : (
            <button className="icon-btn sm" title="Start" aria-label="Start mock" onClick={start}>
              <Play size={11} />
            </button>
          )
        }
      />
      {mock.running && open.has(key) && (isSql ? <SchemaTree connection={conn} kind={mock.kind} depth={1} q={q} open={open} toggle={toggle} menu={menu} /> : mock.kind === "mongodb" ? <MongoTree connection={conn} depth={1} q={q} open={open} toggle={toggle} menu={menu} /> : mock.kind === "dynamodb" ? <DynamoTree connection={conn} depth={1} q={q} menu={menu} /> : mock.kind === "firestore" ? <FirestoreTree connection={conn} depth={1} q={q} menu={menu} /> : <RedisLeaf connection={conn} depth={1} />)}
    </div>
  );
}

function ConnectionNode({ conn, q, open, toggle, menu }: { conn: ConnectionDef; q: string; open: Set<string>; toggle: (k: string, v?: boolean) => void; menu: MenuOpener }) {
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const connected = useStore((s) => s.workspace?.connected.includes(conn.id));
  const [connecting, setConnecting] = useState(false);
  const key = `conn:${conn.id}`;
  const isSql = conn.kind !== "redis" && !CONSOLE_KINDS.includes(conn.kind);
  const isMongo = conn.kind === "mongodb";
  const isDynamo = conn.kind === "dynamodb";
  const isFirestore = conn.kind === "firestore";
  const isSearch = conn.kind === "elasticsearch";
  const consoleSql = consoleStart(conn.kind);
  const expand = async () => {
    if (open.has(key)) return toggle(key, false);
    if (!connected) {
      setConnecting(true);
      const ok = await attempt(`Could not connect to ${conn.name}`, () => call("connection.connect", { id: conn.id }));
      setConnecting(false);
      if (!ok) return;
    }
    toggle(key, true);
  };
  const schema = defaultSchema(conn.kind, conn.id);
  if (q && !matches(q, conn.name, conn.host, conn.database) && !open.has(key)) return null;
  return (
    <div>
      <Row
        icon={
          <>
            {conn.color ? <span className="color-tag" style={{ background: conn.color }} /> : null}
            <KindIcon kind={conn.kind} />
          </>
        }
        label={conn.name}
        meta={connecting ? <Spinner size={11} /> : connected ? <span className="status-dot on" title="Connected" /> : undefined}
        title={`${kindLabel(conn.kind)} · ${conn.kind === "sqlite" ? conn.path : `${conn.host}:${conn.port}${conn.database ? `/${conn.database}` : ""}`}${conn.read_only ? "\nRead-only" : ""}`}
        expanded={open.has(key)}
        onToggle={expand}
        onClick={expand}
        onDoubleClick={() => openTab(conn.kind === "redis" ? { kind: "redis", connection: conn.id } : { kind: "query", connection: conn.id, sql: consoleSql, title: isSql ? conn.name : `${conn.name} console` })}
        onContextMenu={(e) =>
          menu(e, [
            ...(isSql
              ? [
                  { label: "New SQL query", icon: <Code2 size={14} />, onClick: () => openTab({ kind: "query", connection: conn.id, sql: "", title: conn.name }) },
                  { label: "Diagram", icon: <Network size={14} />, onClick: () => openTab({ kind: "diagram", connection: conn.id, schema }) },
                ]
              : isMongo
                ? [{ label: "Open console", icon: <Braces size={14} />, onClick: () => openTab({ kind: "query", connection: conn.id, sql: "show collections", title: `${conn.name} console` }) }]
                : isDynamo || isFirestore || isSearch
                ? [{ label: "Open console", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection: conn.id, sql: consoleSql, title: `${conn.name} console` }) }]
                : [
                    { label: "Browse keys", icon: <KeyRound size={14} />, onClick: () => openTab({ kind: "redis", connection: conn.id }) },
                    { label: "Open console", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection: conn.id, sql: "", title: `${conn.name} console` }) },
                  ]),
            "sep",
            { label: "Edit…", icon: <Pencil size={14} />, onClick: () => openDialog({ kind: "connection", connection: conn }) },
            { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: conn.id }) },
            ...(connected ? [{ label: "Disconnect", icon: <Unplug size={14} />, onClick: () => (toggle(key, false), call("connection.disconnect", { id: conn.id })) }] : []),
            {
              label: "Delete",
              icon: <Trash2 size={14} />,
              danger: true,
              onClick: () => openDialog({ kind: "confirm", title: `Delete ${conn.name}?`, body: "The saved connection and its stored password are removed. The database itself is not touched.", confirmLabel: "Delete", danger: true, onConfirm: () => attempt("Delete failed", () => call("connection.delete", { id: conn.id })).then(() => undefined) }),
            },
          ])
        }
      />
      {open.has(key) && (isSql ? <SchemaTree connection={conn.id} kind={conn.kind} depth={1} q={q} open={open} toggle={toggle} menu={menu} /> : isMongo ? <MongoTree connection={conn.id} depth={1} q={q} open={open} toggle={toggle} menu={menu} /> : isDynamo ? <DynamoTree connection={conn.id} depth={1} q={q} menu={menu} /> : isFirestore ? <FirestoreTree connection={conn.id} depth={1} q={q} menu={menu} /> : isSearch ? <SearchTree connection={conn.id} depth={1} q={q} menu={menu} /> : <RedisLeaf connection={conn.id} depth={1} />)}
    </div>
  );
}

function SearchTree({ connection, depth, q, menu }: { connection: string; depth: number; q: string; menu: MenuOpener }) {
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const [hidden, setHidden] = useState(false);
  const r = useAsync(() => call<{ indices: { name: string; docs?: number; health: string; aliases: string[] }[] }>("search.indices", { connection, include_hidden: hidden }), [connection, version, hidden]);
  if (r.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((r.error as Error).message ?? r.error)}</div>;
  if (!r.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const shown = r.data.indices.filter((i) => matches(q, i.name, ...i.aliases));
  return (
    <>
      {shown.length === 0 && <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>{q ? "No match" : "No indices yet — create one in the console"}</div>}
      {shown.map((i) => (
        <Row
          key={i.name}
          depth={depth}
          icon={<Search size={13} className={i.health === "red" ? "danger" : "muted"} />}
          label={i.name}
          meta={i.docs !== undefined ? <span className="faint xs">{i.docs.toLocaleString()}</span> : undefined}
          title={`${i.name} · ${i.health}${i.aliases.length ? ` · aliases: ${i.aliases.join(", ")}` : ""}`}
          active={activeTab?.kind === "search" && activeTab.connection === connection && activeTab.index === i.name}
          onClick={() => openTab({ kind: "search", connection, index: i.name })}
          onContextMenu={(e) =>
            menu(e, [
              { label: "Open documents", icon: <Rows3 size={14} />, onClick: () => openTab({ kind: "search", connection, index: i.name }) },
              { label: "Search in console", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection, sql: consoleTemplate(i.name), title: `${i.name} console` }) },
              { label: "Copy name", icon: <Copy size={14} />, onClick: () => navigator.clipboard.writeText(i.name) },
              { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: connection, object: i.name }) },
            ])
          }
        />
      ))}
      <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>
        <button className="link xs" onClick={() => setHidden((h) => !h)}>
          {hidden ? "Hide system indices" : "Show system indices"}
        </button>
      </div>
    </>
  );
}

function FirestoreTree({ connection, depth, q, menu }: { connection: string; depth: number; q: string; menu: MenuOpener }) {
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const colls = useAsync(() => call<{ collections: string[] }>("firestore.collections", { connection }), [connection, version]);
  if (colls.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((colls.error as Error).message ?? colls.error)}</div>;
  if (!colls.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const shown = colls.data.collections.filter((c) => matches(q, c));
  if (shown.length === 0) return <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>{q ? "No match" : "No collections yet — add a document to create one"}</div>;
  return (
    <>
      {shown.map((c) => (
        <Row
          key={c}
          depth={depth}
          icon={<FolderTree size={13} className="muted" />}
          label={c}
          active={activeTab?.kind === "firestore" && activeTab.connection === connection && activeTab.collection === c}
          onClick={() => openTab({ kind: "firestore", connection, collection: c })}
          onContextMenu={(e) =>
            menu(e, [
              { label: "Open documents", icon: <Rows3 size={14} />, onClick: () => openTab({ kind: "firestore", connection, collection: c }) },
              { label: "Query in console", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection, sql: `query ${c} limit 20`, title: `${c} console` }) },
              { label: "Collection group query", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection, sql: `query group ${c} limit 20`, title: `${c} group` }) },
              { label: "Copy name", icon: <Copy size={14} />, onClick: () => navigator.clipboard.writeText(c) },
              { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: connection, object: c }) },
            ])
          }
        />
      ))}
    </>
  );
}

function DynamoTree({ connection, depth, q, menu }: { connection: string; depth: number; q: string; menu: MenuOpener }) {
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const tables = useAsync(() => call<{ tables: string[] }>("dynamo.tables", { connection }), [connection, version]);
  if (tables.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((tables.error as Error).message ?? tables.error)}</div>;
  if (!tables.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const shown = tables.data.tables.filter((t) => matches(q, t));
  const readOnly = targetInfo(connection)?.readOnly;
  const newTable = () => openTab({ kind: "query", connection, sql: TABLE_TEMPLATE, title: "New table" });
  return (
    <>
      {shown.length === 0 && (
        <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>
          {q ? "No match" : readOnly ? "No tables" : <a href="#" onClick={(e) => (e.preventDefault(), newTable())}>Create a table</a>}
        </div>
      )}
      {shown.map((t) => (
        <Row
          key={t}
          depth={depth}
          icon={<Table2 size={13} className="muted" />}
          label={t}
          active={activeTab?.kind === "dynamo" && activeTab.connection === connection && activeTab.table === t}
          onClick={() => openTab({ kind: "dynamo", connection, table: t })}
          onContextMenu={(e) =>
            menu(e, [
              { label: "Open items", icon: <Rows3 size={14} />, onClick: () => openTab({ kind: "dynamo", connection, table: t }) },
              { label: "Scan in console", icon: <Terminal size={14} />, onClick: () => openTab({ kind: "query", connection, sql: `Scan {\n  "TableName": ${JSON.stringify(t)},\n  "Limit": 25\n}`, title: `${t} console` }) },
              { label: "Copy name", icon: <Copy size={14} />, onClick: () => navigator.clipboard.writeText(t) },
              { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: connection, object: t }) },
              ...(readOnly
                ? []
                : [
                    "sep" as const,
                    { label: "New table…", icon: <Plus size={14} />, onClick: newTable },
                    {
                      label: "Delete table",
                      icon: <Trash2 size={14} />,
                      danger: true,
                      onClick: () =>
                        openDialog({
                          kind: "confirm",
                          title: `Delete table ${t}?`,
                          body: "The table, its items and its indexes are deleted.",
                          confirmLabel: "Delete table",
                          danger: true,
                          onConfirm: () => attempt("Delete failed", () => call("dynamo.table.delete", { connection, table: t })).then(() => undefined),
                        }),
                    },
                  ]),
            ])
          }
        />
      ))}
    </>
  );
}

const SYSTEM_DBS = new Set(["admin", "config", "local"]);

function MongoTree({ connection, depth, q, open, toggle, menu }: { connection: string; depth: number; q: string; open: Set<string>; toggle: (k: string, v?: boolean) => void; menu: MenuOpener }) {
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const dbs = useAsync(() => call<{ databases: MongoDatabase[]; default: string }>("mongo.databases", { connection }), [connection, version]);
  if (dbs.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((dbs.error as Error).message ?? dbs.error)}</div>;
  if (!dbs.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const def = dbs.data.default;
  const names = [...new Set([def, ...dbs.data.databases.map((d) => d.name)])].sort((a, b) => (a === def ? -1 : b === def ? 1 : Number(SYSTEM_DBS.has(a)) - Number(SYSTEM_DBS.has(b)) || a.localeCompare(b)));
  return (
    <>
      {names.map((db) => {
        const key = `mongodb:${connection}|${db}`;
        const expanded = open.has(key) || (db === def && !open.has(`${key}:closed`)) || !!q;
        return (
          <div key={db}>
            <Row
              depth={depth}
              icon={<FolderOpen size={13} className="muted" />}
              label={<span className={SYSTEM_DBS.has(db) ? "muted" : undefined}>{db}</span>}
              expanded={expanded}
              onToggle={() => (db === def ? toggle(`${key}:closed`, expanded) : toggle(key))}
              onContextMenu={(e) =>
                menu(e, [
                  { label: "Open console", icon: <Braces size={14} />, onClick: () => useStore.getState().openTab({ kind: "query", connection, sql: `use ${db}\nshow collections`, title: `${db} console` }) },
                  { label: "Refresh", icon: <RefreshCw size={14} />, onClick: () => dbs.reload() },
                ])
              }
            />
            {expanded && <MongoCollections connection={connection} database={db} depth={depth + 1} q={q} menu={menu} />}
          </div>
        );
      })}
    </>
  );
}

function MongoCollections({ connection, database, depth, q, menu }: { connection: string; database: string; depth: number; q: string; menu: MenuOpener }) {
  const version = useStore((s) => s.schemaVersion[connection] ?? 0);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const colls = useAsync(() => call<MongoCollection[]>("mongo.collections", { connection, database }), [connection, database, version]);
  if (colls.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((colls.error as Error).message ?? colls.error)}</div>;
  if (!colls.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const shown = colls.data.filter((c) => matches(q, c.name));
  const readOnly = targetInfo(connection)?.readOnly;
  if (shown.length === 0) return <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>{q ? "No match" : "No collections yet — insert a document to create one"}</div>;
  return (
    <>
      {shown.map((c) => (
        <Row
          key={c.name}
          depth={depth}
          icon={<Layers size={13} className="muted" />}
          label={c.name}
          meta={c.count !== null ? c.count.toLocaleString() : undefined}
          active={activeTab?.kind === "mongo" && activeTab.connection === connection && activeTab.database === database && activeTab.collection === c.name}
          onClick={() => openTab({ kind: "mongo", connection, database, collection: c.name })}
          onContextMenu={(e) =>
            menu(e, [
              { label: "Open documents", icon: <Rows3 size={14} />, onClick: () => openTab({ kind: "mongo", connection, database, collection: c.name }) },
              { label: "Query in console", icon: <Braces size={14} />, onClick: () => openTab({ kind: "query", connection, sql: `use ${database}\ndb.getCollection(${JSON.stringify(c.name)}).find({}).limit(20)`, title: `${c.name} console` }) },
              { label: "Copy name", icon: <Copy size={14} />, onClick: () => navigator.clipboard.writeText(c.name) },
              { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: connection, object: `${database}.${c.name}` }) },
              ...(readOnly || c.type !== "collection"
                ? []
                : [
                    "sep" as const,
                    {
                      label: "Drop collection",
                      icon: <Trash2 size={14} />,
                      danger: true,
                      onClick: () =>
                        openDialog({
                          kind: "confirm",
                          title: `Drop ${c.name}?`,
                          body: "The collection, its documents and its indexes are deleted.",
                          confirmLabel: "Drop collection",
                          danger: true,
                          onConfirm: () => attempt("Drop failed", () => call("mongo.collection.drop", { connection, database, collection: c.name })).then(() => undefined),
                        }),
                    },
                  ]),
            ])
          }
        />
      ))}
    </>
  );
}

/** A Redis connection's numbered databases (the default one and every one holding keys). */
function RedisLeaf({ connection, depth }: { connection: string; depth: number }) {
  const openTab = useStore((s) => s.openTab);
  const version = useStore((s) => s.dataVersion[connection] ?? 0);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  const r = useAsync(() => call<{ default: number; databases: { db: number; keys: number }[] }>("redis.databases", { connection }), [connection, version]);
  if (r.error) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{String((r.error as Error).message ?? r.error)}</div>;
  if (!r.data) return <div className="tree-empty"><Spinner size={12} /></div>;
  const dbs = [...r.data.databases];
  if (!dbs.some((d) => d.db === r.data!.default)) dbs.unshift({ db: r.data.default, keys: 0 });
  dbs.sort((a, b) => a.db - b.db);
  return (
    <>
      {dbs.map((d) => (
        <Row
          key={d.db}
          depth={depth}
          icon={<KeyRound size={13} className="kind-redis" />}
          label={`db${d.db}`}
          meta={<span className="faint xs">{d.keys.toLocaleString()}</span>}
          active={activeTab?.kind === "redis" && activeTab.connection === connection && (activeTab.db ?? r.data!.default) === d.db}
          onClick={() => openTab({ kind: "redis", connection, db: d.db === r.data!.default ? undefined : d.db })}
        />
      ))}
      <Row depth={depth} icon={<Terminal size={13} className="kind-redis" />} label="Console" onClick={() => openTab({ kind: "query", connection, sql: "", title: `${targetInfo(connection)?.name ?? "Redis"} console` })} />
    </>
  );
}

function SchemaTree({ connection, kind, depth, q, open, toggle, menu }: { connection: string; kind: DbKind; depth: number; q: string; open: Set<string>; toggle: (k: string, v?: boolean) => void; menu: MenuOpener }) {
  const schemas = useStore((s) => s.schemas[connection]);
  const loadSchemas = useStore((s) => s.loadSchemas);
  const def = defaultSchema(kind, connection);
  useEffect(() => {
    loadSchemas(connection);
  }, [connection, loadSchemas]);
  if (schemas === undefined || schemas === "loading") return <div className="tree-empty"><Spinner size={12} /></div>;
  if (!Array.isArray(schemas)) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{schemas.error}</div>;
  const list = schemas.length ? schemas : [def];
  // Single-schema databases show tables directly.
  if (list.length === 1) return <TableList connection={connection} kind={kind} schema={list[0]} depth={depth} q={q} menu={menu} />;
  const ordered = [...list].sort((a, b) => (a === def ? -1 : b === def ? 1 : a.localeCompare(b)));
  return (
    <>
      {ordered.map((s) => {
        const key = `schema:${connection}|${s}`;
        const expanded = open.has(key) || (s === def && !open.has(`${key}:closed`));
        return (
          <div key={s}>
            <Row
              depth={depth}
              icon={<FolderOpen size={13} className="muted" />}
              label={s}
              expanded={expanded}
              onToggle={() => (s === def ? toggle(`${key}:closed`, expanded) : toggle(key))}
              onContextMenu={(e) =>
                menu(e, [
                  { label: "Diagram", icon: <Network size={14} />, onClick: () => useStore.getState().openTab({ kind: "diagram", connection, schema: s }) },
                  { label: "New table…", icon: <Wand2 size={14} />, onClick: () => useStore.getState().openDialog({ kind: "table-designer", connection, schema: s }) },
                  { label: "Refresh", icon: <RefreshCw size={14} />, onClick: () => useStore.getState().loadTables(connection, s, true) },
                ])
              }
            />
            {expanded && <TableList connection={connection} kind={kind} schema={s} depth={depth + 1} q={q} menu={menu} />}
          </div>
        );
      })}
    </>
  );
}

function TableList({ connection, kind, schema, depth, q, menu }: { connection: string; kind: DbKind; schema: string; depth: number; q: string; menu: MenuOpener }) {
  const key = `${connection}|${schema}`;
  const tables = useStore((s) => s.tables[key]);
  const loadTables = useStore((s) => s.loadTables);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTab));
  useEffect(() => {
    loadTables(connection, schema);
  }, [connection, schema, loadTables]);
  if (tables === undefined || tables === "loading") return <div className="tree-empty"><Spinner size={12} /></div>;
  if (!Array.isArray(tables)) return <div className="tree-empty" style={{ color: "var(--danger)" }}>{tables.error}</div>;
  const shown = tables.filter((t) => matches(q, t.name));
  const readOnly = targetInfo(connection)?.readOnly;
  return (
    <>
      <Row
        depth={depth}
        icon={<Network size={13} className="muted" />}
        label={<span className="muted">Diagram</span>}
        onClick={() => openTab({ kind: "diagram", connection, schema })}
        actions={
          !readOnly && (
            <button className="icon-btn sm" title="New table" aria-label="New table" onClick={() => openDialog({ kind: "table-designer", connection, schema })}>
              <Plus size={12} />
            </button>
          )
        }
      />
      {shown.length === 0 && <div className="tree-empty" style={{ paddingLeft: 26 + depth * 12 }}>{q ? "No match" : "No tables"}</div>}
      {shown.map((t) => (
        <Row
          key={t.name}
          depth={depth}
          icon={t.kind === "view" ? <Eye size={13} className="muted" /> : <Table2 size={13} className="muted" />}
          label={t.name}
          active={activeTab?.kind === "table" && activeTab.connection === connection && activeTab.schema === schema && activeTab.table === t.name}
          onClick={() => openTab({ kind: "table", connection, schema, table: t.name })}
          onContextMenu={(e) =>
            menu(e, [
              { label: "Open data", icon: <Rows3 size={14} />, onClick: () => openTab({ kind: "table", connection, schema, table: t.name }) },
              { label: "Query", icon: <Code2 size={14} />, onClick: () => openTab({ kind: "query", connection, sql: `SELECT *\nFROM ${qualifiedName(kind, schema, t.name)}\nLIMIT 100;`, title: `${t.name} query` }) },
              { label: "Docs", icon: <BookOpen size={14} />, onClick: () => openTab({ kind: "docs", target: connection, object: `${schema}.${t.name}` }) },
              ...(readOnly || t.kind === "view"
                ? []
                : [
                    "sep" as const,
                    { label: "Edit table…", icon: <Wand2 size={14} />, onClick: () => openDialog({ kind: "table-designer", connection, schema, table: t.name }) },
                    { label: "Generate rows…", icon: <Sparkles size={14} />, onClick: () => openDialog({ kind: "generate", connection, schema, table: t.name }) },
                    {
                      label: "Drop table",
                      icon: <Trash2 size={14} />,
                      danger: true,
                      onClick: () =>
                        openDialog({
                          kind: "confirm",
                          title: `Drop ${t.name}?`,
                          body: "The table and all its rows are deleted.",
                          confirmLabel: "Drop table",
                          danger: true,
                          onConfirm: () => attempt("Drop failed", () => call("db.query", { connection, sql: `DROP TABLE ${qualifiedName(kind, schema, t.name)}` })).then(() => undefined),
                        }),
                    },
                  ]),
            ])
          }
        />
      ))}
    </>
  );
}
