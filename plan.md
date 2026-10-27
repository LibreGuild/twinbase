# TwinBase — Project Plan

> TwinBase = *digital twin* + *database*. One lightweight desktop application that is both a
> **visual database client** and a set of **database mock servers**, built for developers who
> prototype schemas, test queries, and build applications against databases they cannot easily
> reach from their own machine.

- **Organization:** LibreGuild
- **License:** Apache-2.0 (open source)
- **Status:** Phase 1 in progress
- **Audience:** developers — in enterprises or working alone — who design schemas, prototype
  backends, and test application code locally.

This file is the single source of truth for *what* we build and in *which order*. The `docs/`
folder explains *how* it is built (architecture, engine design, file formats, MCP, testing).

---

## 1. Problem

1. Most production databases live in the cloud inside private networks (VPCs). Office and personal
   machines often cannot reach them, so local development becomes guesswork.
2. Running real database servers locally (natively or in Docker) costs memory, CPU, setup time, and
   maintenance — too heavy just to prototype a schema or run application tests.
3. Developers want to *see* their data and schema: browse tables, edit values, understand how
   tables relate, and know exactly what a statement will change **before** committing it.

## 2. Solution overview

TwinBase has two halves in one app, plus integrations:

| Part | What it does |
|---|---|
| **Client** | Connect to real servers (cloud, LAN, localhost) or to TwinBase mocks. Browse schemas, view and edit table data, run SQL, preview the effect of statements, visualize relationships. |
| **Mock servers** | Lightweight local servers that speak the **real wire protocols** (PostgreSQL, MySQL, Redis, …). Application code uses its normal driver/ORM, pointed at the mock's host and port — no code changes. Internals are simple; correctness of the protocol and the data matters, speed does not. |
| **Prototyping tools** | Visual schema designer, ER diagram, sample-data generator, seed scripts, saved queries with documentation, scenario checks. |
| **MCP server** | Coding agents (Claude Code, Codex, Gemini CLI, Antigravity, Cursor, …) control TwinBase through MCP. Everything an agent does appears live in the UI. |
| **CLI** | The same core runs headless (`twinbase serve`, `twinbase mock …`) for CI pipelines and test suites. |
| **Proxy** *(last phase)* | A transparent, protocol-preserving TCP proxy deployed inside the cloud network. See §9. |

## 3. Decisions log

| # | Decision | Reason |
|---|---|---|
| D1 | **Rust** core + **Tauri 2** desktop shell | Mock servers are long-running network daemons parsing binary protocols: Rust gives low memory, no GC pauses, safe parsing. Tauri uses the OS WebView: small installer and low RAM, matching the "lighter than Docker" promise. The same Rust core ships as a headless CLI. |
| D2 | **React + TypeScript + Vite** frontend | Largest ecosystem for data grids, code editors and diagrams. |
| D3 | Mock engine **"TwinLite"**: protocol front-ends over an embedded **SQLite** store with a dialect translation layer (PostgreSQL / MySQL → SQLite) and emulated system catalogs | Tiny (≈2 MB), very low memory, transactional, file-based (easy export/import). Writing a SQL engine from scratch is out of scope; SQLite supplies storage, transactions, constraints, CTEs, window functions, JSON, `RETURNING`, `ON CONFLICT`. Our layer supplies dialect, types, catalogs, and error codes. |
| D4 | Engine is pluggable | A later "native" engine (real PostgreSQL binaries via `postgresql_embedded`) can be offered for 100% fidelity without changing the protocol layer or UI. |
| D5 | Redis mock is a native in-memory store with snapshot persistence | Redis semantics are small and well defined; no SQL engine needed. |
| D6 | Client drivers: `tokio-postgres`, `mysql_async`, `rusqlite`, `redis` (with TLS via rustls) | Mature, async, give us raw access to arbitrary result types. |
| D7 | Workspace = a plain folder of **TOML** files (+ mock data files), Git-friendly | Easy to review, share, and version. Secrets never live in the workspace. |
| D8 | One backend API, many callers | The UI (Tauri IPC), the MCP server, and the CLI call the same core API, so behavior is identical and every action — human or AI — emits the same events. |
| D9 | MCP runs **inside the app** on `127.0.0.1` (Streamable HTTP), plus a **stdio bridge** (`twinbase mcp`) | HTTP for agents that support it; stdio bridge for every agent that only supports stdio. The app must be open — by design, so the user sees what the agent does. |
| D10 | Real-server connections are **read-only for AI by default**; mocks are read-write | Prevents an agent from accidentally modifying production data. Per-connection setting. |
| D11 | Proxy (last phase) is a **transparent L4 TCP proxy** | Clients keep their protocol and driver; they only change host and port. Runs on any VM in any cloud; no vendor dependency. Not built in Phase 1, but the core keeps protocol code reusable for it. |
| D12 | System fonts, no bundled web fonts; CSS variables for theming | Smaller bundle; native look; light/dark switch is instant. |

## 4. Core concepts and UX model

**Workspace** — a folder the user opens (default `~/TwinBase/Default`). Holds:
`connections/`, `mocks/`, `queries/`, `twinbase.toml`. Shareable via Git or via export bundles.

**Connection** — a saved target: a real server *or* a running mock. Kinds in Phase 1:
PostgreSQL, MySQL, SQLite (file), Redis. Each has an *AI access* level (none / read-only / read-write)
and an optional *read-only* flag that also protects against human mistakes.

**Mock server** — engine kind + port + options + data. Many mocks can run at the same time. Each
mock automatically has a matching connection so the UI can browse it.

**Query collections** — folders of saved queries. A saved query = SQL (or Redis commands) +
Markdown documentation + default connection + optional *expectation* (row count, "must fail", …)
used by the scenario runner.

**Tabs** — the main area holds many tabs at once, across different connections:
- *Table* tab: data grid (view/edit), structure, indexes, relationships.
- *Query* tab: editor, results (multiple result sets), messages, docs, preview diff.
- *Diagram* tab: ER diagram of a schema; doubles as the visual schema designer.
- *Redis* tab: key browser, value viewers per type, command console.
- *Mock* tab: status, ports, connection snippets for many languages, seed/reset, traffic log.
- *Settings*, *MCP setup*, *Welcome*.

