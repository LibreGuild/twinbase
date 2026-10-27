// Global UI state: workspace snapshot, tabs, activity and toasts. Server data that belongs to a
// single view (rows, results) stays in that view.

import { create } from "zustand";
import { call, errorMessage, subscribe } from "./api";
import type {
  Activity, ConnectionDef, CoreEvent, DbKind, Filter, MockStatus, SchemaSource, Settings, TableInfo, TrafficEvent, WorkspaceInfo,
} from "./types";

export type Tab =
  | { id: string; kind: "home" }
  | { id: string; kind: "table"; connection: string; schema: string; table: string; filters?: Filter[] }
  | { id: string; kind: "query"; connection?: string; sql: string; title: string; saved?: { collection: string; id: string }; autorun?: boolean }
  | { id: string; kind: "diagram"; connection: string; schema: string }
  | { id: string; kind: "redis"; connection: string; db?: number }
  | { id: string; kind: "mongo"; connection: string; database: string; collection: string }
  | { id: string; kind: "dynamo"; connection: string; table: string }
  | { id: string; kind: "firestore"; connection: string; collection: string }
  | { id: string; kind: "search"; connection: string; index: string }
  | { id: string; kind: "mock"; mockId: string; section?: string }
  | { id: string; kind: "collection"; collectionId: string }
  | { id: string; kind: "docs"; target?: string; object?: string }
  | { id: string; kind: "bootcamp" }
  | { id: string; kind: "lesson"; lessonId: string }
  | { id: string; kind: "schema"; from?: SchemaSource; to?: SchemaSource };

export type TabInput = Tab extends infer T ? (T extends Tab ? Omit<T, "id"> & { id?: string } : never) : never;

export interface Toast {
  id: number;
  kind: "info" | "success" | "error";
  title: string;
  body?: string;
}

/** A connectable target as the explorer shows it: saved connections and mock servers. */
export interface Target {
  id: string;
  name: string;
  kind: DbKind;
  isMock: boolean;
  color?: string;
  readOnly: boolean;
}

export interface AppInfo {
  version: string;
  first_run: boolean;
  default_workspace: string;
  platform: string;
  mcp: { listening: number | null };
}

export type RailPanel = "explorer" | "queries";

interface State {
  ready: boolean;
  loadError?: string;
  info?: AppInfo;
  /** Start page (choose or create a workspace) is showing. */
  showStart: boolean;
  rail: RailPanel;
  settings?: Settings;
  /** Port the MCP endpoint is actually listening on (null when it could not start). */
  mcpListening?: number | null;
  workspace?: WorkspaceInfo;
  /** The Bootcamp workspace (its own folder in the app directory) and whether it is open. */
  bootcamp?: { path: string; active: boolean };
  tabs: Tab[];
  activeTab: string;
  activity: Activity[];
  toasts: Toast[];
  agents: string[];
  /** Bumped when a connection's schema / data changes (views refetch on change). */
  schemaVersion: Record<string, number>;
  dataVersion: Record<string, number>;
  /** Explorer cache: `${connection}|${schema}` → tables. */
  tables: Record<string, TableInfo[] | "loading" | { error: string }>;
  schemas: Record<string, string[] | "loading" | { error: string }>;
  dirtyTabs: Record<string, boolean>;
  showActivity: boolean;
  palette: boolean;
  dialog: DialogState | null;

  init(): Promise<void>;
  refreshWorkspace(): Promise<void>;
  updateSettings(patch: Partial<Settings> | Record<string, unknown>): Promise<void>;
  openTab(tab: TabInput): void;
  closeTab(id: string): void;
  setActive(id: string): void;
  updateTab(id: string, patch: Partial<Tab>): void;
  setDirty(id: string, dirty: boolean): void;
  toast(kind: Toast["kind"], title: string, body?: string): void;
  dismissToast(id: number): void;
  loadSchemas(connection: string, force?: boolean): Promise<string[]>;
  loadTables(connection: string, schema: string, force?: boolean): Promise<void>;
  set(patch: Partial<State>): void;
  setRail(panel: RailPanel): void;
  openDialog(d: DialogState): void;
  closeDialog(): void;
}

