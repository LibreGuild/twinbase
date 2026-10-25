// Query collection: documentation, saved queries and scenario runs (expectations as tests).

import { CheckCircle2, FileText, FlaskConical, Pencil, Play, Plus, Trash2, XCircle } from "lucide-react";
import { useState } from "react";
import { ConfirmDialog, Empty, Spinner } from "../components/ui";
import { call } from "../lib/api";
import { duration, kindLabel, plural } from "../lib/format";
import { renderMarkdown } from "../lib/markdown";
import { attempt, useStore, useTargets, type Tab } from "../lib/store";
import type { Expectation, ScenarioResult } from "../lib/types";

type CollectionTab = Extract<Tab, { kind: "collection" }>;

export function expectationText(e?: Expectation): string {
  if (!e) return "";
  switch (e.kind) {
    case "rows":
      return `returns ${e.count} row${e.count === 1 ? "" : "s"}`;
    case "empty":
      return "returns no rows";
    case "succeeds":
      return "succeeds";
    case "fails":
      return e.code ? `fails with ${e.code}` : "fails";
  }
}

export function CollectionView({ tab }: { tab: CollectionTab }) {
  const ws = useStore((s) => s.workspace);
  const openTab = useStore((s) => s.openTab);
  const openDialog = useStore((s) => s.openDialog);
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const [connection, setConnection] = useState("");
  const [results, setResults] = useState<ScenarioResult[] | null>(null);
  const [running, setRunning] = useState(false);
  const [remove, setRemove] = useState<{ collection: string; id: string; name: string } | "collection" | null>(null);
  const collection = ws?.collections.find((c) => c.id === tab.collectionId);
  if (!ws || !collection) return <Empty title="This collection no longer exists" />;
  const queries = ws.queries.filter((q) => q.collection === collection.id);
  const withExpect = queries.filter((q) => q.expect).length;

  const runAll = async () => {
    setRunning(true);
    const r = await attempt("Scenario run failed", () => call<ScenarioResult[]>("scenario.run", { collection: collection.id, connection: connection || undefined }));
    setRunning(false);
    if (r) setResults(r);
  };
  const resultFor = (id: string) => results?.find((r) => r.id === id);
  const passed = results?.filter((r) => r.passed).length ?? 0;

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <FileText size={15} className="muted" /> {collection.name}
        </div>
        <span className="badge">{plural(queries.length, "query", "queries")}</span>
        <div className="grow" />
        <select className="select sm" value={connection} onChange={(e) => setConnection(e.target.value)} aria-label="Run against" title="Connection for queries that have none saved">
          <option value="">Each query's own connection</option>
          {targets.map((t) => (
            <option key={t.id} value={t.id}>
              {t.name} · {kindLabel(t.kind)}
            </option>
          ))}
        </select>
        <button className="btn sm primary" disabled={!queries.length || running} onClick={runAll} title="Run every query and check its expectation">
          {running ? <Spinner size={12} /> : <FlaskConical size={13} />} Run scenarios
        </button>
        <button className="btn sm ghost" onClick={() => openDialog({ kind: "collection", collectionId: collection.id })}>
          <Pencil size={13} /> Edit
        </button>
        <button className="btn sm ghost danger" onClick={() => setRemove("collection")} aria-label="Delete collection">
          <Trash2 size={13} />
        </button>
      </div>
      <div className="pane">
        <div className="page">
          {collection.docs ? <div className="markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(collection.docs) }} /> : <p className="faint">No description. Use Edit to document what these queries are for.</p>}
          {results && (
            <div className={`note-box row`} style={{ margin: "14px 0", borderColor: passed === results.length ? "var(--success-soft)" : "var(--danger-soft)" }}>
              {passed === results.length ? <CheckCircle2 size={16} style={{ color: "var(--success)" }} /> : <XCircle size={16} style={{ color: "var(--danger)" }} />}
              <strong>
                {passed} of {results.length} passed
              </strong>
              <span className="faint">({withExpect} with expectations; others pass when they succeed)</span>
            </div>
          )}
          <div className="row" style={{ margin: "18px 0 8px" }}>
            <h2 style={{ margin: 0 }} className="grow">
              Queries
            </h2>
            <button className="btn sm" onClick={() => openTab({ kind: "query", sql: "", title: "New query", connection: connection || undefined })}>
              <Plus size={13} /> New query
            </button>
          </div>
          {queries.length === 0 ? (
            <div className="faint">No saved queries yet. Write one in the SQL editor and use “Save as…”.</div>
          ) : (
            <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
              {queries.map((q) => {
                const r = resultFor(q.id);
                return (
                  <div key={q.id} className="card" style={{ padding: "10px 12px" }}>
                    <div className="row">
                      {r && (r.passed ? <CheckCircle2 size={15} style={{ color: "var(--success)", flex: "none" }} /> : <XCircle size={15} style={{ color: "var(--danger)", flex: "none" }} />)}
                      <a
                        href="#"
                        style={{ fontWeight: 600 }}
                        className="ellipsis"
                        onClick={(e) => {
                          e.preventDefault();
                          openTab({ kind: "query", sql: q.sql, title: q.name, connection: q.connection, saved: { collection: q.collection, id: q.id } });
                        }}
                      >
                        {q.name}
                      </a>
                      {q.expect && <span className="badge info">{expectationText(q.expect)}</span>}
                      {q.tags.map((t) => (
                        <span key={t} className="badge">
                          {t}
                        </span>
                      ))}
                      <span className="grow" />
                      {r && <span className="faint xs">{duration(r.elapsed_ms)}</span>}
                      <button className="icon-btn sm" title="Open" onClick={() => openTab({ kind: "query", sql: q.sql, title: q.name, connection: q.connection, saved: { collection: q.collection, id: q.id } })} aria-label="Open query">
                        <Play size={12} />
                      </button>
                      <button className="icon-btn sm" title="Delete" onClick={() => setRemove({ collection: q.collection, id: q.id, name: q.name })} aria-label="Delete query">
                        <Trash2 size={12} />
                      </button>
                    </div>
                    <code className="faint xs ellipsis" style={{ display: "block", marginTop: 4 }}>
                      {q.sql.replace(/\s+/g, " ")}
                    </code>
                    {q.docs && <div className="markdown small" style={{ marginTop: 6 }} dangerouslySetInnerHTML={{ __html: renderMarkdown(q.docs) }} />}
                    {r && !r.passed && (
                      <div className="xs" style={{ color: "var(--danger)", marginTop: 4 }}>
                        Got: {r.actual}
                        {r.error ? ` — ${r.error}` : ""}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </div>
      {remove && (
        <ConfirmDialog
          title={remove === "collection" ? `Delete ${collection.name}?` : `Delete ${remove.name}?`}
          body={remove === "collection" ? `The collection and its ${queries.length} saved queries are removed from the workspace.` : "The saved query is removed from the workspace."}
          confirmLabel="Delete"
          danger
          onClose={() => setRemove(null)}
          onConfirm={async () => {
            if (remove === "collection") await attempt("Delete failed", () => call("collection.delete", { id: collection.id }));
            else await attempt("Delete failed", () => call("query.delete", { collection: remove.collection, id: remove.id }));
          }}
        />
      )}
    </>
  );
}