**Activity panel** — live feed of everything that changed state: queries run, rows edited, mocks
started, and every MCP tool call (labelled with the agent's name).

## 5. Phase 1 scope (the groundwork)

Phase 1 delivers **all product features** end to end with the four P1 databases. Later phases mostly
add databases on top of the same groundwork.

### 5.1 Database support in Phase 1

| Database | Client | Mock | Wire-compatible products the mock/client also cover |
|---|---|---|---|
| PostgreSQL | ✅ | ✅ | Aurora PostgreSQL, Cloud SQL, AlloyDB, Supabase, Neon, (CockroachDB, YugabyteDB: client) |
| MySQL | ✅ | ✅ | MariaDB, Aurora MySQL, Cloud SQL MySQL, PlanetScale, (TiDB: client) |
| SQLite | ✅ | — (SQLite has no server; it is a file) | libSQL files |
| Redis | ✅ | ✅ | Valkey, Memorystore, ElastiCache, Dragonfly, KeyDB |

### 5.2 Mock servers
- Create, start, stop, restart, delete; several running concurrently; auto-start on app launch (opt-in).
- Real wire protocols: PostgreSQL v3 (simple + extended query, text + binary formats, SSL refusal,
  cleartext/MD5/SCRAM-capable auth, cancel), MySQL (handshake, `COM_QUERY`, prepared statements,
  `COM_INIT_DB`), Redis RESP2/RESP3 (`HELLO`).
- Auth modes: *accept any credentials* (default, so any app config works) or *require* a user/password.
- PostgreSQL databases auto-created on first connect (configurable); schemas supported.
- MySQL databases (`CREATE DATABASE`, `USE`).
- Emulated system catalogs good enough for drivers, ORMs and tools: `pg_catalog` subset,
  `information_schema` subset, `SHOW …`/`DESCRIBE` for MySQL, `version()`, session variables.
- Correct error codes (PostgreSQL SQLSTATE, MySQL error numbers) for constraint violations, missing
  objects, syntax errors — applications can test their error handling.
- Persistence: data saved in the workspace (file-backed) or ephemeral (temporary).
- **Seed script** per mock + **Reset to seed** (ideal for tests).
- **Traffic log**: every statement the mock received, with duration and result/error.
- **Connection snippets**: ready-to-copy connection strings/code for Node, Python, Go, Java, Rust,
  .NET, PHP, Ruby and the CLI tools (`psql`, `mysql`, `redis-cli`).

### 5.3 Client
- Connection manager: add/edit/test/delete connections; TLS options; password kept outside the
  workspace (app secret store); color label per connection (e.g. red for production).
- Schema explorer: databases → schemas → tables/views → columns, keys, indexes; refresh; filter.
- **Data grid**: virtualized rows and columns (handles millions of rows via paged fetch), resizable
  columns, sorting, filtering, NULL rendering, type-aware alignment, **no overflow**: long values are
  truncated with an ellipsis and open in a **value inspector** (pretty JSON, long text, binary as hex).
- **Inline editing with staged changes**: edit cells, add and delete rows; changes are highlighted and
  only applied on *Commit* (inside a transaction); *Discard* reverts. Generated SQL is shown.
- **SQL editor** (CodeMirror 6): dialect-aware highlighting, schema-aware autocomplete, run all /
  run selection / run statement under cursor, multiple result sets, timing, affected rows, errors with
  positions, row cap with "fetch more".
- Query history per connection.
- Result export: CSV, JSON, SQL `INSERT`s; copy cells/rows.
- **Redis**: key browser with pattern + type filter (SCAN-based), value viewers/editors for string,
  hash, list, set, sorted set, TTL management, command console with history.
- Read-only connections block writes in the UI and in MCP.

### 5.4 Change preview (local twin)
Before committing a statement to a server, TwinBase can run it against a **local twin**:
1. Parse the statement.
2. Copy the affected tables' schema, plus the rows the statement would touch (found with a read-only
   `SELECT` using the same `WHERE`), plus rows referenced by foreign keys, into an ephemeral TwinLite
   database.
3. Execute the statement on the twin and show a row/cell-level **diff** (added / changed / deleted),
   affected counts, cascades and constraint errors.
4. *Commit* runs the statement on the real server inside a transaction; if the affected row count
   differs from the preview, TwinBase rolls back and warns that the data changed in between.

Known limits (shown in the UI): server-side triggers/functions are not reproduced; very large affected
sets are capped (exact counts, sampled rows); DDL previews show the schema diff.

### 5.5 Prototyping features
- **ER diagram**: tables, columns, PK/FK markers, relationship edges with cardinality, auto-layout,
  zoom/pan, minimap, focus on a table and its neighbours, export as SVG/PNG.
- **Visual schema designer**: create/alter tables, columns, types, nullability, defaults, unique,
  primary and foreign keys — directly on the diagram or in a form; shows the generated DDL
  (dialect-aware) before applying.
- **Sample data generator**: fill tables with realistic data (names, emails, dates, prices, …) that
  respects types, uniqueness and foreign keys.
- **Seed scripts** and **reset**.
- **Scenario runner**: run a collection of saved queries with expectations (row count, empty,
  must-fail-with-code) against a mock and report which cases pass — quick edge-case coverage.
- **Schema export**: export the schema as DDL for PostgreSQL / MySQL / SQLite.

### 5.6 Saved queries, documentation and history
- Collections (folders) of queries; each query has SQL, Markdown docs, tags, default connection,
  optional expectation.
- Run a saved query on any compatible connection.
- Stored as TOML files in the workspace (diff-friendly).

### 5.7 Export / import and sharing
- Export a **bundle** (`.twinbase`, a zip) containing any selection of: mock servers (definition +
  data + seed), query collections, connection definitions (never secrets), schema DDL.
- Import a bundle into the current workspace (name conflicts are renamed, never overwritten).
- Whole workspaces are plain folders and can also be shared through Git.

### 5.8 MCP integration
- MCP server inside the app on `http://127.0.0.1:<port>/mcp` (Streamable HTTP), protected by a
  local token and Origin/Host validation.
- `twinbase mcp` stdio bridge for agents that only support stdio.
- Tools (initial set): list/create/start/stop mocks; list connections; list schemas/tables;
  describe table; get ER graph; run SQL; read rows with filters; insert/update/delete rows; Redis
  commands and key scan; list/save/run saved queries; generate sample data; preview a statement;
  export/import bundles.
- Every tool call appears in the Activity panel and refreshes affected views live.
- AI access levels per connection (D10).
- **MCP setup screen** with copy-paste instructions for Claude Code, Claude Desktop, Codex CLI,
  Gemini CLI, Google Antigravity, Cursor, VS Code (Copilot), Windsurf, Zed, and a generic JSON config.

### 5.9 UX requirements
- Light and dark themes (+ follow system). Dark theme: warm dark grey-brown, inspired by Claude's
  dark palette but darker — never pure black. Accent: Claude-style terracotta.
- Dense but readable typography; monospace for data; nothing overflows its container — truncation
  with tooltips and expandable inspectors everywhere.
