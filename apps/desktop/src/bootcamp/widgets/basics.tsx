// Widgets for the relational chapters: table anatomy, joins, index race, B-tree search and
// normalisation.

import { useEffect, useMemo, useState } from "react";

const PEOPLE = [
  { id: 1, name: "Ada", city: "London", joined: "2024-01-12" },
  { id: 2, name: "Grace", city: "New York", joined: "2024-03-02" },
  { id: 3, name: "Linus", city: "Helsinki", joined: "2024-03-20" },
];
const COLS = [
  ["id", "integer"],
  ["name", "text"],
  ["city", "text"],
  ["joined", "date"],
] as const;

export function TableAnatomy() {
  const [hover, setHover] = useState<{ row?: number; col?: number }>({});
  const label =
    hover.col === 0 && hover.row === undefined
      ? "Primary key — unique and never null: identifies each row."
      : hover.col !== undefined && hover.row === undefined
        ? `Column “${COLS[hover.col][0]}” — one attribute, every value has type ${COLS[hover.col][1]}.`
        : hover.row !== undefined
          ? `Row ${hover.row + 1} — one record: everything about ${PEOPLE[hover.row].name}.`
          : "Hover a column header or a row.";
  return (
    <div className="bw">
      <table className="bw-table">
        <thead>
          <tr>
            {COLS.map(([c, t], i) => (
              <th key={c} className={hover.col === i ? "hl" : ""} onMouseEnter={() => setHover({ col: i })} onMouseLeave={() => setHover({})}>
                {i === 0 && <span className="bw-key">PK</span>} {c}
                <span className="bw-type">{t}</span>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {PEOPLE.map((p, r) => (
            <tr key={p.id} className={hover.row === r ? "hl" : ""} onMouseEnter={() => setHover({ row: r })} onMouseLeave={() => setHover({})}>
              {[p.id, p.name, p.city, p.joined].map((v, c) => (
                <td key={c} className={hover.col === c ? "hl" : ""}>
                  {v}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      <div className="bw-caption">{label}</div>
    </div>
  );
}

const CUSTOMERS = [
  { id: 1, name: "Ada" },
  { id: 2, name: "Grace" },
  { id: 3, name: "Linus" },
];
const ORDERS = [
  { id: 101, customer: 1 },
  { id: 102, customer: 1 },
  { id: 103, customer: 2 },
  { id: 104, customer: 9 },
];
type JoinKind = "INNER" | "LEFT" | "RIGHT" | "FULL";

export function JoinExplorer() {
  const [kind, setKind] = useState<JoinKind>("INNER");
  const rows = useMemo(() => {
    const out: { c?: (typeof CUSTOMERS)[number]; o?: (typeof ORDERS)[number] }[] = [];
    for (const c of CUSTOMERS) {
      const os = ORDERS.filter((o) => o.customer === c.id);
      if (os.length) os.forEach((o) => out.push({ c, o }));
      else if (kind === "LEFT" || kind === "FULL") out.push({ c });
    }
    if (kind === "RIGHT" || kind === "FULL") for (const o of ORDERS) if (!CUSTOMERS.some((c) => c.id === o.customer)) out.push({ o });
    if (kind === "RIGHT") return out.filter((r) => r.o);
    return out;
  }, [kind]);
  const leftOn = kind === "LEFT" || kind === "FULL";
  const rightOn = kind === "RIGHT" || kind === "FULL";
  return (
    <div className="bw">
      <div className="seg" style={{ marginBottom: 12 }}>
        {(["INNER", "LEFT", "RIGHT", "FULL"] as JoinKind[]).map((k) => (
          <button key={k} className={kind === k ? "on" : ""} onClick={() => setKind(k)}>
            {k} JOIN
          </button>
        ))}
      </div>
      <div className="bw-join">
        <svg viewBox="0 0 200 110" className="bw-venn" aria-hidden>
          <circle cx="75" cy="55" r="45" className={`venn l${leftOn ? " on" : ""}`} />
          <circle cx="125" cy="55" r="45" className={`venn r${rightOn ? " on" : ""}`} />
          <path d="M100 17.6 A45 45 0 0 1 100 92.4 A45 45 0 0 1 100 17.6Z" className="venn mid on" />
          <text x="45" y="59" className="venn-t">customers</text>
          <text x="128" y="59" className="venn-t">orders</text>
        </svg>
        <table className="bw-table">
          <thead>
            <tr>
              <th>customer</th>
              <th>order</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r, i) => (
              <tr key={i}>
                <td className={r.c ? "" : "null"}>{r.c ? `${r.c.id} · ${r.c.name}` : "NULL"}</td>
                <td className={r.o ? "" : "null"}>{r.o ? `#${r.o.id} (customer ${r.o.customer})` : "NULL"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="bw-caption">
        Linus has no orders; order #104 points at customer 9, who doesn't exist. {kind} JOIN returns {rows.length} rows.
      </div>
    </div>
  );
}

export function IndexRace() {
  const [exp, setExp] = useState(6);
  const n = Math.round(10 ** exp);
  const scan = n;
  const tree = Math.max(1, Math.ceil(Math.log2(n)));
  const scanMs = scan / 5_000_000;
  const fmt = (x: number) => (x >= 1e9 ? `${(x / 1e9).toFixed(1)} B` : x >= 1e6 ? `${(x / 1e6).toFixed(1)} M` : x >= 1e3 ? `${(x / 1e3).toFixed(1)} K` : String(x));
  return (
    <div className="bw">
      <label className="bw-slider">
        <span>Rows in the table</span>
        <input type="range" min={2} max={10} step={0.5} value={exp} onChange={(e) => setExp(Number(e.target.value))} />
        <strong>{fmt(n)}</strong>
      </label>
      <div className="bw-race">
        <div className="lane">
          <div className="lane-h">Sequential scan</div>
          <div className="bar-track">
            <div className="bar scan" style={{ width: "100%" }} />
          </div>
          <div className="lane-n">
            {fmt(scan)} rows read · ~{scanMs < 1 ? "<1" : scanMs.toFixed(0)} ms
          </div>
        </div>
        <div className="lane">
          <div className="lane-h">B-tree index</div>
          <div className="bar-track">
            <div className="bar idx" style={{ width: `${Math.max(1.5, (tree / n) * 100)}%` }} />
          </div>
          <div className="lane-n">~{tree} comparisons · 3–4 page reads</div>
        </div>
      </div>
      <div className="bw-caption">Ten times more rows makes the scan ten times slower, but adds only ~3 comparisons to the index lookup.</div>
    </div>
  );
}

const TREE = { keys: [40, 80], children: [{ keys: [10, 20, 30] }, { keys: [50, 60, 70] }, { keys: [90, 95, 99] }] };

export function BTree() {
  const [target, setTarget] = useState("60");
  const [step, setStep] = useState(-1);
  const t = Number(target);
  const childIndex = t < TREE.keys[0] ? 0 : t < TREE.keys[1] ? 1 : 2;
  const found = TREE.children[childIndex].keys.includes(t);
  useEffect(() => {
    setStep(-1);
    const timers = [setTimeout(() => setStep(0), 250), setTimeout(() => setStep(1), 900), setTimeout(() => setStep(2), 1500)];
    return () => timers.forEach(clearTimeout);
  }, [target]);
  const nodeW = 150;
  return (
    <div className="bw">
      <label className="bw-slider">
        <span>Search for key</span>
        <input className="input sm mono" style={{ width: 80 }} value={target} onChange={(e) => setTarget(e.target.value.replace(/\D/g, "").slice(0, 3))} />
      </label>
      <svg viewBox="0 0 520 170" className="bw-svg" role="img" aria-label="B-tree search">
        <g>
          {TREE.children.map((_, i) => (
            <line key={i} x1={260} y1={48} x2={30 + i * 180 + nodeW / 2} y2={112} className={`edge${step >= 1 && i === childIndex ? " on" : ""}`} />
          ))}
          <rect x={185} y={14} width={nodeW} height={34} rx={8} className={`node${step >= 0 ? " on" : ""}`} />
          {TREE.keys.map((k, i) => (
            <text key={k} x={215 + i * 55} y={36} className="node-k">
              {k}
            </text>
          ))}
          {TREE.children.map((c, i) => (
            <g key={i}>
              <rect x={30 + i * 180} y={112} width={nodeW} height={34} rx={8} className={`node leaf${step >= 1 && i === childIndex ? " on" : ""}`} />
              {c.keys.map((k, j) => (
                <text key={k} x={50 + i * 180 + j * 45} y={134} className={`node-k${step >= 2 && k === t ? " hit" : ""}`}>
                  {k}
                </text>
              ))}
            </g>
          ))}
        </g>
      </svg>
      <div className="bw-caption">
        {step < 2 ? "Walking from the root…" : found ? `Found ${t} after visiting 2 nodes.` : `${t || "?"} is not in the tree — still only 2 nodes visited to be sure.`} Each node is one page; real nodes hold hundreds of keys.
      </div>
    </div>
  );
}

const STAGES = [
  {
    title: "Unnormalised",
    tables: [{ name: "orders", cols: ["order_id", "customer", "city", "products"], rows: [["101", "Ada", "London", "keyboard, guide"], ["103", "Ada", "London", "hub"]] }],
    note: "Lists in a cell and the customer's city repeated on every order.",
  },
  {
    title: "1NF — one value per cell",
    tables: [{ name: "orders", cols: ["order_id", "customer", "city", "product"], rows: [["101", "Ada", "London", "keyboard"], ["101", "Ada", "London", "guide"], ["103", "Ada", "London", "hub"]] }],
    note: "Atomic values, but Ada's city now appears three times.",
  },
  {
    title: "2NF / 3NF — each fact once",
    tables: [
      { name: "customers", cols: ["id", "name", "city"], rows: [["1", "Ada", "London"]] },
      { name: "orders", cols: ["id", "customer_id"], rows: [["101", "1"], ["103", "1"]] },
      { name: "order_items", cols: ["order_id", "product"], rows: [["101", "keyboard"], ["101", "guide"], ["103", "hub"]] },
    ],
    note: "Change Ada's city in exactly one row; orders refer to her by id.",
  },
];

export function Normalize() {
  const [i, setI] = useState(0);
  const s = STAGES[i];
  return (
    <div className="bw">
      <div className="row" style={{ marginBottom: 10 }}>
        <strong>{s.title}</strong>
        <span className="grow" />
        <button className="btn sm" disabled={i === 0} onClick={() => setI(i - 1)}>
          Back
        </button>
        <button className="btn sm primary" disabled={i === STAGES.length - 1} onClick={() => setI(i + 1)}>
          Normalise next step
        </button>
      </div>
      <div className="bw-tables">
        {s.tables.map((t) => (
          <div key={t.name}>
            <div className="bw-tname mono">{t.name}</div>
            <table className="bw-table">
              <thead>
                <tr>
                  {t.cols.map((c) => (
                    <th key={c}>{c}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {t.rows.map((r, j) => (
                  <tr key={j}>
                    {r.map((v, k) => (
                      <td key={k}>{v}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
      </div>
      <div className="bw-caption">{s.note}</div>
    </div>
  );
}
