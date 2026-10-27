// Settings: appearance, data limits, workspace and AI agent (MCP) setup.

import { Bot, Database, Download, ExternalLink, FolderOpen, Info, Monitor, Moon, Palette, RefreshCw, Settings as SettingsIcon, Sun, X } from "lucide-react";
import { useState } from "react";
import { CodeBlock, ConfirmDialog, CopyButton, ErrorBox, Field, Spinner, useAsync, useEscape } from "../components/ui";
import { call, openExternal, runtime } from "../lib/api";
import { attempt, useStore } from "../lib/store";
import type { McpConfig } from "../lib/types";

const SECTIONS = [
  ["appearance", "Appearance", Palette],
  ["data", "Data & safety", Database],
  ["agents", "AI agents (MCP)", Bot],
  ["workspace", "Workspace", FolderOpen],
  ["about", "About", Info],
] as const;

const UI_FONTS = [
  { id: "inter", name: "Inter", hint: "Default · crisp and neutral", family: '"Inter Variable"' },
  { id: "atkinson", name: "Atkinson Hyperlegible", hint: "Designed for readability", family: '"Atkinson Hyperlegible Next Variable"' },
  { id: "plex", name: "IBM Plex Sans", hint: "Technical, slightly condensed", family: '"IBM Plex Sans Variable"' },
  { id: "system", name: "System", hint: "Your operating system font", family: "-apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui" },
];

const MONO_FONTS = [
  { id: "jetbrains-mono", name: "JetBrains Mono", hint: "Default · tall, clear letters", family: '"JetBrains Mono Variable"' },
  { id: "fira-code", name: "Fira Code", hint: "Programming ligatures", family: '"Fira Code Variable"' },
  { id: "system", name: "System monospace", hint: "SF Mono, Consolas, …", family: "ui-monospace, Menlo, Consolas" },
];