- Multiple connections and mocks open at once in tabs; keyboard shortcuts; command palette.
- Resizable panes; state (open tabs, pane sizes, theme) restored on restart.

### 5.10 Settings and security
- App settings (theme, font size, row cap, MCP port/enabled, default mock ports, telemetry: none).
- Secrets (passwords) stored outside the workspace in the app data directory with owner-only file
  permissions; OS keychain integration planned.
- Mock servers bind to `127.0.0.1` by default; binding to all interfaces is an explicit option with a
  warning.

### 5.11 CLI (headless)
- `twinbase serve` — run the core (mocks + MCP) without the GUI, optionally for a workspace.
- `twinbase mock postgres|mysql|redis --port … [--seed file]` — start one mock (CI usage).
- `twinbase mcp` — stdio bridge to a running app or `serve` instance.
- `twinbase export` / `twinbase import`.

### 5.12 Quality, performance and resource budgets
- Automated tests for every layer: engine translation, protocol servers (tested with real drivers:
  `tokio-postgres`, `mysql_async`, `redis`, plus `psql`/`mysql`/`redis-cli` and Python/Node drivers),
  client drivers against both mocks and real servers (Docker in CI), core API, MCP, frontend units.
- **Fidelity suite**: the same SQL scripts run on real PostgreSQL/MySQL (Docker) and on the mocks;
  results are compared.
- Budgets (to be measured and recorded in `docs/performance.md`):
  - Desktop installer ≤ 40 MB; headless CLI ≤ 30 MB.
  - GUI idle memory ≤ 150 MB (including WebView processes).
  - Idle mock server ≤ 15 MB plus its data.
  - Cold start < 1.5 s; grid scrolling stays smooth on million-row tables (paged fetch).

## 6. Phase 1 milestones

Status: ☐ not started · ◐ in progress · ☑ done

| # | Milestone | Status |
|---|---|---|
| M0 | Repository scaffolding, plan, docs skeleton, license | ☑ |
| M1 | TwinLite engine: storage, PostgreSQL dialect translation, types, catalogs, error mapping | ☑ |
| M2 | PostgreSQL mock server (pgwire) + driver/tool compatibility tests | ☑ |
| M3 | MySQL dialect + MySQL mock server + compatibility tests | ☑ |
| M4 | Redis mock server + compatibility tests | ☑ |
| M5 | Client drivers (PostgreSQL, MySQL, SQLite, Redis): introspection, paging, staged changes | ☑ |
| M6 | Core: workspace files, managers, events, API, saved queries, history, export/import, sample data | ☑ |
| M7 | Change preview (local twin) | ☑ |
| M8 | MCP server + stdio bridge + setup instructions | ☑ |
| M9 | CLI | ☑ |
| M10 | Desktop UI: shell, themes, explorer, tabs, grid, editor, ER diagram/designer, Redis, mocks, activity, settings | ☑ |
| M11 | End-to-end testing, packaging, size/memory measurement, docs pass | ◐ |

Status notes (2026-10-25):

- M11 done: browser-driven end-to-end pass of the UI against the real core (templates, grid edits
  with review/preview/save, FK navigation, SQL preview → commit with cascades, designer create and
  alter, Redis browser and console, mock traffic, connections to real PostgreSQL, agent access rules,
  live agent activity, export with live snapshot); macOS release bundle built, launched, and measured
  (see docs/performance.md); docs written.
- M11 open: Windows and Linux bundles not yet built or tested; code signing/notarization; automated
  UI tests inside the Tauri webview; cold-start time not yet measured.

## 6b. Phase 2 and 3 milestones

Started 2026-10-25. The TwinBase Proxy stays deferred. Order is by value and risk; each milestone
ships with tests, core API, MCP tools, UI and docs.

| # | Milestone | Status |
|---|---|---|
| P2.1 | Fault injection for mocks: latency, dropped connections, forced errors (per mock, rule-based) | ☑ |
| P2.2 | Schema snapshots, diffs between snapshots/connections, migration script generation | ☑ |
| U1 | UI refresh (user request 2026-10-25): accent #55A9A0; top bar with logo, workspace dropdown, search, status pills, settings; settings as a pop-up; start page (new / open / default workspace, repo-friendly note); font family selection with a readable default; rounder, more premium styling; premium home page with generated artwork | ☑ |
| P2.3 | `EXPLAIN` visualizer (PostgreSQL JSON plans, MySQL JSON plans, TwinLite plans) | ☑ |
| P2.4 | OS keychain for secrets (file fallback) and SSH tunnels for client connections | ☑ |
| P2.5 | MongoDB: wire-protocol mock (OP_MSG, CRUD, queries, aggregation basics, indexes, SCRAM) + client + document UI | ☑ |
| P2.6 | Amazon DynamoDB: HTTP mock (tables, items, expressions, query/scan, batch, transactions, indexes) + client (SigV4) | ☑ |
| P2.7 | Google Firestore: gRPC mock compatible with `FIRESTORE_EMULATOR_HOST` + REST client (service accounts) | ☑ |
| P2.8 | Optional native PostgreSQL engine for mocks (real `postgres` binaries, traffic still visible) | ☑ |
| P3.1 | SQL Server client (TDS) with dialect support in grid, editor, DDL | ☑ |
| P3.2 | ClickHouse client (HTTP interface) | ☑ |
| P3.3 | Elasticsearch / OpenSearch client (document UI, query DSL console) | ☑ |
| P3.4 | Cassandra / ScyllaDB client (CQL) | ☑ |
| P3.5 | BigQuery and Spanner clients (Google auth: service account, ADC) | ☑ |
| P3.6 | Oracle client (requires Oracle Instant Client at runtime) | ☑ |
| P3.7 | SQL Server TDS mock (T-SQL dialect in TwinLite) | ☑ |
| R | Full code review: security, performance, edge cases — findings fixed and logged | ☑ |

### Work log

- 2026-10-25 — Phase 2/3 started.
- P2.1 done: `twinbase-mock::faults` (rules with substring/regex match, probability, delay/error/drop;
  refuse-connections switch; live updates), hooked into PostgreSQL (simple + extended; drop = FATAL
  57P01), MySQL (error numbers) and Redis (error prefixes); persisted in `mock.toml`; API
  `mock.faults.set`, MCP `set_mock_faults`, UI Faults tab with presets; tests in all three wire suites
  and core.
