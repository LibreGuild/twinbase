// Redis browser: key list with pattern/type filters, typed value editors and a command console.

import { Clock, KeyRound, Plus, RefreshCw, Save, Search, Terminal, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { ConfirmDialog, Dialog, Empty, ErrorBox, Field, Spinner, useAsync } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { attempt, targetInfo, useStore, type Tab } from "../lib/store";
import type { RedisKey, RedisValue } from "../lib/types";

type RedisTab = Extract<Tab, { kind: "redis" }>;
const TYPES = ["", "string", "hash", "list", "set", "zset"];

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
  const [pattern, setPattern] = useState("*");
  const [type, setType] = useState("");
  const [keys, setKeys] = useState<RedisKey[]>([]);
  const [cursor, setCursor] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>();
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [showConsole, setShowConsole] = useState(true);
  const info = useAsync(() => call<{ info: string; keys: number }>("redis.info", { connection }), [connection, dataVersion]);

  const scan = useCallback(
    async (from: number, append: boolean) => {
      setLoading(true);
      try {
        const r = await call<{ cursor: number; keys: RedisKey[] }>("redis.scan", { connection, pattern: pattern || "*", type, cursor: from, count: 500 });
        setKeys((k) => (append ? [...k, ...r.keys] : r.keys.sort((a, b) => a.key.localeCompare(b.key))));
        setCursor(r.cursor);
        setError(undefined);
      } catch (e) {
        setError(e);
      } finally {
        setLoading(false);
      }
    },
    [connection, pattern, type],
  );

  useEffect(() => {
    scan(0, false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection, type, dataVersion]);

  const version = info.data?.info.split("\n").find((l) => l.startsWith("redis_version"))?.split(":")[1]?.trim();

  return (
    <>
      <div className="toolbar">
        <div className="title">
          <KeyRound size={15} className="kind-redis" /> {target?.name ?? connection}
        </div>
        {version && <span className="badge">Redis {version}</span>}
        {info.data && <span className="badge">{info.data.keys.toLocaleString()} keys</span>}
        {readOnly && <span className="badge warning">read-only</span>}
        <div className="grow" />
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
            ) : (
              keys.map((k) => (
                <div key={k.key} className={`key-item${selected === k.key ? " on" : ""}`} onClick={() => setSelected(k.key)} title={k.key}>
                  <span className={`type-badge t-${k.type}`}>{k.type}</span>
                  <span className="k">{k.key}</span>
                  {k.ttl_ms >= 0 && <Clock size={11} className="faint" aria-label={`expires in ${ttlText(k.ttl_ms)}`} />}
                </div>
              ))
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
            {selected ? (
              <KeyEditor key={`${selected}|${dataVersion}`} connection={connection} name={selected} readOnly={readOnly} onDeleted={() => (setSelected(null), scan(0, false))} />
            ) : (
              <Empty icon={<KeyRound size={26} />} title="Select a key">
                Or type commands in the console below.
              </Empty>
            )}
          </div>
          {showConsole && <Console connection={connection} />}
        </div>
      </div>
      {creating && (
        <NewKeyDialog
          connection={connection}
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

function KeyEditor({ connection, name, readOnly, onDeleted }: { connection: string; name: string; readOnly: boolean; onDeleted: () => void }) {
  const toast = useStore((s) => s.toast);
  const value = useAsync(() => call<RedisValue>("redis.get", { connection, key: name }), [connection, name]);
  const meta = useAsync(async () => (await call<{ keys: RedisKey[] }>("redis.scan", { connection, pattern: name.replace(/([*?[\]\\])/g, "\\$1"), count: 10 })).keys.find((k) => k.key === name), [connection, name]);
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
    const r = await attempt("Could not save key", () => call("redis.set", { connection, key: name, value: draft, ttl_ms: ttlMs }));
    if (r) {
      toast("success", "Saved", name);
      value.reload();
    }
  };

  const setExpiry = async (ms: number | null) => {
    const r = await attempt("Could not set TTL", () => call("redis.expire", { connection, key: name, ttl_ms: ms }));
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

  const count = draft.type === "hash" ? draft.fields.length : draft.type === "list" ? draft.items.length : draft.type === "set" || draft.type === "zset" ? draft.members.length : draft.value.length;

  return (
    <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="row">
        <span className={`type-badge t-${draft.type}`}>{draft.type}</span>
        <strong className="mono ellipsis grow selectable" title={name}>
          {name}
        </strong>
        <span className="faint xs">{draft.type === "string" ? `${count} chars` : `${count} items`}</span>
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
      {!readOnly && (
        <div className="row">
          {draft.type !== "string" && (
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
            const r = await attempt("Could not delete key", () => call("redis.delete", { connection, keys: [name] }));
            if (r) onDeleted();
          }}
        />
      )}
    </div>
  );
}

function NewKeyDialog({ connection, onClose, onCreated }: { connection: string; onClose: () => void; onCreated: (key: string) => void }) {
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
      await call("redis.set", { connection, key, value, ttl_ms: ttl ? Number(ttl) * 1000 : undefined });
      onCreated(key);
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const hint = { string: "The value.", hash: "One field=value per line.", list: "One item per line.", set: "One member per line.", zset: "One “member score” per line.", none: "" }[type];
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
            {TYPES.filter(Boolean).map((t) => (
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

function Console({ connection }: { connection: string }) {
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
      const out = await call("redis.command", { connection, command: cmd });
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
