// Window chrome: the top bar (logo and workspace switcher beside the window buttons; mock and agent
// status, search, activity, theme and settings on the right; our own window controls on Windows and
// Linux) and the icon rail that switches the sidebar panel.

import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Activity as ActivityIcon, Bot, BookMarked, Check, ChevronDown, Database, FolderOpen, FolderPlus, GitCompare, Home, Minus, Moon, Play, Plus,
  GraduationCap, Search, Settings as SettingsIcon, Sparkles, Square, Sun, Upload, X,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import logo from "../assets/logo.png";
import { call, pickPath, runtime } from "../lib/api";
import { kindLabel } from "../lib/format";
import { attempt, openBootcamp, switchWorkspace, useStore } from "../lib/store";
import { useAppliedTheme } from "../lib/theme";
import { useContextMenu, type MenuEntry } from "./ui";

/** macOS draws its own window buttons over the bar (on the left); elsewhere we draw ours (right). */
const isMac = typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || navigator.userAgent);

function lastSegment(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/** Browser dev mode only: `?mac` draws macOS window buttons, for screenshots that match the app. */
const demoMac = !runtime.isTauri && typeof location !== "undefined" && new URLSearchParams(location.search).has("mac");

/** The window's top bar; it replaces the system title bar. Empty space drags the window. */
export function TopBar() {
  const ws = useStore((s) => s.workspace);
  const custom = runtime.isTauri && !isMac;
  return (
    <header className={`topbar${(runtime.isTauri && isMac) || demoMac ? " mac" : ""}`} data-tauri-drag-region>
      {demoMac && (
        <span className="demo-traffic" aria-hidden>
          <i />
          <i />
          <i />
        </span>
      )}
      <div className="topbar-left" data-tauri-drag-region>
        <img className="topbar-logo" src={logo} alt="" draggable={false} />
        {ws ? <WorkspaceSwitch /> : <span className="topbar-name">TwinBase</span>}
      </div>
      <div className="drag" data-tauri-drag-region />
      {ws && <TopActions />}
      {custom && <WindowControls />}
    </header>
  );
}

function WindowControls() {
  const [maximized, setMaximized] = useState(false);
  useEffect(() => {
    const win = getCurrentWindow();
    void win.isMaximized().then(setMaximized);
    const unlisten = win.onResized(() => void win.isMaximized().then(setMaximized));
    return () => {
      void unlisten.then((f) => f());
    };
  }, []);
  return (
    <div className="window-controls">
      <button aria-label="Minimize" onClick={() => void getCurrentWindow().minimize()}>
        <Minus size={15} strokeWidth={1.6} />
      </button>
      <button aria-label={maximized ? "Restore" : "Maximize"} onClick={() => void getCurrentWindow().toggleMaximize()}>
        {maximized ? (
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.2" aria-hidden>
            <rect x="1" y="3" width="8" height="8" rx="1" />
            <path d="M3 3V2a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H9" />
          </svg>
        ) : (
          <Square size={12} strokeWidth={1.6} />
        )}
      </button>
      <button className="close" aria-label="Close" onClick={() => void getCurrentWindow().close()}>
        <X size={16} strokeWidth={1.6} />
      </button>
    </div>
  );
}

/** The workspace name and its menu: recent workspaces, new/open, bundles, the Bootcamp. */
function WorkspaceSwitch() {
  const ws = useStore((s) => s.workspace);
  const settings = useStore((s) => s.settings);
  const set = useStore((s) => s.set);
  const openDialog = useStore((s) => s.openDialog);
  const inBootcamp = useStore((s) => !!s.bootcamp?.active);
  const menu = useContextMenu();
  if (!ws || !settings) return null;

  const workspaceMenu = (e: React.MouseEvent) => {
    const bootcampPath = useStore.getState().bootcamp?.path;
    const bootcampActive = !!useStore.getState().bootcamp?.active;
    const recent = settings.recent_workspaces.filter((p) => p !== ws.path && p !== bootcampPath).slice(0, 8);
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
      ...(bootcampActive ? [] : [{ label: "Bootcamp", sub: "Lessons, quizzes and labs in their own workspace", icon: <GraduationCap size={14} />, onClick: () => attempt("Could not open the Bootcamp", () => openBootcamp()) }]),
      { label: "Start page", icon: <Sparkles size={14} />, onClick: () => set({ showStart: true }) },
    ];
    menu.open(e, items, { anchor: e.currentTarget as HTMLElement, header: "Workspaces" });
  };

  return (
    <>
      <button className={`ws-switch${inBootcamp ? " bootcamp" : ""}`} onClick={workspaceMenu} title={`${ws.name}\n${ws.path}`} aria-haspopup="menu">
        {inBootcamp && <GraduationCap size={14} className="c-bootcamp" />}
        <span className="ellipsis">{ws.name}</span>
        <ChevronDown size={14} className="faint" />
      </button>
      {menu.node}
    </>
  );
}

