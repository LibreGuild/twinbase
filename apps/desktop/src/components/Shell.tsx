// Tab bar and activity panel.

import { Activity as ActivityIcon, Bot, Braces, Code2, FileText, FolderTree, GitCompare, KeyRound, Layers, LayoutGrid, Rows3, Search, Server, Terminal, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { duration, timeOfDay } from "../lib/format";
import { targetInfo, useStore, type Tab } from "../lib/store";
import { ConfirmDialog, KindIcon } from "./ui";

export function tabTitle(t: Tab): string {
  const ws = useStore.getState().workspace;
  switch (t.kind) {
    case "home":
      return "Home";
    case "schema":
      return "Schema snapshots";
    case "table":
      return t.table;
    case "query":
      return t.title || "Query";
    case "diagram":
      return `${targetInfo(t.connection)?.name ?? "Diagram"} · diagram`;
    case "redis":
      return `${targetInfo(t.connection)?.name ?? "Redis"} · keys`;
    case "mongo":
      return t.collection;
    case "dynamo":
      return t.table;
    case "firestore":
      return t.collection.split("/").pop() ?? t.collection;
    case "search":
      return t.index;
    case "mock":
      return ws?.mocks.find((m) => m.id === t.mockId)?.name ?? "Mock";
    case "collection":
      return ws?.collections.find((c) => c.id === t.collectionId)?.name ?? "Collection";
  }
}

function tabIcon(t: Tab): ReactNode {
  switch (t.kind) {
    case "table":
      return <Rows3 size={13} className="muted" />;
    case "query":
      return targetInfo(t.connection)?.kind === "mongodb" ? (
        <Braces size={13} className="kind-mongo" />
      ) : targetInfo(t.connection)?.kind === "dynamodb" ? (
        <Terminal size={13} className="kind-dynamo" />
      ) : targetInfo(t.connection)?.kind === "firestore" ? (
        <FileText size={13} className="kind-firestore" />
      ) : targetInfo(t.connection)?.kind === "elasticsearch" ? (
        <Terminal size={13} className="kind-es" />
      ) : (
        <Code2 size={13} className="muted" />
      );
    case "dynamo":
      return <Rows3 size={13} className="kind-dynamo" />;
    case "firestore":
      return <FolderTree size={13} className="kind-firestore" />;
    case "search":
      return <Search size={13} className="kind-es" />;
    case "mongo":
      return <Layers size={13} className="kind-mongo" />;
    case "diagram":
      return <LayoutGrid size={13} className="muted" />;
    case "redis":
      return <KeyRound size={13} className="kind-redis" />;
    case "mock": {
      const m = useStore.getState().workspace?.mocks.find((x) => x.id === t.mockId);
      return m ? <KindIcon kind={m.kind} size={13} /> : <Server size={13} />;
    }
    case "collection":
      return <FileText size={13} className="muted" />;
    case "schema":
      return <GitCompare size={13} className="muted" />;
    default:
      return null;
  }
}

export function TabBar() {
  const tabs = useStore((s) => s.tabs);
  const active = useStore((s) => s.activeTab);
  const dirty = useStore((s) => s.dirtyTabs);
  const setActive = useStore((s) => s.setActive);
  const closeTab = useStore((s) => s.closeTab);
  useStore((s) => s.workspace); // titles depend on workspace names
  const [confirm, setConfirm] = useState<string | null>(null);
  const activeRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active]);
  const close = (id: string) => (dirty[id] ? setConfirm(id) : closeTab(id));
  return (
    <div className="tabbar" role="tablist">
      {tabs.map((t) => {
        const title = tabTitle(t);
        return (
          <div
            key={t.id}
            ref={t.id === active ? activeRef : undefined}
            className={`tab${t.id === active ? " active" : ""}`}
            role="tab"
            aria-selected={t.id === active}
            title={title}
            onClick={() => setActive(t.id)}
            onMouseDown={(e) => e.button === 1 && (e.preventDefault(), close(t.id))}
          >
            {tabIcon(t)}
            <span className="title">{title}</span>
            {dirty[t.id] ? <span className="dirty" title="Unsaved changes" /> : null}
            <button
              className="icon-btn sm close"
              aria-label={`Close ${title}`}
              onClick={(e) => {
                e.stopPropagation();
                close(t.id);
              }}
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
      {confirm && <ConfirmDialog title="Close tab with unsaved changes?" body="Staged edits or unsaved SQL in this tab will be lost." confirmLabel="Close tab" danger onClose={() => setConfirm(null)} onConfirm={() => useStore.getState().closeTab(confirm)} />}
    </div>
  );
}

export function ActivityPanel() {
  const activity = useStore((s) => s.activity);
  const set = useStore((s) => s.set);
  const openTab = useStore((s) => s.openTab);
  const [filter, setFilter] = useState<"all" | "agents" | "errors">("all");
  const list = useMemo(() => {
    const l = activity.filter((a) => (filter === "agents" ? a.actor.kind === "agent" : filter === "errors" ? !a.ok : true));
    return l.slice(-500).reverse();
  }, [activity, filter]);
  return (
    <div className="activity" id="activity-panel">
      <div className="activity-head">
        <ActivityIcon size={14} />
        <span>Activity</span>
        <div className="seg" style={{ marginLeft: 8 }}>
          {(["all", "agents", "errors"] as const).map((f) => (
            <button key={f} className={filter === f ? "on" : ""} onClick={() => setFilter(f)} style={{ height: 22 }}>
              {f === "all" ? "All" : f === "agents" ? "AI agents" : "Errors"}
            </button>
          ))}
        </div>
        <span className="grow" />
        <button className="icon-btn sm" title="Clear" aria-label="Clear activity" onClick={() => set({ activity: [] })}>
          <Trash2 size={13} />
        </button>
        <button className="icon-btn sm" title="Hide (⌘/Ctrl+J)" aria-label="Hide activity" onClick={() => set({ showActivity: false })}>
          <X size={13} />
        </button>
      </div>
      <div className="activity-list">
        {list.length === 0 && <div className="faint" style={{ padding: 12 }}>Changes made by you, AI agents and the CLI show up here.</div>}
        {list.map((a) => (
          <div key={a.id} className="activity-item">
            <span className="when">{timeOfDay(a.at)}</span>
            <span className={`who${a.actor.kind === "agent" ? " agent" : ""}`} title={a.actor.kind === "agent" ? a.actor.name : a.actor.kind}>
              {a.actor.kind === "agent" && <Bot size={12} style={{ marginRight: 4, verticalAlign: -2 }} />}
              {a.actor.kind === "agent" ? a.actor.name : a.actor.kind === "cli" ? "CLI" : "You"}
            </span>
            <span className="what">
              <span>{a.summary}</span>
              {a.connection && <span className="faint xs"> · {targetInfo(a.connection)?.name ?? a.connection}</span>}
              {a.sql && (
                <a
                  href="#"
                  className="sql"
                  title={`${a.sql}\n\nClick to open in the SQL editor`}
                  onClick={(e) => {
                    e.preventDefault();
                    if (a.action.startsWith("redis.")) openTab({ kind: "redis", connection: a.connection! });
                    else openTab({ kind: "query", connection: a.connection, sql: a.sql!, title: "From activity" });
                  }}
                >
                  {a.sql.replace(/\s+/g, " ")}
                </a>
              )}
              {a.error && <span className="err">{a.error}</span>}
            </span>
            <span className="faint xs">{duration(a.duration_ms)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
