// Fault injection editor for a mock server: latency, errors, dropped and refused connections.

import { ArrowDown, ArrowUp, Plus, Save, Trash2, Zap } from "lucide-react";
import { useEffect, useState } from "react";
import { ErrorBox, Spinner } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { useStore } from "../lib/store";
import type { FaultAction, FaultConfig, FaultRule, MockKind, MockStatus } from "../lib/types";

const EMPTY: FaultConfig = { enabled: false, refuse_connections: false, rules: [] };

/** Typical error codes per protocol, offered as suggestions. */
export const ERROR_CODES: Record<MockKind, { code: string; label: string }[]> = {
  postgres: [
    { code: "40001", label: "serialization failure" },
    { code: "40P01", label: "deadlock detected" },
    { code: "57014", label: "statement timeout / canceled" },
    { code: "53300", label: "too many connections" },
    { code: "25006", label: "read-only transaction" },
    { code: "08006", label: "connection failure" },
  ],
  mysql: [
    { code: "1213", label: "deadlock" },
    { code: "1205", label: "lock wait timeout" },
    { code: "1040", label: "too many connections" },
    { code: "1290", label: "read-only server" },
    { code: "3024", label: "max execution time exceeded" },
  ],
  mssql: [
    { code: "1205", label: "deadlock victim" },
    { code: "1222", label: "lock request timeout" },
    { code: "40613", label: "database unavailable (Azure, transient)" },
    { code: "40501", label: "service busy (Azure throttling)" },
    { code: "3960", label: "snapshot isolation update conflict" },
    { code: "3906", label: "database is read-only" },
  ],
  redis: [
    { code: "READONLY", label: "read-only replica" },
    { code: "BUSY", label: "busy running a script" },
    { code: "LOADING", label: "loading dataset" },
    { code: "OOM", label: "out of memory" },
  ],
  mongodb: [
    { code: "11000", label: "duplicate key" },
    { code: "112", label: "write conflict" },
    { code: "50", label: "operation exceeded time limit" },
    { code: "189", label: "primary stepped down (retryable)" },
    { code: "91", label: "shutdown in progress (retryable)" },
    { code: "10107", label: "not writable primary" },
  ],
  dynamodb: [
    { code: "ProvisionedThroughputExceededException", label: "throttled (SDKs retry)" },
    { code: "ThrottlingException", label: "request rate too high" },
    { code: "TransactionConflictException", label: "transaction conflict" },
    { code: "RequestLimitExceeded", label: "account request limit" },
    { code: "InternalServerError", label: "internal error (HTTP 500)" },
    { code: "ServiceUnavailable", label: "service unavailable (HTTP 503)" },
  ],
  firestore: [
    { code: "UNAVAILABLE", label: "unavailable (SDKs retry)" },
    { code: "ABORTED", label: "transaction contention (retried)" },
    { code: "DEADLINE_EXCEEDED", label: "deadline exceeded" },
    { code: "RESOURCE_EXHAUSTED", label: "quota exceeded" },
    { code: "PERMISSION_DENIED", label: "security rules denied" },
    { code: "INTERNAL", label: "internal error" },
  ],
};

function presets(kind: MockKind): { label: string; rules: FaultRule[]; refuse?: boolean }[] {
  const writes =
    kind === "redis"
      ? "/^(set|hset|del|incr|lpush|rpush|sadd|zadd|expire)\\b/"
      : kind === "mongodb"
        ? "/\\.(insert|update|replace|delete|findOneAnd)/"
        : kind === "dynamodb"
          ? "/^(PutItem|UpdateItem|DeleteItem|BatchWriteItem|TransactWriteItems)\\b/"
          : kind === "firestore"
            ? "/^(Commit|UpdateDocument|CreateDocument|DeleteDocument|BatchWrite)\\b/"
          : "/^\\s*(insert|update|delete)\\b/";
  const deadlock = kind === "postgres" ? "40P01" : kind === "mysql" ? "1213" : kind === "mssql" ? "1205" : kind === "mongodb" ? "112" : kind === "dynamodb" ? "TransactionConflictException" : kind === "firestore" ? "ABORTED" : "BUSY";
  const readonly = kind === "postgres" ? "25006" : kind === "mysql" ? "1290" : kind === "mssql" ? "3906" : kind === "mongodb" ? "10107" : kind === "dynamodb" ? "ProvisionedThroughputExceededException" : kind === "firestore" ? "PERMISSION_DENIED" : "READONLY";
  const r = (matches: string, probability: number, action: FaultAction, name: string): FaultRule => ({ enabled: true, name, matches, probability, action });
  return [
    { label: "Slow database (300 ms)", rules: [r("", 1, { type: "delay", ms: 300, jitter_ms: 100 }, "slow")] },
    { label: "Latency spikes (10% · 3 s)", rules: [r("", 0.1, { type: "delay", ms: 3000, jitter_ms: 1000 }, "spikes")] },
    { label: "Deadlocks on writes (10%)", rules: [r(writes, 0.1, { type: "error", code: deadlock }, "deadlocks")] },
    { label: "Read-only (writes fail)", rules: [r(writes, 1, { type: "error", code: readonly }, "read-only")] },
    { label: "Flaky connections (5% dropped)", rules: [r("", 0.05, { type: "drop" }, "flaky")] },
    { label: "Database down", rules: [], refuse: true },
  ];
}