/** Right side of the top bar: search, mock and agent status, activity, theme and settings. */
function TopActions() {
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
  const menu = useContextMenu();

  // Agent actions not yet seen in the Activity panel (counted from when the window opened).
  const agentActions = useMemo(() => activity.filter((a) => a.actor.kind === "agent").length, [activity]);
  const [seen, setSeen] = useState<number | null>(null);
  useEffect(() => {
    if (seen === null || showActivity) setSeen(agentActions);
  }, [showActivity, agentActions, seen]);
  const unseen = agentActions - (seen ?? agentActions);

  if (!ws || !settings) return null;
  const running = ws.mocks.filter((m) => m.running);

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
    <div className="topbar-actions">
      <button className="tb-search" onClick={() => set({ palette: true })} aria-label="Search" title="Search tables, mocks, queries and commands">
        <Search size={13} />
        <span>Search</span>
        <kbd>⌘K</kbd>
      </button>
      <button className={`tb-pill${running.length ? " live" : ""}`} onClick={mocksMenu} title="Mock servers" aria-haspopup="menu">
        <span className={`status-dot${running.length ? " on" : ""}`} />
        {running.length ? `${running.length} running` : "Mocks"}
        <ChevronDown size={13} />
      </button>
      <button
        className={`tb-pill${settings.mcp.enabled && mcpListening === null ? " warn" : ""}`}
        onClick={() => openDialog({ kind: "settings", section: "agents" })}
        title={!settings.mcp.enabled ? "AI agent access is off" : mcpListening ? `MCP endpoint http://127.0.0.1:${mcpListening}/mcp` : `Not listening: port ${settings.mcp.port} is in use`}
      >
        <Bot size={13} />
        <span className="ellipsis" style={{ maxWidth: 160 }}>{!settings.mcp.enabled ? "Agents off" : mcpListening ? (agents.length ? agents.join(", ") : "Agents") : "MCP unavailable"}</span>
      </button>
      <span className="tb-sep" />
      <button className={`tb-icon${showActivity ? " on" : ""}`} onClick={() => set({ showActivity: !showActivity })} title="Activity (⌘/Ctrl+J)" aria-label="Activity">
        <ActivityIcon size={15} />
        {unseen > 0 && !showActivity && <span className="dot-badge">{unseen > 99 ? "99+" : unseen}</span>}
      </button>
      <button className="tb-icon" onClick={() => updateSettings({ theme: theme === "dark" ? "light" : "dark" })} title="Toggle light / dark" aria-label="Toggle theme">
        {theme === "dark" ? <Sun size={15} /> : <Moon size={15} />}
      </button>
      <button className="tb-icon" onClick={() => openDialog({ kind: "settings" })} title="Settings (⌘/Ctrl+,)" aria-label="Settings">
        <SettingsIcon size={15} />
      </button>
      {menu.node}
    </div>
  );
}

export function Rail() {
  const rail = useStore((s) => s.rail);
  const setRail = useStore((s) => s.setRail);
  const openTab = useStore((s) => s.openTab);
  const ws = useStore((s) => s.workspace);
  const queries = ws?.queries.length ?? 0;
  const running = ws?.mocks.filter((m) => m.running).length ?? 0;
  const inBootcamp = useStore((s) => !!s.bootcamp?.active);
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
      <span className="grow" />
      <button className={`rail-btn bootcamp${inBootcamp ? " on" : ""}`} onClick={() => attempt("Could not open the Bootcamp", () => openBootcamp())} title="Bootcamp: learn databases with lessons, quizzes and labs" aria-label="Bootcamp">
        <GraduationCap size={18} />
      </button>

    </nav>
  );
}
