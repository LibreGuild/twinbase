// Visual table designer: create a table or alter an existing one, with the generated SQL shown
// before anything runs.

import { ArrowDown, ArrowUp, Eye, KeyRound, Plus, Trash2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { PreviewPanel } from "../components/PreviewPanel";
import { CodeBlock, Dialog, ErrorBox, Field, Spinner } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { alterTableSql, createTableSql, fromSchema, idColumn, newColumn, TYPE_SUGGESTIONS, validateDesign, type DesignColumn, type DesignTable, type SqlKind } from "../lib/ddlgen";
import { useGraph } from "../lib/schema";
import { targetInfo, useStore } from "../lib/store";
import type { PreviewResult, TableSchema } from "../lib/types";

export function TableDesigner({ connection, schema, table, onClose }: { connection: string; schema: string; table?: string; onClose: () => void }) {
  const kind = (targetInfo(connection)?.kind ?? "postgres") as SqlKind;
  const toast = useStore((s) => s.toast);
  const openTab = useStore((s) => s.openTab);
  const { tables } = useGraph(connection, schema);
  const [original, setOriginal] = useState<DesignTable | null>(null);
  const [design, setDesign] = useState<DesignTable>(() => ({ name: "", columns: [idColumn(kind), newColumn(kind, { name: "name", nullable: false }), newColumn(kind, { name: "created_at", type: kind === "postgres" ? "timestamptz" : "datetime", nullable: false, default: "CURRENT_TIMESTAMP" })] }));
  const [loadError, setLoadError] = useState<string>();
  const [error, setError] = useState<string>();
  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [busy, setBusy] = useState<"preview" | "apply" | null>(null);

  useEffect(() => {
    if (!table) return;
    call<TableSchema>("db.describe", { connection, schema, table }).then(
      (s) => {
        const d = fromSchema(kind, s);
        setOriginal(d);
        setDesign(structuredClone(d));
      },
      (e) => setLoadError(errorMessage(e)),
    );
  }, [connection, schema, table, kind]);

  const plan = useMemo(() => {
    const invalid = validateDesign(design);
    if (invalid) return { sql: "", notes: [] as string[], invalid };
    if (original) {
      const p = alterTableSql(kind, original, design);
      return { sql: p.statements.join("\n"), notes: p.notes, invalid: p.statements.length ? undefined : "No changes yet." };
    }
    return { sql: createTableSql(kind, design), notes: [] as string[], invalid: undefined };
  }, [design, original, kind]);

  const refTargets = useMemo(() => {
    const list: string[] = [];
    for (const t of tables ?? []) for (const c of t.columns) if (c.primary_key || c.unique) list.push(`${t.name}.${c.name}`);
    if (!original) for (const c of design.columns) if (c.primaryKey && c.name && design.name) list.push(`${design.name}.${c.name}`);
    return list;
  }, [tables, design, original]);

  const setCol = (i: number, patch: Partial<DesignColumn>) => {
    setDesign((d) => ({ ...d, columns: d.columns.map((c, j) => (j === i ? { ...c, ...patch } : c)) }));
    setPreview(null);
  };
  const move = (i: number, delta: number) =>
    setDesign((d) => {
      const cols = d.columns.slice();
      const j = i + delta;
      if (j < 0 || j >= cols.length) return d;
      [cols[i], cols[j]] = [cols[j], cols[i]];
      return { ...d, columns: cols };
    });

  const runPreview = async () => {
    setBusy("preview");
    setError(undefined);
    try {
      setPreview(await call<PreviewResult>("db.preview", { connection, schema, sql: plan.sql }));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const apply = async () => {
    setBusy("apply");
    setError(undefined);
    try {
      await call("db.query", { connection, sql: plan.sql });
      toast("success", original ? "Table updated" : "Table created", design.name);
      if (!original) openTab({ kind: "table", connection, schema, table: design.name });
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const fkAware = kind !== "sqlite" || !original;
  return (
    <Dialog
      wide
      title={table ? `Edit table ${table}` : "New table"}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={runPreview} disabled={!!plan.invalid || !!busy} title="Run on a local twin first">
            {busy === "preview" ? <Spinner /> : <Eye size={14} />} Preview
          </button>
          <span className="grow faint small">{plan.invalid}</span>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" onClick={apply} disabled={!!plan.invalid || !!busy}>
            {busy === "apply" && <Spinner />} {original ? "Apply changes" : "Create table"}
          </button>
        </>
      }
    >
      <ErrorBox error={loadError} />
      <Field label="Table name">
        <input className="input mono" style={{ maxWidth: 320 }} value={design.name} onChange={(e) => setDesign((d) => ({ ...d, name: e.target.value.trim() ? e.target.value.replace(/\s+/g, "_") : "" }))} placeholder="orders" autoFocus={!table} />
      </Field>
      <div style={{ overflowX: "auto" }}>
        <table className="simple-table" style={{ minWidth: 820 }}>
          <thead>
            <tr>
              <th>Column</th>
              <th>Type</th>
              <th title="Primary key">PK</th>
              <th title="Automatically generated (serial / AUTO_INCREMENT)">Auto</th>
              <th title="Allows NULL">Null</th>
              <th title="Unique">Uniq</th>
              <th>Default</th>
              {fkAware && <th>References</th>}
              <th />
            </tr>
          </thead>
          <tbody>
            {design.columns.map((c, i) => (
              <tr key={i}>
                <td style={{ padding: 3 }}>
                  <input className="input sm mono" style={{ width: "100%", minWidth: 120 }} value={c.name} onChange={(e) => setCol(i, { name: e.target.value.replace(/\s+/g, "_") })} placeholder="column" aria-label="Column name" />
                </td>
                <td style={{ padding: 3 }}>
                  <input className="input sm mono" list={`types-${kind}`} style={{ width: 140 }} value={c.type} onChange={(e) => setCol(i, { type: e.target.value })} aria-label="Type" />
                </td>
                <td>
                  <input type="checkbox" checked={c.primaryKey} onChange={(e) => setCol(i, { primaryKey: e.target.checked, nullable: e.target.checked ? false : c.nullable })} aria-label="Primary key" />
                </td>
                <td>
                  <input type="checkbox" checked={c.auto} disabled={!!c.original} onChange={(e) => setCol(i, { auto: e.target.checked })} aria-label="Auto increment" />
                </td>
                <td>
                  <input type="checkbox" checked={c.nullable} disabled={c.primaryKey} onChange={(e) => setCol(i, { nullable: e.target.checked })} aria-label="Nullable" />
                </td>
                <td>
                  <input type="checkbox" checked={c.unique} disabled={c.primaryKey} onChange={(e) => setCol(i, { unique: e.target.checked })} aria-label="Unique" />
                </td>
                <td style={{ padding: 3 }}>
                  <input className="input sm mono" style={{ width: 130 }} value={c.default} disabled={c.auto} onChange={(e) => setCol(i, { default: e.target.value })} placeholder="none" aria-label="Default" />
                </td>
                {fkAware && (
                  <td style={{ padding: 3 }}>
                    <div className="row" style={{ gap: 4 }}>
                      <select className="select sm" style={{ width: 150 }} value={c.references} onChange={(e) => setCol(i, { references: e.target.value })} aria-label="References">
                        <option value="">—</option>
                        {refTargets.map((r) => (
                          <option key={r}>{r}</option>
                        ))}
                        {c.references && !refTargets.includes(c.references) && <option>{c.references}</option>}
                      </select>
                      {c.references && (
                        <select className="select sm" value={c.onDelete} onChange={(e) => setCol(i, { onDelete: e.target.value as DesignColumn["onDelete"] })} aria-label="On delete" title="ON DELETE">
                          <option value="">no action</option>
                          <option value="CASCADE">cascade</option>
                          <option value="SET NULL">set null</option>
                          <option value="RESTRICT">restrict</option>
                        </select>
                      )}
                    </div>
                  </td>
                )}
                <td style={{ whiteSpace: "nowrap" }}>
                  {!original && (
                    <>
                      <button className="icon-btn sm" onClick={() => move(i, -1)} aria-label="Move up">
                        <ArrowUp size={12} />
                      </button>
                      <button className="icon-btn sm" onClick={() => move(i, 1)} aria-label="Move down">
                        <ArrowDown size={12} />
                      </button>
                    </>
                  )}
                  <button className="icon-btn sm" onClick={() => setDesign((d) => ({ ...d, columns: d.columns.filter((_, j) => j !== i) }))} aria-label="Remove column">
                    <Trash2 size={12} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <datalist id={`types-${kind}`}>
        {TYPE_SUGGESTIONS[kind].map((t) => (
          <option key={t} value={t} />
        ))}
      </datalist>
      <div className="row">
        <button className="btn sm" onClick={() => setDesign((d) => ({ ...d, columns: [...d.columns, newColumn(kind)] }))}>
          <Plus size={13} /> Column
        </button>
        {!design.columns.some((c) => c.primaryKey) && (
          <button className="btn sm ghost" onClick={() => setDesign((d) => ({ ...d, columns: [idColumn(kind), ...d.columns] }))}>
            <KeyRound size={13} /> Add id key
          </button>
        )}
      </div>
      {plan.sql && <CodeBlock code={plan.sql} maxHeight={200} />}
      {plan.notes.map((n, i) => (
        <div key={i} className="note-box small">
          {n}
        </div>
      ))}
      <ErrorBox error={error} />
      {preview && (
        <div style={{ border: "1px solid var(--border)", borderRadius: 8 }}>
          <PreviewPanel preview={preview} />
        </div>
      )}
    </Dialog>
  );
}
