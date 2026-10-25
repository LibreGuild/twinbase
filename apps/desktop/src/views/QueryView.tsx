// SQL editor tab: run statements, preview writes on a local twin, commit, save with docs.

import { AlertTriangle, BookmarkPlus, CheckCircle2, ClipboardCopy, Clock, Eye, FileText, Gauge, History, Network, Play, PlayCircle, Save, XCircle } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { DataGrid } from "../components/DataGrid";
import { PlanView } from "../components/PlanView";
import { PreviewPanel } from "../components/PreviewPanel";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { Dialog, Empty, Resizer, restoredSize, Spinner, useContextMenu } from "../components/ui";
import { call, CoreCallError, errorMessage } from "../lib/api";
import { cellText, duration, isReadOnlySql, kindLabel, plural, relativeTime, riskyStatement } from "../lib/format";
import { renderMarkdown } from "../lib/markdown";
import { completionSchema, useGraph } from "../lib/schema";
import { attempt, defaultSchema, targetInfo, useStore, useTargets, type Tab } from "../lib/store";
import type { HistoryEntry, PreviewResult, QueryPlan, QueryResult } from "../lib/types";

type QueryTab = Extract<Tab, { kind: "query" }>;

interface RunState {
  results: QueryResult[];
  error?: { message: string; code?: string; statement?: number };
  elapsed: number;
  at: number;
}