export function MockFaults({ mock }: { mock: MockStatus }) {
  const toast = useStore((s) => s.toast);
  const saved = mock.faults ?? EMPTY;
  const [draft, setDraft] = useState<FaultConfig>(saved);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => setDraft(mock.faults ?? EMPTY), [mock.faults]);
  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);
  const codes = ERROR_CODES[mock.kind];

  const save = async (next: FaultConfig = draft) => {
    setBusy(true);
    setError(undefined);
    try {
      await call("mock.faults.set", { id: mock.id, faults: next });
      toast("success", next.enabled ? "Fault injection active" : "Fault injection off", next.enabled ? "Applied immediately — watch the Traffic tab." : undefined);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const setRule = (i: number, patch: Partial<FaultRule>) => setDraft((d) => ({ ...d, rules: d.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) }));
  const move = (i: number, delta: number) =>
    setDraft((d) => {
      const rules = d.rules.slice();
      const j = i + delta;
      if (j < 0 || j >= rules.length) return d;
      [rules[i], rules[j]] = [rules[j], rules[i]];
      return { ...d, rules };
    });

  return (
    <div className="pane">
      <div className="page">
        <h2 style={{ marginTop: 0 }} className="row">
          <Zap size={16} /> Fault injection
        </h2>
        <p>
          Test how your application copes with a slow or failing database. Rules run in order for every statement or command that matches; changes apply to connected clients
          immediately. Injected faults show up in the Traffic tab as <code>[fault]</code>.
        </p>
        <div className="card" style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          <label className="check">
            <input type="checkbox" checked={draft.enabled} onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })} />
            <strong>Enabled</strong>
          </label>
          <label className="check">
            <input type="checkbox" checked={draft.refuse_connections} onChange={(e) => setDraft({ ...draft, refuse_connections: e.target.checked })} />
            Refuse new connections (the database looks down; existing connections keep working)
          </label>
        </div>
        <h3>Presets</h3>
        <div className="row wrap">
          {presets(mock.kind).map((p) => (
            <button
              key={p.label}
              className="btn sm"
              onClick={() => setDraft((d) => ({ enabled: true, refuse_connections: p.refuse ?? d.refuse_connections, rules: [...d.rules, ...p.rules] }))}
            >
              {p.label}
            </button>
          ))}
        </div>
        <h3>Rules</h3>
        {draft.rules.length === 0 ? (
          <div className="faint small">No rules. Add one or start from a preset.</div>
        ) : (
          <div style={{ overflowX: "auto" }}>
            <table className="simple-table" style={{ minWidth: 760 }}>
              <thead>
                <tr>
                  <th style={{ width: 28 }} />
                  <th>When the statement matches</th>
                  <th style={{ width: 90 }}>Chance</th>
                  <th style={{ width: 110 }}>Action</th>
                  <th>Details</th>
                  <th style={{ width: 80 }} />
                </tr>
              </thead>
              <tbody>
                {draft.rules.map((r, i) => (
                  <tr key={i}>
                    <td>
                      <input type="checkbox" checked={r.enabled} onChange={(e) => setRule(i, { enabled: e.target.checked })} aria-label="Rule enabled" />
                    </td>
                    <td style={{ padding: 3 }}>
                      <input className="input sm mono" style={{ width: "100%" }} value={r.matches} placeholder="anything (or text, or /regex/)" onChange={(e) => setRule(i, { matches: e.target.value })} aria-label="Match" />
                    </td>
                    <td style={{ padding: 3 }}>
                      <div className="row" style={{ gap: 4 }}>
                        <input
                          className="input sm"
                          style={{ width: 56 }}
                          value={Math.round(r.probability * 1000) / 10}
                          onChange={(e) => setRule(i, { probability: Math.min(100, Math.max(0, Number(e.target.value.replace(/[^\d.]/g, "")) || 0)) / 100 })}
                          aria-label="Probability percent"
                        />
                        <span className="faint">%</span>
                      </div>
                    </td>
                    <td style={{ padding: 3 }}>
                      <select
                        className="select sm"
                        value={r.action.type}
                        onChange={(e) => {
                          const t = e.target.value as FaultAction["type"];
                          setRule(i, { action: t === "delay" ? { type: "delay", ms: 500, jitter_ms: 0 } : t === "error" ? { type: "error", code: codes[0].code } : { type: "drop" } });
                        }}
                        aria-label="Action"
                      >
                        <option value="delay">Delay</option>
                        <option value="error">Error</option>
                        <option value="drop">Drop connection</option>
                      </select>
                    </td>
                    <td style={{ padding: 3 }}>
                      {r.action.type === "delay" && (
                        <div className="row" style={{ gap: 4 }}>
                          <input className="input sm" style={{ width: 72 }} value={r.action.ms} onChange={(e) => setRule(i, { action: { ...(r.action as Extract<FaultAction, { type: "delay" }>), ms: Number(e.target.value.replace(/\D/g, "")) || 0 } })} aria-label="Delay ms" />
                          <span className="faint xs">ms ±</span>
                          <input className="input sm" style={{ width: 64 }} value={r.action.jitter_ms ?? 0} onChange={(e) => setRule(i, { action: { ...(r.action as Extract<FaultAction, { type: "delay" }>), jitter_ms: Number(e.target.value.replace(/\D/g, "")) || 0 } })} aria-label="Jitter ms" />
                          <span className="faint xs">ms</span>
                        </div>
                      )}
                      {r.action.type === "error" && (
                        <div className="row" style={{ gap: 4 }}>
                          <input className="input sm mono" style={{ width: 90 }} list={`codes-${mock.kind}`} value={r.action.code ?? ""} placeholder="code" onChange={(e) => setRule(i, { action: { ...(r.action as Extract<FaultAction, { type: "error" }>), code: e.target.value } })} aria-label="Error code" />
                          <input className="input sm grow" value={r.action.message ?? ""} placeholder="message (optional)" onChange={(e) => setRule(i, { action: { ...(r.action as Extract<FaultAction, { type: "error" }>), message: e.target.value } })} aria-label="Error message" />
                        </div>
                      )}
                      {r.action.type === "drop" && <span className="faint xs">Close the client connection</span>}
                    </td>
                    <td style={{ whiteSpace: "nowrap" }}>
                      <button className="icon-btn sm" onClick={() => move(i, -1)} aria-label="Move up">
                        <ArrowUp size={12} />
                      </button>
                      <button className="icon-btn sm" onClick={() => move(i, 1)} aria-label="Move down">
                        <ArrowDown size={12} />
                      </button>
                      <button className="icon-btn sm" onClick={() => setDraft((d) => ({ ...d, rules: d.rules.filter((_, j) => j !== i) }))} aria-label="Remove rule">
                        <Trash2 size={12} />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <datalist id={`codes-${mock.kind}`}>
          {codes.map((c) => (
            <option key={c.code} value={c.code}>
              {c.label}
            </option>
          ))}
        </datalist>
        <div className="row" style={{ marginTop: 10 }}>
          <button className="btn sm" onClick={() => setDraft((d) => ({ ...d, rules: [...d.rules, { enabled: true, matches: "", probability: 1, action: { type: "delay", ms: 500, jitter_ms: 0 } }] }))}>
            <Plus size={13} /> Rule
          </button>
          <span className="grow" />
          {saved.enabled && (
            <button className="btn sm" disabled={busy} onClick={() => save({ ...saved, enabled: false })}>
              Turn off
            </button>
          )}
          {dirty && (
            <button className="btn sm ghost" onClick={() => setDraft(saved)}>
              Revert
            </button>
          )}
          <button className="btn sm primary" disabled={!dirty || busy} onClick={() => save()}>
            {busy ? <Spinner size={12} /> : <Save size={13} />} Apply
          </button>
        </div>
        <ErrorBox error={error} />
      </div>
    </div>
  );
}
