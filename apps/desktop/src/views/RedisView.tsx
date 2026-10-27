// Redis browser: numbered databases, key list or namespace tree with pattern/type filters, typed
// value editors (strings, hashes, lists, sets, sorted sets, streams, JSON), server info and a
// command console.

import { ChevronDown, ChevronRight, Clock, Folder, Info, KeyRound, List, ListTree, Plus, RefreshCw, Save, Search, Terminal, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ConfirmDialog, Dialog, Empty, ErrorBox, Field, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { attempt, targetInfo, useStore, type Tab } from "../lib/store";
import type { RedisDatabase, RedisKey, RedisValue } from "../lib/types";

type RedisTab = Extract<Tab, { kind: "redis" }>;
const TYPES = ["", "string", "hash", "list", "set", "zset", "stream"];
const NEW_TYPES = ["string", "hash", "list", "set", "zset"] as const;

/** Keys grouped by `:` into folders (the usual Redis naming convention). */
interface KeyFolder {
  name: string;
  path: string;
  folders: Map<string, KeyFolder>;
  keys: RedisKey[];
  count: number;
}

function buildTree(keys: RedisKey[]): KeyFolder {
  const root: KeyFolder = { name: "", path: "", folders: new Map(), keys: [], count: 0 };
  for (const k of keys) {
    const parts = k.key.split(":");
    let node = root;
    node.count++;
    for (let i = 0; i < parts.length - 1; i++) {
      const path = parts.slice(0, i + 1).join(":");
      let next = node.folders.get(parts[i]);
      if (!next) {
        next = { name: parts[i], path, folders: new Map(), keys: [], count: 0 };
        node.folders.set(parts[i], next);
      }
      next.count++;
      node = next;
    }
    node.keys.push(k);
  }
  return root;
}

function ttlText(ms: number): string {
  if (ms < 0) return "no expiry";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

export function RedisView({ tab }: { tab: RedisTab }) {
  const { connection } = tab;
  const target = targetInfo(connection);
  const readOnly = !!target?.readOnly;
  const dataVersion = useStore((s) => s.dataVersion[connection] ?? 0);
  const updateTab = useStore((s) => s.updateTab);
  const [db, setDbState] = useState<number | undefined>(tab.db);
  const setDb = (n: number) => {
    setDbState(n);
    setSelected(null);
    updateTab(tab.id, { db: n });
  };
  const [mode, setMode] = useState<"list" | "tree">(() => (localStorage.getItem("twinbase.redis.mode") === "list" ? "list" : "tree"));
  const [showInfo, setShowInfo] = useState(false);
  const databases = useAsync(() => call<{ count: number; default: number; databases: RedisDatabase[] }>("redis.databases", { connection }), [connection, dataVersion]);
  const [pattern, setPattern] = useState("*");
  const [type, setType] = useState("");
  const [keys, setKeys] = useState<RedisKey[]>([]);
  const [cursor, setCursor] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>();
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [showConsole, setShowConsole] = useState(true);
  const info = useAsync(() => call<{ info: string; keys: number; db: number }>("redis.info", { connection, db }), [connection, dataVersion, db]);

  const scan = useCallback(
    async (from: number, append: boolean) => {
      setLoading(true);
      try {
        const r = await call<{ cursor: number; keys: RedisKey[] }>("redis.scan", { connection, db, pattern: pattern || "*", type, cursor: from, count: 500 });
        setKeys((k) => (append ? [...k, ...r.keys] : r.keys.sort((a, b) => a.key.localeCompare(b.key))));
        setCursor(r.cursor);
        setError(undefined);
      } catch (e) {
        setError(e);
      } finally {
        setLoading(false);
      }
    },
    [connection, pattern, type, db],
  );

  useEffect(() => {
    scan(0, false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection, type, dataVersion, db]);
  const tree = useMemo(() => buildTree(keys), [keys]);
  const currentDb = db ?? databases.data?.default ?? 0;
  const dbChoices = useMemo(() => {
    const n = Math.min(databases.data?.count ?? 16, 64);
    const counts = new Map((databases.data?.databases ?? []).map((d) => [d.db, d.keys]));
    return Array.from({ length: n }, (_, i) => ({ db: i, keys: counts.get(i) ?? 0 }));
  }, [databases.data]);

  const version = info.data?.info.split("\n").find((l) => l.startsWith("redis_version"))?.split(":")[1]?.trim();

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <KeyRound size={15} className="kind-redis" /> {target?.name ?? connection}
        </div>
        {version && <span className="badge">Redis {version}</span>}
        <select className="select sm" value={currentDb} onChange={(e) => setDb(Number(e.target.value))} aria-label="Database" title="Numbered database (SELECT n)">
          {dbChoices.map((d) => (
            <option key={d.db} value={d.db}>
              db{d.db}
              {d.keys ? ` · ${d.keys.toLocaleString()} keys` : ""}
            </option>
          ))}
        </select>
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
        <div className="seg" role="group" aria-label="Key layout">
          <button className={mode === "tree" ? "on" : ""} title="Group keys by ':' namespaces" onClick={() => (setMode("tree"), localStorage.setItem("twinbase.redis.mode", "tree"))}>
            <ListTree size={13} /> Tree
          </button>
          <button className={mode === "list" ? "on" : ""} title="Flat key list" onClick={() => (setMode("list"), localStorage.setItem("twinbase.redis.mode", "list"))}>
            <List size={13} /> List
          </button>
        </div>
        <button className={`btn sm ghost${showInfo ? " on" : ""}`} onClick={() => setShowInfo((v) => !v)}>
          <Info size={13} /> Server
        </button>
        <button className={`btn sm ghost${showConsole ? " on" : ""}`} onClick={() => setShowConsole((v) => !v)}>
          <Terminal size={13} /> Console
        </button>
        {!readOnly && (
          <button className="btn sm primary" onClick={() => setCreating(true)}>
            <Plus size={13} /> Key
          </button>
        )}
      </div>
      <div className="redis-layout">
        <div className="key-list">
          <form
            className="row"
            style={{ padding: 8, borderBottom: "1px solid var(--border)" }}
            onSubmit={(e) => {
              e.preventDefault();
              scan(0, false);
            }}
          >
            <div className="row grow" style={{ position: "relative" }}>
              <Search size={13} className="faint" style={{ position: "absolute", left: 8 }} />
              <input className="input sm mono grow" style={{ paddingLeft: 26 }} value={pattern} onChange={(e) => setPattern(e.target.value)} placeholder="pattern, e.g. user:*" aria-label="Key pattern" />
            </div>
            <select className="select sm" value={type} onChange={(e) => setType(e.target.value)} aria-label="Key type">
              {TYPES.map((t) => (
                <option key={t} value={t}>
                  {t || "all types"}
                </option>
              ))}
            </select>
            <button className="icon-btn" type="submit" title="Scan" aria-label="Scan">
              {loading ? <Spinner size={13} /> : <RefreshCw size={13} />}
            </button>
          </form>
          <div style={{ flex: 1, overflow: "auto" }}>
            {error ? (
              <div style={{ padding: 10 }}>
                <ErrorBox error={error} />
              </div>
            ) : keys.length === 0 && !loading ? (
              <Empty icon={<KeyRound size={22} />} title="No keys">
                {pattern !== "*" ? "Nothing matches this pattern." : "This database is empty."}
              </Empty>
            ) : mode === "tree" ? (
              <KeyTree node={tree} depth={0} selected={selected} onSelect={setSelected} />
            ) : (
              keys.map((k) => <KeyRow key={k.key} k={k} label={k.key} depth={0} selected={selected === k.key} onSelect={setSelected} />)
            )}
            {cursor !== 0 && (
              <div style={{ padding: 8 }}>
                <button className="btn sm" style={{ width: "100%" }} onClick={() => scan(cursor, true)} disabled={loading}>
                  Load more
                </button>
              </div>
            )}
          </div>
          <div className="grid-footer">{keys.length.toLocaleString()} shown</div>
        </div>
        <div style={{ flex: 1, minWidth: 0, display: "flex", flexDirection: "column" }}>
          <div style={{ flex: 1, minHeight: 0, overflow: "auto" }}>
            {showInfo ? (
              <ServerInfo connection={connection} db={db} onClose={() => setShowInfo(false)} />
            ) : selected ? (
              <KeyEditor key={`${selected}|${dataVersion}|${currentDb}`} connection={connection} db={db} name={selected} readOnly={readOnly} onDeleted={() => (setSelected(null), scan(0, false))} />
            ) : (
              <Empty icon={<KeyRound size={26} />} title="Select a key">
                Or type commands in the console below.
              </Empty>
            )}
          </div>
          {showConsole && <Console connection={connection} db={db} />}
        </div>
      </div>
      {creating && (
        <NewKeyDialog
          connection={connection}
          db={db}
          onClose={() => setCreating(false)}
          onCreated={(k) => {
            setCreating(false);
            setSelected(k);
            scan(0, false);
          }}
        />
      )}
    </>
  );
}

function KeyRow({ k, label, depth, selected, onSelect }: { k: RedisKey; label: string; depth: number; selected: boolean; onSelect: (key: string) => void }) {
  return (
    <div className={`key-item${selected ? " on" : ""}`} style={{ paddingLeft: 10 + depth * 14 }} onClick={() => onSelect(k.key)} title={k.key}>
      <span className={`type-badge t-${k.type}`}>{k.type}</span>
      <span className="k">{label}</span>
      {k.ttl_ms >= 0 && <Clock size={11} className="faint" aria-label={`expires in ${ttlText(k.ttl_ms)}`} />}
    </div>
  );
}

function KeyTree({ node, depth, selected, onSelect }: { node: KeyFolder; depth: number; selected: string | null; onSelect: (key: string) => void }) {
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const folders = [...node.folders.values()].sort((a, b) => a.name.localeCompare(b.name));
  return (
    <>
      {folders.map((f) => {
        const isOpen = open.has(f.path) || node.folders.size === 1;
        return (
          <div key={f.path}>
            <div
              className="key-item key-folder"
              style={{ paddingLeft: 6 + depth * 14 }}
              onClick={() =>
                setOpen((s) => {
                  const n = new Set(s);
                  if (n.has(f.path)) n.delete(f.path);
                  else n.add(f.path);
                  return n;
                })
              }
            >
              {isOpen ? <ChevronDown size={12} className="faint" /> : <ChevronRight size={12} className="faint" />}
              <Folder size={13} className="kind-redis" />
              <span className="k">{f.name}</span>
              <span className="faint xs">{f.count.toLocaleString()}</span>
            </div>
            {isOpen && <KeyTree node={f} depth={depth + 1} selected={selected} onSelect={onSelect} />}
          </div>
        );
      })}
      {[...node.keys]
        .sort((a, b) => a.key.localeCompare(b.key))
        .map((k) => (
          <KeyRow key={k.key} k={k} label={node.path ? k.key.slice(node.path.length + 1) : k.key} depth={depth} selected={selected === k.key} onSelect={onSelect} />
        ))}
    </>
  );
}

/** `INFO everything`, one card per section. */
function ServerInfo({ connection, db, onClose }: { connection: string; db?: number; onClose: () => void }) {
  const info = useAsync(() => call<{ info: string }>("redis.info", { connection, db, section: "everything" }), [connection, db]);
  const sections = useMemo(() => {
    const out: { name: string; rows: [string, string][] }[] = [];
    for (const line of (info.data?.info ?? "").split(/\r?\n/)) {
      if (line.startsWith("# ")) out.push({ name: line.slice(2), rows: [] });
      else if (line.includes(":") && out.length) {
        const i = line.indexOf(":");
        out[out.length - 1].rows.push([line.slice(0, i), line.slice(i + 1)]);
      }
    }
    return out.filter((s) => s.rows.length);
  }, [info.data]);
  return (
    <div style={{ padding: "12px 14px" }}>
      <div className="row" style={{ marginBottom: 10 }}>
        <Info size={15} className="kind-redis" />
        <strong>Server information</strong>
        <span className="grow" />
        <button className="icon-btn" onClick={() => info.reload()} title="Refresh" aria-label="Refresh">
          <RefreshCw size={13} />
        </button>
        <button className="icon-btn" onClick={onClose} title="Close" aria-label="Close">
          <X size={14} />
        </button>
      </div>
      {info.error ? <ErrorBox error={info.error} /> : !info.data ? <Spinner /> : null}
      <div className="info-grid">
        {sections.map((s) => (
          <div key={s.name} className="card info-card">
            <div className="card-title">{s.name}</div>
            <dl className="kv">
              {s.rows.slice(0, 40).map(([k, v]) => (
                <div key={k} style={{ display: "contents" }}>
                  <dt className="mono">{k}</dt>
                  <dd className="mono">{v}</dd>
                </div>
              ))}
            </dl>
          </div>
        ))}
      </div>
    </div>
  );
}

function KeyEditor({ connection, db, name, readOnly, onDeleted }: { connection: string; db?: number; name: string; readOnly: boolean; onDeleted: () => void }) {
  const toast = useStore((s) => s.toast);
  const value = useAsync(() => call<RedisValue>("redis.get", { connection, db, key: name }), [connection, name, db]);
  const meta = useAsync(async () => (await call<{ keys: RedisKey[] }>("redis.scan", { connection, db, pattern: name.replace(/([*?[\]\\])/g, "\\$1"), count: 10 })).keys.find((k) => k.key === name), [connection, name, db]);
  const [draft, setDraft] = useState<RedisValue | null>(null);
  const [ttl, setTtl] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  useEffect(() => {
    setDraft(value.data ?? null);
  }, [value.data]);
  const dirty = JSON.stringify(draft) !== JSON.stringify(value.data);

  const save = async () => {
    if (!draft) return;
    const ttlMs = meta.data && meta.data.ttl_ms > 0 ? meta.data.ttl_ms : undefined;
    const r = await attempt("Could not save key", () => call("redis.set", { connection, db, key: name, value: draft, ttl_ms: ttlMs }));
    if (r) {
      toast("success", "Saved", name);
      value.reload();
    }
  };

  const setExpiry = async (ms: number | null) => {
    const r = await attempt("Could not set TTL", () => call("redis.expire", { connection, db, key: name, ttl_ms: ms }));
    if (r) meta.reload();
  };

  if (value.error) return <div style={{ padding: 14 }}><ErrorBox error={value.error} /></div>;
  if (!draft) return <Empty icon={<Spinner />} title="Loading…" />;
  if (draft.type === "none") return <Empty title="Key no longer exists" />;

  const rows = (items: string[][], headers: string[], onChange: (items: string[][]) => void) => (
    <table className="simple-table">
      <thead>
        <tr>
          {headers.map((h) => (
            <th key={h}>{h}</th>
          ))}
          <th style={{ width: 30 }} />
        </tr>
      </thead>
      <tbody>
        {items.map((it, i) => (
          <tr key={i}>
            {it.map((v, j) => (
              <td key={j} style={{ padding: 3 }}>
                <input
                  className="input sm mono"
                  style={{ width: "100%" }}
                  value={v}
                  readOnly={readOnly || (headers[0] === "#" && j === 0)}
                  onChange={(e) => onChange(items.map((row, x) => (x === i ? row.map((c, y) => (y === j ? e.target.value : c)) : row)))}
                  aria-label={headers[j]}
                />
              </td>
            ))}
            <td>
              {!readOnly && (
                <button className="icon-btn sm" onClick={() => onChange(items.filter((_, x) => x !== i))} aria-label="Remove">
                  <X size={12} />
                </button>
              )}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );

  let body: React.ReactNode;
  switch (draft.type) {
    case "string":
      body = <textarea className="textarea mono" style={{ width: "100%", minHeight: 240 }} value={draft.value} readOnly={readOnly} onChange={(e) => setDraft({ type: "string", value: e.target.value })} aria-label="Value" />;
      break;
    case "hash":
      body = rows(draft.fields, ["field", "value"], (items) => setDraft({ type: "hash", fields: items as [string, string][] }));
      break;
    case "list":
      body = rows(draft.items.map((v, i) => [String(i), v]), ["#", "value"], (items) => setDraft({ type: "list", items: items.map((r) => r[1]) }));
      break;
    case "set":
      body = rows(draft.members.map((m) => [m]), ["member"], (items) => setDraft({ type: "set", members: items.map((r) => r[0]) }));
      break;
    case "zset":
      body = rows(draft.members.map(([m, s]) => [m, String(s)]), ["member", "score"], (items) => setDraft({ type: "zset", members: items.map((r) => [r[0], Number(r[1]) || 0]) }));
      break;
    case "stream":
      body = (
        <>
          <table className="simple-table">
            <thead>
              <tr>
                <th style={{ width: 190 }}>id</th>
                <th>fields</th>
              </tr>
            </thead>
            <tbody>
              {draft.entries.map(([id, fields]) => (
                <tr key={id}>
                  <td className="mono">{id}</td>
                  <td className="mono">
                    {fields.map(([f, v]) => (
                      <span key={f} className="stream-field">
                        <span className="faint">{f}</span> {v}
                      </span>
                    ))}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <div className="faint xs">Streams are append-only here: add entries with XADD in the console.</div>
        </>
      );
      break;
    case "json":
      body = <textarea className="textarea mono" style={{ width: "100%", minHeight: 240 }} value={draft.value} readOnly={readOnly} onChange={(e) => setDraft({ type: "json", value: e.target.value })} aria-label="JSON document" />;
      break;
  }

  const addItem = () => {
    switch (draft.type) {
      case "hash":
        return setDraft({ ...draft, fields: [...draft.fields, ["", ""]] });
      case "list":
        return setDraft({ ...draft, items: [...draft.items, ""] });
      case "set":
        return setDraft({ ...draft, members: [...draft.members, ""] });
      case "zset":
        return setDraft({ ...draft, members: [...draft.members, ["", 0]] });
    }
  };

  const count =
    draft.type === "hash" ? draft.fields.length : draft.type === "list" ? draft.items.length : draft.type === "set" || draft.type === "zset" ? draft.members.length : draft.type === "stream" ? draft.entries.length : draft.value.length;
  const editable = !readOnly && draft.type !== "stream";

  return (
    <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="row">
        <span className={`type-badge t-${draft.type}`}>{draft.type}</span>
        <strong className="mono ellipsis grow selectable" title={name}>
          {name}
        </strong>
        <span className="faint xs">{draft.type === "string" || draft.type === "json" ? `${count} chars` : draft.type === "stream" ? `${count} entries` : `${count} items`}</span>
      </div>
      <div className="row wrap">
        <span className="badge">
          <Clock size={11} /> {meta.data ? ttlText(meta.data.ttl_ms) : "…"}
        </span>
        {!readOnly && (
          <>
            <input className="input sm" style={{ width: 110 }} placeholder="TTL seconds" value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ""))} aria-label="TTL in seconds" />
            <button className="btn sm" disabled={!ttl} onClick={() => setExpiry(Number(ttl) * 1000)}>
              Set TTL
            </button>
            {meta.data && meta.data.ttl_ms >= 0 && (
              <button className="btn sm ghost" onClick={() => setExpiry(null)}>
                Persist
              </button>
            )}
            <span className="grow" />
            <button className="btn sm danger" onClick={() => setConfirmDelete(true)}>
              <Trash2 size={13} /> Delete
            </button>
          </>
        )}
      </div>
      {body}
      {editable && (
        <div className="row">
          {draft.type !== "string" && draft.type !== "json" && (
            <button className="btn sm" onClick={addItem}>
              <Plus size={13} /> Add
            </button>
          )}
          <span className="grow" />
          {dirty && (
            <button className="btn sm ghost" onClick={() => setDraft(value.data ?? null)}>
              Revert
            </button>
          )}
          <button className="btn sm primary" disabled={!dirty} onClick={save}>
            <Save size={13} /> Save
          </button>
        </div>
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete key?"
          body={name}
          confirmLabel="Delete"
          danger
          onClose={() => setConfirmDelete(false)}
          onConfirm={async () => {
            const r = await attempt("Could not delete key", () => call("redis.delete", { connection, db, keys: [name] }));
            if (r) onDeleted();
          }}
        />
      )}
    </div>
  );
}

function NewKeyDialog({ connection, db, onClose, onCreated }: { connection: string; db?: number; onClose: () => void; onCreated: (key: string) => void }) {
  const [key, setKey] = useState("");
  const [type, setType] = useState<RedisValue["type"]>("string");
  const [text, setText] = useState("");
  const [ttl, setTtl] = useState("");
  const [error, setError] = useState<string>();
  const create = async () => {
    const lines = text.split("\n").filter((l) => l.length);
    let value: RedisValue;
    switch (type) {
      case "hash":
        value = { type, fields: lines.map((l) => { const i = l.indexOf("="); return (i < 0 ? [l, ""] : [l.slice(0, i), l.slice(i + 1)]) as [string, string]; }) };
        break;
      case "list":
        value = { type, items: lines };
        break;
      case "set":
        value = { type, members: lines };
        break;
      case "zset":
        value = { type, members: lines.map((l) => { const [m, s] = l.split(/\s+(?=[-\d.]+$)/); return [m, Number(s) || 0] as [string, number]; }) };
        break;
      default:
        value = { type: "string", value: text };
    }
    try {
      await call("redis.set", { connection, db, key, value, ttl_ms: ttl ? Number(ttl) * 1000 : undefined });
      onCreated(key);
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const hint = { string: "The value.", hash: "One field=value per line.", list: "One item per line.", set: "One member per line.", zset: "One “member score” per line.", stream: "", json: "", none: "" }[type];
  return (
    <Dialog
      title="New key"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!key} onClick={create}>
            Create
          </button>
        </>
      }
    >
      <div className="grid-3">
        <Field label="Key">
          <input className="input mono" autoFocus value={key} onChange={(e) => setKey(e.target.value)} placeholder="user:1" />
        </Field>
        <Field label="Type">
          <select className="select" value={type} onChange={(e) => setType(e.target.value as RedisValue["type"])}>
            {NEW_TYPES.map((t) => (
              <option key={t}>{t}</option>
            ))}
          </select>
        </Field>
        <Field label="TTL (seconds)">
          <input className="input" value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ""))} placeholder="none" />
        </Field>
      </div>
      <Field label="Value" hint={hint}>
        <textarea className="textarea mono" rows={7} value={text} onChange={(e) => setText(e.target.value)} />
      </Field>
      <ErrorBox error={error} />
    </Dialog>
  );
}

function formatReply(v: unknown, indent = ""): string {
  if (v === null || v === undefined) return `${indent}(nil)`;
  if (Array.isArray(v)) return v.length ? v.map((x, i) => `${indent}${i + 1}) ${formatReply(x, "").replace(/\n/g, `\n${indent}   `)}`).join("\n") : `${indent}(empty array)`;
  if (typeof v === "object") return JSON.stringify(v, null, 2);
  if (typeof v === "number") return `(integer) ${v}`;
  return `"${String(v)}"`;
}

export function formatRedisReply(v: unknown): string {
  return formatReply(v);
}

function Console({ connection, db }: { connection: string; db?: number }) {
  const [log, setLog] = useState<{ cmd: string; out?: string; err?: string }[]>([]);
  const [input, setInput] = useState("");
  const [hist, setHist] = useState<string[]>([]);
  const [pos, setPos] = useState(-1);
  const logRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [log]);
  const run = async () => {
    const cmd = input.trim();
    if (!cmd) return;
    setInput("");
    setHist((h) => [cmd, ...h.filter((x) => x !== cmd)].slice(0, 100));
    setPos(-1);
    if (/^clear$/i.test(cmd)) return setLog([]);
    try {
      const out = await call("redis.command", { connection, db, command: cmd });
      setLog((l) => [...l.slice(-300), { cmd, out: formatReply(out) }]);
    } catch (e) {
      setLog((l) => [...l.slice(-300), { cmd, err: errorMessage(e) }]);
    }
  };
  return (
    <div className="console">
      <div className="console-log" ref={logRef}>
        {log.length === 0 && <div className="faint">Type a Redis command, e.g. HGETALL user:1 — “clear” empties this log.</div>}
        {log.map((l, i) => (
          <div key={i}>
            <div className="cmd">&gt; {l.cmd}</div>
            {l.out !== undefined && <div className="out">{l.out}</div>}
            {l.err && <div className="err">(error) {l.err}</div>}
          </div>
        ))}
      </div>
      <div className="console-input">
        <span className="kind-redis mono">&gt;</span>
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="command"
          aria-label="Redis command"
          onKeyDown={(e) => {
            if (e.key === "Enter") run();
            else if (e.key === "ArrowUp" && hist.length) {
              const p = Math.min(hist.length - 1, pos + 1);
              setPos(p);
              setInput(hist[p]);
              e.preventDefault();
            } else if (e.key === "ArrowDown") {
              const p = pos - 1;
              setPos(Math.max(-1, p));
              setInput(p >= 0 ? hist[p] : "");
              e.preventDefault();
            }
          }}
        />
      </div>
    </div>
  );
}

type QueryTab = Extract<Tab, { kind: "query" }>;

/** Query tab on a Redis connection: a script of commands, run line by line. */
export function RedisConsoleView({ tab }: { tab: QueryTab }) {
  const connection = tab.connection!;
  const updateTab = useStore((s) => s.updateTab);
  const setDirty = useStore((s) => s.setDirty);
  const [script, setScript] = useState(tab.sql || "PING\nINFO keyspace\nSCAN 0 MATCH * COUNT 20");
  const [db, setDb] = useState<number | undefined>(undefined);
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<{ cmd: string; out?: string; err?: string; ms: number }[]>([]);
  const databases = useAsync(() => call<{ count: number; default: number; databases: RedisDatabase[] }>("redis.databases", { connection }), [connection]);
  const editor = useRef<HTMLTextAreaElement>(null);

  const run = async (onlySelection: boolean) => {
    const el = editor.current;
    let text = script;
    if (onlySelection && el && el.selectionStart !== el.selectionEnd) text = script.slice(el.selectionStart, el.selectionEnd);
    else if (onlySelection && el) {
      // The line under the cursor.
      const start = script.lastIndexOf("\n", el.selectionStart - 1) + 1;
      const end = script.indexOf("\n", el.selectionStart);
      text = script.slice(start, end < 0 ? undefined : end);
    }
    const lines = text.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#") && !l.startsWith("//"));
    if (!lines.length) return;
    setBusy(true);
    const out: typeof results = [];
    for (const cmd of lines) {
      const t0 = performance.now();
      try {
        const r = await call("redis.command", { connection, db, command: cmd });
        out.push({ cmd, out: formatReply(r), ms: performance.now() - t0 });
      } catch (e) {
        out.push({ cmd, err: errorMessage(e), ms: performance.now() - t0 });
        break;
      }
    }
    setResults(out);
    setBusy(false);
  };

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <Terminal size={15} className="kind-redis" /> {targetInfo(connection)?.name ?? connection}
        </div>
        <select className="select sm" value={db ?? databases.data?.default ?? 0} onChange={(e) => setDb(Number(e.target.value))} aria-label="Database">
          {Array.from({ length: Math.min(databases.data?.count ?? 16, 64) }, (_, i) => (
            <option key={i} value={i}>
              db{i}
              {(() => {
                const n = databases.data?.databases.find((d) => d.db === i)?.keys;
                return n ? ` · ${n.toLocaleString()} keys` : "";
              })()}
            </option>
          ))}
        </select>
        <span className="grow" />
        <span className="faint xs">⌘/Ctrl+Enter runs the current line · ⇧ runs all</span>
        <button className="btn sm" disabled={busy} onClick={() => run(true)}>
          Run line
        </button>
        <button className="btn sm primary" disabled={busy} onClick={() => run(false)}>
          {busy ? <Spinner size={12} /> : null} Run all
        </button>
      </div>
      <div className="split-v">
        <textarea
          ref={editor}
          className="redis-script mono"
          value={script}
          spellCheck={false}
          aria-label="Redis commands"
          onChange={(e) => {
            setScript(e.target.value);
            updateTab(tab.id, { sql: e.target.value });
            setDirty(tab.id, true);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              run(!e.shiftKey);
            }
          }}
        />
        <div className="redis-results">
          {results.length === 0 && <div className="faint" style={{ padding: 12 }}>One command per line; lines starting with # are comments. Results appear here.</div>}
          {results.map((r, i) => (
            <div key={i} className="redis-result">
              <div className="cmd mono">
                &gt; {r.cmd} <span className="faint xs">{r.ms.toFixed(1)} ms</span>
              </div>
              {r.out !== undefined && <pre className="out">{r.out}</pre>}
              {r.err && <pre className="err">(error) {r.err}</pre>}
            </div>
          ))}
        </div>
      </div>
    </>
  );
}
