// Query plan visualizer: an indented tree of plan steps with cost/time bars, row estimates vs
// actuals, details and tuning hints.

import { ChevronDown, ChevronRight, Lightbulb } from "lucide-react";
import { useMemo, useState } from "react";
import { duration } from "../lib/format";
import type { PlanNode, QueryPlan } from "../lib/types";
import { CodeBlock } from "./ui";

function fmtRows(n?: number): string {
  if (n === undefined || n === null) return "–";
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e4) return `${(n / 1e3).toFixed(0)}k`;
  return Number.isInteger(n) ? n.toLocaleString() : n.toFixed(1);
}

/** Metric for the bar: total time (per loop × loops) when analyzed, else estimated cost. */
function metric(n: PlanNode, analyzed: boolean): number {
  if (analyzed && n.actual_ms !== undefined) return n.actual_ms * (n.loops ?? 1);
  return n.est_cost ?? 0;
}

function maxMetric(nodes: PlanNode[], analyzed: boolean): number {
  let m = 0;
  const walk = (n: PlanNode) => {
    m = Math.max(m, metric(n, analyzed));
    n.children.forEach(walk);
  };
  nodes.forEach(walk);
  return m;
}

function Node({ node, depth, analyzed, max, path, collapsed, toggle }: { node: PlanNode; depth: number; analyzed: boolean; max: number; path: string; collapsed: Set<string>; toggle: (p: string) => void }) {
  const value = metric(node, analyzed);
  const share = max > 0 ? value / max : 0;
  const color = share >= 0.6 ? "var(--danger)" : share >= 0.3 ? "var(--warning)" : "var(--accent)";
  const off = node.est_rows !== undefined && node.actual_rows !== undefined ? Math.max(node.est_rows, 1) / Math.max(node.actual_rows, 1) : 1;
  const misestimate = off > 10 || off < 0.1;
  const open = !collapsed.has(path);
  return (
    <>
      <div className="plan-row" style={{ paddingLeft: 10 + depth * 18 }}>
        <div className="plan-main">
          <button className="icon-btn sm" style={{ visibility: node.children.length ? "visible" : "hidden" }} onClick={() => toggle(path)} aria-label={open ? "Collapse" : "Expand"}>
            {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          </button>
          <div style={{ minWidth: 0 }}>
            <div className="ellipsis">
              <strong>{node.op}</strong>
              {node.relation && !node.op.includes(node.relation) && <span className="mono muted"> {node.relation}</span>}
              {node.index && !node.op.includes(node.index) && <span className="badge info" style={{ marginLeft: 6 }}>{node.index}</span>}
            </div>
            {node.details.map((d, i) => (
              <div key={i} className="mono faint xs ellipsis" title={d}>
                {d}
              </div>
            ))}
          </div>
        </div>
        <div className="plan-num" title="Estimated rows">
          {fmtRows(node.est_rows)}
        </div>
        <div className="plan-num" title="Actual rows" style={misestimate ? { color: "var(--warning)", fontWeight: 650 } : undefined}>
          {analyzed ? fmtRows(node.actual_rows) : ""}
          {analyzed && node.loops && node.loops > 1 ? <span className="faint xs"> ×{fmtRows(node.loops)}</span> : null}
        </div>
        <div className="plan-bar" title={analyzed ? `${duration(value)} total` : `cost ${value.toFixed(2)}`}>
          <div className="track">
            <div className="fill" style={{ width: `${Math.max(2, share * 100)}%`, background: color }} />
          </div>
          <span className="xs muted">{analyzed && node.actual_ms !== undefined ? duration(value) : node.est_cost !== undefined ? node.est_cost.toFixed(1) : ""}</span>
        </div>
      </div>
      {open && node.children.map((c, i) => <Node key={i} node={c} depth={depth + 1} analyzed={analyzed} max={max} path={`${path}.${i}`} collapsed={collapsed} toggle={toggle} />)}
    </>
  );
}

export function PlanView({ plan }: { plan: QueryPlan }) {
  const [raw, setRaw] = useState(false);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const max = useMemo(() => maxMetric(plan.roots, plan.analyzed), [plan]);
  const toggle = (p: string) =>
    setCollapsed((s) => {
      const n = new Set(s);
      if (n.has(p)) n.delete(p);
      else n.add(p);
      return n;
    });
  return (
    <div className="pane">
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
        <div className="row wrap">
          <span className="badge">{plan.format}</span>
          {plan.analyzed ? <span className="badge success">analyzed</span> : <span className="badge">estimates</span>}
          {plan.planning_ms !== undefined && <span className="faint xs">planning {duration(plan.planning_ms)}</span>}
          {plan.execution_ms !== undefined && <span className="faint xs">execution {duration(plan.execution_ms)}</span>}
          <span className="grow" />
          <div className="seg">
            <button className={!raw ? "on" : ""} onClick={() => setRaw(false)}>
              Tree
            </button>
            <button className={raw ? "on" : ""} onClick={() => setRaw(true)}>
              Raw
            </button>
          </div>
        </div>
        {plan.hints.map((h, i) => (
          <div key={i} className="note-box row" style={{ alignItems: "flex-start", borderColor: "var(--accent-soft)" }}>
            <Lightbulb size={14} style={{ color: "var(--accent-text)", flex: "none", marginTop: 2 }} />
            <span>{h}</span>
          </div>
        ))}
        {raw ? (
          <CodeBlock code={plan.raw || "(empty)"} maxHeight={10000} />
        ) : plan.roots.length === 0 ? (
          <div className="faint">The server returned no plan steps.</div>
        ) : (
          <div className="plan">
            <div className="plan-row plan-head">
              <div className="plan-main">Step</div>
              <div className="plan-num">Est. rows</div>
              <div className="plan-num">{plan.analyzed ? "Actual rows" : ""}</div>
              <div className="plan-bar">{plan.analyzed ? "Time (incl. children)" : "Cost"}</div>
            </div>
            {plan.roots.map((r, i) => (
              <Node key={i} node={r} depth={0} analyzed={plan.analyzed} max={max} path={String(i)} collapsed={collapsed} toggle={toggle} />
            ))}
          </div>
        )}
        <div className="faint xs">
          {plan.analyzed
            ? "ANALYZE ran the statement inside a transaction that was rolled back."
            : "Estimates come from the server's planner. Use Explain analyze for real timings (the statement runs and is rolled back)."}
        </div>
      </div>
    </div>
  );
}