function toCsv(r: QueryResult): string {
  const esc = (s: string) => (/[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  return [r.columns.map((c) => esc(c.name)).join(","), ...r.rows.map((row) => row.map((v) => (v === null ? "" : esc(cellText(v)))).join(","))].join("\n");
}

function toMarkdown(r: QueryResult): string {
  // Backslashes first, so a cell ending in "\" cannot un-escape the next pipe.
  const esc = (s: string) => s.replace(/\\/g, "\\\\").replace(/\|/g, "\\|").replace(/\n/g, " ");
  return [
    `| ${r.columns.map((c) => esc(c.name)).join(" | ")} |`,
    `| ${r.columns.map(() => "---").join(" | ")} |`,
    ...r.rows.map((row) => `| ${row.map((v) => esc(cellText(v))).join(" | ")} |`),
  ].join("\n");
}

export function QueryView({ tab }: { tab: QueryTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const ws = useStore((s) => s.workspace);
  const settings = useStore((s) => s.settings);
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const openDialog = useStore((s) => s.openDialog);
  const toast = useStore((s) => s.toast);
  const editor = useRef<SqlEditorHandle>(null);
  const editorBox = useRef<HTMLDivElement>(null);
  const [sql, setSql] = useState(tab.sql);
  const [run, setRun] = useState<RunState | null>(null);
  const [running, setRunning] = useState(false);
  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [plan, setPlan] = useState<QueryPlan | null>(null);
  const [previewSql, setPreviewSql] = useState("");
  const [panel, setPanel] = useState<string>("messages");
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const [confirm, setConfirm] = useState<{ text: string; reason: string } | null>(null);
  const [showDocs, setShowDocs] = useState(false);
  const menu = useContextMenu();

  const connection = tab.connection && targets.some((t) => t.id === tab.connection) ? tab.connection : undefined;
  const target = targetInfo(connection);
  const schema = target ? defaultSchema(target.kind, target.id) : undefined;
  const { tables } = useGraph(connection, schema);
  const completion = useMemo(() => completionSchema(tables), [tables]);

  const saved = tab.saved ? ws?.queries.find((q) => q.collection === tab.saved!.collection && q.id === tab.saved!.id) : undefined;
  const dirty = saved ? saved.sql !== sql : false;
  useEffect(() => {
    setDirty(tab.id, dirty);
  }, [dirty, tab.id, setDirty]);

  // Keep the tab's SQL in the store so it survives re-renders and is used by "Save".
  useEffect(() => {
    const t = setTimeout(() => sql !== tab.sql && updateTab(tab.id, { sql }), 300);
    return () => clearTimeout(t);
  }, [sql, tab.id, tab.sql, updateTab]);

  // Saved query loaded from elsewhere (explorer) replaces the text.
  useEffect(() => {
    if (tab.sql !== sql && !dirty) setSql(tab.sql);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.sql]);

  const execute = async (text: string, skipConfirm = false) => {
    if (!connection) {
      toast("error", "Choose a connection first");
      return;
    }
    if (!text.trim()) return;
    const reason = riskyStatement(text);
    if (!skipConfirm && reason && settings?.confirm_destructive && !target?.isMock) {
      setConfirm({ text, reason });
      return;
    }
    setRunning(true);
    const started = performance.now();
    try {
      const results = await call<QueryResult[]>("db.query", { connection, sql: text });
      setRun({ results, elapsed: performance.now() - started, at: Date.now() });
      const firstRows = results.findIndex((r) => r.columns.length > 0);
      setPanel(firstRows >= 0 ? `r${firstRows}` : "messages");
    } catch (e) {
      const err = e instanceof CoreCallError ? e.error : { message: errorMessage(e) };
      setRun({ results: [], error: { message: [err.message, "detail" in err ? err.detail : undefined].filter(Boolean).join("\n"), code: "code" in err ? err.code : undefined, statement: "statement_index" in err ? err.statement_index : undefined }, elapsed: performance.now() - started, at: Date.now() });
      setPanel("messages");
    } finally {
      setRunning(false);
      if (panel === "history") loadHistory();
    }
  };

  const runPreview = async () => {
    if (!connection) return;
    const text = editor.current?.selectionOrAll() ?? sql;
    setRunning(true);
    try {
      const p = await call<PreviewResult>("db.preview", { connection, schema, sql: text });
      setPreview(p);
      setPreviewSql(text);
      setPanel("preview");
    } catch (e) {
      toast("error", "Preview failed", errorMessage(e));
    } finally {
      setRunning(false);
    }
  };

  const explain = async (analyze: boolean) => {
    if (!connection) return;
    // The selection when there is one, otherwise the statement at the cursor.
    const ed = editor.current;
    const selected = ed?.selectionOrAll() ?? sql;
    const text = ed && selected !== ed.getValue() ? selected : (ed?.currentStatement() ?? sql);
    if (!text.trim()) return;
    setRunning(true);
    try {
      setPlan(await call<QueryPlan>("db.explain", { connection, sql: text, analyze }));
      setPanel("plan");
    } catch (e) {
      toast("error", "Explain failed", errorMessage(e));
    } finally {
      setRunning(false);
    }
  };

  const commitPreview = async () => {
    if (!connection || !preview) return;
    const r = await attempt("Commit failed", () => call<{ affected: number }>("db.commit", { connection, sql: previewSql, expected_affected: preview.expected_affected }));
    if (r) {
      toast("success", "Committed", `${plural(r.affected, "row")} affected — matches the preview.`);
      setPreview(null);
      setPanel("messages");
      setRun({ results: [{ statement: previewSql, columns: [], rows: [], affected: r.affected, truncated: false, elapsed_ms: 0, message: "Committed in one transaction" }], elapsed: 0, at: Date.now() });
    }
  };

  const save = async () => {
    if (saved) {
      const r = await attempt("Could not save query", () => call("query.save", { query: { ...saved, sql } }));
      if (r) toast("success", "Query saved", saved.name);
    } else openDialog({ kind: "save-query", tabId: tab.id, sql, connection });
  };

  const loadHistory = async () => {
    if (!connection) return setHistory([]);
    setHistory((await attempt("Could not load history", () => call<HistoryEntry[]>("db.history", { connection }))) ?? []);
  };

  const results = run?.results ?? [];
  const readOnlyText = isReadOnlySql(sql);

  return (
    <div className="split-v">
      <div className="toolbar">
        <select
          className="select sm"
          style={{ maxWidth: 240 }}
          value={connection ?? ""}
          onChange={(e) => updateTab(tab.id, { connection: e.target.value || undefined })}
          aria-label="Connection"
        >
          <option value="">Choose connection…</option>
          {targets.map((t) => (
            <option key={t.id} value={t.id}>
              {t.isMock ? "◆ " : ""}
              {t.name} · {kindLabel(t.kind)}
            </option>
          ))}
        </select>
        <button className="btn sm primary" disabled={!connection || running} onClick={() => execute(editor.current?.selectionOrAll() ?? sql)} title="Run all or selection (⌘/Ctrl+Enter)">
          {running ? <Spinner size={13} /> : <Play size={13} />} Run
        </button>
        <button className="btn sm" disabled={!connection || running} onClick={() => execute(editor.current?.currentStatement() ?? sql)} title="Run statement at cursor (Shift+Enter)">
          <PlayCircle size={13} /> Current
        </button>
        <button
          className="btn sm"
          disabled={!connection || running || readOnlyText || target?.kind === "clickhouse" || target?.kind === "cassandra" || target?.kind === "spanner" || target?.kind === "bigquery" || target?.kind === "oracle"}
          onClick={runPreview}
          title={
            target?.kind === "clickhouse"
                ? "Not available for ClickHouse: mutations are asynchronous and have no transactions"
                : target?.kind === "cassandra"
                  ? "Not available for Cassandra: writes are upserts without transactions"
                  : target?.kind === "spanner" || target?.kind === "bigquery"
                    ? "Not available for Spanner and BigQuery yet: the local twin cannot run GoogleSQL"
                    : target?.kind === "oracle"
                      ? "Not available for Oracle yet: the local twin cannot run Oracle SQL"
                : "Run on a local copy and show the row-level diff, without changing the server"
          }
        >
          <Eye size={13} /> Preview
        </button>
        <button
          className="btn sm"
          disabled={!connection || running}
          title="Show the query plan for the statement at the cursor"
          onClick={(e) =>
            menu.open(
              e,
              [
                { label: "Explain", sub: "Planner estimates; nothing runs", icon: <Network size={14} />, onClick: () => explain(false) },
                { label: "Explain analyze", sub: "Runs the statement in a transaction that is rolled back", icon: <Gauge size={14} />, onClick: () => explain(true) },
              ],
              { anchor: e.currentTarget as HTMLElement },
            )
          }
        >
          <Network size={13} /> Explain
        </button>
        <div className="sep" />
        <button className="btn sm ghost" onClick={save} title="Save (⌘/Ctrl+S)">
          {saved ? <Save size={13} /> : <BookmarkPlus size={13} />} {saved ? "Save" : "Save as…"}
        </button>
        {saved && (
          <button className="btn sm ghost" onClick={() => openDialog({ kind: "save-query", tabId: tab.id, sql, connection, saved: tab.saved })} title="Edit name, docs and expectation">
            <FileText size={13} /> Details
          </button>
        )}
        {saved?.docs && (
          <button className={`btn sm ghost${showDocs ? " on" : ""}`} onClick={() => setShowDocs((v) => !v)}>
            Docs
          </button>
        )}
        <div className="grow" />
        {target?.readOnly && <span className="badge warning">read-only connection</span>}
        {target?.isMock && <span className="badge accent">mock</span>}
      </div>
      {showDocs && saved?.docs && <div className="markdown note-box" style={{ margin: "8px 10px 0", maxHeight: 180, overflow: "auto" }} dangerouslySetInnerHTML={{ __html: renderMarkdown(saved.docs) }} />}
      <div className="editor-host" ref={editorBox} style={{ height: "var(--editor-h, 40%)", flex: "none", ["--editor-h" as string]: restoredSize("--editor-h") }}>
        <SqlEditor
          ref={editor}
          value={sql}
          kind={target?.kind}
          schema={completion}
          onChange={setSql}
          onRun={() => execute(editor.current?.selectionOrAll() ?? sql)}
          onRunCurrent={() => execute(editor.current?.currentStatement() ?? sql)}
          onSave={save}
          placeholder="Write SQL. ⌘/Ctrl+Enter runs everything (or the selection), Shift+Enter runs the statement at the cursor."
        />
      </div>
      <HeightResizer target={() => editorBox.current} />
      <div className="result-tabs" role="tablist">
        {results.map((r, i) =>
          r.columns.length > 0 ? (
            <button key={i} className={panel === `r${i}` ? "on" : ""} onClick={() => setPanel(`r${i}`)}>
              Result {i + 1} <span className="faint xs">{r.rows.length}{r.truncated ? "+" : ""}</span>
            </button>
          ) : null,
        )}
        <button className={panel === "messages" ? "on" : ""} onClick={() => setPanel("messages")}>
          {run?.error ? <XCircle size={13} style={{ color: "var(--danger)" }} /> : null} Messages
        </button>
        {preview && (
          <button className={panel === "preview" ? "on" : ""} onClick={() => setPanel("preview")}>
            <Eye size={13} /> Preview
          </button>
        )}
        {plan && (
          <button className={panel === "plan" ? "on" : ""} onClick={() => setPanel("plan")}>
            <Network size={13} /> Plan
          </button>
        )}
        <button
          className={panel === "history" ? "on" : ""}
          onClick={() => {
            setPanel("history");
            loadHistory();
          }}
        >
          <History size={13} /> History
        </button>
        <span className="grow" />
        {run && <span className="faint xs" style={{ whiteSpace: "nowrap" }}>{duration(run.elapsed)} · {relativeTime(run.at)}</span>}
      </div>
      <div style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}>
        {panel.startsWith("r") && results[Number(panel.slice(1))] ? (
          <ResultGrid
            result={results[Number(panel.slice(1))]}
            onCopy={(e) =>
              menu.open(e, [
                { label: "Copy as CSV", onClick: () => navigator.clipboard.writeText(toCsv(results[Number(panel.slice(1))])) },
                { label: "Copy as JSON", onClick: () => {
                  const r = results[Number(panel.slice(1))];
                  navigator.clipboard.writeText(JSON.stringify(r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c.name, row[i]]))), null, 2));
                } },
                { label: "Copy as Markdown table", onClick: () => navigator.clipboard.writeText(toMarkdown(results[Number(panel.slice(1))])) },
              ])
            }
          />
        ) : panel === "preview" && preview ? (
          <div className="pane">
            <div className="row" style={{ padding: "10px 14px 0" }}>
              <span className="muted small grow">
                Previewed {preview.statements.length === 1 ? "1 statement" : `${preview.statements.length} statements`}. Committing runs the same SQL in one transaction and rolls back if the affected row count differs from the preview.
              </span>
              <button className="btn sm" onClick={() => setPreview(null)}>
                Discard
              </button>
              <button className="btn sm primary" disabled={!preview.ok || target?.readOnly} onClick={commitPreview}>
                <CheckCircle2 size={13} /> Commit
              </button>
            </div>
            <PreviewPanel preview={preview} />
          </div>
        ) : panel === "plan" && plan ? (
          <PlanView plan={plan} />
        ) : panel === "history" ? (
          <div className="pane">
            {history === null ? (
              <Empty icon={<Spinner />} title="Loading…" />
            ) : history.length === 0 ? (
              <Empty icon={<Clock size={22} />} title="No history yet">
                Statements you run on this connection appear here.
              </Empty>
            ) : (
              <table className="simple-table">
                <tbody>
                  {history.slice().reverse().map((h, i) => (
                    <tr key={i} style={{ cursor: "pointer" }} onClick={() => setSql(h.sql)} title="Load into editor">
                      <td style={{ width: 22 }}>{h.ok ? <CheckCircle2 size={13} style={{ color: "var(--success)" }} /> : <XCircle size={13} style={{ color: "var(--danger)" }} />}</td>
                      <td className="mono" style={{ maxWidth: 600 }}>
                        <div className="ellipsis">{h.sql.replace(/\s+/g, " ")}</div>
                        {h.error && <div className="xs" style={{ color: "var(--danger)" }}>{h.error}</div>}
                      </td>
                      <td className="faint xs" style={{ whiteSpace: "nowrap" }}>{duration(h.elapsed_ms)}</td>
                      <td className="faint xs" style={{ whiteSpace: "nowrap" }}>{relativeTime(h.at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        ) : (
          <div className="messages">
            {!run && <span className="faint">Run a query to see results here.</span>}
            {run?.results.map((r, i) => (
              <div key={i}>
                <span className="ok">✓</span> <span className="faint">[{i + 1}]</span> {r.statement.replace(/\s+/g, " ").slice(0, 160)}
                {" — "}
                {r.message ?? (r.columns.length ? `${plural(r.rows.length, "row")}${r.truncated ? " (truncated)" : ""}` : r.affected !== undefined && r.affected !== null ? `${plural(r.affected, "row")} affected` : "ok")}
                {r.elapsed_ms > 0 && <span className="faint"> · {duration(r.elapsed_ms)}</span>}
              </div>
            ))}
            {run?.error && (
              <div className="err" style={{ whiteSpace: "pre-wrap" }}>
                ✗ {run.error.statement !== undefined && run.error.statement !== null ? `[statement ${run.error.statement + 1}] ` : ""}
                {run.error.message}
                {run.error.code && <span className="faint"> (code {run.error.code})</span>}
                {run.results.length === 0 && run.error.statement ? <div className="faint">Earlier statements in this run were executed.</div> : null}
              </div>
            )}
          </div>
        )}
      </div>
      {confirm && (
        <Dialog
          title="This statement changes many rows"
          icon={<AlertTriangle size={18} style={{ color: "var(--warning)" }} />}
          onClose={() => setConfirm(null)}
          footer={
            <>
              <button className="btn" onClick={() => setConfirm(null)}>
                Cancel
              </button>
              <button
                className="btn"
                onClick={() => {
                  setConfirm(null);
                  runPreview();
                }}
              >
                <Eye size={14} /> Preview first
              </button>
              <button
                className="btn danger"
                onClick={() => {
                  const text = confirm.text;
                  setConfirm(null);
                  execute(text, true);
                }}
              >
                Run anyway
              </button>
            </>
          }
        >
          <p className="muted" style={{ margin: 0 }}>
            {confirm.reason} You are connected to <strong>{target?.name}</strong>, which is not a mock server.
          </p>
        </Dialog>
      )}
      {menu.node}
    </div>
  );
}

function ResultGrid({ result, onCopy }: { result: QueryResult; onCopy: (e: React.MouseEvent) => void }) {
  const columns = useMemo(() => result.columns.map((c) => ({ name: c.name, type: c.type_name })), [result]);
  return (
    <div className="grid-wrap">
      <DataGrid columns={columns} rows={result.rows} />
      <div className="grid-footer">
        <span>
          {plural(result.rows.length, "row")}
          {result.truncated ? " (row limit reached — add LIMIT or raise it in Settings)" : ""}
        </span>
        <span className="spacer" />
        <button className="btn sm ghost" onClick={onCopy}>
          <ClipboardCopy size={13} /> Copy…
        </button>
      </div>
    </div>
  );
}

function HeightResizer({ target }: { target: () => HTMLElement | null }) {
  return <Resizer axis="y" target={target} variable="--editor-h" min={80} max={2000} />;
}