- P2.2 done: `core::schema_diff` (canonical type/default comparison, change list, ordered up/down
  scripts for PG/MySQL/SQLite incl. SQLite table rebuilds, rename/data-loss warnings); snapshots in
  `snapshots/<id>.json`, API `schema.snapshot.*`/`schema.diff`, MCP `snapshot_schema`,
  `list_schema_snapshots`, `diff_schemas`; Schema tab UI; snapshots in bundles. Round-trip tests on
  PG and MySQL mocks (v1 → up → equals v2 → down → equals v1). Fixed along the way: MySQL string
  defaults were unquoted (broke DDL export defaults); workspace ids are now validated against path
  traversal in every save/delete.
- U1 done (user request): teal accent and refreshed tokens (larger radii, layered surfaces, focus
  rings, motion); custom top bar (logo, workspace switcher menu, search, mock/agent status pills,
  activity, theme, settings) with transparent macOS title bar and drag regions; icon rail (Home,
  Databases, Saved queries, Schemas); settings pop-up; start page (default / new / existing
  workspace, repo-friendly guidance); bundled fonts with Inter + JetBrains Mono defaults and
  pickers; premium home (hero, stats, templates, agents banner) with artwork generated by OpenAI
  (gpt-image-2 does not support transparency, so gpt-image-1.5 was used for the transparent assets);
  teal app icon. `TWINBASE_HOME` now also isolates the default workspace. Verified in the browser
  (light/dark, start page, workspace creation and switching, settings, fonts, palette, data views).
- P2.3 done: `core::explain` parses PostgreSQL JSON/text, MySQL TREE/tabular and SQLite plans into one
  tree with hints (full scans with filters → index columns, misestimates, spills, filesort/temp
  B-trees); `db.explain` (ANALYZE runs in a rolled-back transaction via the new
  `SqlClient::query_rolled_back`, which keeps the connection locked and always rolls back; analyzing
  writes needs write access); MCP `explain_query`; SQL editor Explain menu and Plan tab. Tested on
  the PG mock and real PostgreSQL 16 (JSON plan, actual rows, write rollback, failed ANALYZE leaves no
  open transaction).
- P2.4 done: `client::ssh` (russh, ring backend): password / key (+passphrase) / ssh-agent auth,
  host keys checked against ~/.ssh/known_hosts then TwinBase's own known_hosts (trust on first use,
  changed keys refused), local port forwarding; TLS keeps the real host name (PG `hostaddr`, MySQL
  hostname override). Connection definitions carry `ssh`; SSH secrets stored like passwords. Secrets
  now go to the OS keychain (keyring 3) with migration from `secrets.toml`; isolated app dirs
  (TWINBASE_HOME, tests) always use the file store; secret files are written atomically with 0600.
  Tested against an OpenSSH bastion in Docker (password, key with passphrase, wrong password, changed
  host key) and a real keychain round trip (opt-in test). Incident: before the isolation rule, a test
  run wrote 8 throwaway test entries into the login keychain; they were removed (only temp-path
  accounts) and the rule now prevents it.
- P2.5 done: `twinbase-mock::mongo` — OP_MSG + OP_QUERY handshake, ~45 commands, query/update/
  projection/sort semantics (`query.rs`, positional `$`/`$[]`/`$[id]`), aggregation (`agg.rs`: 20
  stages, ~100 expression operators), store with unique/sparse indexes and BSON snapshots (unreadable
  snapshots moved aside), cursors with getMore, SCRAM-SHA-1/256 (`scram.rs`), fault injection on
  shell-like traffic text. Client: official `mongodb` driver (bson 3), connection strings (password
  stripped into the keychain on save), mongosh-syntax parser (`mongo_shell.rs`) for values and
  statements, console runner, schema sampling. Core: `mongo.*` API, seeds as Extended JSON
  (`seed.json`, validated, capture/reset), MCP tools `mongo_query`, `mongo_list_collections`,
  `mongo_collection_schema`, access classification of statements. UI: database/collection tree,
  collection tab (documents JSON/table, edit/duplicate/insert/delete, aggregation, indexes, schema),
  console for query tabs on MongoDB connections, mock dialog/seed editor/fault presets. Tests: 16 mock
  unit tests, 3 wire tests with the official driver, client suite run against the mock AND real
  MongoDB 7 (identical results), core e2e (seeds, API, console, agent access, URI passwords), mongosh
  2 against the mock from the mongo:7 image, UI checked in the browser. Also fixed: client tests had
  stopped compiling after P2.4 (missing `ssh`/`via`).
