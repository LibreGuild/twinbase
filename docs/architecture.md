# Architecture

TwinBase is a Rust application with a web-technology UI. All behaviour lives in Rust crates; the UI
and the AI-agent interface are two thin front ends over the same API.

```
                 ┌────────────── desktop app (Tauri 2) ──────────────┐
  React UI ──IPC──┤ `api(method, params)` command · event forwarding  │
                 │                                                   │
  AI agents ─HTTP─┤ 127.0.0.1:47800/mcp  (MCP, bearer token)          │
  (stdio bridge:  │                                                   │
   `twinbase mcp`)│              twinbase-core  (App)                 │
                 │   workspaces · settings · JSON API · events ·     │
                 │   twin preview · sample data · bundles · MCP      │
                 │        │                         │                │
                 │  twinbase-client            twinbase-mock          │
                 │  (PostgreSQL, MySQL, SQL    (PG / MySQL / Redis /  │
                 │   Server, SQLite, Redis,     MongoDB / DynamoDB /  │
                 │   MongoDB,                                         │
                 │   DynamoDB, Firestore)       Firestore servers)    │
                 │                                   │                │
                 │                           twinbase-engine (TwinLite)│
                 └───────────────────────────────────────────────────┘
         your application ──TCP (real protocols)──▶ mock servers
```

## Crates

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `twinbase-engine` | TwinLite: SQLite storage, PostgreSQL/MySQL SQL translation, types, catalogs, errors | rusqlite, sqlparser |
| `twinbase-mock` | Wire-protocol servers, authentication, traffic log, fault injection; Redis, MongoDB, DynamoDB and Firestore stores | engine, pgwire, opensrv-mysql, bson, hyper, tonic |
| `twinbase-firestore-proto` | Firestore v1 gRPC types generated from Google's protos; JSON conversions | tonic, prost |
| `twinbase-client` | Connections to real databases, introspection, paging, staged edits, transactions; SQL Server (TDS), Oracle (ODPI-C), ClickHouse (HTTP), Cassandra/ScyllaDB (CQL), Spanner and BigQuery (REST, Google auth), Elasticsearch/OpenSearch (REST), MongoDB, DynamoDB (SigV4) and Firestore (gRPC) drivers and console languages | tokio-postgres, mysql_async, tiberius, oracle, scylla, rusqlite, redis, mongodb, hyper, tonic, rustls |
| `twinbase-core` | `App` state, workspace files, the JSON API, preview, sample data, bundles, MCP, HTTP | engine, mock, client, axum |
| `twinbase-cli` | `twinbase` binary | core |
| `apps/desktop/src-tauri` | Desktop shell: window, `api` command, events, MCP endpoint lifetime | core, tauri |

The engine and mock crates have no knowledge of the UI; the client crate has no knowledge of mocks.
Mock servers are reached by the core through the same client code used for real databases (a mock is
just a connection with id `mock:<id>`), which keeps "what works on a mock" and "what works on
production" the same code path.

## One API for everyone

`twinbase_core::api::call(app, method, params, actor)` is the single entry point. Methods are grouped
by prefix: `app.*`, `settings.*`, `workspace.*`, `connection.*`, `mock.*`, `db.*`, `redis.*`,
`collection.*`, `query.*`, `scenario.run`, `bundle.*`, `activity.list`, `mcp.config`.

- The **desktop UI** calls it through one Tauri command, `api`.
- **MCP tools** are a curated set of tools translated into API calls (`crates/core/src/mcp.rs`).
- The **CLI** calls it directly (`export`, `import`), or serves it (`serve`).
- In **browser dev mode** (`twinbase serve --dev-api`) it is exposed as `POST /api/<method>`.

Every call carries an `Actor` (`User`, `Agent { name }`, `Cli`). Mutating calls, and every call made by
an agent, are recorded as `Activity` and broadcast as events, which is how the UI shows agent actions
live.

## Events

`App` owns a broadcast channel of `Event`s: `activity`, `mock_status`, `mock_traffic`,
`connection_status`, `schema_changed`, `data_changed`, `workspace_changed`, `mcp_client`. The desktop
shell forwards them to the webview as `twinbase://event`; dev mode streams them over SSE. Views use
`schema_changed`/`data_changed` to refresh: an open table reloads when an agent changes its rows,
unless you have unsaved edits (then it offers to reload).

## Access rules

- Saved connections have an **AI access** level: `none`, `read-only` (default) or `read-write`.
  Read-only means agents can introspect and run statements classified as read-only; writes are
  refused with a clear error and logged.
- Mock servers are always writable by agents.
- A connection can also be **read-only for you** (UI blocks edits and writes).
- On real databases the SQL editor asks before `DROP`, `TRUNCATE`, and unfiltered `UPDATE`/`DELETE`,
  and offers "Preview first".

## Security model

- The MCP/HTTP endpoint binds **127.0.0.1 only** and requires a bearer token (constant-time check).
  Requests with a non-local `Origin` are rejected (DNS-rebinding protection).
- Passwords and SSH secrets live in the OS keychain (or, without one, `secrets.toml` with mode
  0600); the MCP token lives in `settings.toml` (mode 0600) so the CLI bridge can read it. None of
  them ever goes into workspace files, exports or bundles. The stdio bridge reads the token locally, so agent
  configurations for it contain no secret.
- The desktop webview uses a strict CSP and only the `api` command plus native open/save dialogs.

## Persistence

- **App directory** (`~/Library/Application Support/TwinBase`, `%APPDATA%\TwinBase`,
  `~/.config/TwinBase`, or `TWINBASE_HOME`): settings, secrets, recent workspaces.
- **Workspace folder**: plain TOML/SQL/Markdown files meant for Git — see
  [workspace-format.md](workspace-format.md).
- **Mock data**: SQLite files (SQL mocks) or a JSON snapshot (Redis) inside the workspace when the mock
  is persistent, otherwise a temporary directory.

## Change preview (local twin)

`db.preview` copies the tables a statement touches (and the tables reachable through foreign keys)
from the target database into a temporary TwinLite engine, runs the statements there, and diffs the
rows. `db.commit` then runs the same SQL on the server in one transaction and rolls back if the
affected-row count differs from the preview. See [client.md](client.md).

## Designed for the future proxy

The last planned phase is a transparent L4 TCP proxy (TwinBase Proxy) that lets the app reach private
databases through a VM in any cloud without changing the wire protocol. Nothing in the client assumes
direct reachability: a connection is only `host:port` plus credentials, so pointing it at a proxy
needs no new code path. See `plan.md` §9.