export function SettingsDialog({ section: initial, onClose }: { section?: string; onClose: () => void }) {
  const settings = useStore((s) => s.settings);
  const updateSettings = useStore((s) => s.updateSettings);
  const [section, setSection] = useState(initial ?? "appearance");
  useEscape(onClose);
  if (!settings) return null;
  const save = (patch: Record<string, unknown>) => attempt("Could not save settings", () => updateSettings(patch));

  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog settings-dialog" role="dialog" aria-modal="true" aria-label="Settings">
        <div className="dialog-head" style={{ borderBottom: "1px solid var(--border)", paddingBottom: 12 }}>
          <SettingsIcon size={17} className="muted" />
          <h3>Settings</h3>
          <button className="icon-btn" onClick={onClose} aria-label="Close settings" title="Close (Esc)" autoFocus>
            <X size={16} />
          </button>
        </div>
        <div className="dialog-body">
          <nav className="settings-nav" aria-label="Settings sections">
            {SECTIONS.map(([id, label, Icon]) => (
              <button key={id} className={section === id ? "on" : ""} onClick={() => setSection(id)}>
                <Icon size={15} /> {label}
              </button>
            ))}
          </nav>
          <div className="settings-body">
            <div className="page">
              {section === "appearance" && (
                <>
                  <h1>Appearance</h1>
                  <h2>Theme</h2>
                  <div className="seg">
                    {(
                      [
                        ["system", "System", <Monitor size={14} key="m" />],
                        ["light", "Light", <Sun size={14} key="s" />],
                        ["dark", "Dark", <Moon size={14} key="d" />],
                      ] as const
                    ).map(([id, label, icon]) => (
                      <button key={id} className={settings.theme === id ? "on" : ""} onClick={() => save({ theme: id })}>
                        {icon} {label}
                      </button>
                    ))}
                  </div>
                  <h2>Interface font</h2>
                  <div className="font-choice">
                    {UI_FONTS.map((f) => (
                      <button key={f.id} className={settings.font_family === f.id ? "on" : ""} onClick={() => save({ font_family: f.id })}>
                        <div className="sample" style={{ fontFamily: f.family }}>
                          {f.name}
                        </div>
                        <div className="name">{f.hint}</div>
                      </button>
                    ))}
                  </div>
                  <h2>Code font</h2>
                  <div className="font-choice">
                    {MONO_FONTS.map((f) => (
                      <button key={f.id} className={settings.mono_font === f.id ? "on" : ""} onClick={() => save({ mono_font: f.id })}>
                        <div className="sample" style={{ fontFamily: f.family, fontSize: 15 }}>
                          SELECT id FROM t;
                        </div>
                        <div className="name">
                          {f.name} · {f.hint}
                        </div>
                      </button>
                    ))}
                  </div>
                  <h2>Text size</h2>
                  <div className="row">
                    <input type="range" min={11} max={16} value={settings.font_size || 13} onChange={(e) => save({ font_size: Number(e.target.value) })} aria-label="Text size" />
                    <span className="muted">{settings.font_size || 13}px</span>
                  </div>
                </>
              )}
              {section === "data" && (
                <>
                  <h1>Data &amp; safety</h1>
                  <div className="grid-2" style={{ marginTop: 16 }}>
                    <Field label="Rows per page in tables" hint="How many rows the table view loads at a time.">
                      <input className="input" type="number" min={10} max={5000} defaultValue={settings.page_size} onBlur={(e) => save({ page_size: Math.max(10, Math.min(5000, Number(e.target.value) || 200)) })} />
                    </Field>
                    <Field label="Maximum rows per query result" hint="The SQL editor stops reading after this many rows per statement.">
                      <input className="input" type="number" min={100} max={100000} defaultValue={settings.row_limit} onBlur={(e) => save({ row_limit: Math.max(100, Math.min(100000, Number(e.target.value) || 1000)) })} />
                    </Field>
                  </div>
                  <h2>Mock servers</h2>
                  <Field label="PostgreSQL bin folder" hint="Used by PostgreSQL mocks on the Real PostgreSQL engine. Leave empty to find an installation automatically (Homebrew, Postgres.app, PATH, …).">
                    <input className="input mono" defaultValue={settings.postgres_bin ?? ""} placeholder="/opt/homebrew/opt/postgresql@17/bin" onBlur={(e) => save({ postgres_bin: e.target.value.trim() })} />
                  </Field>
                  <h2>Drivers</h2>
                  <Field label="Oracle Instant Client folder" hint="Oracle connections load Oracle Instant Client (Basic or Basic Light). Leave empty to look in the usual places (~/Downloads/instantclient_*, /opt/oracle, …).">
                    <input className="input mono" defaultValue={settings.oracle_client ?? ""} placeholder="/opt/oracle/instantclient_23_3" onBlur={(e) => save({ oracle_client: e.target.value.trim() })} />
                  </Field>
                  <h2>Safety</h2>
                  <label className="check">
                    <input type="checkbox" checked={settings.confirm_destructive} onChange={(e) => save({ confirm_destructive: e.target.checked })} />
                    Ask before running DROP, TRUNCATE, or UPDATE/DELETE without WHERE on real (non-mock) databases
                  </label>
                </>
              )}
              {section === "agents" && <AgentsSettings />}
              {section === "workspace" && <WorkspaceSettings />}
              {section === "about" && <About />}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

function AgentsSettings() {
  const settings = useStore((s) => s.settings)!;
  const updateSettings = useStore((s) => s.updateSettings);
  const agents = useStore((s) => s.agents);
  const mcpListening = useStore((s) => s.mcpListening);
  const config = useAsync(() => call<McpConfig>("mcp.config"), [settings.mcp.port, settings.mcp.token]);
  const [active, setActive] = useState("claude-code");
  const [regen, setRegen] = useState(false);
  const [port, setPort] = useState(String(settings.mcp.port));
  const agent = config.data?.agents.find((a) => a.id === active) ?? config.data?.agents.find((a) => a.detected) ?? config.data?.agents[0];
  const [installing, setInstalling] = useState(false);
  const install = async (id: string) => {
    setInstalling(true);
    const r = await attempt("Could not install", () => call<{ path: string; backup?: string }>("mcp.install", { agent: id }));
    setInstalling(false);
    if (r) {
      useStore.getState().toast("success", "Installed — restart the agent to load TwinBase", r.backup ? `${r.path}\nOriginal kept as ${r.backup}` : r.path);
      config.reload();
    }
  };
  return (
    <>
      <h1 className="row">
        <Bot size={20} /> AI agents
      </h1>
      <p>
        TwinBase is an MCP server. Coding agents such as Claude Code, Codex, Gemini CLI or Antigravity can list connections, design schemas, run SQL, edit rows and manage mock servers. Everything they do appears live in the Activity panel. Real
        databases stay read-only for agents unless you allow writes per connection; mock servers are writable.
      </p>
      <div className="card" style={{ display: "flex", flexDirection: "column", gap: 12 }}>
        <label className="check">
          <input type="checkbox" checked={settings.mcp.enabled} onChange={(e) => attempt("Could not save", () => updateSettings({ mcp: { enabled: e.target.checked } }))} />
          <strong>Allow AI agents to connect</strong>
        </label>
        <div className="grid-2">
          <Field label="Endpoint" hint="Listens on 127.0.0.1 only; requests need the token.">
            <div className="row">
              <code className="ellipsis grow selectable">{config.data?.url}</code>
              {config.data && <CopyButton text={config.data.url} />}
            </div>
          </Field>
          <Field label="Port" hint="Restart TwinBase after changing it.">
            <div className="row">
              <input className="input" style={{ width: 100 }} value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
              <button className="btn sm" disabled={Number(port) === settings.mcp.port || Number(port) < 1024} onClick={() => attempt("Could not save", () => updateSettings({ mcp: { port: Number(port) } }))}>
                Save
              </button>
            </div>
          </Field>
        </div>
        <Field label="Access token">
          <div className="row">
            <code className="grow ellipsis selectable">{settings.mcp.token.slice(0, 8)}••••••••••••••••</code>
            <CopyButton text={settings.mcp.token} label="Copy" small />
            <button className="btn sm ghost" onClick={() => setRegen(true)}>
              <RefreshCw size={13} /> New token
            </button>
          </div>
        </Field>
        {settings.mcp.enabled && mcpListening === null && (
          <ErrorBox error={`TwinBase is not listening for agents: port ${settings.mcp.port} is in use (another TwinBase window or \`twinbase serve\`?). Choose another port and restart.`} />
        )}
        {agents.length > 0 && <div className="small">Connected this session: {agents.map((a) => <span key={a} className="badge accent" style={{ marginRight: 4 }}>{a}</span>)}</div>}
      </div>
      <h2>Set up your agent</h2>
      <ErrorBox error={config.error} />
      {config.loading && !config.data && <Spinner />}
      {config.data && (
        <>
          <div className="agent-legend faint xs">
            <span className="agent-state" /> found on this computer <span className="agent-state installed" /> TwinBase installed
          </div>
          <div className="agent-grid">
            {config.data.agents.map((a) => (
              <button key={a.id} className={`agent-pick${agent?.id === a.id ? " on" : ""}`} onClick={() => setActive(a.id)}>
                <span className="ellipsis">{a.name}</span>
                {a.install?.installed ? (
                  <span className="agent-state installed" title="TwinBase is installed in this agent" />
                ) : a.detected && a.id !== "stdio" && a.id !== "http" ? (
                  <span className="agent-state" title="Found on this computer" />
                ) : null}
              </button>
            ))}
          </div>
          {agent && (
            <div className="agent-setup">
              <p style={{ marginTop: 0 }}>{agent.steps}</p>
              {(agent.command || agent.install || agent.link) && (
                <div className="agent-quick">
                  {agent.command && (
                    <div className="agent-cmd">
                      <span className="faint xs">Terminal</span>
                      <code className="selectable">{agent.command}</code>
                      <CopyButton text={agent.command} small />
                    </div>
                  )}
                  <div className="row wrap">
                    {agent.install && (
                      <button className="btn primary sm" disabled={installing} onClick={() => install(agent.id)} title={agent.install.path}>
                        <Download size={13} /> {agent.install.installed ? "Reinstall" : "Install"} in {agent.name}
                      </button>
                    )}
                    {agent.link && (
                      <a className="btn sm" href={agent.link} onClick={(e) => (e.preventDefault(), void openExternal(agent.link!))}>
                        <ExternalLink size={13} /> Open in Cursor
                      </a>
                    )}
                    {agent.install && <span className="faint xs mono ellipsis" title={agent.install.path}>{agent.install.path}</span>}
                  </div>
                </div>
              )}
              <details className="agent-manual" open={!agent.command && !agent.install}>
                <summary>Configure by hand</summary>
                <CodeBlock code={agent.config} />
              </details>
            </div>
          )}
          <div className="note-box" style={{ marginTop: 14 }}>
            {config.data.note} The stdio bridge (<code>{config.data.cli} mcp</code>) reads the token from this computer automatically, so its configuration contains no secret.
          </div>
        </>
      )}
      {regen && (
        <ConfirmDialog
          title="Create a new token?"
          body="Agents configured with the old token stop working until you update their configuration. The stdio bridge picks up the new token automatically."
          confirmLabel="Create new token"
          onClose={() => setRegen(false)}
          onConfirm={() => attempt("Could not save", () => updateSettings({ mcp: { token: `tb_${crypto.randomUUID().replace(/-/g, "")}` } })).then(() => undefined)}
        />
      )}
    </>
  );
}

function WorkspaceSettings() {
  const ws = useStore((s) => s.workspace);
  const settings = useStore((s) => s.settings);
  const openDialog = useStore((s) => s.openDialog);
  if (!ws) return null;
  return (
    <>
      <h1>Workspace</h1>
      <p>A workspace is a folder of plain files (TOML, SQL, Markdown) you can keep in Git and share. Passwords stay on this computer, outside the folder.</p>
      <dl className="kv" style={{ marginTop: 14 }}>
        <dt>Name</dt>
        <dd>{ws.name}</dd>
        <dt>Folder</dt>
        <dd className="mono selectable">{ws.path}</dd>
        <dt>Contents</dt>
        <dd>
          {ws.connections.length} connections · {ws.mocks.length} mock servers · {ws.collections.length} collections · {ws.queries.length} saved queries
        </dd>
      </dl>
      <div className="row" style={{ marginTop: 16 }}>
        <button className="btn" onClick={() => openDialog({ kind: "workspace" })}>
          <FolderOpen size={14} /> Open or create workspace…
        </button>
        <button className="btn" onClick={() => openDialog({ kind: "export" })}>
          Export bundle…
        </button>
        <button className="btn" onClick={() => openDialog({ kind: "import" })}>
          Import bundle…
        </button>
      </div>
      {settings && settings.recent_workspaces.length > 1 && (
        <>
          <h2>Recent</h2>
          {settings.recent_workspaces.map((p) => (
            <div key={p} className="mono small" style={{ padding: "3px 0" }}>
              {p}
            </div>
          ))}
        </>
      )}
    </>
  );
}

function About() {
  const info = useAsync(() => call<{ version: string; app_dir: string; workspace: string; secret_store: string }>("app.info"), []);
  return (
    <>
      <h1>TwinBase</h1>
      <p>Database client and wire-protocol mock servers for prototyping. Open source under the Apache License 2.0, by LibreGuild.</p>
      {info.data && (
        <dl className="kv">
          <dt>Version</dt>
          <dd>{info.data.version}</dd>
          <dt>App data</dt>
          <dd className="mono selectable">{info.data.app_dir}</dd>
          <dt>Runtime</dt>
          <dd>{runtime.isTauri ? "Desktop app" : "Browser (development)"}</dd>
          <dt>Passwords stored in</dt>
          <dd>{info.data.secret_store === "keychain" ? "the operating system keychain" : "a private file in the app data folder"}</dd>
        </dl>
      )}
    </>
  );
}
