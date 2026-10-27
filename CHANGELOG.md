# Changelog

Notable changes to TwinBase. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project uses [semantic versioning](https://semver.org/).

## 0.2.0 — 2026-10-27

### Added
- **Bootcamp**: a built-in course on databases and system design — 10 chapters, 34 short lessons
  with interactive illustrations, six kinds of quizzes, and 11 hands-on labs on mock servers, with
  XP, levels, streaks and badges. Runs in its own workspace so projects are never touched.
- **pgvector** in PostgreSQL mocks: `vector`, `halfvec` and `sparsevec` types, all distance
  operators and functions, vector aggregates, HNSW/IVFFlat index DDL, text and binary formats.
  MySQL 9 and SQL Server 2025 vector functions too. The client shows extension types in full
  (`halfvec(1536)`).
- **Documentation everywhere**: Markdown docs for the workspace (README.md), connections, mock
  servers, tables and their columns, collections and indexes — with Write/Preview/Split editing and
  Mermaid diagrams. Docs travel with bundles and are available to AI agents.
- **Redis**: numbered databases (picker and sidebar tree), namespace tree view, streams and RedisJSON
  values, a server-info panel, and a Redis console tab that runs scripts line by line.
- **AI agents**: one-click **Install** into Cursor, VS Code, Gemini CLI, Antigravity, Claude
  Desktop, Windsurf, Kiro, Copilot CLI and opencode; one-line commands for Claude Code, Codex, Gemini
  CLI and VS Code; `twinbase agent install <agent>`; 21 new MCP tools (docs, connections, mock
  settings, seeds, traffic, history, collections, snapshots, bundles, workspace overview).

### Changed
- The top bar is now the window's title bar: the macOS window buttons are centred in it, and Windows
  and Linux get TwinBase's own window controls. Tabs live in the first row of the work panel.
- The tab strip scrolls with the mouse wheel and shows a scrollbar on hover.
- Text on accent-coloured buttons is white.
- Links in docs open in the system browser.

### Fixed
- Bundles now include MongoDB, DynamoDB and Firestore seeds (`seed.json`).
- Capturing a Redis mock's data as its seed reads every key, not only the first SCAN page.

### Security
- Agents can no longer raise a connection's AI access level, or re-point a saved connection's host
  or credentials, through `connection.save`. Docs of connections closed to AI are hidden from agents.
- `zip` upgraded to 9.x (extraction advisories); `cargo audit` runs in CI.
- Very large sparse vectors are computed sparsely instead of being expanded in memory.

## 0.1.0 — 2026-10-25

### Added
- First public version of TwinBase.
- **Database client** for PostgreSQL, MySQL, SQL Server, Oracle, ClickHouse, Cassandra/ScyllaDB,
  Spanner, BigQuery, SQLite, Redis, MongoDB, DynamoDB, Firestore and Elasticsearch/OpenSearch:
  browsing, editing with staged changes, SQL and console editors, query plans, ER diagrams, DDL
  conversion between dialects, schema snapshots, diffs and migration scripts, SSH tunnels, OS
  keychain for passwords.
- **Wire-protocol mock servers** for PostgreSQL, MySQL, SQL Server, Redis, MongoDB, DynamoDB and
  Firestore, backed by the TwinLite engine (SQL on SQLite) or in-memory stores, with seeds, reset,
  live traffic, fault injection and an optional real-PostgreSQL engine.
- **Change preview** on a local twin: see the rows a statement would change before it runs.
- **MCP server** for AI coding agents, with per-connection access levels enforced by statement
  checks and read-only transactions, and an activity log.
- Saved queries with scenarios, sample data generation, `.twinbase` bundles, and the `twinbase`
  command line (headless mocks, `serve`, MCP stdio bridge).