export type DialogState =
  | { kind: "connection"; connection?: ConnectionDef }
  | { kind: "mock"; mock?: MockStatus; mockKind?: MockStatus["kind"] }
  | { kind: "export" }
  | { kind: "import" }
  | { kind: "workspace" }
  | { kind: "generate"; connection: string; schema: string; table: string }
  | { kind: "table-designer"; connection: string; schema: string; table?: string }
  | { kind: "save-query"; tabId: string; sql: string; connection?: string; saved?: { collection: string; id: string } }
  | { kind: "collection"; collectionId?: string }
  | { kind: "confirm"; title: string; body: string; confirmLabel: string; danger?: boolean; onConfirm: () => void | Promise<void> }
  | { kind: "settings"; section?: string }
  | { kind: "new-workspace" };

const trafficListeners = new Set<(mockId: string, e: TrafficEvent) => void>();
export function onTraffic(fn: (mockId: string, e: TrafficEvent) => void) {
  trafficListeners.add(fn);
  return () => {
    trafficListeners.delete(fn);
  };
}

let toastId = 0;
let unsubscribe: (() => void) | undefined;
let themeListener = false;

function tabKey(t: TabInput): string | undefined {
  switch (t.kind) {
    case "home":
      return "home";
    case "schema":
      return "schema";
    case "table":
      return `table:${t.connection}:${t.schema}:${t.table}`;
    case "diagram":
      return `diagram:${t.connection}:${t.schema}`;
    case "redis":
      return `redis:${t.connection}:${t.db ?? ""}`;
    case "mongo":
      return `mongo:${t.connection}:${t.database}:${t.collection}`;
    case "dynamo":
      return `dynamo:${t.connection}:${t.table}`;
    case "firestore":
      return `firestore:${t.connection}:${t.collection}`;
    case "search":
      return `search:${t.connection}:${t.index}`;
    case "mock":
      return `mock:${t.mockId}`;
    case "collection":
      return `collection:${t.collectionId}`;
    case "docs":
      return `docs:${t.target ?? ""}:${t.object ?? ""}`;
    case "bootcamp":
      return "bootcamp";
    case "lesson":
      return `lesson:${t.lessonId}`;
    case "query":
      return t.saved ? `query:${t.saved.collection}:${t.saved.id}` : undefined;
  }
}

const UI_FONTS: Record<string, string> = {
  inter: '"Inter Variable"',
  atkinson: '"Atkinson Hyperlegible Next Variable"',
  plex: '"IBM Plex Sans Variable"',
  system: "",
};
const MONO_FONTS: Record<string, string> = {
  "jetbrains-mono": '"JetBrains Mono Variable"',
  "fira-code": '"Fira Code Variable"',
  system: "",
};
const UI_FALLBACK = '-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, Roboto, sans-serif';
const MONO_FALLBACK = 'ui-monospace, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';

/** Apply typeface and size settings to the document. */
export function applyFonts(s: Pick<Settings, "font_family" | "mono_font" | "font_size">) {
  const root = document.documentElement.style;
  const ui = UI_FONTS[s.font_family] ?? UI_FONTS.inter;
  const mono = MONO_FONTS[s.mono_font] ?? MONO_FONTS["jetbrains-mono"];
  root.setProperty("--font-ui", ui ? `${ui}, ${UI_FALLBACK}` : UI_FALLBACK);
  root.setProperty("--font-mono", mono ? `${mono}, ${MONO_FALLBACK}` : MONO_FALLBACK);
  root.setProperty("--fs", `${s.font_size || 13}px`);
}

function storedRail(): RailPanel {
  try {
    return localStorage.getItem("twinbase.rail") === "queries" ? "queries" : "explorer";
  } catch {
    return "explorer";
  }
}

