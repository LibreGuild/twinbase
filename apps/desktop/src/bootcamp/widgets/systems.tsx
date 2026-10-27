// Widgets for transactions, storage, caching, distribution and vectors.

import { useEffect, useMemo, useRef, useState } from "react";

// ------------------------------------------------------------------ isolation levels
type Level = "read-uncommitted" | "read-committed" | "repeatable-read" | "serializable";
type Scenario = "dirty" | "nonrepeatable" | "phantom" | "lost";
const LEVEL_NAMES: Record<Level, string> = { "read-uncommitted": "Read uncommitted", "read-committed": "Read committed", "repeatable-read": "Repeatable read", serializable: "Serializable" };
const SCENARIOS: Record<Scenario, { name: string; steps: [string, string][]; prevented: Level[]; bad: string; good: string }> = {
  dirty: {
    name: "Dirty read",
    steps: [
      ["T1", "UPDATE accounts SET balance = 0 WHERE id = 1"],
      ["T2", "SELECT balance FROM accounts WHERE id = 1"],
      ["T1", "ROLLBACK"],
    ],
    prevented: ["read-committed", "repeatable-read", "serializable"],
    bad: "T2 read 0 — a value that never officially existed.",
    good: "T2 still reads 100: uncommitted changes are invisible.",
  },
  nonrepeatable: {
    name: "Non-repeatable read",
    steps: [
      ["T2", "SELECT balance … → 100"],
      ["T1", "UPDATE … SET balance = 50; COMMIT"],
      ["T2", "SELECT balance … again"],
    ],
    prevented: ["repeatable-read", "serializable"],
    bad: "T2's second read returns 50: the same row changed under it.",
    good: "T2 sees 100 both times: it reads from its snapshot.",
  },
  phantom: {
    name: "Phantom",
    steps: [
      ["T2", "SELECT COUNT(*) FROM orders WHERE total > 100 → 3"],
      ["T1", "INSERT INTO orders (total) VALUES (500); COMMIT"],
      ["T2", "SELECT COUNT(*) … again"],
    ],
    prevented: ["serializable"],
    bad: "The count becomes 4: a phantom row appeared.",
    good: "The count stays 3 for T2 (PostgreSQL's repeatable read also prevents this).",
  },
  lost: {
    name: "Lost update",
    steps: [
      ["T1", "SELECT stock → 10"],
      ["T2", "SELECT stock → 10"],
      ["T1", "UPDATE stock = 10 - 1; COMMIT"],
      ["T2", "UPDATE stock = 10 - 1; COMMIT"],
    ],
    prevented: ["serializable"],
    bad: "Two sales but stock is 9, not 8: T1's update was overwritten.",
    good: "T2 fails with a serialization error and retries, reading 9.",
  },
};

export function TxIsolation() {
  const [level, setLevel] = useState<Level>("read-committed");
  const [scenario, setScenario] = useState<Scenario>("dirty");
  const [step, setStep] = useState(0);
  const s = SCENARIOS[scenario];
  const done = step >= s.steps.length;
  const prevented = s.prevented.includes(level);
  useEffect(() => {
    setStep(0);
  }, [scenario, level]);
  return (
    <div className="bw">
      <div className="row wrap" style={{ marginBottom: 10 }}>
        <select className="select sm" value={scenario} onChange={(e) => setScenario(e.target.value as Scenario)}>
          {Object.entries(SCENARIOS).map(([k, v]) => (
            <option key={k} value={k}>
              {v.name}
            </option>
          ))}
        </select>
        <select className="select sm" value={level} onChange={(e) => setLevel(e.target.value as Level)}>
          {Object.entries(LEVEL_NAMES).map(([k, v]) => (
            <option key={k} value={k}>
              {v}
            </option>
          ))}
        </select>
        <span className="grow" />
        <button className="btn sm" onClick={() => setStep(0)}>
          Restart
        </button>
        <button className="btn sm primary" disabled={done} onClick={() => setStep(step + 1)}>
          Next step
        </button>
      </div>
      <div className="bw-timeline">
        <div className="tl-head">
          <span>T1</span>
          <span>T2</span>
        </div>
        {s.steps.map(([who, text], i) => (
          <div key={i} className={`tl-row${i < step ? " shown" : ""}${i === step - 1 ? " now" : ""}`}>
            <span className={who === "T1" ? "tl-cell" : "tl-cell empty"}>{who === "T1" ? text : ""}</span>
            <span className={who === "T2" ? "tl-cell" : "tl-cell empty"}>{who === "T2" ? text : ""}</span>
          </div>
        ))}
      </div>
      <div className={`bw-verdict${done ? (prevented ? " ok" : " bad") : ""}`}>{done ? (prevented ? `✓ Prevented at ${LEVEL_NAMES[level]}. ${s.good}` : `✗ Happens at ${LEVEL_NAMES[level]}. ${s.bad}`) : "Step through both transactions."}</div>
    </div>
  );
}

