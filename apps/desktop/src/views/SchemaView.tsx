// Schema snapshots and diffs: compare snapshots and live connections, generate migrations.

import { ArrowRight, Camera, Code2, GitCompare, Minus, Pencil, Plus, Trash2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { CodeBlock, ConfirmDialog, Empty, ErrorBox, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { kindLabel, relativeTime } from "../lib/format";
import { attempt, useStore, useTargets, type Tab } from "../lib/store";
import type { SchemaChange, SchemaDiff, SchemaSource, SnapshotInfo, SqlDialect } from "../lib/types";

type SchemaTab = Extract<Tab, { kind: "schema" }>;

const encode = (s?: SchemaSource) => (!s ? "" : "snapshot" in s ? `snap:${s.snapshot}` : `live:${s.connection}`);
const decode = (v: string): SchemaSource | undefined => (v.startsWith("snap:") ? { snapshot: v.slice(5) } : v.startsWith("live:") ? { connection: v.slice(5) } : undefined);

function describe(c: SchemaChange): { sign: "+" | "−" | "~"; text: string } {
  switch (c.kind) {
    case "table_added":
      return { sign: "+", text: "new table" };
    case "table_removed":
      return { sign: "−", text: "table removed" };
    case "column_added":
      return { sign: "+", text: `column ${c.column} ${c.data_type}` };
    case "column_removed":
      return { sign: "−", text: `column ${c.column}` };
    case "column_changed":
      return { sign: "~", text: `column ${c.column}: ${c.details.join(", ")}` };
    case "primary_key_changed":
      return { sign: "~", text: `primary key (${c.from.join(", ") || "none"}) → (${c.to.join(", ") || "none"})` };
    case "foreign_key_added":
      return { sign: "+", text: `foreign key (${c.columns.join(", ")}) → ${c.ref_table}` };
    case "foreign_key_removed":
      return { sign: "−", text: `foreign key (${c.columns.join(", ")}) → ${c.ref_table}` };
    case "index_added":
      return { sign: "+", text: `${c.unique ? "unique " : ""}index ${c.name} (${c.columns.join(", ")})` };
    case "index_removed":
      return { sign: "−", text: `${c.unique ? "unique " : ""}index ${c.name} (${c.columns.join(", ")})` };
  }
}

export function SchemaView({ tab }: { tab: SchemaTab }) {
  const targets = useTargets().filter((t) => t.kind !== "redis");
  const wsVersion = useStore((s) => s.workspace);
  const openTab = useStore((s) => s.openTab);
  const toast = useStore((s) => s.toast);
  const snapshots = useAsync(() => call<SnapshotInfo[]>("schema.snapshot.list"), [wsVersion]);
  const [from, setFrom] = useState(encode(tab.from));
  const [to, setTo] = useState(encode(tab.to));
  const [dialect, setDialect] = useState<SqlDialect | "">("");
  const [result, setResult] = useState<SchemaDiff | null>(null);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [script, setScript] = useState<"up" | "down">("up");
  const [snapConn, setSnapConn] = useState(targets[0]?.id ?? "");
  const [snapLabel, setSnapLabel] = useState("");
  const [taking, setTaking] = useState(false);
  const [remove, setRemove] = useState<SnapshotInfo | null>(null);

  useEffect(() => {
    if (tab.from) setFrom(encode(tab.from));
    if (tab.to) setTo(encode(tab.to));
  }, [tab.from, tab.to]);
  useEffect(() => {
    if (!snapConn && targets[0]) setSnapConn(targets[0].id);
  }, [targets, snapConn]);

  const compare = async () => {
    const f = decode(from);
    const t = decode(to);
    if (!f || !t) return;
    setBusy(true);
    setError(undefined);
    try {
      setResult(await call<SchemaDiff>("schema.diff", { from: f, to: t, dialect: dialect || undefined }));
      setScript("up");
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setBusy(false);
    }
  };

  const take = async () => {
    setTaking(true);
    const r = await attempt("Snapshot failed", () => call<SnapshotInfo>("schema.snapshot.create", { connection: snapConn, label: snapLabel || undefined }));
    setTaking(false);
    if (r) {
      toast("success", "Snapshot saved", `${r.label} · ${r.tables} tables`);
      setSnapLabel("");
      snapshots.reload();
    }
  };

  const grouped = useMemo(() => {
    const m = new Map<string, SchemaChange[]>();
    for (const c of result?.changes ?? []) m.set(c.table, [...(m.get(c.table) ?? []), c]);
    return [...m.entries()];
  }, [result]);

  const fromLive = decode(from);
  const runOn = fromLive && "connection" in fromLive ? fromLive.connection : undefined;
  const sql = result ? (script === "up" ? result.up_sql : result.down_sql) : "";
  const sources = (
    <>
      <option value="">Choose…</option>
      {snapshots.data && snapshots.data.length > 0 && (
        <optgroup label="Snapshots">
          {snapshots.data.map((s) => (
            <option key={s.id} value={`snap:${s.id}`}>
              {s.label} ({s.tables} tables)
            </option>
          ))}
        </optgroup>
      )}
      <optgroup label="Live connections">
        {targets.map((t) => (
          <option key={t.id} value={`live:${t.id}`}>
            {t.name} · {kindLabel(t.kind)}
          </option>
        ))}
      </optgroup>
    </>
  );

  return (
    <div style={{ display: "flex", flex: 1, minHeight: 0 }}>
      <aside style={{ width: 300, borderRight: "1px solid var(--border)", display: "flex", flexDirection: "column", minHeight: 0, flex: "none" }}>
        <div style={{ padding: 12, borderBottom: "1px solid var(--border)", display: "flex", flexDirection: "column", gap: 8 }}>
          <strong className="row">
            <Camera size={14} /> Take a snapshot
          </strong>
          <select className="select sm" value={snapConn} onChange={(e) => setSnapConn(e.target.value)} aria-label="Connection to snapshot">
            {targets.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name} · {kindLabel(t.kind)}
              </option>
            ))}
          </select>
          <div className="row">
            <input className="input sm grow" placeholder="Label (optional)" value={snapLabel} onChange={(e) => setSnapLabel(e.target.value)} aria-label="Snapshot label" />
            <button className="btn sm primary" disabled={!snapConn || taking} onClick={take}>
              {taking ? <Spinner size={12} /> : <Camera size={13} />} Save
            </button>
          </div>
        </div>
        <div style={{ flex: 1, overflow: "auto", padding: 6 }}>
          {snapshots.loading && !snapshots.data && <Spinner />}
          {snapshots.data?.length === 0 && <div className="faint small" style={{ padding: 8 }}>No snapshots yet. Snapshots are stored in the workspace (<code>snapshots/</code>) and can be committed with your code.</div>}
          {snapshots.data?.map((s) => (
            <div key={s.id} className="card" style={{ padding: "8px 10px", marginBottom: 6 }}>
              <div className="row">
                <strong className="ellipsis grow" title={s.label}>
                  {s.label}
                </strong>
                <button className="icon-btn sm" title="Delete snapshot" aria-label="Delete snapshot" onClick={() => setRemove(s)}>
                  <Trash2 size={12} />
                </button>
              </div>
              <div className="faint xs">
                {s.source} · {s.schema} · {s.tables} tables · {relativeTime(Date.parse(s.taken_at))}
              </div>
              <div className="row" style={{ marginTop: 6, gap: 4 }}>
                <button className="btn sm" onClick={() => setFrom(`snap:${s.id}`)}>
                  From
                </button>
                <button className="btn sm" onClick={() => setTo(`snap:${s.id}`)}>
                  To
                </button>
              </div>
            </div>
          ))}
        </div>
      </aside>
      <div className="pane">
        <div className="page" style={{ maxWidth: 1100 }}>
          <h1 className="row">
            <GitCompare size={20} /> Compare schemas
          </h1>
          <p>Compare snapshots or live connections. The script turns <strong>From</strong> into <strong>To</strong>; the down script reverses it.</p>
          <div className="row wrap" style={{ gap: 8 }}>
            <select className="select" style={{ minWidth: 220 }} value={from} onChange={(e) => setFrom(e.target.value)} aria-label="From">
              {sources}
            </select>
            <ArrowRight size={16} className="muted" />
            <select className="select" style={{ minWidth: 220 }} value={to} onChange={(e) => setTo(e.target.value)} aria-label="To">
              {sources}
            </select>
            <select className="select" value={dialect} onChange={(e) => setDialect(e.target.value as SqlDialect | "")} aria-label="Script dialect" title="Script dialect">
              <option value="">Script for the From side</option>
              <option value="postgres">PostgreSQL script</option>
              <option value="mysql">MySQL script</option>
              <option value="sqlite">SQLite script</option>
              <option value="mssql">SQL Server script</option>
              <option value="clickhouse">ClickHouse script</option>
              <option value="cassandra">CQL script</option>
              <option value="spanner">Spanner script</option>
              <option value="bigquery">BigQuery script</option>
              <option value="oracle">Oracle script</option>
            </select>
            <button className="btn primary" disabled={!from || !to || busy} onClick={compare}>
              {busy ? <Spinner /> : <GitCompare size={14} />} Compare
            </button>
          </div>
          <div style={{ marginTop: 14 }}>
            <ErrorBox error={error} />
          </div>
          {!result && !error && !busy && <Empty icon={<GitCompare size={26} />} title="Pick two schemas to compare" />}
          {result && (
            <>
              <h2>
                {result.identical ? "No differences" : `${result.changes.length} change${result.changes.length === 1 ? "" : "s"}`}
                <span className="faint" style={{ fontWeight: 400, fontSize: 13 }}>
                  {" "}
                  · {result.from_label} → {result.to_label}
                </span>
              </h2>
              {result.warnings.map((w, i) => (
                <div key={i} className="note-box" style={{ marginBottom: 6, borderColor: "var(--warning-soft)" }}>
                  {w}
                </div>
              ))}
              {grouped.map(([table, changes]) => (
                <div key={table} style={{ marginBottom: 10 }}>
                  <div className="mono" style={{ fontWeight: 600, marginBottom: 4 }}>
                    {table}
                  </div>
                  {changes.map((c, i) => {
                    const d = describe(c);
                    const color = d.sign === "+" ? "var(--success)" : d.sign === "−" ? "var(--danger)" : "var(--warning)";
                    return (
                      <div key={i} className="row small" style={{ paddingLeft: 10 }}>
                        <span style={{ color, width: 14, display: "inline-flex" }}>{d.sign === "+" ? <Plus size={12} /> : d.sign === "−" ? <Minus size={12} /> : <Pencil size={12} />}</span>
                        <span className="mono">{d.text}</span>
                      </div>
                    );
                  })}
                </div>
              ))}
              {!result.identical && (
                <>
                  <div className="row" style={{ marginTop: 16, marginBottom: 8 }}>
                    <div className="seg">
                      <button className={script === "up" ? "on" : ""} onClick={() => setScript("up")}>
                        Up migration
                      </button>
                      <button className={script === "down" ? "on" : ""} onClick={() => setScript("down")}>
                        Down migration
                      </button>
                    </div>
                    <span className="faint xs">{kindLabel(result.dialect)}</span>
                    <span className="grow" />
                    <button
                      className="btn sm"
                      onClick={() =>
                        openTab({
                          kind: "query",
                          connection: script === "up" ? runOn : undefined,
                          sql: `-- ${script === "up" ? "Up" : "Down"} migration: ${result.from_label} → ${result.to_label}\n-- Review, then Preview or Run.\n\n${sql}\n`,
                          title: `${script === "up" ? "Up" : "Down"} migration`,
                        })
                      }
                    >
                      <Code2 size={13} /> Open in SQL editor
                    </button>
                  </div>
                  <CodeBlock code={sql} maxHeight={420} />
                </>
              )}
            </>
          )}
        </div>
      </div>
      {remove && (
        <ConfirmDialog
          title={`Delete snapshot "${remove.label}"?`}
          body="The snapshot file is removed from the workspace."
          confirmLabel="Delete"
          danger
          onClose={() => setRemove(null)}
          onConfirm={async () => {
            if (await attempt("Delete failed", () => call("schema.snapshot.delete", { id: remove.id }))) snapshots.reload();
          }}
        />
      )}
    </div>
  );
}
