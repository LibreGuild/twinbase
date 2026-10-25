// Create/edit dialogs for saved connections and mock servers.

import { CheckCircle2, FolderOpen, XCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { Dialog, ErrorBox, Field, Spinner } from "../components/ui";
import { call, errorMessage, pickPath } from "../lib/api";
import { kindLabel } from "../lib/format";
import { useStore } from "../lib/store";
import type { AiAccess, ConnectionDef, DbKind, MockDef, MockKind, MockStatus, TlsMode } from "../lib/types";

const DEFAULT_PORTS: Record<DbKind, number> = { postgres: 5432, mysql: 3306, sqlite: 0, mssql: 1433, clickhouse: 8123, cassandra: 9042, spanner: 0, bigquery: 0, oracle: 1521, elasticsearch: 9200, redis: 6379, mongodb: 27017, dynamodb: 0, firestore: 0 };
const COLORS = ["", "#d97757", "#e0b04f", "#6cc485", "#79a8e0", "#b48ee0", "#ec6a5f"];

/** A MongoDB connection string stays whole in `host` (it can list several hosts, use SRV and
 * carry options); only the password moves out, to be stored on this computer. */
function parseMongoUrl(url: string): (Partial<ConnectionDef> & { password?: string }) | null {
  const m = /^(mongodb(?:\+srv)?:\/\/)(?:([^:@/]*)(?::([^@/]*))?@)?([^/?]*)(\/[^?]*)?(\?.*)?$/.exec(url.trim());
  if (!m) return null;
  const [, scheme, user, password, hosts, path, query] = m;
  const dec = (s?: string) => (s ? decodeURIComponent(s) : "");
  const tls = /[?&](tls|ssl)=true/i.test(query ?? "") || scheme.includes("+srv") ? "verify-full" : "disable";
  return {
    kind: "mongodb",
    host: `${scheme}${user ? `${user}@` : ""}${hosts}${path ?? ""}${query ?? ""}`,
    port: 0,
    user: dec(user),
    password: password !== undefined ? dec(password) : undefined,
    database: dec(path?.slice(1)) || undefined,
    tls,
  };
}

/** Parse postgres://, mysql://, redis://, mongodb:// URLs into connection fields. */
export function parseConnectionUrl(url: string): Partial<ConnectionDef> & { password?: string } | null {
  if (/^mongodb(\+srv)?:\/\//i.test(url.trim())) return parseMongoUrl(url);
  try {
    const u = new URL(url.trim());
    const scheme = u.protocol.replace(/:$/, "");
    const kind: DbKind | undefined =
      scheme === "postgres" || scheme === "postgresql"
        ? "postgres"
        : scheme === "mysql" || scheme === "mariadb"
          ? "mysql"
          : scheme === "redis" || scheme === "rediss"
            ? "redis"
            : scheme === "sqlserver" || scheme === "mssql"
              ? "mssql"
              : undefined;
    if (!kind) return null;
    const ssl = u.searchParams.get("sslmode") ?? u.searchParams.get("ssl-mode");
    const tls: TlsMode = scheme === "rediss" || ssl === "require" || ssl === "REQUIRED" ? "require" : ssl === "verify-full" || ssl === "VERIFY_IDENTITY" ? "verify-full" : "disable";
    const db = decodeURIComponent(u.pathname.replace(/^\//, "")) || u.searchParams.get("database") || "";
    return {
      kind,
      host: u.hostname.replace(/^\[|\]$/g, ""),
      port: Number(u.port) || DEFAULT_PORTS[kind],
      user: decodeURIComponent(u.username),
      password: u.password ? decodeURIComponent(u.password) : undefined,
      database: db || undefined,
      tls,
    };
  } catch {
    return null;
  }
}

export function ConnectionDialog({ connection, onClose }: { connection?: ConnectionDef; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [def, setDef] = useState<ConnectionDef>(
    connection ?? { id: "", name: "", kind: "postgres", host: "localhost", port: 5432, user: "", database: "", tls: "disable", read_only: false, ai_access: "read-only" },
  );
  const [password, setPassword] = useState<string | undefined>(undefined);
  const [sshSecret, setSshSecret] = useState<string | undefined>(undefined);
  const [url, setUrl] = useState("");
  const [test, setTest] = useState<{ ok: boolean; version?: string; error?: string; elapsed_ms?: number } | null>(null);
  const [busy, setBusy] = useState<"test" | "save" | null>(null);
  const [error, setError] = useState<string>();
  const set = (patch: Partial<ConnectionDef>) => {
    setDef((d) => ({ ...d, ...patch }));
    setTest(null);
  };

  const payload = () => {
    const d: ConnectionDef = { ...def, name: def.name.trim() || (def.kind === "sqlite" ? (def.path ?? "").split(/[\\/]/).pop() || "SQLite" : `${def.host}${def.database ? `/${def.database}` : ""}`) };
    if (d.kind === "sqlite") Object.assign(d, { host: "", port: 0, user: "" });
    if (!d.database) delete d.database;
    if (!d.color) delete d.color;
    if (d.ssh && !d.ssh.host.trim()) delete d.ssh;
    if (d.ssh && d.ssh.auth !== "key") delete d.ssh.key_path;
    delete d.has_password;
    delete d.has_ssh_secret;
    return d;
  };

  const runTest = async () => {
    setBusy("test");
    try {
      setTest(await call("connection.test", { connection: payload(), password, ssh_secret: sshSecret }));
    } catch (e) {
      setTest({ ok: false, error: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    setBusy("save");
    try {
      await call("connection.save", { connection: payload(), password, ssh_secret: sshSecret });
      toast("success", connection ? "Connection updated" : "Connection saved", payload().name);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const network = def.kind !== "sqlite";
  return (
    <Dialog
      title={connection ? `Edit ${connection.name}` : "New connection"}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={runTest} disabled={!!busy}>
            {busy === "test" && <Spinner />} Test
          </button>
          {test && (
            <span className="row small grow" style={{ color: test.ok ? "var(--success)" : "var(--danger)", minWidth: 0 }}>
              {test.ok ? <CheckCircle2 size={14} /> : <XCircle size={14} />}
              <span className="ellipsis" title={test.ok ? test.version : test.error}>
                {test.ok ? `Connected · ${test.version ?? ""} · ${test.elapsed_ms} ms` : test.error}
              </span>
            </span>
          )}
          {!test && <span className="grow" />}
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" onClick={save} disabled={!!busy}>
            {busy === "save" && <Spinner />} Save
          </button>
        </>
      }
    >
      {!connection && (
        <Field label="Paste a connection URL (optional)" hint="postgres://user:pass@host:5432/db · mysql://… · sqlserver://… · redis://… · mongodb://… or mongodb+srv://… — fills the fields below.">
          <input
            className="input mono"
            value={url}
            placeholder="postgres://user:password@db.example.com:5432/app?sslmode=require"
            onChange={(e) => {
              setUrl(e.target.value);
              const p = parseConnectionUrl(e.target.value);
              if (p) {
                const { password: pw, ...rest } = p;
                set(rest);
                if (pw !== undefined) setPassword(pw);
              }
            }}
          />
        </Field>
      )}
      <div className="grid-2">
        <Field label="Name">
          <input className="input" value={def.name} onChange={(e) => set({ name: e.target.value })} placeholder="Production replica" autoFocus />
        </Field>
        <Field label="Type">
          <select className="select" value={def.kind} onChange={(e) => set({ kind: e.target.value as DbKind, port: DEFAULT_PORTS[e.target.value as DbKind] })}>
            {(["postgres", "mysql", "mssql", "oracle", "clickhouse", "cassandra", "sqlite", "redis", "mongodb", "dynamodb", "firestore", "spanner", "bigquery", "elasticsearch"] as const).map((k) => (
              <option key={k} value={k}>
                {kindLabel(k)}
              </option>
            ))}
          </select>
        </Field>
      </div>
      {def.kind === "firestore" || def.kind === "spanner" || def.kind === "bigquery" ? (
        <>
          <div className="grid-2">
            <Field
              label="Endpoint"
              hint={`Empty for Google Cloud. host:port for the ${def.kind === "firestore" ? "Firestore emulator or a TwinBase mock" : def.kind === "spanner" ? "Spanner emulator (its REST port, 9020)" : "BigQuery emulator (9050)"}.`}
            >
              <input className="input mono" value={def.host} onChange={(e) => set({ host: e.target.value, port: 0 })} placeholder={def.kind === "spanner" ? "spanner.googleapis.com" : def.kind === "bigquery" ? "bigquery.googleapis.com" : "firestore.googleapis.com"} />
            </Field>
            <Field
              label={def.kind === "spanner" ? "Database" : "Project ID"}
              hint={def.kind === "spanner" ? "project/instance/database (the project can come from the key file: instance/database)." : def.kind === "bigquery" ? "project, or project.dataset to make a dataset the default." : "project or project/database for a named database."}
            >
              <input className="input mono" value={def.database ?? ""} onChange={(e) => set({ database: e.target.value })} placeholder={def.kind === "spanner" ? "my-project/my-instance/my-db" : "from the key file"} />
            </Field>
          </div>
          {!def.host.trim() ? (
            <Field
              label="Service account key"
              hint={
                def.kind === "firestore"
                  ? "A JSON key file (IAM → Service accounts → Keys). Empty: GOOGLE_APPLICATION_CREDENTIALS."
                  : "A JSON key file (IAM → Service accounts → Keys). Empty: Application Default Credentials (gcloud auth application-default login, or GOOGLE_APPLICATION_CREDENTIALS)."
              }
            >
              <div className="row">
                <input className="input mono grow" value={def.path ?? ""} onChange={(e) => set({ path: e.target.value })} placeholder="~/keys/my-project-sa.json" />
                <button
                  className="btn"
                  onClick={async () => {
                    const p = await pickPath({ title: "Choose service account key", extensions: ["json"] });
                    if (p) set({ path: p });
                  }}
                >
                  <FolderOpen size={14} /> Browse
                </button>
              </div>
            </Field>
          ) : (
            <Field label="Token (optional)" hint="Emulators accept any token; a TwinBase mock with a password expects that password.">
              <input className="input" type="password" value={password ?? ""} placeholder={connection?.has_password ? "••••••••" : "owner"} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
            </Field>
          )}
        </>
      ) : def.kind === "dynamodb" ? (
        <>
          <div className="grid-2">
            <Field label="Endpoint" hint="Empty for AWS. A URL such as http://localhost:8000 for DynamoDB Local or another endpoint.">
              <input className="input mono" value={def.host} onChange={(e) => set({ host: e.target.value, port: 0 })} placeholder="AWS (dynamodb.<region>.amazonaws.com)" />
            </Field>
            <Field label="Region">
              <input className="input mono" value={def.database ?? ""} onChange={(e) => set({ database: e.target.value })} placeholder="us-east-1" />
            </Field>
          </div>
          <div className="grid-2">
            <Field label="Access key ID" hint="Empty: use AWS_ACCESS_KEY_ID or ~/.aws/credentials (AWS_PROFILE).">
              <input className="input mono" value={def.user} onChange={(e) => set({ user: e.target.value })} autoComplete="off" placeholder="from the environment" />
            </Field>
            <Field label="Secret access key" hint={connection?.has_password && password === undefined ? "Saved on this computer. Type to replace, or clear." : "Stored on this computer only (OS keychain), never in the workspace."}>
              <input className="input" type="password" value={password ?? ""} placeholder={connection?.has_password ? "••••••••" : ""} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
            </Field>
          </div>
        </>
      ) : network ? (
        <>
          <div className="grid-3">
            <Field label={def.kind === "mongodb" ? "Host or connection string" : "Host"} hint={
                def.kind === "clickhouse"
                  ? "HTTP interface (8123, or 8443 with TLS). A URL such as https://….clickhouse.cloud:8443 works too."
                  : def.kind === "elasticsearch"
                    ? "Elasticsearch or OpenSearch REST endpoint (9200). A URL such as https://….es.io:443 works too."
                    : def.kind === "cassandra"
                      ? "Any node of a Cassandra or ScyllaDB cluster (CQL, 9042). TwinBase reaches the cluster through this address."
                      : undefined
              }
            >
              <input className={`input${def.host.includes("://") ? " mono" : ""}`} value={def.host} onChange={(e) => set({ host: e.target.value })} placeholder={def.kind === "mongodb" ? "localhost or mongodb+srv://…" : ""} />
            </Field>
            <Field label="Port" hint={def.host.includes("://") ? "From the connection string." : undefined}>
              <input className="input" value={def.port || ""} disabled={def.host.includes("://")} onChange={(e) => set({ port: Number(e.target.value.replace(/\D/g, "")) || 0 })} />
            </Field>
            <Field
              label={def.kind === "redis" ? "DB index" : def.kind === "cassandra" ? "Keyspace" : def.kind === "oracle" ? "Service name" : "Database"}
              hint={def.kind === "oracle" ? "Needs Oracle Instant Client on this computer (Settings › Data)." : undefined}
            >
              <input className="input" value={def.database ?? ""} onChange={(e) => set({ database: e.target.value })} placeholder={def.kind === "redis" ? "0" : ""} />
            </Field>
          </div>
          <div className="grid-2">
            <Field
              label="User"
              hint={
                def.kind === "mssql"
                  ? "SQL Server login. Leave empty to sign in with a Microsoft Entra access token in the password field."
                  : def.kind === "elasticsearch"
                    ? "Basic authentication. Leave empty to use an API key (id:key or its base64 form) in the password field."
                    : undefined
              }
            >
              <input className="input" value={def.user} onChange={(e) => set({ user: e.target.value })} autoComplete="off" />
            </Field>
            <Field label="Password" hint={connection?.has_password && password === undefined ? "Saved on this computer. Type to replace, or clear." : "Stored on this computer only (OS keychain), never in the workspace."}>
              <input className="input" type="password" value={password ?? ""} placeholder={connection?.has_password ? "••••••••" : ""} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
            </Field>
          </div>
          <Field label="TLS" hint={def.kind === "mssql" ? "Off encrypts only the login (Encrypt=no). Required = TrustServerCertificate; Verify checks the certificate against the system's trusted roots (needed for Azure SQL)." : undefined}>
            <div className="seg">
              {(
                [
                  ["disable", "Off"],
                  ["require", "Required"],
                  ["verify-full", "Verify certificate"],
                ] as const
              ).map(([id, label]) => (
                <button key={id} className={def.tls === id ? "on" : ""} onClick={() => set({ tls: id })}>
                  {label}
                </button>
              ))}
            </div>
          </Field>
        </>
      ) : (
        <Field label="Database file">
          <div className="row">
            <input className="input mono grow" value={def.path ?? ""} onChange={(e) => set({ path: e.target.value })} placeholder="/path/to/app.db" />
            <button
              className="btn"
              onClick={async () => {
                const p = await pickPath({ title: "Choose SQLite database", extensions: ["db", "sqlite", "sqlite3", "db3"] });
                if (p) set({ path: p });
              }}
            >
              <FolderOpen size={14} /> Browse
            </button>
          </div>
        </Field>
      )}
      {network && (
        <div className="card" style={{ padding: "10px 12px", display: "flex", flexDirection: "column", gap: 10, background: "var(--surface-2)" }}>
          <label className="check">
            <input
              type="checkbox"
              checked={!!def.ssh}
              onChange={(e) => set({ ssh: e.target.checked ? { host: "", port: 22, user: "", auth: "password" } : undefined })}
            />
            <strong>Connect through an SSH tunnel</strong>
            <span className="faint small">— for databases in a private network, reached via a bastion host</span>
          </label>
          {def.ssh && (
            <>
              <div className="grid-3">
                <Field label="SSH host">
                  <input className="input" value={def.ssh.host} placeholder="bastion.example.com" onChange={(e) => set({ ssh: { ...def.ssh!, host: e.target.value } })} />
                </Field>
                <Field label="Port">
                  <input className="input" value={def.ssh.port || ""} placeholder="22" onChange={(e) => set({ ssh: { ...def.ssh!, port: Number(e.target.value.replace(/\D/g, "")) || 0 } })} />
                </Field>
                <Field label="User">
                  <input className="input" value={def.ssh.user} onChange={(e) => set({ ssh: { ...def.ssh!, user: e.target.value } })} autoComplete="off" />
                </Field>
              </div>
              <Field label="Authentication">
                <div className="seg">
                  {(
                    [
                      ["password", "Password"],
                      ["key", "Private key"],
                      ["agent", "SSH agent"],
                    ] as const
                  ).map(([id, label]) => (
                    <button key={id} className={def.ssh!.auth === id ? "on" : ""} onClick={() => set({ ssh: { ...def.ssh!, auth: id } })}>
                      {label}
                    </button>
                  ))}
                </div>
              </Field>
              {def.ssh.auth === "key" && (
                <Field label="Private key file">
                  <div className="row">
                    <input className="input mono grow" value={def.ssh.key_path ?? ""} placeholder="~/.ssh/id_ed25519" onChange={(e) => set({ ssh: { ...def.ssh!, key_path: e.target.value } })} />
                    <button
                      className="btn"
                      onClick={async () => {
                        const p = await pickPath({ title: "Choose SSH private key" });
                        if (p) set({ ssh: { ...def.ssh!, key_path: p } });
                      }}
                    >
                      <FolderOpen size={14} /> Browse
                    </button>
                  </div>
                </Field>
              )}
              {def.ssh.auth !== "agent" && (
                <Field
                  label={def.ssh.auth === "key" ? "Key passphrase" : "SSH password"}
                  hint={connection?.has_ssh_secret && sshSecret === undefined ? "Saved on this computer. Type to replace." : "Stored on this computer only (OS keychain), never in the workspace."}
                >
                  <input className="input" type="password" value={sshSecret ?? ""} placeholder={connection?.has_ssh_secret ? "••••••••" : def.ssh.auth === "key" ? "(none)" : ""} onChange={(e) => setSshSecret(e.target.value)} autoComplete="new-password" />
                </Field>
              )}
              <div className="faint xs">
                The bastion's host key is checked against ~/.ssh/known_hosts; an unknown key is trusted on first use and remembered, and a changed key is refused.
              </div>
            </>
          )}
        </div>
      )}
      <div className="grid-2">
        <Field label="AI agent access" hint="What MCP agents may do on this connection.">
          <select className="select" value={def.ai_access} onChange={(e) => set({ ai_access: e.target.value as AiAccess })}>
            <option value="none">No access</option>
            <option value="read-only">Read only</option>
            <option value="read-write">Read and write</option>
          </select>
        </Field>
        <Field label="Color tag">
          <div className="row">
            {COLORS.map((c) => (
              <button
                key={c || "none"}
                aria-label={c || "No color"}
                onClick={() => set({ color: c || undefined })}
                style={{ width: 20, height: 20, borderRadius: 10, border: (def.color ?? "") === c ? "2px solid var(--text)" : "1px solid var(--border-strong)", background: c || "transparent", cursor: "pointer" }}
              />
            ))}
          </div>
        </Field>
      </div>
      <label className="check">
        <input type="checkbox" checked={def.read_only} onChange={(e) => set({ read_only: e.target.checked })} /> Read-only for me too (block edits and writes in the UI)
      </label>
      <ErrorBox error={error} />
    </Dialog>
  );
}

export function MockDialog({ mock, mockKind, onClose }: { mock?: MockStatus; mockKind?: MockKind; onClose: () => void }) {
  const openTab = useStore((s) => s.openTab);
  const toast = useStore((s) => s.toast);
  const [def, setDef] = useState<MockDef>(() => {
    if (mock) {
      const { running: _r, connection_id: _c, error: _e, ...rest } = mock;
      return rest;
    }
    return { id: "", name: "", kind: mockKind ?? "postgres", host: "127.0.0.1", port: 0, auth: { mode: "any" }, database: "app", persist: true, auto_start: false };
  });
  const [start, setStart] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const set = (patch: Partial<MockDef>) => setDef((d) => ({ ...d, ...patch }));

  const save = async () => {
    setBusy(true);
    setError(undefined);
    try {
      const name = def.name.trim() || `${kindLabel(def.kind)} mock`;
      const saved = await call<MockDef>("mock.save", { mock: { ...def, name, description: def.description || undefined } });
      if (!mock && start) await call("mock.start", { id: saved.id });
      if (!mock) openTab({ kind: "mock", mockId: saved.id });
      toast("success", mock ? "Mock server updated" : "Mock server created", `${name} on ${saved.host}:${saved.port}`);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={mock ? `Mock settings · ${mock.name}` : "New mock server"}
      onClose={onClose}
      footer={
        <>
          {!mock && (
            <label className="check grow">
              <input type="checkbox" checked={start} onChange={(e) => setStart(e.target.checked)} /> Start now
            </label>
          )}
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" onClick={save} disabled={busy}>
            {busy && <Spinner />} {mock ? "Save" : "Create"}
          </button>
        </>
      }
    >
      <Field label="Type">
        <div className="seg">
          {(["postgres", "mysql", "mssql", "redis", "mongodb", "dynamodb", "firestore"] as const).map((k) => (
            <button key={k} disabled={!!mock} className={def.kind === k ? "on" : ""} onClick={() => set({ kind: k, port: 0 })}>
              {kindLabel(k)}
            </button>
          ))}
        </div>
      </Field>
      <div className="grid-2">
        <Field label="Name">
          <input className="input" autoFocus value={def.name} onChange={(e) => set({ name: e.target.value })} placeholder={`${kindLabel(def.kind)} mock`} />
        </Field>
        <Field label="Port" hint={def.port ? undefined : "A free port is chosen automatically."}>
          <input className="input" value={def.port || ""} placeholder="auto" onChange={(e) => set({ port: Number(e.target.value.replace(/\D/g, "")) || 0 })} />
        </Field>
      </div>
      {def.kind === "postgres" && <EngineField engine={def.engine ?? "twinlite"} editing={!!mock} onChange={(engine) => set({ engine })} />}
      <div className="grid-2">
        {def.kind !== "redis" && def.kind !== "dynamodb" && def.kind !== "firestore" && (
          <Field label="Database name" hint={def.kind === "mssql" ? "The mock serves one database whatever name clients ask for; seeds load into it." : undefined}>
            <input className="input" value={def.database} onChange={(e) => set({ database: e.target.value })} />
          </Field>
        )}
        <Field label="Listen on" hint={def.host === "0.0.0.0" ? "Reachable from other devices on your network (LAN, containers)." : "Only this computer."}>
          <select className="select" value={def.host} onChange={(e) => set({ host: e.target.value })}>
            <option value="127.0.0.1">127.0.0.1 (this computer)</option>
            <option value="0.0.0.0">0.0.0.0 (all interfaces)</option>
          </select>
        </Field>
      </div>
      <Field
        label="Authentication"
        hint={
          def.auth.mode === "password"
            ? "Clients must use exactly these credentials."
            : def.kind === "mongodb"
              ? "No credentials: connect with mongodb://host:port/db. (MongoDB's SCRAM login proves the password to both sides, so a mock cannot accept any password.)"
              : def.kind === "dynamodb"
                ? "Any access key and secret are accepted, so SDK configs work unchanged. With a user and password, requests must be signed with that access key ID and secret."
                : def.kind === "firestore"
                  ? "Like the Firestore emulator: SDKs connect through FIRESTORE_EMULATOR_HOST with any project id. With a password, requests must send it as the bearer token."
                : "Any user name and password is accepted, so existing app configs work unchanged."
        }
      >
        <div className="row wrap">
          <div className="seg">
            <button className={def.auth.mode === "any" ? "on" : ""} onClick={() => set({ auth: { mode: "any" } })}>
              Accept any
            </button>
            <button className={def.auth.mode === "password" ? "on" : ""} onClick={() => set({ auth: { mode: "password", user: def.kind === "redis" ? "default" : def.kind === "dynamodb" ? "AKIDTWINBASE" : "app", password: "" } })}>
              User &amp; password
            </button>
          </div>
          {def.auth.mode === "password" && (
            <>
              <input className="input" style={{ width: 140 }} value={def.auth.user} onChange={(e) => set({ auth: { ...(def.auth as { mode: "password"; user: string; password: string }), user: e.target.value } })} placeholder="user" aria-label="User" />
              <input className="input" style={{ width: 160 }} type="password" value={def.auth.password} onChange={(e) => set({ auth: { ...(def.auth as { mode: "password"; user: string; password: string }), password: e.target.value } })} placeholder="password" aria-label="Password" />
            </>
          )}
        </div>
      </Field>
      <div className="row wrap" style={{ gap: 18 }}>
        <label className="check">
          <input type="checkbox" checked={def.persist} onChange={(e) => set({ persist: e.target.checked })} /> Keep data in the workspace
        </label>
        <label className="check">
          <input type="checkbox" checked={def.auto_start} onChange={(e) => set({ auto_start: e.target.checked })} /> Start when TwinBase opens
        </label>
      </div>
      <Field label="Description">
        <textarea className="textarea" rows={2} value={def.description ?? ""} onChange={(e) => set({ description: e.target.value })} placeholder="What this mock is for (shared with exports)" />
      </Field>
      {def.auth.mode === "password" && def.persist && <div className="faint xs">Mock credentials are saved in the workspace files so teammates can use the same mock; don't reuse real passwords.</div>}
      <ErrorBox error={error} />
    </Dialog>
  );
}

/** PostgreSQL mock engine: built-in TwinLite, or a real PostgreSQL server found on this computer. */
function EngineField({ engine, editing, onChange }: { engine: "twinlite" | "native"; editing: boolean; onChange: (e: "twinlite" | "native") => void }) {
  const [found, setFound] = useState<{ version: string; bin: string } | null | undefined>();
  useEffect(() => {
    if (engine !== "native" || found !== undefined) return;
    call<{ found: { version: string; bin: string } | null }>("mock.native.detect", {})
      .then((r) => setFound(r.found))
      .catch(() => setFound(null));
  }, [engine, found]);
  const hint =
    engine === "twinlite"
      ? "Built in and instant. Emulates PostgreSQL on SQLite: common SQL, types and catalogs."
      : found === undefined
        ? "Looking for PostgreSQL…"
        : found
          ? `PostgreSQL ${found.version} (${found.bin}). Full PostgreSQL behaviour: PL/pgSQL, extensions, exact errors and plans.`
          : "PostgreSQL is not installed. Install it (brew install postgresql@17, Postgres.app, apt install postgresql) or set its bin folder in Settings › Data.";
  return (
    <Field label="Engine" hint={editing && engine === "native" ? `${hint} Switching engines starts from the seed; existing data stays with the other engine.` : hint}>
      <div className="seg">
        <button className={engine === "twinlite" ? "on" : ""} onClick={() => onChange("twinlite")}>
          TwinLite
        </button>
        <button className={engine === "native" ? "on" : ""} onClick={() => onChange("native")}>
          Real PostgreSQL
        </button>
      </div>
    </Field>
  );
}
