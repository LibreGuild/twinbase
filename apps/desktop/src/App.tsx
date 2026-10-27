import { lazy, Suspense, useEffect, useRef } from "react";
import { BootcampHome } from "./bootcamp/BootcampHome";
import { LessonPlayer } from "./bootcamp/LessonPlayer";
import { CommandPalette } from "./components/CommandPalette";
import { DocsView } from "./components/Docs";
import { Rail, TopBar } from "./components/Chrome";
import { ActivityPanel, TabBar } from "./components/Shell";
import { Sidebar } from "./components/Sidebar";
import { Empty, Resizer, restoredSize, Spinner, Toasts } from "./components/ui";
import { DialogHost } from "./dialogs/WorkDialogs";
import { runtime } from "./lib/api";
import { applyTheme, targetInfo, useStore, type Tab } from "./lib/store";
import { CollectionView } from "./views/CollectionView";
import { HomeView } from "./views/HomeView";
import { MockView } from "./views/MockView";
import { DynamoQueryView, DynamoTableView } from "./views/DynamoView";
import { FirestoreCollectionView, FirestoreQueryView } from "./views/FirestoreView";
import { SearchIndexView, SearchQueryView } from "./views/SearchView";
import { MongoCollectionView, MongoQueryView } from "./views/MongoView";
import { QueryView } from "./views/QueryView";
import { RedisConsoleView, RedisView } from "./views/RedisView";
import { SchemaView } from "./views/SchemaView";
import { StartPage } from "./views/StartPage";
import { TableView } from "./views/TableView";

// The diagram pulls in the graph libraries; load them on first use.
const DiagramView = lazy(() => import("./views/DiagramView").then((m) => ({ default: m.DiagramView })));

function TabContent({ tab }: { tab: Tab }) {
  switch (tab.kind) {
    case "home":
      return <HomeView />;
    case "table":
      return <TableView tab={tab} />;
    case "query": {
      // Query tabs on document stores are consoles in their own language.
      const kind = targetInfo(tab.connection)?.kind;
      if (kind === "mongodb") return <MongoQueryView tab={tab} />;
      if (kind === "dynamodb") return <DynamoQueryView tab={tab} />;
      if (kind === "firestore") return <FirestoreQueryView tab={tab} />;
      if (kind === "elasticsearch") return <SearchQueryView tab={tab} />;
      if (kind === "redis") return <RedisConsoleView tab={tab} />;
      return <QueryView tab={tab} />;
    }
    case "firestore":
      return <FirestoreCollectionView tab={tab} />;
    case "search":
      return <SearchIndexView tab={tab} />;
    case "dynamo":
      return <DynamoTableView tab={tab} />;
    case "mongo":
      return <MongoCollectionView tab={tab} />;
    case "diagram":
      return (
        <Suspense fallback={<Empty icon={<Spinner size={20} />} title="Loading diagram…" />}>
          <DiagramView tab={tab} />
        </Suspense>
      );
    case "redis":
      return <RedisView tab={tab} />;
    case "mock":
      return <MockView tab={tab} />;
    case "collection":
      return <CollectionView tab={tab} />;
    case "schema":
      return <SchemaView tab={tab} />;
    case "docs":
      return <DocsView target={tab.target} object={tab.object} />;
    case "bootcamp":
      return <BootcampHome />;
    case "lesson":
      return <LessonPlayer lessonId={tab.lessonId} tabId={tab.id} />;
  }
}

export function App() {
  const ready = useStore((s) => s.ready);
  const loadError = useStore((s) => s.loadError);
  const tabs = useStore((s) => s.tabs);
  const active = useStore((s) => s.activeTab);
  const showActivity = useStore((s) => s.showActivity);
  const showStart = useStore((s) => s.showStart);
  const rail = useStore((s) => s.rail);
  // Remount tab contents when the workspace changes (fresh scroll positions and state).
  const workspacePath = useStore((s) => s.workspace?.path ?? "");
  const init = useStore((s) => s.init);
  const sidebar = useRef<HTMLElement>(null);
  const activity = useRef<HTMLDivElement>(null);

  useEffect(() => {
    init();
  }, [init]);

  const theme = useStore((s) => s.settings?.theme);
  useEffect(() => {
    if (theme) applyTheme(theme);
  }, [theme]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      const s = useStore.getState();
      const k = e.key.toLowerCase();
      if (k === "k" || (k === "p" && e.shiftKey)) s.set({ palette: !s.palette });
      else if (k === "t") s.openTab({ kind: "query", sql: "", title: "New query" });
      else if (k === "w" && runtime.isTauri) {
        if (s.dirtyTabs[s.activeTab]) return;
        s.closeTab(s.activeTab);
      } else if (k === ",") s.openDialog({ kind: "settings" });
      else if (k === "j") s.set({ showActivity: !s.showActivity });
      else if (/^[1-9]$/.test(k) && !e.shiftKey) {
        const t = s.tabs[Number(k) - 1];
        if (t) s.setActive(t.id);
      } else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (!ready) {
    return (
      <div className="app">
        <TopBar />
        {loadError ? (
          <Empty title="TwinBase core is not available">
            <div className="error-box" style={{ maxWidth: 560 }}>
              {loadError}
            </div>
            <button className="btn" onClick={() => init()}>
              Retry
            </button>
          </Empty>
        ) : (
          <Empty icon={<Spinner size={22} />} title="Starting TwinBase…" />
        )}
      </div>
    );
  }

  return (
    <div className="app">
      <TopBar />
      <div className="app-main">
        <Rail />
        <aside className="sidebar" ref={sidebar} style={{ ["--sidebar-w" as string]: restoredSize("--sidebar-w") }}>
          <Sidebar panel={rail} />
        </aside>
        <Resizer axis="x" target={() => sidebar.current} variable="--sidebar-w" min={210} max={520} />
        <main className="workarea">
          <TabBar />
          <div className="content">
            {tabs.map((t) => (
              <section key={`${workspacePath}:${t.id}`} className="tab-panel" hidden={t.id !== active} role="tabpanel">
                <TabContent tab={t} />
              </section>
            ))}
          </div>
          {showActivity && (
            <>
              <Resizer axis="y" target={() => activity.current?.querySelector(".activity") ?? null} variable="--activity-h" min={90} max={600} invert />
              <div ref={activity} style={{ ["--activity-h" as string]: restoredSize("--activity-h") }}>
                <ActivityPanel />
              </div>
            </>
          )}
        </main>
      </div>
      {showStart && <StartPage />}
      <DialogHost />
      <CommandPalette />
      <Toasts />
    </div>
  );
}
