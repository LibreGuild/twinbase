// Row-level diff produced by running changes on a local twin (db.preview).

import { AlertTriangle, CheckCircle2, Info, XCircle } from "lucide-react";
import { cellPreview, plural } from "../lib/format";
import type { Cell, PreviewResult, TableDiff } from "../lib/types";

function CellView({ v }: { v: Cell }) {
  return <span title={cellPreview(v)}>{v === null ? <i className="faint">NULL</i> : cellPreview(v)}</span>;
}

function DiffTable({ d }: { d: TableDiff }) {
  const total = d.added.length + d.removed.length + d.changed.length;
  return (
    <div className="diff-section">
      <div className="diff-head">
        <span className="mono">{d.table}</span>
        {d.added.length > 0 && <span className="badge success">+{d.added.length} added</span>}
        {d.changed.length > 0 && <span className="badge warning">~{d.changed.length} changed</span>}
        {d.removed.length > 0 && <span className="badge danger">−{d.removed.length} removed</span>}
        {total === 0 && <span className="badge">no row changes</span>}
        {d.sampled && <span className="badge info" title="Only part of this table was copied to the twin">sampled</span>}
      </div>
      {total > 0 && (
        <div className="diff-scroll">
          <table className="diff-table">
            <thead>
              <tr>
                <th style={{ width: 28 }} />
                {d.columns.map((c) => (
                  <th key={c}>{c}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {d.changed.slice(0, 500).map((r, i) => (
                <tr key={`c${i}`}>
                  <td className="faint">~</td>
                  {r.after.map((v, j) =>
                    r.changed.includes(j) ? (
                      <td key={j} className="changed">
                        <span className="old">
                          <CellView v={r.before[j]} />
                        </span>
                        <span className="new">
                          <CellView v={v} />
                        </span>
                      </td>
                    ) : (
                      <td key={j}>
                        <CellView v={v} />
                      </td>
                    ),
                  )}
                </tr>
              ))}
              {d.added.slice(0, 500).map((r, i) => (
                <tr key={`a${i}`} className="added">
                  <td>+</td>
                  {r.map((v, j) => (
                    <td key={j}>
                      <CellView v={v} />
                    </td>
                  ))}
                </tr>
              ))}
              {d.removed.slice(0, 500).map((r, i) => (
                <tr key={`r${i}`} className="removed">
                  <td>−</td>
                  {r.map((v, j) => (
                    <td key={j}>
                      <CellView v={v} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {total > 1500 && <div className="faint xs" style={{ marginTop: 4 }}>Showing the first 500 rows of each kind.</div>}
    </div>
  );
}

export function PreviewPanel({ preview }: { preview: PreviewResult }) {
  return (
    <div style={{ padding: "12px 14px" }}>
      <div className="row" style={{ marginBottom: 10 }}>
        {preview.ok ? <CheckCircle2 size={16} style={{ color: "var(--success)" }} /> : <XCircle size={16} style={{ color: "var(--danger)" }} />}
        <strong>{preview.ok ? "Preview ran on a local twin — nothing has changed on the server yet." : "The changes failed on the local twin."}</strong>
      </div>
      <div style={{ display: "flex", flexDirection: "column", gap: 4, marginBottom: 12 }}>
        {preview.statements.map((s, i) => (
          <div key={i} className="row small" style={{ alignItems: "flex-start" }}>
            {s.error ? <XCircle size={14} style={{ color: "var(--danger)", flex: "none", marginTop: 2 }} /> : <CheckCircle2 size={14} style={{ color: "var(--success)", flex: "none", marginTop: 2 }} />}
            <code className="ellipsis grow" title={s.sql}>
              {s.sql}
            </code>
            <span className={s.error ? "" : "faint"} style={s.error ? { color: "var(--danger)", whiteSpace: "pre-wrap" } : { flex: "none" }}>
              {s.error ?? (s.affected !== undefined && s.affected !== null && !/^\s*(CREATE|ALTER|DROP|COMMENT|RENAME)\b/i.test(s.sql) ? plural(s.affected, "row") : "ok")}
            </span>
          </div>
        ))}
      </div>
      {preview.notes.map((n, i) => (
        <div key={i} className="note-box row" style={{ marginBottom: 8, alignItems: "flex-start" }}>
          {/sample|approx|not|cannot|skipp/i.test(n) ? <AlertTriangle size={14} style={{ flex: "none", marginTop: 2, color: "var(--warning)" }} /> : <Info size={14} style={{ flex: "none", marginTop: 2 }} />}
          <span>{n}</span>
        </div>
      ))}
      {preview.tables.map((t) => (
        <DiffTable key={t.table} d={t} />
      ))}
      {preview.ok && preview.tables.length === 0 && <div className="muted">No table data changes.</div>}
    </div>
  );
}