- P2.6 done: `twinbase-mock::dynamo` — hyper HTTP/1.1 server for the JSON 1.0 protocol with
  `x-amz-crc32`, SigV4 verification (checked against the AWS documentation's worked example), typed
  values with 38-digit BigDecimal numbers, full expression language with the reserved-word list and
  unused-placeholder checks (`expr.rs`), tables with GSIs/LSIs (sparse, projected), Query/Scan
  paging by Limit and 1 MB, parallel scan segments, batch ops, all-or-nothing transactions with
  CancellationReasons, TTL sweeping, tags, JSON snapshots; faults by exception name (connection drops
  close the socket). Client: own SigV4 signer + kept-alive hyper client (rustls for AWS), endpoint/
  region/credential chain (fields, env, ~/.aws/credentials), console script parser, (un)marshalling.
  Core: `dynamo.*` API (item browser builds expressions with placeholders so reserved words work),
  seeds as `{"tables": [...]}`, capture/reset, snippets (AWS CLI, JS v3, boto3, Go v2, Java v2), MCP
  `dynamodb_request` / `dynamodb_list_tables`, access classification of operations. UI: tables tree,
  table tab (scan/query builder with index, sort conditions, typed filters, paging, JSON/table views,
  typed item editor, table info), request console, DynamoDB connection fields, fault presets.
  JSON tree/table widgets moved to `components/JsonDocs.tsx` and shared with MongoDB. Tests: 7 unit
  tests, client parity suite on the mock (with SigV4) and DynamoDB Local 2.5 (identical), boto3
  script identical on both, core e2e, UI checked in the browser.
- P2.7 done: new crate `twinbase-firestore-proto` (googleapis protos vendored, tonic/prost codegen with
  vendored protoc; REST-typed and plain-with-markers JSON conversions shared by mock and client).
  Mock `twinbase-mock::firestore`: tonic gRPC service with every Firestore RPC used by SDKs (Get,
  List, Create, Update, Delete, BatchGet, Begin/Commit/Rollback, RunQuery, RunAggregationQuery,
  ListCollectionIds, BatchWrite, PartitionQuery, Listen); query engine with Firestore value order,
  implicit orders, cursors, projections, collection groups; field transforms and preconditions;
  optimistic transactions (ABORTED on contention); realtime Listen with consistent-snapshot markers;
  JSON snapshots. Decision: the client uses the gRPC API (same generated types) instead of REST — one
  protocol for Google, the emulator and the mock — with self-signed service-account JWTs (ring RS256),
  so no token exchange. Client: statement language (get/list/query/count/collections/set/update/add/
  delete), paging by name cursors. Core `firestore.*` API, seeds `{"documents": {...}}`, snippets
  (env, firebase-admin, web SDK, Python, Go, Java, firebase.json), MCP `firestore_query` /
  `firestore_list_collections`. UI: collections tree, collection tab (documents, where/order/paging,
  subcollection navigation with breadcrumbs, JSON editor, table view), console, connection fields
  (endpoint, project, key file), fault presets (gRPC statuses); JSON viewer renders Firestore markers
  as values. Tests: 3 unit, gRPC wire test incl. Listen, client suite, core e2e, the official Python SDK
  (filters, cursors, aggregations, transforms, transactions, on_snapshot, collection groups) against
  the mock, UI checked in the browser.
- P2.8 done: `twinbase-mock::native_pg`. Decision: TwinBase does not bundle PostgreSQL (size, per-OS
  builds, licence notices); it detects an installation (setting `postgres_bin`, `TWINBASE_PG_BIN`,
  PATH, Homebrew, Postgres.app, Debian/RHEL layouts, Windows installer; newest wins). The mock runs
  `initdb` (trust auth, UTF8, C locale) and `postgres` with no TCP listener, only a unix socket in a
  0700 temp directory, `fsync=off`; the mock port is a protocol-aware proxy that does the mock's
  auth, creates missing roles/databases, logs statements (simple + extended protocol, row counts from
  CommandComplete, SQLSTATE errors) and injects faults in protocol order (error delivered before the
  ReadyForQuery of a substituted Sync), forwards cancel requests, answers SSL/GSS with N. Stale
  servers from a crash are stopped via postmaster.pid; stop = `pg_ctl stop -m fast`, kill fallback.
  Seeds run through an admin session; reset/capture/explain reuse the generic paths. Mock def gets
  `engine = "native"`; MCP `create_mock_server` takes `engine`; CLI `twinbase mock postgres --native
  [BIN]`; API `mock.native.detect`; UI engine selector with detection hint, settings field, badge.
  Tests (gated on `TWINBASE_PG_BIN`; run with PostgreSQL 17.11 test binaries from Maven Central,
  sha1-verified, in the scratchpad): proxy test (PL/pgSQL, jsonb, window functions, COPY, faults on
  both protocols, traffic, persistence, password auth), core e2e (seed, query, explain, capture,
  reset), CLI smoke with psql, UI checked in the browser.
- P3.1 done: `twinbase-client::mssql` on tiberius 0.13 (TDS 7.3+, rustls). Decisions: scripts split
  at `GO` lines; inside a batch each statement runs alone followed by `SELECT @@ROWCOUNT` (tiberius
  exposes either result sets or row counts, not both), and batches with variables, blocks or module
  definitions run whole; session options that change what statements return (`SHOWPLAN_*`,
  `STATISTICS PROFILE/XML`, `NOEXEC`, …) are tracked so no probe is added then. Login-only
  encryption without certificate checks for TLS Off (as Microsoft's drivers), TrustServerCertificate
  for Required, OS roots for Verify; Entra access tokens (empty user); Azure SQL redirects. Exact
  decimal text, trimmed fractional seconds, GUIDs upper-case, bigints beyond 2^53 as strings.
  Introspection from `sys.*` (exact type text, identity, defaults with SQL Server's parentheses
  removed, indexes, FKs). Grid: `OFFSET … FETCH`, `[ident]`, `N'…'`, `[` escaped in LIKE. Plans:
  new `SqlClient::query_wrapped` (setup / statement / guaranteed teardown on one locked connection)
  for `SHOWPLAN_ALL` and `STATISTICS PROFILE` (rolled back), parsed into the plan tree with hints.
  DDL export, schema diff (ADD, ALTER COLUMN, named default and PK constraints dropped via dynamic
  lookups, constraint-backed indexes) and the UI table designer write T-SQL; engine types gained
  `mssql_column_type` and SQL Server type names. Preview is refused with an explanation (TwinLite has
  no T-SQL; revisit with P3.7). Snippets (.NET, EF Core, Node mssql, Prisma, pyodbc, SQLAlchemy, Go,
  JDBC, Spring, sqlcmd). Found and fixed on the way: rustls had two crypto providers once tiberius
  pulled in aws-lc-rs (ring is now installed as the process default); driver connect futures are boxed
  (a debug-build stack overflow on 2 MiB stacks); the read-only classifier treated `SELECT … INTO` as
  a read and keywords inside string literals as writes — now string/comment-aware, `SELECT INTO`,
  `EXEC`, `CALL` count as writes (also in the UI check, which now splits at `GO`). Tests: unit
  (batch splitting, numeric text, defaults, paging SQL, showplan parsing, designer T-SQL), client
  suite and core e2e against SQL Server 2022 in Docker (types, errors with numbers, introspection,
  paging, staged edits with stale-row rollback, rolled-back runs, truncation, plans, DDL export to
  T-SQL and PostgreSQL, schema diff applied and converging, agent access), UI checked in the browser.
- P3.2 done: new `twinbase-client::http::HttpConn` (kept-alive HTTP/1.1 + rustls, one retry on a
  closed connection) now shared by DynamoDB and ClickHouse (and the HTTP clients to come).
  `twinbase-client::clickhouse`: HTTP interface, session per client (USE/SET/temporary tables last),
  JSONCompact with quoted 64-bit integers and decimals (exact), `max_result_rows` + `break` for
  truncation, rows written from `X-ClickHouse-Summary`, errors parsed into message + code name
  (advice paragraphs dropped), URLs or host+port, TLS 8443. Introspection from `system.tables`,
  `system.columns`, `system.data_skipping_indices` (sorting key as primary key; MATERIALIZED/ALIAS as
  generated). Decision: grid edits are refused (no transactions, mutations asynchronous) — the grid is
  read-only with an explanation, and preview is refused. Plans from `EXPLAIN json = 1, indexes = 1`
  with granule counts and a "read completely" hint. Engine types gained `parse_clickhouse` (names
  clash with PostgreSQL's: `Int8` is 8-bit) and `clickhouse_column_type`; DDL export writes
  `MergeTree ORDER BY` with `Nullable` types; schema diff uses ADD/MODIFY/DROP COLUMN and reports
  sorting-key/index differences instead of scripting them. UI: kind, connection hint, MySQL-style
  editor dialect, read-only grid, designer DDL (MergeTree, MODIFY COLUMN, REMOVE DEFAULT, RENAME
  TABLE), DDL dialect choice. Snippets (curl, clickhouse-client, Node, Python clickhouse-connect,
  Go HTTP, JDBC). Tests: unit (errors, endpoints, integers, plan parsing, designer), client suite and
  core e2e against ClickHouse 25.8 in Docker (types, sessions, errors, introspection, paging/filters,
  refused edits, truncation, FORMAT output, plans and hints, DDL to ClickHouse and PostgreSQL, schema
  diff applied and converging, agent access); DynamoDB suites re-run on the shared HTTP client; UI
  checked in the browser.
- P3.3 done: `twinbase-client::search_client` on the shared `HttpConn`: product detection
  (Elasticsearch vs OpenSearch), basic auth or API key, URL or host+port endpoints; indices with
  counts and aliases, flattened mappings (multi-fields), `_search` with sequence numbers, optimistic
  saves (`if_seq_no`/`if_primary_term`), create-only puts, bulk deletes, `refresh=wait_for`. Console
  parser for Dev Tools syntax (method + path lines, JSON bodies, NDJSON for `_bulk`/`_msearch`,
  `"""` strings, comments); request paths are validated (no absolute URLs or whitespace).
  Read/write classification by method and endpoint (`_search`, `_count`, `_sql`, `_query`, … are
  reads; `_bulk`, `_doc`, `_update_by_query`, … always writes). Core `search.*` API with access checks,
  history and change events; MCP `search_request`, `search_list_indices`, `search_index_mapping`;
  snippets. UI: index tree (system indices toggle), index tab (query string or DSL, sort by mapping
  fields, paging, JSON/table, fields panel, document editor with conflict detection), Dev Tools
  console with per-request result tabs, hits view and raw JSON. Consolidated the per-kind console
  defaults (`CONSOLE_KINDS`, `consoleStart`) that were duplicated in the sidebar, command palette and
  home page. Tests: unit (console parsing, read classification, errors), client suite and core e2e
  against Elasticsearch 8.17 in Docker (bulk, search, mapping, conflicts, create-only, deletes, error
  stop, `_cat`, SQL, agent access), UI checked in the browser.
- P3.4 done: `twinbase-client::cassandra` on the `scylla` 1.9 driver (CQL, rustls). Decision: every
  node is reached through the contact address (an address translator), because nodes advertise
  addresses private to their network (Docker, Kubernetes, NAT, SSH tunnels); the contacted node
  coordinates. Rows stream with a page limit; exact decimal/varint text, timestamps, blobs,
  collections, UDTs, tuples, LWT `[applied]` rows; errors mapped to CQL error kinds. Introspection from
  `system_schema` (partition then clustering columns as the primary key, kept as a "partition key"
  index so DDL can rebuild `PRIMARY KEY ((…), …)`; secondary indexes; materialized views).
  ScyllaDB detected via `system.versions`. Grid: read-ahead paging (no OFFSET), no total, `ALLOW
  FILTERING` with filters, sorting refused with an explanation; staged edits as typed CQL
  (`cql_literal`: uuid, blob, numbers, sets/lists/maps) in one logged batch. Explain checks the
  statement's shape. Engine types gained `parse_cql`/`cql_column_type`; DDL export and schema diff
  (ADD/DROP, warnings for type/key changes) write CQL; preview refused. UI: kind, keyspace field,
  hints, CQL editor dialect, designer (PRIMARY KEY, ADD/DROP, key renames only), DDL dialect choice.
  Snippets (cqlsh, Node, Python, Go gocql, Java). Tests: unit (exact numbers, error kinds, typed
  literals, paging SQL, designer), client suite and core e2e against Cassandra 5.0 in Docker (types,
  LWT, errors, introspection, paging/filters, typed batch edits, truncation, DDL to CQL and
  PostgreSQL, explain hints, schema diff applied and converging, agent access), UI checked in the
  browser.
- P3.5 done: shared `google_auth` (service-account keys signing self-signed JWTs per audience,
  ADC `authorized_user` refresh-token exchange with caching, fixed tokens for emulators/mocks;
  Firestore moved onto it). `twinbase-client::spanner` (REST): sessions recreated when expired,
  single-use strong reads, DML in its own read-write transaction with exact counts, consecutive DDL
  as one `updateDdl` with operation polling, staged edits in one read-write transaction with the
  count check, rolled-back runs, `PLAN`/`PROFILE` plans through a new `SqlClient::native_plan`.
  Introspection from INFORMATION_SCHEMA incl. interleaving (as a parent reference) and FKs.
  `twinbase-client::bigquery` (REST): query jobs with polling, typed row decoding (STRUCT, ARRAY,
  TIMESTAMP, BYTES, JSON), datasets/tables/table metadata (unenforced PK/FK, clustering), free
  `tabledata.list` pages through a new `SqlClient::fetch_page` hook, dry-run cost estimates;
  read-only grid. GoogleSQL in sqlgen (backtick names, backslash-escaped strings, case-insensitive
  contains via LOWER, typed literals `googlesql_literal`), engine `parse_googlesql` /
  `googlesql_column_type`, DDL (Spanner `PRIMARY KEY` after the columns + `INTERLEAVE IN PARENT`,
  BigQuery `NOT ENFORCED` keys), schema diffs, explain parsers (Spanner plan nodes, BigQuery dry
  run with cost), snippets, UI (connection fields shared with Firestore, designer DDL, dialect
  lists). Found and fixed on the way: the shared HTTP client retried requests after a connection
  closed mid-request, which could replay a non-idempotent POST; it now retries only requests that
  were never sent or are idempotent, and POSTs reuse a connection only if it was used in the last
  4 s. Also: `target/` had reached 87 GB (dev builds with full debug info for every test binary);
  dev builds now keep line tables only (dependencies none) — about 2.5 GB after a rebuild. Tests:
  unit (auth files, value decoding, literals, plans, designer), client suites and core e2e against
  the Spanner and BigQuery emulators (types, DDL batches, DML counts, interleaving, paging/filters,
  typed transactional edits, stale-edit rollback, rolled-back runs, tabledata.list, DDL export,
  schema diff applied and converging, agent access). Not verified against Google Cloud itself (no
  real credentials used); the emulators' gaps are noted in the tests (Spanner REST error details,
  BigQuery tabledata.list mixing same-named tables). UI checked in the browser. Later in the run macOS began
  refusing connections to loopback from sockets bound to the wildcard address (EADDRNOTAVAIL), which
  the Cassandra driver does; it now binds loopback contacts to loopback.
- P3.6 done (not verified against a live Oracle database): `twinbase-client::oracle_db` on the
  `oracle` crate (ODPI-C, compiled in; Instant Client loaded at run time from the setting
  `oracle_client`, `TWINBASE_ORACLE_CLIENT`, or common install folders, with a clear message when
  missing). Blocking calls run on worker threads. SQL*Plus-style splitting (`;` stripped, PL/SQL
  units to `/`), autocommit statements, transactional staged edits with the count check, rolled-back
  runs, typed cells (exact NUMBERs, dates, timestamps with zones, RAW/BLOB hex), `ORA-` codes.
  Introspection from `ALL_*` views (identity, defaults, P/U/R constraints, indexes). sqlgen paging
  with `OFFSET … FETCH` and case-insensitive filters; engine types parse and write Oracle types
  (`NUMBER(p,s)`, `VARCHAR2(n CHAR)`, …); DDL (identity and DEFAULT placed before NOT NULL, as Oracle
  requires), schema diff (`ADD`, `MODIFY (…)`, `DROP PRIMARY KEY`), explain via `EXPLAIN PLAN` +
  `PLAN_TABLE` (using `query_wrapped` for guaranteed cleanup), snippets, UI (kind, service-name
  field, Instant Client setting, PL/SQL editor dialect, designer). Decision: Instant Client and the
  Oracle database images were not downloaded — both require accepting Oracle's license terms on the
  user's behalf — so the end-to-end suite (`crates/client/tests/oracle.rs`) is gated on env vars.
  Tested: unit (splitting, connect strings, type text, paging SQL, plan rows, designer), core tests
  for DDL conversion to Oracle and the missing-client message; full workspace suite green.
- P3.7 done: TwinLite T-SQL dialect (`Dialect::MsSql`): case-insensitive names, `dbo`, one
  database per engine whatever name clients use, `TOP`/`OFFSET … FETCH`, `IDENTITY` (moves past
  explicit ids; `SET IDENTITY_INSERT` accepted), `OUTPUT INSERTED/DELETED` → `RETURNING`, `N'…'`,
  string `+`, T-SQL functions and `@@` variables (`COUNT`/`@@ROWCOUNT` typed `int`, `COUNT_BIG`
  `bigint`), unnamed computed columns, `ALTER COLUMN`, named default constraints (`ADD … DEFAULT …
  FOR`, `DROP CONSTRAINT`), `sys.*` catalog views, SQL Server error numbers. New `engine::tsql`: a
  batch interpreter (GO, statements without semicolons, variables, IF/WHILE/BEGIN…END, TRY…CATCH with
  `ERROR_*()`, THROW/RAISERROR/PRINT/RETURN, `EXEC (…)` and `sp_executesql`, loop cap). Division by
  zero now fails in the PostgreSQL and SQL Server dialects (22012), as on the real servers.
  `twinbase-mock::tds`: TDS 7.4 server — PRELOGIN with TLS as SQL Server negotiates it (login-only,
  full, none; self-signed certificate, TLS records wrapped in PRELOGIN packets during the handshake),
  LOGIN7 (password de-obfuscation, 18456), LOGINACK/ENVCHANGE, SQL batches, RPC
  (`sp_executesql`/`sp_prepare`/`sp_prepexec`/`sp_execute`/`sp_unprepare`, typed parameter decoding),
  transaction-manager requests with transaction ENVCHANGEs, attention, typed result columns, PRINT as
  INFO, CurCmd in DONE tokens, faults (error numbers) and traffic. Wired into core (kind `mssql`,
  port 11433+, seeds with `IDENTITY_INSERT`, capture/reset), CLI (`twinbase mock mssql`), MCP, UI (mock
  dialog, sidebar menu, fault presets 1205/3906, seed editor), docs. Change preview now works for SQL
  Server (dbo) on a T-SQL twin, for mocks and real servers (`SELECT TOP` reads). Tested: engine T-SQL
  suite, wire tests with tiberius (login-only/full/no TLS, auth failure, parameters, types, OUTPUT,
  errors, transactions), Microsoft `sqlcmd` 18 (ODBC Driver 18) from the `mssql/server:2022` image
  with mandatory and optional encryption, core e2e (generate, browse, edit, preview, seed capture and
  reset, T-SQL migrations round trip), real SQL Server core test (now including preview), UI in the
  browser; full workspace suite green.
- R done — full review of trust boundaries (HTTP/MCP endpoint, agent access, bundle import, mock wire
  parsers, Tauri shell, secrets), performance and edge cases. Findings, all fixed with tests:
  - **Security — agent read-only bypass:** the statement check read every dialect's strings and
    comments the same way, so `'\'`, `#` or `$$'$$` could hide a data-modifying CTE from it on
    PostgreSQL (and `/*! … */`, Oracle `q'[…]'` elsewhere). The lexer is now per dialect (backslash
    escapes only where the server uses them, `E'…'`, `#` comments only for MySQL/GoogleSQL,
    brackets only for SQL Server/SQLite, dollar quoting, Oracle q-quotes, MySQL executable comments).
    Defence in depth: restricted actors' reads now run server-side read-only (`BEGIN TRANSACTION
    READ ONLY` on PostgreSQL, `START TRANSACTION READ ONLY` on MySQL, `SET TRANSACTION READ ONLY` on
    Oracle, SQLite `query_only`, ClickHouse `readonly=2`, rolled back on SQL Server). TwinLite now
    implements read-only transactions too (25006 / MySQL 1792).
  - **Security — agents on real-PostgreSQL mocks** worked as the bootstrap superuser, so `COPY …
    PROGRAM`, `pg_read_file`, untrusted languages or `query_to_xml` reached the computer. Agents are
    now refused those statement classes there (COPY, DO, CALL, LOAD, CREATE FUNCTION/PROCEDURE/
    LANGUAGE/EXTENSION/TRIGGER/RULE, ALTER SYSTEM/ROLE, server-file and query-string functions).
  - **Security — bundle import:** manifest ids were not validated, Windows `\` in entry names could
    escape the workspace, entries were read fully into memory (zip bombs), and a native mock's
    PostgreSQL cluster (whose configuration can run programs) was imported. Ids are validated, entry
    names must be plain parts (and pass `enclosed_name`), writes stream with a 4 GB cap, clusters are
    skipped and imported native mocks start on TwinLite, with notes shown in the UI.
  - **Robustness — denial of service in mock parsers:** a Redis `*2147483647` header made the server
    reserve tens of GB (process abort); the PostgreSQL binary-array decoder and the native-PG proxy
    reserved memory from client-supplied lengths; TDS parameter decoding could panic on oversized
    DECIMAL or short TIME values. All bounded / checked.
  - **Panics on UTF-8 boundaries:** MCP result truncation, the CLI stdio bridge's de-chunking (bytes
    split inside a character) and PostgreSQL identifier truncation of long non-ASCII names.
  - **MCP endpoint:** sessions were never freed (now `DELETE /mcp` ends them and the table is
    capped); truncated tool results no longer repeat the full payload as structured content;
    invalid JSON in dev API calls is an error instead of `{}`; the event stream checks the origin.
  - **Edge cases:** concurrent first use of a connection could open two connections (now one connect
    per id at a time); one Escape closed every stacked dialog and menu (now only the top layer);
    Redis with certificate verification through an SSH tunnel failed with a confusing name mismatch
    (now a clear message); DynamoDB mock connections never showed as closed in Traffic; Firestore
    point-in-time reads silently returned current data (now UNIMPLEMENTED); huge scripts are not kept
    in query history; `CREATE TABLE … AS SELECT` from `generate_series` typed numbers as text (filters
    compared text); division by zero returned NULL instead of failing on PostgreSQL/SQL Server.
  - **Performance:** the grid counted every matching row on every page (a full scan of big tables
    each time); counts now stop at 100,000 and show "100,000+".
  Reviewed without findings: token check (constant time) and origin rules, 127.0.0.1-only binding,
  owner-only secret files and keychain use, Tauri CSP and capabilities, SQL literal/identifier
  quoting, bounded traffic/activity/history logs, API guards before client-kind unwraps. Residual
  risks noted: TwinLite opens SQLite files from bundles without `trusted_schema=OFF` (only TwinBase
  functions are callable), and mocks listening on 0.0.0.0 accept any credentials unless a password
  is set (the dialog says so).

- 2026-10-27 — **Feedback round + Bootcamp** (user request; not pushed):
  - UI: the top bar is the window's title bar (macOS buttons centred natively via
    `src-tauri/src/traffic_lights.rs`, as in Zorvik; own window controls on Windows/Linux with
    `decorations: false`); tabs inside the work panel. A first attempt merged tabs into the title
    row; the user preferred Zorvik's two rows.
    scrollable tab strip (wheel → horizontal, hover scrollbar, edge fade), white text on accent
    buttons (`--accent-strong`), Home tab icon.
  - Docs at every level (`core/src/docs.rs`, `docs.*` API, `components/Docs.tsx`): workspace
    README.md, connection/mock docs fields, object docs with per-column notes in `*/docs/*.toml`;
    Markdown via marked + DOMPurify with Mermaid (lazy, strict, themed).
  - pgvector in TwinLite (`engine/src/vector.rs`): vector/halfvec/sparsevec, operators (unparseable
    `<#> <+> <~> <%>` swapped to geometric tokens before parsing), functions, aggregates, binary wire
    format, catalog entries; MySQL 9 / SQL Server 2025 vector functions; client shows full extension
    types.
  - Redis client: per-database connections (`RedisClient::at`), keyspace/databases API, streams and
    RedisJSON values, server info, console guards, sidebar DB tree, Redis console tab.
  - Bootcamp (`apps/desktop/src/bootcamp/`, `core/src/bootcamp.rs`): own workspace with three lab
    mocks, 10 chapters / 34 lessons / 11 labs, 11 widgets, 6 quiz types, XP/levels/streaks/16 badges
    (gpt-image-1.5 art; gpt-image-2 lacks transparency). Labs verified against a live core.
  - MCP: 21 new tools; agent install (one-liners, Cursor deep link, config-file Install with backup,
    `twinbase agent install`); agents can't escalate access or re-point saved connections.
  - Review: sparse-vector expansion DoS, zip 2→9 (advisories), cargo audit in CI with documented
    exceptions, bundle export missed `seed.json`, Redis seed capture read one SCAN page, sample data
    duplicated composite keys, Chrome's Promise-returning `scrollTo` broke an effect.
  - README screenshots, GitHub Pages site (`site/`, `pages.yml`).

## 7. Later phases

**Phase 2 — more databases and depth**
- MongoDB (client + wire-protocol mock), Google Firestore (client + gRPC mock compatible with
  `FIRESTORE_EMULATOR_HOST`), Amazon DynamoDB (client + HTTP mock).
- SSH tunnels for client connections; OS keychain for secrets.
- Optional *native* PostgreSQL engine for 100% fidelity (D4).
- Schema snapshots and diffs between versions; migration script generation.
- `EXPLAIN` visualizer; fault injection for mocks (latency, dropped connections, forced errors).

**Phase 3 — breadth**
- SQL Server (client, then TDS mock), Cassandra/ScyllaDB, ClickHouse, Elasticsearch/OpenSearch,
  BigQuery and Spanner clients, Oracle client.

**Final phase — TwinBase Proxy** (see §9).

## 8. Non-goals (for now)
- Performance parity with real databases (mocks are for correctness, not speed or load tests).
- Replication, clustering, or high availability in mocks.
- Stored procedures/PLpgSQL execution in the TwinLite engine (planned only via the native engine).
- A hosted/cloud version of TwinBase.

## 9. Future: TwinBase Proxy (design constraints we respect now)
- A **transparent TCP proxy**: the application or TwinBase client connects to the proxy's IP and
  port exactly as it would to the database. The protocol is not changed or wrapped; the client does
  not know it is talking to a proxy. The proxy forwards bytes to the real database inside the
  private network.
- Deployable on any VM or container host in any cloud (GCP, AWS, Azure, on-prem). Platforms that
  only accept HTTP ingress (for example Cloud Run) are not targets.
- Later, protocol-aware features may be layered on for PostgreSQL/MySQL (audit log, read-only
  enforcement, guardrails, masking) without changing what the client sees.
- Must be deployed with network-level access control (firewall/allow-lists, private connectivity
  such as VPN) and TLS — exposing a private database through a proxy needs the owning security
  team's approval.
- Phase 1 impact: protocol code lives in reusable crates; nothing in the client assumes a direct
  connection.

## 10. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Mock fidelity gaps break some ORMs/tools | Fidelity suite against real servers; compatibility matrix in docs; pluggable native engine (D4). |
| SQLite semantics differ (typing, `ALTER TABLE`, case sensitivity) | Strict typing via catalog metadata, table-rebuild strategy for `ALTER`, `case_sensitive_like`, explicit translation tests. |
| WebView differences across OSes | Avoid exotic CSS/JS APIs; test on macOS, Windows, Linux. |
| AI agents modifying real data | AI access levels (D10), read-only connections, activity log. |
| Large result sets exhausting memory | Row caps, paged fetches, streaming, virtualized grid. |
