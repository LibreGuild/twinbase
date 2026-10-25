// ⌘/Ctrl+K: jump to tables, mocks, connections, saved queries, or run an action.

import { Code2, FileText, FolderOpen, KeyRound, LayoutGrid, Plug, Rows3, Search, Server, Settings, Upload } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { CONSOLE_KINDS, consoleStart, kindLabel } from "../lib/format";
import { targetInfo, useStore } from "../lib/store";

interface Item {
  id: string;
  label: string;
  hint?: string;
  icon: ReactNode;
  run: () => void;
}

export function CommandPalette() {
  const open = useStore((s) => s.palette);
  const set = useStore((s) => s.set);
  if (!open) return null;
  return <Palette onClose={() => set({ palette: false })} />;
}

function Palette({ onClose }: { onClose: () => void }) {
  const ws = useStore((s) => s.workspace);
  const tables = useStore((s) => s.tables);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const items = useMemo<Item[]>(() => {
    if (!ws) return [];
    const out: Item[] = [
      { id: "a:query", label: "New SQL query", icon: <Code2 size={14} />, hint: "⌘T", run: () => openTab({ kind: "query", sql: "", title: "New query" }) },
      { id: "a:mock", label: "New mock server", icon: <Server size={14} />, run: () => openDialog({ kind: "mock" }) },
      { id: "a:conn", label: "New connection", icon: <Plug size={14} />, run: () => openDialog({ kind: "connection" }) },
      { id: "a:import", label: "Import bundle", icon: <Upload size={14} />, run: () => openDialog({ kind: "import" }) },
      { id: "a:export", label: "Export bundle", icon: <Upload size={14} style={{ transform: "rotate(180deg)" }} />, run: () => openDialog({ kind: "export" }) },
      { id: "a:ws", label: "Open workspace", icon: <FolderOpen size={14} />, run: () => openDialog({ kind: "workspace" }) },
      { id: "a:schema", label: "Schema snapshots and diff", icon: <LayoutGrid size={14} />, run: () => openTab({ kind: "schema" }) },
      { id: "a:settings", label: "Settings", icon: <Settings size={14} />, hint: "⌘,", run: () => useStore.getState().openDialog({ kind: "settings" }) },
      { id: "a:agents", label: "Connect an AI agent (MCP setup)", icon: <Settings size={14} />, run: () => useStore.getState().openDialog({ kind: "settings", section: "agents" }) },
    ];
    for (const m of ws.mocks) out.push({ id: `m:${m.id}`, label: m.name, hint: `${kindLabel(m.kind)} mock :${m.port}`, icon: <Server size={14} />, run: () => openTab({ kind: "mock", mockId: m.id }) });
    for (const c of ws.connections)
      out.push({ id: `c:${c.id}`, label: c.name, hint: kindLabel(c.kind), icon: c.kind === "redis" ? <KeyRound size={14} /> : <Plug size={14} />, run: () => (c.kind === "redis" ? openTab({ kind: "redis", connection: c.id }) : openTab({ kind: "query", connection: c.id, sql: consoleStart(c.kind), title: CONSOLE_KINDS.includes(c.kind) ? `${c.name} console` : c.name })) });
    for (const [key, list] of Object.entries(tables)) {
      if (!Array.isArray(list)) continue;
      const [connection, schema] = key.split("|");
      const name = targetInfo(connection)?.name ?? connection;
      out.push({ id: `d:${key}`, label: `Diagram: ${name} / ${schema}`, icon: <LayoutGrid size={14} />, run: () => openTab({ kind: "diagram", connection, schema }) });
      for (const t of list) out.push({ id: `t:${key}|${t.name}`, label: t.name, hint: `${name} · ${schema}`, icon: <Rows3 size={14} />, run: () => openTab({ kind: "table", connection, schema, table: t.name }) });
    }
    for (const c of ws.collections) out.push({ id: `col:${c.id}`, label: c.name, hint: "collection", icon: <FileText size={14} />, run: () => openTab({ kind: "collection", collectionId: c.id }) });
    for (const x of ws.queries) out.push({ id: `q:${x.collection}:${x.id}`, label: x.name, hint: "saved query", icon: <Code2 size={14} />, run: () => openTab({ kind: "query", sql: x.sql, title: x.name, connection: x.connection, saved: { collection: x.collection, id: x.id } }) });
    return out;
  }, [ws, tables, openTab, openDialog]);

  const shown = useMemo(() => {
    const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
    if (!terms.length) return items.slice(0, 60);
    return items
      .map((it) => {
        const hay = `${it.label} ${it.hint ?? ""}`.toLowerCase();
        if (!terms.every((t) => hay.includes(t))) return null;
        const score = (it.label.toLowerCase().startsWith(terms[0]) ? 0 : 1) + it.label.length / 1000;
        return { it, score };
      })
      .filter((x): x is { it: Item; score: number } => !!x)
      .sort((a, b) => a.score - b.score)
      .slice(0, 60)
      .map((x) => x.it);
  }, [items, q]);

  useEffect(() => {
    setSel(0);
  }, [q]);
  useEffect(() => {
    listRef.current?.querySelector(".palette-item.on")?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  const choose = (it?: Item) => {
    if (!it) return;
    onClose();
    it.run();
  };

  return createPortal(
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog palette" role="dialog" aria-label="Command palette">
        <div className="row" style={{ padding: "10px 12px", borderBottom: "1px solid var(--border)" }}>
          <Search size={15} className="faint" />
          <input
            autoFocus
            className="grow"
            style={{ border: 0, outline: "none", background: "transparent", fontSize: 14 }}
            placeholder="Go to table, mock, connection, saved query, or action…"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") (setSel((s) => Math.min(shown.length - 1, s + 1)), e.preventDefault());
              else if (e.key === "ArrowUp") (setSel((s) => Math.max(0, s - 1)), e.preventDefault());
              else if (e.key === "Enter") choose(shown[sel]);
              else if (e.key === "Escape") onClose();
            }}
            aria-label="Search"
          />
        </div>
        <div className="palette-list" ref={listRef}>
          {shown.length === 0 && <div className="faint" style={{ padding: 12 }}>No matches. Tables appear here after you expand a connection once.</div>}
          {shown.map((it, i) => (
            <div key={it.id} className={`palette-item${i === sel ? " on" : ""}`} onMouseEnter={() => setSel(i)} onClick={() => choose(it)}>
              <span className="muted">{it.icon}</span>
              <span className="ellipsis">{it.label}</span>
              {it.hint && <span className="ph">{it.hint}</span>}
            </div>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}
