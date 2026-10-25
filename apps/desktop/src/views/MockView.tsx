// Mock server tab: status and controls, connection snippets, live traffic and the seed script.

import { Activity as ActivityIcon, Braces, Camera, Code2, FileText, KeyRound, Network, Pause, Pencil, Play, RotateCcw, Rows3, Save, Square, Terminal, Trash2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { SqlEditor } from "../components/SqlEditor";
import { MockFaults } from "./MockFaults";
import { CodeBlock, ConfirmDialog, CopyButton, Empty, ErrorBox, Spinner, useAsync } from "../components/ui";
import { call } from "../lib/api";
import { duration, kindLabel, timeOfDay } from "../lib/format";
import { attempt, onTraffic, useStore, type Tab } from "../lib/store";
import type { Snippet, TrafficEvent } from "../lib/types";

type MockTab = Extract<Tab, { kind: "mock" }>;

export function MockView({ tab }: { tab: MockTab }) {
  const mock = useStore((s) => s.workspace?.mocks.find((m) => m.id === tab.mockId));
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const updateTab = useStore((s) => s.updateTab);
  const toast = useStore((s) => s.toast);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<"reset" | "delete" | null>(null);
  const section = tab.section ?? "connect";

  if (!mock) return <Empty title="This mock server no longer exists" />;

  const act = async (method: string, label: string) => {
    setBusy(true);
    await attempt(label, () => call(method, { id: mock.id }));
    setBusy(false);
  };

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <span className={`status-dot${mock.running ? " on" : mock.error ? " err" : ""}`} />
          <span className="ellipsis" style={{ maxWidth: 260 }}>
            {mock.name}
          </span>
        </div>
        <span className="badge">{kindLabel(mock.kind)} mock</span>
        {mock.engine === "native" && (
          <span className="badge accent" title="A real PostgreSQL server from your installation runs behind this mock">
            real PostgreSQL
          </span>
        )}
        <span className="badge mono">
          {mock.host}:{mock.port}
        </span>
        {!mock.persist && <span className="badge warning" title="Data is discarded when the server stops">in-memory</span>}
        {mock.faults?.enabled && (
          <button className="badge warning" style={{ border: 0, cursor: "pointer" }} onClick={() => updateTab(tab.id, { section: "faults" })} title="Fault injection is active">
            faults on
          </button>
        )}
        <div className="grow" />
        {mock.running ? (
          <button className="btn sm" disabled={busy} onClick={() => act("mock.stop", "Could not stop the mock")}>
            <Square size={12} /> Stop
          </button>
        ) : (
          <button className="btn sm primary" disabled={busy} onClick={() => act("mock.start", "Could not start the mock")}>
            {busy ? <Spinner size={12} /> : <Play size={12} />} Start
          </button>
        )}
        <button className="btn sm ghost" onClick={() => setConfirm("reset")} title="Delete all data and reload the seed script">
          <RotateCcw size={13} /> Reset
        </button>
        <button className="btn sm ghost" onClick={() => openDialog({ kind: "mock", mock })}>
          <Pencil size={13} /> Settings
        </button>
        <button className="btn sm ghost danger" onClick={() => setConfirm("delete")}>
          <Trash2 size={13} />
        </button>
      </div>
      {mock.error && !mock.running && (
        <div style={{ padding: "8px 10px 0" }}>
          <ErrorBox error={mock.error} />
        </div>
      )}
      <div className="subtabs" role="tablist">
        {[
          ["connect", "Connect"],
          ["traffic", "Traffic"],
          ["faults", mock.faults?.enabled ? "Faults ●" : "Faults"],
          ["seed", mock.kind === "redis" ? "Seed commands" : mock.kind === "mongodb" ? "Seed documents" : mock.kind === "dynamodb" ? "Seed tables" : mock.kind === "firestore" ? "Seed documents" : "Seed SQL"],
        ].map(([id, label]) => (
          <button key={id} className={section === id ? "on" : ""} role="tab" aria-selected={section === id} onClick={() => updateTab(tab.id, { section: id })}>
            {label}
          </button>
        ))}
      </div>
      {section === "connect" && (
        <div className="pane">
          <div className="page">
            <div className="cards" style={{ marginBottom: 20 }}>
              {mock.kind === "firestore" ? (
                <div className="card action" onClick={() => openTab({ kind: "query", connection: mock.connection_id, sql: "collections", title: `${mock.name} console` })}>
                  <FileText size={18} className="kind-firestore" />
                  <div>
                    <div className="card-title">Console</div>
                    <div className="card-desc">get, query, count, set, update and delete documents.</div>
                  </div>
                </div>
              ) : mock.kind === "dynamodb" ? (
                <div className="card action" onClick={() => openTab({ kind: "query", connection: mock.connection_id, sql: "ListTables {}", title: `${mock.name} console` })}>
                  <Terminal size={18} className="kind-dynamo" />
                  <div>
                    <div className="card-title">Request console</div>
                    <div className="card-desc">Create tables and run any DynamoDB API operation.</div>
                  </div>
                </div>
              ) : mock.kind === "mongodb" ? (
                <div className="card action" onClick={() => openTab({ kind: "query", connection: mock.connection_id, sql: "show collections", title: `${mock.name} console` })}>
                  <Braces size={18} className="kind-mongo" />
                  <div>
                    <div className="card-title">Console</div>
                    <div className="card-desc">mongosh-style statements: find, aggregate, insert, indexes.</div>
                  </div>
                </div>
              ) : mock.kind === "redis" ? (
                <div className="card action" onClick={() => openTab({ kind: "redis", connection: mock.connection_id })}>
                  <KeyRound size={18} className="kind-redis" />
                  <div>
                    <div className="card-title">Browse keys</div>
                    <div className="card-desc">Edit values and run commands.</div>
                  </div>
                </div>
              ) : (
                <>
                  <div className="card action" onClick={() => openTab({ kind: "query", connection: mock.connection_id, sql: "", title: `${mock.name} SQL` })}>
                    <Code2 size={18} className="muted" />
                    <div>
                      <div className="card-title">SQL editor</div>
                      <div className="card-desc">Create tables, insert data, run queries.</div>
                    </div>
                  </div>
                  <div className="card action" onClick={() => openTab({ kind: "diagram", connection: mock.connection_id, schema: mock.kind === "postgres" ? "public" : mock.database })}>
                    <Network size={18} className="muted" />
                    <div>
                      <div className="card-title">Diagram</div>
                      <div className="card-desc">Design tables and relationships visually.</div>
                    </div>
                  </div>
                  <div className="card action" onClick={() => openDialog({ kind: "table-designer", connection: mock.connection_id, schema: mock.kind === "postgres" ? "public" : mock.database })}>
                    <Rows3 size={18} className="muted" />
                    <div>
                      <div className="card-title">New table</div>
                      <div className="card-desc">Columns, keys and references without SQL.</div>
                    </div>
                  </div>
                </>
              )}
            </div>
            {mock.running ? (
              <Snippets id={mock.id} />
            ) : (
              <div className="note-box">Start the server to connect applications to it. Any existing data is kept{mock.persist ? "" : " only while it runs"}.</div>
            )}
          </div>
        </div>
      )}
      {section === "traffic" && <Traffic id={mock.id} running={mock.running} />}
      {section === "seed" && <Seed id={mock.id} kind={mock.kind} />}
      {section === "faults" && <MockFaults mock={mock} />}
      {confirm === "reset" && (
        <ConfirmDialog
          title="Reset mock data?"
          body="All current data is deleted and the seed script runs again. Capture the current data as the seed first if you want to keep it."
          confirmLabel="Reset"
          danger
          onClose={() => setConfirm(null)}
          onConfirm={async () => {
            if (await attempt("Reset failed", () => call("mock.reset", { id: mock.id }))) toast("success", "Mock reset", mock.name);
          }}
        />
      )}
      {confirm === "delete" && (
        <ConfirmDialog
          title={`Delete ${mock.name}?`}
          body="The server stops and its definition, data and seed script are removed from the workspace."
          confirmLabel="Delete mock"
          danger
          onClose={() => setConfirm(null)}
          onConfirm={async () => {
            await attempt("Delete failed", () => call("mock.delete", { id: mock.id }));
          }}
        />
      )}
    </>
  );
}