// ------------------------------------------------------------------ cache-aside simulator
export function CacheSim() {
  const [ttl, setTtl] = useState(30);
  const [capacity, setCapacity] = useState(5);
  const [stats, setStats] = useState({ hits: 0, misses: 0, time: 0 });
  const [log, setLog] = useState<{ key: string; hit: boolean }[]>([]);
  const cache = useRef(new Map<string, { at: number; used: number }>());
  const clock = useRef(0);
  const request = (n: number) => {
    let { hits, misses } = stats;
    const entries: { key: string; hit: boolean }[] = [];
    for (let i = 0; i < n; i++) {
      clock.current += 2;
      // Zipf-ish popularity: a few hot products, a long tail.
      const r = Math.random();
      const key = `product:${r < 0.6 ? 1 + Math.floor(Math.random() * 3) : 4 + Math.floor(Math.random() * 12)}`;
      const e = cache.current.get(key);
      if (e && clock.current - e.at < ttl) {
        hits++;
        e.used = clock.current;
        entries.push({ key, hit: true });
      } else {
        misses++;
        if (!e && cache.current.size >= capacity) {
          const lru = [...cache.current.entries()].sort((a, b) => a[1].used - b[1].used)[0];
          if (lru) cache.current.delete(lru[0]);
        }
        cache.current.set(key, { at: clock.current, used: clock.current });
        entries.push({ key, hit: false });
      }
    }
    setStats({ hits, misses, time: clock.current });
    setLog((l) => [...entries.reverse(), ...l].slice(0, 12));
  };
  const total = stats.hits + stats.misses;
  const rate = total ? stats.hits / total : 0;
  const avg = total ? (stats.hits * 0.5 + stats.misses * 20) / total : 0;
  const reset = () => {
    cache.current.clear();
    clock.current = 0;
    setStats({ hits: 0, misses: 0, time: 0 });
    setLog([]);
  };
  return (
    <div className="bw">
      <div className="bw-controls">
        <label className="bw-slider">
          <span>TTL</span>
          <input type="range" min={2} max={120} value={ttl} onChange={(e) => setTtl(Number(e.target.value))} />
          <strong>{ttl}s</strong>
        </label>
        <label className="bw-slider">
          <span>Capacity</span>
          <input type="range" min={1} max={15} value={capacity} onChange={(e) => setCapacity(Number(e.target.value))} />
          <strong>{capacity} keys</strong>
        </label>
      </div>
      <div className="row" style={{ margin: "8px 0" }}>
        <button className="btn sm primary" onClick={() => request(1)}>
          1 request
        </button>
        <button className="btn sm" onClick={() => request(50)}>
          50 requests
        </button>
        <button className="btn sm ghost" onClick={reset}>
          Flush cache
        </button>
      </div>
      <div className="bw-stats">
        <div>
          <b>{(rate * 100).toFixed(0)}%</b>
          <span>hit rate</span>
        </div>
        <div>
          <b>{avg.toFixed(1)} ms</b>
          <span>avg latency (hit 0.5 ms, miss 20 ms)</span>
        </div>
        <div>
          <b>{cache.current.size}</b>
          <span>keys cached</span>
        </div>
      </div>
      <div className="bw-log">
        {log.map((e, i) => (
          <span key={i} className={e.hit ? "hit" : "miss"}>
            {e.hit ? "HIT" : "MISS"} {e.key}
          </span>
        ))}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ consistent hashing
function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) / 4294967296;
}
const KEYS = Array.from({ length: 36 }, (_, i) => `user:${i + 1}`);
const COLORS = ["#55a9a0", "#e0954a", "#5b8fd9", "#c9605c", "#8a6bd1", "#4faa55", "#e5c454"];

