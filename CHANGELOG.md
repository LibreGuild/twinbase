# Changelog

Notable changes to TwinBase. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project uses [semantic versioning](https://semver.org/).

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