export function applyTheme(theme: Settings["theme"] | undefined) {
  const dark = theme === "dark" || ((theme ?? "system") === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}

export const useStore = create<State>((set, get) => ({
  ready: false,
  showStart: false,
  rail: storedRail(),
  tabs: [{ id: "home", kind: "home" }],
  activeTab: "home",
  activity: [],
  toasts: [],
  agents: [],
  schemaVersion: {},
  dataVersion: {},
  tables: {},
  schemas: {},
  dirtyTabs: {},
  showActivity: false,
  palette: false,
  dialog: null,

  async init() {
    try {
      const [settings, workspace, activity, info, bootcamp] = await Promise.all([
        call<Settings>("settings.get"),
        call<WorkspaceInfo>("workspace.get"),
        call<Activity[]>("activity.list", { limit: 300 }),
        call<AppInfo>("app.info"),
        call<{ path: string; active: boolean }>("bootcamp.info").catch(() => undefined),
      ]);
      applyTheme(settings.theme);
      applyFonts(settings);
      let started = false;
      try {
        started = localStorage.getItem("twinbase.started") === "1";
      } catch {
        /* storage unavailable */
      }
      set({
        settings,
        workspace,
        info,
        bootcamp,
        activity: activity.slice().reverse(),
        mcpListening: info.mcp.listening,
        showStart: get().showStart || (info.first_run && !started),
        ready: true,
        loadError: undefined,
      });
      // The desktop shell binds the MCP port right after start-up; check again shortly.
      if (info.mcp.listening === null) {
        setTimeout(() => call<AppInfo>("app.info").then((i) => set({ mcpListening: i.mcp.listening }), () => undefined), 1500);
      }
      if (!themeListener) {
        themeListener = true;
        window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => applyTheme(get().settings?.theme));
      }
      unsubscribe?.();
      unsubscribe = subscribe(handleEvent);
    } catch (e) {
      set({ loadError: errorMessage(e), ready: false });
    }
  },

  async refreshWorkspace() {
    const workspace = await call<WorkspaceInfo>("workspace.get");
    // Drop tabs whose objects disappeared.
    const mocks = new Set(workspace.mocks.map((m) => m.id));
    const collections = new Set(workspace.collections.map((c) => c.id));
    const targets = new Set([...workspace.connections.map((c) => c.id), ...workspace.mocks.map((m) => m.connection_id)]);
    const tabs = get().tabs.filter((t) => {
      if (t.kind === "mock") return mocks.has(t.mockId);
      if (t.kind === "collection") return collections.has(t.collectionId);
      if (t.kind === "table" || t.kind === "diagram" || t.kind === "redis" || t.kind === "mongo" || t.kind === "dynamo" || t.kind === "firestore" || t.kind === "search") return targets.has(t.connection);
      return true;
    });
    const activeTab = tabs.some((t) => t.id === get().activeTab) ? get().activeTab : (tabs[tabs.length - 1]?.id ?? "home");
    set({ workspace, tabs: tabs.length ? tabs : [{ id: "home", kind: "home" }], activeTab: tabs.length ? activeTab : "home" });
  },

  async updateSettings(patch) {
    const settings = await call<Settings>("settings.update", { settings: patch });
    applyTheme(settings.theme);
    applyFonts(settings);
    set({ settings });
  },

  openTab(input) {
    const key = tabKey(input);
    const existing = key ? get().tabs.find((t) => tabKey(t) === key) : undefined;
    if (existing) {
      set({ activeTab: existing.id });
      return;
    }
    const id = input.id ?? key ?? `tab-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
    const tab = { ...input, id } as Tab;
    set((s) => ({ tabs: [...s.tabs, tab], activeTab: id }));
  },

  closeTab(id) {
    set((s) => {
      const idx = s.tabs.findIndex((t) => t.id === id);
      if (idx < 0) return s;
      const tabs = s.tabs.filter((t) => t.id !== id);
      const dirtyTabs = { ...s.dirtyTabs };
      delete dirtyTabs[id];
      let activeTab = s.activeTab;
      if (activeTab === id) activeTab = tabs[Math.min(idx, tabs.length - 1)]?.id ?? "";
      if (!tabs.length) return { tabs: [{ id: "home", kind: "home" } as Tab], activeTab: "home", dirtyTabs };
      return { tabs, activeTab, dirtyTabs };
    });
  },

  setActive(id) {
    set({ activeTab: id });
  },

  updateTab(id, patch) {
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? ({ ...t, ...patch } as Tab) : t)) }));
  },

  setDirty(id, dirty) {
    if (!!get().dirtyTabs[id] === dirty) return;
    set((s) => ({ dirtyTabs: { ...s.dirtyTabs, [id]: dirty } }));
  },

  toast(kind, title, body) {
    const id = ++toastId;
    set((s) => ({ toasts: [...s.toasts.slice(-4), { id, kind, title, body }] }));
    setTimeout(() => get().dismissToast(id), kind === "error" ? 9000 : 4500);
  },

  dismissToast(id) {
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  },

  async loadSchemas(connection, force) {
    const cur = get().schemas[connection];
    if (Array.isArray(cur) && !force) return cur;
    set((s) => ({ schemas: { ...s.schemas, [connection]: "loading" } }));
    try {
      const list = await call<string[]>("db.schemas", { connection });
      set((s) => ({ schemas: { ...s.schemas, [connection]: list } }));
      return list;
    } catch (e) {
      set((s) => ({ schemas: { ...s.schemas, [connection]: { error: errorMessage(e) } } }));
      return [];
    }
  },

  async loadTables(connection, schema, force) {
    const key = `${connection}|${schema}`;
    if (Array.isArray(get().tables[key]) && !force) return;
    if (!force) set((s) => ({ tables: { ...s.tables, [key]: "loading" } }));
    try {
      const list = await call<TableInfo[]>("db.tables", { connection, schema });
      set((s) => ({ tables: { ...s.tables, [key]: list } }));
    } catch (e) {
      set((s) => ({ tables: { ...s.tables, [key]: { error: errorMessage(e) } } }));
    }
  },

  set(patch) {
    set(patch);
  },

  setRail(panel) {
    try {
      localStorage.setItem("twinbase.rail", panel);
    } catch {
      /* storage unavailable */
    }
    set({ rail: panel });
  },

  openDialog(d) {
    set({ dialog: d });
  },

  closeDialog() {
    set({ dialog: null });
  },
}));

let refreshTimer: ReturnType<typeof setTimeout> | undefined;
function refreshSoon() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => useStore.getState().refreshWorkspace().catch(() => undefined), 120);
}

function handleEvent(e: CoreEvent) {
  const s = useStore.getState();
  switch (e.type) {
    case "activity": {
      const { type: _t, ...a } = e;
      useStore.setState((st) => ({ activity: [...st.activity.slice(-999), a] }));
      // Mutations made elsewhere (agents, CLI) change the workspace snapshot.
      if (a.ok && /^(mock|connection|collection|query|bundle|workspace)\./.test(a.action)) refreshSoon();
      break;
    }
    case "mock_status":
      useStore.setState((st) => ({
        workspace: st.workspace && {
          ...st.workspace,
          mocks: st.workspace.mocks.map((m) => (m.id === e.id ? { ...m, running: e.running, port: e.port, error: e.error } : m)),
        },
      }));
      if (e.error) s.toast("error", "Mock server failed", e.error);
      break;
    case "mock_traffic":
      trafficListeners.forEach((fn) => fn(e.id, e.event));
      break;
    case "connection_status":
      useStore.setState((st) => ({
        workspace: st.workspace && {
          ...st.workspace,
          connected: e.connected ? [...new Set([...st.workspace.connected, e.id])] : st.workspace.connected.filter((c) => c !== e.id),
        },
      }));
      break;
    case "schema_changed": {
      const prefix = `${e.connection}|`;
      useStore.setState((st) => ({ schemaVersion: { ...st.schemaVersion, [e.connection]: (st.schemaVersion[e.connection] ?? 0) + 1 } }));
      for (const key of Object.keys(s.tables)) if (key.startsWith(prefix)) s.loadTables(e.connection, key.slice(prefix.length), true);
      if (Array.isArray(s.schemas[e.connection])) s.loadSchemas(e.connection, true);
      break;
    }
    case "data_changed":
      useStore.setState((st) => ({ dataVersion: { ...st.dataVersion, [e.connection]: (st.dataVersion[e.connection] ?? 0) + 1 } }));
      break;
    case "workspace_changed":
      refreshSoon();
      break;
    case "mcp_client":
      useStore.setState((st) => ({ agents: e.connected ? [...new Set([...st.agents, e.name])] : st.agents.filter((a) => a !== e.name) }));
      if (e.connected) s.toast("info", `${e.name} connected`, "An AI agent is now connected over MCP. Its actions appear in the Activity panel.");
      break;
  }
}

/** Open (or create) a workspace folder and reload everything for it. */
export async function switchWorkspace(path: string, name?: string) {
  await call("workspace.open", { path, name });
  try {
    localStorage.setItem("twinbase.started", "1");
  } catch {
    /* storage unavailable */
  }
  useStore.setState({ tabs: [{ id: "home", kind: "home" }], activeTab: "home", tables: {}, schemas: {}, dirtyTabs: {}, showStart: false, dialog: null });
  await useStore.getState().init();
}

/** Open the Bootcamp: switch to its workspace (lab servers live there) and show the course. */
export async function openBootcamp(lessonId?: string) {
  if (!useStore.getState().bootcamp?.active) {
    await call("bootcamp.open");
    try {
      localStorage.setItem("twinbase.started", "1");
    } catch {
      /* storage unavailable */
    }
    useStore.setState({ tabs: [{ id: "home", kind: "home" }], activeTab: "home", tables: {}, schemas: {}, dirtyTabs: {}, showStart: false, dialog: null });
    await useStore.getState().init();
  }
  const s = useStore.getState();
  s.openTab({ kind: "bootcamp" });
  if (lessonId) s.openTab({ kind: "lesson", lessonId });
}

/** Remember that the user has chosen a workspace, so the start page does not come back. */
export function dismissStart() {
  try {
    localStorage.setItem("twinbase.started", "1");
  } catch {
    /* storage unavailable */
  }
  useStore.setState({ showStart: false });
}

/** All targets (saved connections + mock servers) for pickers and the explorer. */
export function useTargets(): Target[] {
  const ws = useStore((s) => s.workspace);
  if (!ws) return [];
  return [
    ...ws.mocks.map((m) => ({ id: m.connection_id, name: m.name, kind: m.kind as DbKind, isMock: true, readOnly: false })),
    ...ws.connections.map((c) => ({ id: c.id, name: c.name, kind: c.kind, isMock: false, color: c.color, readOnly: c.read_only })),
  ];
}

export function targetInfo(id: string | undefined): Target | undefined {
  const ws = useStore.getState().workspace;
  if (!ws || !id) return undefined;
  const m = ws.mocks.find((x) => x.connection_id === id);
  if (m) return { id, name: m.name, kind: m.kind, isMock: true, readOnly: false };
  const c = ws.connections.find((x) => x.id === id);
  return c && { id, name: c.name, kind: c.kind, isMock: false, color: c.color, readOnly: c.read_only };
}

export function defaultSchema(kind: DbKind, connection: string): string {
  const ws = useStore.getState().workspace;
  if (kind === "postgres") return "public";
  if (kind === "sqlite") return "main";
  if (kind === "mssql") return "dbo";
  // Spanner's default schema has the empty name.
  if (kind === "spanner") return "";
  const m = ws?.mocks.find((x) => x.connection_id === connection);
  if (m) return m.database;
  const db = ws?.connections.find((c) => c.id === connection)?.database ?? "";
  // ClickHouse: the connection's database, else the server's default one. BigQuery: the dataset
  // after the project (project.dataset).
  if (kind === "bigquery") return db.includes(".") ? db.slice(db.indexOf(".") + 1) : "";
  // Oracle: the connected user's schema (upper case unless quoted when created).
  if (kind === "oracle") return (ws?.connections.find((c) => c.id === connection)?.user ?? "").toUpperCase();
  return kind === "clickhouse" ? db || "default" : db;
}

/** Run an async action, reporting failures as a toast. Returns undefined on failure. */
export async function attempt<T>(title: string, fn: () => Promise<T>): Promise<T | undefined> {
  try {
    return await fn();
  } catch (e) {
    useStore.getState().toast("error", title, errorMessage(e));
    return undefined;
  }
}