export function HashRing() {
  const [nodes, setNodes] = useState(["A", "B", "C", "D"]);
  const [prev, setPrev] = useState<Map<string, string> | null>(null);
  const [prevMod, setPrevMod] = useState<Map<string, string> | null>(null);
  const assign = (ns: string[]) => {
    const points = ns.flatMap((n) => [0, 1, 2].map((v) => ({ n, p: hash(`${n}#${v}`) }))).sort((a, b) => a.p - b.p);
    return new Map(KEYS.map((k) => {
      const h = hash(k);
      return [k, (points.find((x) => x.p >= h) ?? points[0]).n];
    }));
  };
  const assignMod = (ns: string[]) => new Map(KEYS.map((k) => [k, ns[Math.floor(hash(k) * 997) % ns.length]]));
  const ring = useMemo(() => assign(nodes), [nodes]);
  const mod = useMemo(() => assignMod(nodes), [nodes]);
  const moved = prev ? KEYS.filter((k) => prev.get(k) !== ring.get(k)).length : 0;
  const movedMod = prevMod ? KEYS.filter((k) => prevMod.get(k) !== mod.get(k)).length : 0;
  const change = (ns: string[]) => {
    setPrev(ring);
    setPrevMod(mod);
    setNodes(ns);
  };
  const color = (n: string) => COLORS["ABCDEFG".indexOf(n) % COLORS.length];
  const R = 70;
  return (
    <div className="bw">
      <div className="row" style={{ marginBottom: 8 }}>
        <button className="btn sm primary" disabled={nodes.length >= 7} onClick={() => change([...nodes, "ABCDEFG"[nodes.length]])}>
          Add server
        </button>
        <button className="btn sm" disabled={nodes.length <= 2} onClick={() => change(nodes.slice(0, -1))}>
          Remove server
        </button>
        <span className="faint small">{nodes.length} servers · 3 virtual nodes each</span>
      </div>
      <div className="bw-ring">
        <svg viewBox="-100 -100 200 200" className="bw-svg" style={{ maxWidth: 240 }} aria-label="Hash ring">
          <circle r={R} className="ring" />
          {nodes.flatMap((n) =>
            [0, 1, 2].map((v) => {
              const a = hash(`${n}#${v}`) * Math.PI * 2 - Math.PI / 2;
              return <circle key={`${n}${v}`} cx={Math.cos(a) * R} cy={Math.sin(a) * R} r={6} fill={color(n)} className="ring-node" />;
            }),
          )}
          {KEYS.map((k) => {
            const a = hash(k) * Math.PI * 2 - Math.PI / 2;
            const movedNow = prev && prev.get(k) !== ring.get(k);
            return <circle key={k} cx={Math.cos(a) * (R - 14)} cy={Math.sin(a) * (R - 14)} r={movedNow ? 3.4 : 2.4} fill={color(ring.get(k)!)} className={movedNow ? "ring-key moved" : "ring-key"} />;
          })}
          {nodes.map((n, i) => (
            <text key={n} x={-60 + i * 20} y={4} className="ring-label" fill={color(n)}>
              {n}
            </text>
          ))}
        </svg>
        <div className="bw-stats vertical">
          <div>
            <b>{prev ? `${Math.round((moved / KEYS.length) * 100)}%` : "–"}</b>
            <span>keys moved with consistent hashing</span>
          </div>
          <div>
            <b>{prevMod ? `${Math.round((movedMod / KEYS.length) * 100)}%` : "–"}</b>
            <span>keys moved with hash % N</span>
          </div>
        </div>
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ replication lag
export function Replication() {
  const [lag, setLag] = useState(3);
  const [leader, setLeader] = useState("Ada");
  const [follower, setFollower] = useState("Ada");
  const [pending, setPending] = useState<string | null>(null);
  const [read, setRead] = useState<{ from: string; value: string } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const write = () => {
    const next = leader === "Ada" ? "Ada Lovelace" : "Ada";
    setLeader(next);
    setPending(next);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      setFollower(next);
      setPending(null);
    }, lag * 1000);
  };
  useEffect(() => () => clearTimeout(timer.current), []);
  return (
    <div className="bw">
      <label className="bw-slider">
        <span>Replication lag</span>
        <input type="range" min={0} max={6} value={lag} onChange={(e) => setLag(Number(e.target.value))} />
        <strong>{lag}s</strong>
      </label>
      <div className="bw-repl">
        <div className="srv leader">
          <div className="srv-h">Leader</div>
          <div className="srv-v mono">name = '{leader}'</div>
        </div>
        <div className={`wire${pending ? " busy" : ""}`}>{pending ? "WAL →" : "in sync"}</div>
        <div className="srv">
          <div className="srv-h">Follower (async)</div>
          <div className="srv-v mono">name = '{follower}'</div>
        </div>
      </div>
      <div className="row" style={{ marginTop: 10 }}>
        <button className="btn sm primary" onClick={write}>
          UPDATE on leader
        </button>
        <button className="btn sm" onClick={() => setRead({ from: "follower", value: follower })}>
          Read from follower
        </button>
        <button className="btn sm" onClick={() => setRead({ from: "leader", value: leader })}>
          Read from leader
        </button>
      </div>
      <div className={`bw-verdict${read ? (read.value === leader ? " ok" : " bad") : ""}`}>
        {read ? (read.value === leader ? `Read '${read.value}' from the ${read.from}: up to date.` : `Read '${read.value}' from the ${read.from}: stale! The user doesn't see their own write yet.`) : "Write on the leader, then read quickly from the follower."}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ LSM vs B-tree
export function LsmVsBtree() {
  const [memtable, setMemtable] = useState<number[]>([]);
  const [sstables, setSstables] = useState<number[][]>([]);
  const [tree, setTree] = useState<number[]>([]);
  const add = () => {
    const k = Math.floor(Math.random() * 90) + 10;
    setTree((t) => [...t, k].sort((a, b) => a - b));
    setMemtable((m) => {
      const next = [...m, k].sort((a, b) => a - b);
      if (next.length >= 4) {
        setSstables((s) => [next, ...s]);
        return [];
      }
      return next;
    });
  };
  const compact = () => setSstables((s) => (s.length > 1 ? [[...new Set(s.flat())].sort((a, b) => a - b)] : s));
  return (
    <div className="bw">
      <div className="row" style={{ marginBottom: 10 }}>
        <button className="btn sm primary" onClick={add}>
          Write a key
        </button>
        <button className="btn sm" disabled={sstables.length < 2} onClick={compact}>
          Compact SSTables
        </button>
        <button
          className="btn sm ghost"
          onClick={() => {
            setMemtable([]);
            setSstables([]);
            setTree([]);
          }}
        >
          Reset
        </button>
      </div>
      <div className="bw-two">
        <div>
          <div className="bw-tname">LSM tree</div>
          <div className="lsm-mem">
            <span className="faint xs">memtable (RAM)</span>
            <div className="chips">{memtable.map((k, i) => <span key={i}>{k}</span>)}</div>
          </div>
          {sstables.map((t, i) => (
            <div key={i} className="lsm-sst">
              <span className="faint xs">SSTable {sstables.length - i} (disk, immutable)</span>
              <div className="chips">{t.map((k, j) => <span key={j}>{k}</span>)}</div>
            </div>
          ))}
          <div className="faint xs" style={{ marginTop: 6 }}>A read may check the memtable and {sstables.length || "no"} SSTable{sstables.length === 1 ? "" : "s"}.</div>
        </div>
        <div>
          <div className="bw-tname">B-tree</div>
          <div className="lsm-mem">
            <span className="faint xs">one sorted structure, updated in place</span>
            <div className="chips">{tree.map((k, i) => <span key={i}>{k}</span>)}</div>
          </div>
          <div className="faint xs" style={{ marginTop: 6 }}>Each write updates the right page now: more random I/O, but a read checks one place.</div>
        </div>
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ vector space
const ITEMS: { label: string; v: [number, number] }[] = [
  { label: "cat", v: [0.82, 0.75] },
  { label: "kitten", v: [0.78, 0.82] },
  { label: "dog", v: [0.7, 0.62] },
  { label: "puppy", v: [0.64, 0.7] },
  { label: "car", v: [-0.7, 0.4] },
  { label: "truck", v: [-0.8, 0.28] },
  { label: "bicycle", v: [-0.55, 0.6] },
  { label: "apple", v: [0.2, -0.75] },
  { label: "banana", v: [0.35, -0.68] },
  { label: "pizza", v: [0.05, -0.55] },
  { label: "SQL", v: [-0.62, -0.58] },
  { label: "database", v: [-0.7, -0.45] },
];
type Metric = "l2" | "cosine" | "ip";

export function VectorSpace() {
  const [q, setQ] = useState<[number, number]>([0.6, 0.55]);
  const [metric, setMetric] = useState<Metric>("cosine");
  const dist = (a: [number, number], b: [number, number]) => {
    const dot = a[0] * b[0] + a[1] * b[1];
    if (metric === "l2") return Math.hypot(a[0] - b[0], a[1] - b[1]);
    if (metric === "ip") return -dot;
    const n = Math.hypot(...a) * Math.hypot(...b);
    return n ? 1 - dot / n : 1;
  };
  const ranked = [...ITEMS].map((it) => ({ ...it, d: dist(q, it.v) })).sort((a, b) => a.d - b.d);
  const top = new Set(ranked.slice(0, 3).map((r) => r.label));
  const S = 100;
  const toPx = (v: [number, number]) => [v[0] * S, -v[1] * S] as const;
  return (
    <div className="bw">
      <div className="seg" style={{ marginBottom: 10 }}>
        {(["cosine", "l2", "ip"] as Metric[]).map((m) => (
          <button key={m} className={metric === m ? "on" : ""} onClick={() => setMetric(m)}>
            {m === "cosine" ? "Cosine" : m === "l2" ? "L2" : "Inner product"} <code className="op">{m === "cosine" ? "<=>" : m === "l2" ? "<->" : "<#>"}</code>
          </button>
        ))}
      </div>
      <div className="bw-ring">
        <svg
          viewBox="-120 -120 240 240"
          className="bw-svg vec"
          style={{ maxWidth: 300, cursor: "crosshair" }}
          onClick={(e) => {
            const r = (e.currentTarget as SVGSVGElement).getBoundingClientRect();
            const x = ((e.clientX - r.left) / r.width) * 240 - 120;
            const y = ((e.clientY - r.top) / r.height) * 240 - 120;
            setQ([Math.max(-1.1, Math.min(1.1, x / S)), Math.max(-1.1, Math.min(1.1, -y / S))]);
          }}
          aria-label="Embedding space; click to move the query"
        >
          <line x1={-115} y1={0} x2={115} y2={0} className="axis" />
          <line x1={0} y1={-115} x2={0} y2={115} className="axis" />
          {ranked.slice(0, 3).map((r) => {
            const [x1, y1] = toPx(q);
            const [x2, y2] = toPx(r.v);
            return <line key={r.label} x1={x1} y1={y1} x2={x2} y2={y2} className="nn-line" />;
          })}
          {metric !== "l2" && <line x1={0} y1={0} x2={toPx(q)[0]} y2={toPx(q)[1]} className="q-ray" />}
          {ITEMS.map((it) => {
            const [x, y] = toPx(it.v);
            return (
              <g key={it.label}>
                <circle cx={x} cy={y} r={top.has(it.label) ? 5 : 3.5} className={top.has(it.label) ? "pt top" : "pt"} />
                <text x={x + 6} y={y + 3} className="pt-label">
                  {it.label}
                </text>
              </g>
            );
          })}
          <circle cx={toPx(q)[0]} cy={toPx(q)[1]} r={6} className="pt query" />
        </svg>
        <ol className="bw-rank">
          {ranked.slice(0, 5).map((r) => (
            <li key={r.label} className={top.has(r.label) ? "top" : ""}>
              <span>{r.label}</span>
              <span className="mono faint">{r.d.toFixed(3)}</span>
            </li>
          ))}
        </ol>
      </div>
      <div className="bw-caption">
        {metric === "cosine" ? "Cosine compares directions from the origin — length doesn't matter." : metric === "l2" ? "L2 measures straight-line distance between the points." : "Inner product rewards both direction and length (pgvector's <#> returns it negated so smaller is closer)."}
      </div>
    </div>
  );
}