function Snippets({ id }: { id: string }) {
  const snippets = useAsync(() => call<Snippet[]>("mock.snippets", { id }), [id]);
  const [active, setActive] = useState<string>("");
  const list = snippets.data ?? [];
  const current = list.find((s) => s.id === active) ?? list[0];
  return (
    <>
      <h2 style={{ marginTop: 0 }}>Connect your application</h2>
      <p>
        This is a real {""}
        <strong>wire-protocol</strong> server: use your usual driver or ORM with the values below. Only the host and port differ from production.
      </p>
      <ErrorBox error={snippets.error} />
      <div className="snippet-tabs" role="tablist">
        {list.map((s) => (
          <button key={s.id} className={current?.id === s.id ? "on" : ""} onClick={() => setActive(s.id)}>
            {s.label}
          </button>
        ))}
      </div>
      {current && <CodeBlock code={current.code} />}
    </>
  );
}

function Traffic({ id, running }: { id: string; running: boolean }) {
  const [events, setEvents] = useState<TrafficEvent[]>([]);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let live = true;
    call<TrafficEvent[]>("mock.traffic", { id, limit: 500 }).then((e) => live && setEvents(e), () => undefined);
    const off = onTraffic((mockId, e) => {
      if (mockId === id && !pausedRef.current) setEvents((list) => [...list.slice(-1999), e]);
    });
    return () => {
      live = false;
      off();
    };
  }, [id]);

  useEffect(() => {
    const el = listRef.current;
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight;
  }, [events]);

  const shown = useMemo(() => {
    const q = filter.toLowerCase();
    return events.filter((e) => (!errorsOnly || (e.kind === "query" && e.error)) && (!q || (e.kind === "query" ? e.text.toLowerCase().includes(q) : e.kind.includes(q))));
  }, [events, filter, errorsOnly]);

  const queries = events.filter((e) => e.kind === "query");
  const errors = queries.filter((e) => e.kind === "query" && e.error).length;
  const clients = new Set(events.filter((e) => e.kind === "connect").map((e) => e.connection)).size;

  return (
    <div style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
      <div className="toolbar">
        <input className="input sm" style={{ width: 220 }} placeholder="Filter statements" value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="Filter traffic" />
        <label className="check small">
          <input type="checkbox" checked={errorsOnly} onChange={(e) => setErrorsOnly(e.target.checked)} /> Errors only
        </label>
        <div className="grow" />
        <span className="faint xs">
          {queries.length} statements · {errors} errors · {clients} connections
        </span>
        <button className="btn sm ghost" onClick={() => setPaused((p) => !p)}>
          {paused ? <Play size={13} /> : <Pause size={13} />} {paused ? "Resume" : "Pause"}
        </button>
        <button className="btn sm ghost" onClick={() => setEvents([])}>
          Clear
        </button>
      </div>
      <div ref={listRef} style={{ flex: 1, overflow: "auto" }}>
        {shown.length === 0 ? (
          <Empty icon={<ActivityIcon size={24} />} title={running ? "Waiting for traffic" : "Server is stopped"}>
            Every statement your application sends to this mock shows up here live.
          </Empty>
        ) : (
          shown.map((e) => (
            <div key={e.id} className="traffic-row">
              <span className="t">{timeOfDay(e.at)}</span>
              <span className="faint xs">#{e.connection}</span>
              {e.kind === "query" ? (
                <span className={`q${e.error ? " err" : ""}`} title={e.error ? `${e.text}\n\n${e.error}` : e.text}>
                  {e.text.replace(/\s+/g, " ")}
                  {e.error && <span className="xs"> — {e.error}</span>}
                </span>
              ) : e.kind === "connect" ? (
                <span className="muted">
                  connected from {e.peer}
                  {e.user ? ` as ${e.user}` : ""}
                  {e.database ? ` to ${e.database}` : ""}
                </span>
              ) : (
                <span className="faint">disconnected</span>
              )}
              <span className="t" style={{ textAlign: "right" }}>
                {e.kind === "query" ? duration(e.duration_us / 1000) : ""}
              </span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

function Seed({ id, kind }: { id: string; kind: string }) {
  const seed = useAsync(() => call<{ sql: string }>("mock.seed.get", { id }), [id]);
  const [text, setText] = useState("");
  const toast = useStore((s) => s.toast);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setText(seed.data?.sql ?? "");
  }, [seed.data]);
  const dirty = text !== (seed.data?.sql ?? "");
  const save = async () => {
    if (await attempt("Could not save the seed", () => call("mock.seed.set", { id, sql: text }))) {
      toast("success", "Seed saved", "It runs whenever the mock is created fresh or reset.");
      seed.reload();
    }
  };
  const capture = async () => {
    setBusy(true);
    const r = await attempt("Could not capture data", () => call<{ sql: string }>("mock.seed.capture", { id }));
    setBusy(false);
    if (r) {
      setText(r.sql);
      seed.setData(r);
      toast("success", "Captured current data as the seed");
    }
  };
  return (
    <div style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
      <div className="toolbar">
        <span className="muted small grow">
          {kind === "redis" ? "One Redis command per line" : kind === "mongodb" ? "Collections, documents and indexes as Extended JSON" : kind === "dynamodb" ? "Tables (CreateTable definitions) with their items in DynamoDB JSON" : kind === "firestore" ? "Documents by path, as JSON" : "SQL"} that loads when the mock starts with empty data, and on every reset. It travels with exported bundles.
        </span>
        <button className="btn sm" disabled={busy} onClick={capture} title="Replace the seed with the current schema and data">
          {busy ? <Spinner size={12} /> : <Camera size={13} />} Capture current data
        </button>
        <CopyButton text={text} />
        <button className="btn sm primary" disabled={!dirty} onClick={save}>
          <Save size={13} /> Save
        </button>
      </div>
      <div style={{ flex: 1, minHeight: 0 }} className="editor-host">
        {seed.loading && !seed.data ? <Empty icon={<Spinner />} title="Loading…" /> : <SqlEditor
            value={text}
            kind={kind === "mysql" ? "mysql" : kind === "mssql" ? "mssql" : kind === "mongodb" ? "mongodb" : kind === "dynamodb" ? "dynamodb" : kind === "firestore" ? "firestore" : "postgres"}
            onChange={setText}
            onSave={save}
            placeholder={kind === "firestore" ? '{ "documents": { "users/alice": { "name": "Alice", "joined": { "__timestamp": "2024-01-01T00:00:00Z" } }, "users/alice/orders/o1": { "total": 25.5 } } }' : kind === "dynamodb" ? '{ "tables": [{ "TableName": "orders", "BillingMode": "PAY_PER_REQUEST", "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }], "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }], "Items": [{ "pk": { "S": "order#1" } }] }] }' : kind === "mongodb" ? '{ "collections": { "users": { "indexes": [{ "key": { "email": 1 }, "unique": true }], "documents": [{ "email": "ann@example.com" }] } } }' : "-- e.g. CREATE TABLE users (...); INSERT INTO users ..."}
          />}
      </div>
    </div>
  );
}
