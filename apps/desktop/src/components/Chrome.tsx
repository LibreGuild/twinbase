// Window chrome: the top bar (logo, workspace switcher, search, status, settings) and the icon
// rail that switches the sidebar panel.

import {
  Activity as ActivityIcon, Bot, BookMarked, Check, ChevronDown, Database, FolderOpen, FolderPlus, GitCompare, Home, Moon, Play, Plus,
  Search, Settings as SettingsIcon, Sparkles, Square, Sun, Upload,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import logo from "../assets/logo.png";
import { call, pickPath, runtime } from "../lib/api";
import { kindLabel } from "../lib/format";
import { attempt, switchWorkspace, useStore } from "../lib/store";
import { useAppliedTheme } from "../lib/theme";
import { useContextMenu, type MenuEntry } from "./ui";

/** Leaves room for the macOS window buttons when the title bar is transparent. */
export function useTopbarInset(): string | undefined {
  const platform = useStore((s) => s.info?.platform);
  return runtime.isTauri && platform === "macos" ? "84px" : undefined;
}

function lastSegment(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

export function TopBar() {
  const ws = useStore((s) => s.workspace);
  const settings = useStore((s) => s.settings);
  const mcpListening = useStore((s) => s.mcpListening);
  const agents = useStore((s) => s.agents);
  const activity = useStore((s) => s.activity);
  const showActivity = useStore((s) => s.showActivity);
  const set = useStore((s) => s.set);
  const openDialog = useStore((s) => s.openDialog);
  const updateSettings = useStore((s) => s.updateSettings);
  const theme = useAppliedTheme();
  const inset = useTopbarInset();
  const menu = useContextMenu();

  // Agent actions not yet seen in the Activity panel (counted from when the window opened).
  const agentActions = useMemo(() => activity.filter((a) => a.actor.kind === "agent").length, [activity]);
  const [seen, setSeen] = useState<number | null>(null);
  useEffect(() => {
    if (seen === null || showActivity) setSeen(agentActions);
  }, [showActivity, agentActions, seen]);
  const unseen = agentActions - (seen ?? agentActions);

  if (!ws || !settings) return <header className="topbar" data-tauri-drag-region style={{ ["--topbar-inset" as string]: inset }} />;
  const running = ws.mocks.filter((m) => m.running);

  const workspaceMenu = (e: React.MouseEvent) => {
    const recent = settings.recent_workspaces.filter((p) => p !== ws.path).slice(0, 8);
    const items: MenuEntry[] = [
      { label: ws.name, sub: ws.path, icon: <Check size={14} style={{ color: "var(--accent-text)" }} />, onClick: () => undefined },
      ...recent.map((p) => ({ label: lastSegment(p), sub: p, icon: <FolderOpen size={14} />, onClick: () => attempt("Could not open workspace", () => switchWorkspace(p)) })),
      "sep",
      { label: "New workspace…", icon: <FolderPlus size={14} />, onClick: () => openDialog({ kind: "new-workspace" }) },
      {
        label: "Open folder…",
        icon: <FolderOpen size={14} />,
        onClick: async () => {
          const p = await pickPath({ directory: true, title: "Open workspace folder" });
          if (p) await attempt("Could not open workspace", () => switchWorkspace(p));
        },
      },
      { label: "Import bundle…", icon: <Upload size={14} />, onClick: () => openDialog({ kind: "import" }) },
      "sep",
      { label: "Start page", icon: <Sparkles size={14} />, onClick: () => set({ showStart: true }) },
    ];
    menu.open(e, items, { anchor: e.currentTarget as HTMLElement, header: "Workspaces" });
  };

  const mocksMenu = (e: React.MouseEvent) => {
    const items: MenuEntry[] = ws.mocks.map((m) => ({
      label: m.name,
      sub: `${kindLabel(m.kind)} · ${m.host}:${m.port}${m.running ? " · running" : ""}`,
      icon: m.running ? <Square size={13} style={{ color: "var(--success)" }} /> : <Play size={13} />,
      onClick: () => attempt(m.running ? "Could not stop the mock" : "Could not start the mock", () => call(m.running ? "mock.stop" : "mock.start", { id: m.id })),
    }));
    if (items.length) items.push("sep");
    items.push({ label: "New mock server…", icon: <Plus size={14} />, onClick: () => openDialog({ kind: "mock" }) });
    menu.open(e, items, { anchor: e.currentTarget as HTMLElement, header: "Mock servers · click to start or stop" });
  };

  return (
    <header className="topbar" data-tauri-drag-region style={{ ["--topbar-inset" as string]: inset }}>
      <div className="logo" data-tauri-drag-region>
        <img src={logo} alt="" draggable={false} />
        <span data-tauri-drag-region>TwinBase</span>
      </div>
      <span className="slash">/</span>
      <button className="tb-button" onClick={workspaceMenu} title={ws.path} aria-haspopup="menu">
        <span className="ellipsis">{ws.name}</span>
        <ChevronDown size={14} className="faint" />
      </button>
      <div className="drag" data-tauri-drag-region />
      <button className="tb-search" onClick={() => set({ palette: true })} aria-label="Search">
        <Search size={14} />
        <span>Search tables, mocks, queries…</span>
        <kbd>⌘K</kbd>
      </button>
      <div className="drag" data-tauri-drag-region />
      <button className={`tb-pill${running.length ? " live" : ""}`} onClick={mocksMenu} title="Mock servers" aria-haspopup="menu">
        <span className={`status-dot${running.length ? " on" : ""}`} />
        {running.length ? `${running.length} running` : "No mocks running"}
        <ChevronDown size={13} />
      </button>
      <button
        className={`tb-pill${settings.mcp.enabled && mcpListening === null ? " warn" : ""}`}
        onClick={() => openDialog({ kind: "settings", section: "agents" })}
        title={!settings.mcp.enabled ? "AI agent access is off" : mcpListening ? `MCP endpoint http://127.0.0.1:${mcpListening}/mcp` : `Not listening: port ${settings.mcp.port} is in use`}
      >
        <Bot size={13} />
        {!settings.mcp.enabled ? "Agents off" : mcpListening ? (agents.length ? agents.join(", ") : "Agents ready") : "MCP unavailable"}
      </button>
      <span className="tb-sep" />
      <button className={`tb-icon${showActivity ? " on" : ""}`} onClick={() => set({ showActivity: !showActivity })} title="Activity (⌘/Ctrl+J)" aria-label="Activity">
        <ActivityIcon size={16} />
        {unseen > 0 && !showActivity && <span className="dot-badge">{unseen > 99 ? "99+" : unseen}</span>}
      </button>
      <button className="tb-icon" onClick={() => updateSettings({ theme: theme === "dark" ? "light" : "dark" })} title="Toggle light / dark" aria-label="Toggle theme">
        {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
      </button>
      <button className="tb-icon" onClick={() => openDialog({ kind: "settings" })} title="Settings (⌘/Ctrl+,)" aria-label="Settings">
        <SettingsIcon size={16} />
      </button>
      {menu.node}
    </header>
  );
}

export function Rail() {
  const rail = useStore((s) => s.rail);
  const setRail = useStore((s) => s.setRail);
  const openTab = useStore((s) => s.openTab);
  const ws = useStore((s) => s.workspace);
  const queries = ws?.queries.length ?? 0;
  const running = ws?.mocks.filter((m) => m.running).length ?? 0;
  return (
    <nav className="rail" aria-label="Sections">
      <button className="rail-btn" onClick={() => openTab({ kind: "home" })} title="Home" aria-label="Home">
        <Home size={18} />
      </button>
      <button className={`rail-btn${rail === "explorer" ? " on" : ""}`} onClick={() => setRail("explorer")} title="Databases and mock servers" aria-label="Databases">
        <Database size={18} />
        {running > 0 && <span className="dot-badge">{running}</span>}
      </button>
      <button className={`rail-btn${rail === "queries" ? " on" : ""}`} onClick={() => setRail("queries")} title="Saved queries" aria-label="Saved queries">
        <BookMarked size={18} />
        {queries > 0 && <span className="dot-badge">{queries > 99 ? "99+" : queries}</span>}
      </button>
      <button className="rail-btn" onClick={() => openTab({ kind: "schema" })} title="Schema snapshots and diff" aria-label="Schema snapshots">
        <GitCompare size={18} />
      </button>

    </nav>
  );
}
