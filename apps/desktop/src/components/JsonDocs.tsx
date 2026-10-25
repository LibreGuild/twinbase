// JSON documents as collapsible trees, cards and a table view (MongoDB documents, DynamoDB items,
// command replies).

import { ChevronDown, ChevronRight, Files, ListTree, Table2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { columnsOf, shortText, typeOf, type Json } from "../lib/mongo";
import { DataGrid } from "./DataGrid";
import { Empty } from "./ui";

export type Doc = Record<string, Json>;

export function Scalar({ v }: { v: Json }) {
  const t = typeOf(v);
  const text = shortText(v, 400);
  const cls = t === "string" ? "jv-str" : t === "int" || t === "double" || t === "long" || t === "decimal" ? "jv-num" : t === "bool" ? "jv-bool" : t === "null" ? "jv-null" : "jv-special";
  return <span className={cls}>{t === "string" ? JSON.stringify(text) : t === "objectId" ? `ObjectId('${text}')` : t === "date" ? `ISODate('${text}')` : text}</span>;
}

export function isContainer(v: Json): v is Json[] | { [k: string]: Json } {
  return !!v && typeof v === "object" && ["object", "array"].includes(typeOf(v));
}

export function Node({ name, v, depth, open: initiallyOpen }: { name?: string; v: Json; depth: number; open?: boolean }) {
  const [open, setOpen] = useState(initiallyOpen ?? depth < 1);
  const key = name !== undefined ? <span className="jv-key">{name}</span> : null;
  if (!isContainer(v)) {
    return (
      <div className="jv-row" style={{ paddingLeft: depth * 16 + 16 }}>
        {key}
        {key && <span className="jv-punct">: </span>}
        <Scalar v={v} />
      </div>
    );
  }
  const entries: [string, Json][] = Array.isArray(v) ? v.map((x, i) => [String(i), x]) : Object.entries(v);
  const summary = Array.isArray(v) ? `Array (${v.length})` : `Object (${entries.length})`;
  return (
    <>
      <div className="jv-row jv-toggle" style={{ paddingLeft: depth * 16 }} onClick={() => setOpen((o) => !o)}>
        <span className="jv-chev">{open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}</span>
        {key}
        {key && <span className="jv-punct">: </span>}
        <span className="jv-summary">{open ? (Array.isArray(v) ? "[" : "{") : summary}</span>
      </div>
      {open && entries.map(([k, x]) => <Node key={k} name={k} v={x} depth={depth + 1} />)}
      {open && (
        <div className="jv-row" style={{ paddingLeft: depth * 16 + 16 }}>
          <span className="jv-summary">{Array.isArray(v) ? "]" : "}"}</span>
        </div>
      )}
    </>
  );
}

export function DocCard({ doc, actions }: { doc: Doc; actions?: ReactNode }) {
  const entries = Object.entries(doc);
  return (
    <div className="doc-card">
      {actions && <div className="doc-actions">{actions}</div>}
      <div className="jv selectable">
        {entries.map(([k, v]) => (
          <Node key={k} name={k} v={v} depth={0} open={false} />
        ))}
        {entries.length === 0 && <span className="faint">{"{ }"}</span>}
      </div>
    </div>
  );
}

export function DocTable({ docs }: { docs: Doc[] }) {
  const cols = useMemo(() => columnsOf(docs), [docs]);
  const columns = useMemo(() => cols.map((c) => ({ name: c, type: [...new Set(docs.filter((d) => c in d).map((d) => typeOf(d[c])))].join(" | ") || "missing" })), [cols, docs]);
  const rows = useMemo(() => docs.map((d) => cols.map((c) => (c in d ? (typeof d[c] === "number" || typeof d[c] === "boolean" ? (d[c] as number | boolean) : d[c] === null ? null : shortText(d[c], 200)) : null))), [docs, cols]);
  return (
    <div className="grid-wrap">
      <DataGrid columns={columns} rows={rows} widthKey="mongo-docs" />
    </div>
  );
}

export function DocList({ docs, view, actions, empty }: { docs: Doc[]; view: "list" | "table"; actions?: (d: Doc, i: number) => ReactNode; empty?: ReactNode }) {
  if (docs.length === 0) return <>{empty ?? <Empty icon={<Files size={22} />} title="No documents" />}</>;
  if (view === "table") return <DocTable docs={docs} />;
  return (
    <div className="doc-list">
      {docs.map((d, i) => (
        <DocCard key={i} doc={d} actions={actions?.(d, i)} />
      ))}
    </div>
  );
}

export function ViewToggle({ view, setView }: { view: "list" | "table"; setView: (v: "list" | "table") => void }) {
  return (
    <div className="seg" role="tablist" aria-label="Result view">
      <button className={view === "list" ? "on" : ""} onClick={() => setView("list")} title="Documents">
        <ListTree size={13} /> JSON
      </button>
      <button className={view === "table" ? "on" : ""} onClick={() => setView("table")} title="Table">
        <Table2 size={13} /> Table
      </button>
    </div>
  );
}

export function useStoredView(key: string): ["list" | "table", (v: "list" | "table") => void] {
  const [view, setView] = useState<"list" | "table">(() => {
    try {
      return localStorage.getItem(key) === "table" ? "table" : "list";
    } catch {
      return "list";
    }
  });
  return [
    view,
    (v) => {
      setView(v);
      try {
        localStorage.setItem(key, v);
      } catch {
        /* storage unavailable */
      }
    },
  ];
}

