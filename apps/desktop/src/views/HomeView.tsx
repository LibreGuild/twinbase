// Start page: quick actions, schema templates and recent activity.

import { ArrowRight, Blocks, BookMarked, Bot, GraduationCap, FileDown, FileUp, GitCompare, KanbanSquare, Newspaper, Pencil, Plug, Server, ShoppingCart } from "lucide-react";
import { useState, type ReactNode } from "react";
import agentArt from "../assets/agent.webp";
import heroArt from "../assets/hero.webp";
import bootcampArt from "../assets/badges/graduate.webp";
import { Markdown } from "../components/Markdown";
import { Dialog, ErrorBox, Field, KindIcon, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { createTableSql, type SqlKind } from "../lib/ddlgen";
import { CONSOLE_KINDS, consoleStart, kindLabel } from "../lib/format";
import { TEMPLATES, type SchemaTemplate } from "../lib/templates";
import { attempt, openBootcamp, useStore } from "../lib/store";
import type { MockDef, MockStatus, SnapshotInfo } from "../lib/types";

const TEMPLATE_ICONS: Record<string, ReactNode> = {
  blog: <Newspaper size={18} />,
  shop: <ShoppingCart size={18} />,
  saas: <Blocks size={18} />,
  tasks: <KanbanSquare size={18} />,
};

export function HomeView() {
  const ws = useStore((s) => s.workspace);
  const openDialog = useStore((s) => s.openDialog);
  const openTab = useStore((s) => s.openTab);
  const setRail = useStore((s) => s.setRail);
  const inBootcamp = useStore((s) => !!s.bootcamp?.active);
  const snapshots = useAsync(() => call<SnapshotInfo[]>("schema.snapshot.list"), [ws?.path]);
  const [template, setTemplate] = useState<SchemaTemplate | null>(null);
  if (!ws) return null;
  const running = ws.mocks.filter((m) => m.running);
  const recent = [...running.map((m) => ({ kind: "mock" as const, m })), ...ws.connections.slice(0, 6).map((c) => ({ kind: "conn" as const, c }))];

  return (
    <div className="pane">
      <div className="page" style={{ maxWidth: 1080 }}>
        <section className="hero">
          <div>
            <span className="eyebrow">Workspace · {ws.name}</span>
            <h1>Your databases, and their twins.</h1>
            <p>
              Run mock PostgreSQL, MySQL, SQL Server, Redis, MongoDB, DynamoDB and Firestore servers that your app reaches with its normal drivers and SDKs, browse and change real databases safely, and let AI agents
              help — every change visible here.
            </p>
            <div className="row wrap">
              <button className="btn primary lg" onClick={() => openDialog({ kind: "mock", mockKind: "postgres" })}>
                <Server size={15} /> New mock server
              </button>
              <button className="btn lg" onClick={() => openDialog({ kind: "connection" })}>
                <Plug size={15} /> Connect a database
              </button>
            </div>
            <button className="btn ghost sm" style={{ marginTop: 10, marginLeft: -8 }} onClick={() => openDialog({ kind: "import" })}>
              <FileUp size={13} /> or import a teammate's bundle
            </button>
          </div>
          <img className="art" src={heroArt} alt="" draggable={false} />
        </section>

        <div className="stat-row">
          <div className="stat" onClick={() => setRail("explorer")} role="button" tabIndex={0}>
            <div className="n">
              {running.length}
              <span className="faint" style={{ fontSize: 14, fontWeight: 500 }}>
                {" "}
                / {ws.mocks.length}
              </span>
            </div>
            <div className="l">
              <Server size={13} /> mock servers running
            </div>
          </div>
          <div className="stat" onClick={() => setRail("explorer")} role="button" tabIndex={0}>
            <div className="n">{ws.connections.length}</div>
            <div className="l">
              <Plug size={13} /> saved connections
            </div>
          </div>
          <div className="stat" onClick={() => setRail("queries")} role="button" tabIndex={0}>
            <div className="n">{ws.queries.length}</div>
            <div className="l">
              <BookMarked size={13} /> saved queries
            </div>
          </div>
          <div className="stat" onClick={() => openTab({ kind: "schema" })} role="button" tabIndex={0}>
            <div className="n">{snapshots.data?.length ?? "–"}</div>
            <div className="l">
              <GitCompare size={13} /> schema snapshots
            </div>
          </div>
        </div>

        <WorkspaceReadme />

        {recent.length > 0 && (
          <>
            <div className="section-head">
              <h2>Jump back in</h2>
            </div>
            <div className="cards">
              {recent.map((r) =>
                r.kind === "mock" ? (
                  <div key={r.m.id} className="card action" onClick={() => openTab({ kind: "mock", mockId: r.m.id })}>
                    <span className={`icon-tile ${r.m.kind === "postgres" ? "pg" : r.m.kind}`}>
                      <KindIcon kind={r.m.kind} size={18} />
                    </span>
                    <div style={{ minWidth: 0 }}>
                      <div className="card-title ellipsis">{r.m.name}</div>
                      <div className="card-desc mono">
                        {kindLabel(r.m.kind)} mock · :{r.m.port}
                      </div>
                    </div>
                  </div>
                ) : (
                  <div
                    key={r.c.id}
                    className="card action"
                    onClick={() => (r.c.kind === "redis" ? openTab({ kind: "redis", connection: r.c.id }) : openTab({ kind: "query", connection: r.c.id, sql: consoleStart(r.c.kind), title: CONSOLE_KINDS.includes(r.c.kind) ? `${r.c.name} console` : r.c.name }))}
                  >
                    <span className="icon-tile neutral">
                      <KindIcon kind={r.c.kind} size={18} />
                    </span>
                    <div style={{ minWidth: 0 }}>
                      <div className="card-title ellipsis">{r.c.name}</div>
                      <div className="card-desc ellipsis">{kindLabel(r.c.kind)}</div>
                    </div>
                  </div>
                ),
              )}
            </div>
          </>
        )}

        <div className="section-head">
          <h2>Prototype from a template</h2>
          <span className="sub">A mock server with related tables and realistic rows, ready for your backend.</span>
        </div>
        <div className="cards">
          {TEMPLATES.map((t) => (
            <div key={t.id} className="card action" onClick={() => setTemplate(t)}>
              <span className="icon-tile">{TEMPLATE_ICONS[t.id] ?? <Server size={18} />}</span>
              <div>
                <div className="card-title">{t.name}</div>
                <div className="card-desc">{t.description}</div>
              </div>
            </div>
          ))}
        </div>

        <div className="section-head">
          <h2>Learn databases</h2>
          <span className="sub">Lessons, quizzes and hands-on labs, with XP and badges.</span>
        </div>
        <div className="banner bootcamp-banner">
          <img src={bootcampArt} alt="" draggable={false} />
          <div>
            <h3>{inBootcamp ? "You are in the Bootcamp workspace" : "TwinBase Bootcamp"}</h3>
            <p>From your first SELECT to indexes, transactions, Redis caching, sharding and vector search. Labs run on mock servers in a workspace of their own, so your projects stay untouched.</p>
          </div>
          <button className="btn primary" onClick={() => attempt("Could not open the Bootcamp", () => openBootcamp())}>
            <GraduationCap size={14} /> {inBootcamp ? "Open lessons" : "Start learning"} <ArrowRight size={14} />
          </button>
        </div>

        <div className="section-head">
          <h2>Work with AI agents</h2>
        </div>
        <div className="banner">
          <img src={agentArt} alt="" draggable={false} />
          <div>
            <h3>Let Claude Code, Codex or Gemini CLI drive TwinBase</h3>
            <p>Agents can design schemas, run SQL, edit rows, inject faults and manage mock servers over MCP. Real databases stay read-only for them unless you allow writes.</p>
          </div>
          <button className="btn primary" onClick={() => openDialog({ kind: "settings", section: "agents" })}>
            <Bot size={14} /> Set up an agent <ArrowRight size={14} />
          </button>
        </div>

        <div className="section-head">
          <h2>Share with your team</h2>
          <span className="sub">Commit the workspace folder with your code, or send a single file.</span>
        </div>
        <div className="row wrap">
          <button className="btn" onClick={() => openDialog({ kind: "export" })}>
            <FileDown size={14} /> Export mocks and queries…
          </button>
          <button className="btn ghost" onClick={() => useStore.getState().set({ showStart: true })}>
            Switch workspace…
          </button>
        </div>
      </div>
      {template && <TemplateDialog template={template} onClose={() => setTemplate(null)} />}
    </div>
  );
}

/** The workspace README (README.md in the folder), shown when it exists. */
function WorkspaceReadme() {
  const ws = useStore((s) => s.workspace);
  const openTab = useStore((s) => s.openTab);
  const readme = useAsync(() => call<{ docs: string }>("docs.get", {}).then((r) => r.docs), [ws]);
  return (
    <>
      <div className="section-head">
        <h2>About this workspace</h2>
        <span className="sub">README.md in the workspace folder.</span>
        <button className="btn sm ghost" style={{ marginLeft: "auto" }} onClick={() => openTab({ kind: "docs" })}>
          <Pencil size={13} /> {readme.data?.trim() ? "Edit" : "Write a README"}
        </button>
      </div>
      {readme.data?.trim() ? (
        <Markdown source={readme.data} className="card readme-card" />
      ) : (
        <div className="card readme-card faint small">No README yet. Describe what this workspace is for so teammates (and AI agents) know where to start.</div>
      )}
    </>
  );
}

function TemplateDialog({ template, onClose }: { template: SchemaTemplate; onClose: () => void }) {
  const openTab = useStore((s) => s.openTab);
  const refresh = useStore((s) => s.refreshWorkspace);
  const toast = useStore((s) => s.toast);
  const [kind, setKind] = useState<"postgres" | "mysql">("postgres");
  const [name, setName] = useState(`${template.name} DB`);
  const [withData, setWithData] = useState(true);
  const [progress, setProgress] = useState<string>();
  const [error, setError] = useState<string>();
  const tables = template.tables(kind as SqlKind);

  const create = async () => {
    setError(undefined);
    try {
      setProgress("Creating mock server…");
      const mock = await call<MockDef>("mock.save", { mock: { id: "", name, kind, port: 0, host: "127.0.0.1", auth: { mode: "any" }, database: "app", persist: true, auto_start: false } });
      await call("mock.start", { id: mock.id });
      const connection = `mock:${mock.id}`;
      setProgress("Creating tables…");
      await call("db.query", { connection, sql: tables.map((t) => createTableSql(kind, t.table)).join("\n\n") });
      if (withData) {
        for (const t of tables) {
          setProgress(`Generating rows for ${t.table.name}…`);
          await call("db.generate", { connection, table: t.table.name, count: t.rows });
        }
      }
      setProgress("Saving the seed…");
      await call("mock.seed.capture", { id: mock.id });
      await refresh();
      const status = useStore.getState().workspace?.mocks.find((m: MockStatus) => m.id === mock.id);
      toast("success", `${template.name} is ready`, status ? `${kindLabel(kind)} on 127.0.0.1:${status.port}` : undefined);
      openTab({ kind: "diagram", connection, schema: kind === "postgres" ? "public" : "app" });
      onClose();
    } catch (e) {
      setError(errorMessage(e));
      setProgress(undefined);
    }
  };

  return (
    <Dialog
      title={`${template.name} template`}
      onClose={onClose}
      footer={
        <>
          {progress && (
            <span className="row muted small grow">
              <Spinner /> {progress}
            </span>
          )}
          <button className="btn" onClick={onClose} disabled={!!progress}>
            Cancel
          </button>
          <button className="btn primary" onClick={create} disabled={!!progress || !name.trim()}>
            Create
          </button>
        </>
      }
    >
      <p className="muted" style={{ margin: 0 }}>
        {template.description}
      </p>
      <div className="grid-2">
        <Field label="Mock server name">
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label="Database">
          <div className="seg">
            {(["postgres", "mysql"] as const).map((k) => (
              <button key={k} className={kind === k ? "on" : ""} onClick={() => setKind(k)}>
                {kindLabel(k)}
              </button>
            ))}
          </div>
        </Field>
      </div>
      <label className="check">
        <input type="checkbox" checked={withData} onChange={(e) => setWithData(e.target.checked)} /> Fill with sample data
      </label>
      <div className="small muted">
        Tables: {tables.map((t) => t.table.name).join(", ")}
      </div>
      <ErrorBox error={error} />
    </Dialog>
  );
}
