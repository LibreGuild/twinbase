# AI agents (MCP)

TwinBase is a [Model Context Protocol](https://modelcontextprotocol.io) server. While the app is open
(or `twinbase serve` runs), coding agents can inspect and change your databases and mock servers.
Everything an agent does is recorded in the **Activity** panel with the agent's name, the SQL it ran
and any error.

## Connecting an agent

Open **Settings → AI agents (MCP)**. It shows the endpoint, a copy button for the token, and
ready-to-paste configuration for Claude Code, Claude Desktop, OpenAI Codex CLI, Gemini CLI, Google
Antigravity, Cursor, VS Code (Copilot), Windsurf, Zed, and generic stdio/HTTP clients. The CLI prints
the same: `twinbase mcp-setup`.

Two transports are offered:

- **Streamable HTTP** at `http://127.0.0.1:47800/mcp` with `Authorization: Bearer <token>`.
- **stdio** via `twinbase mcp`, a bridge to the running app. It reads the token from this computer,
  so its configuration contains no secret. The desktop bundle ships the `twinbase` binary.

Example (Claude Code):

```sh
claude mcp add --transport http twinbase http://127.0.0.1:47800/mcp --header "Authorization: Bearer <token>"
```

## Tools

| Tool | Purpose | Writes |
| --- | --- | --- |
| `list_connections` | Saved connections and mock servers with ids and agent access | |
| `create_mock_server`, `start_mock_server`, `stop_mock_server`, `reset_mock_server` | Manage mocks (PostgreSQL mocks take `engine: "native"` for a real PostgreSQL server) | ✓ |
| `get_connection_snippets` | Connection strings/code for an application | |
| `set_mock_faults` | Latency, errors, dropped or refused connections on a mock | ✓ |
| `list_schemas`, `list_tables`, `describe_table`, `get_schema_graph` | Introspection | |
| `run_sql` | Run statements; read-only statements allowed on read-only connections | ✓ |
| `read_rows` | Filtered, ordered pages of rows | |
| `insert_rows`, `update_rows`, `delete_rows` | Row edits in one transaction | ✓ |
| `explain_query` | Query plan tree with hints; `analyze` runs it in a rolled-back transaction | |
| `preview_sql` | Run on a local twin and return the row-level diff | |
| `generate_sample_data` | Realistic rows respecting types, uniqueness and foreign keys | ✓ |
| `snapshot_schema`, `list_schema_snapshots` | Save and list schema snapshots | ✓ (snapshot) |
| `diff_schemas` | Compare snapshots/live schemas; up and down migration SQL | |
| `export_schema_ddl` | `CREATE` statements, optionally converted to another dialect | |
| `redis_command`, `redis_scan`, `redis_get` | Redis | ✓ (command) |
| `mongo_query` | mongosh-style statements on a MongoDB connection (`db.users.find({...})`, `aggregate`, writes, indexes, `show collections`) | ✓ (writes) |
| `mongo_list_collections`, `mongo_collection_schema` | MongoDB databases, collections with counts, sampled field types and indexes | |
| `dynamodb_request` | Any DynamoDB API operation with its JSON request | ✓ (writes) |
| `dynamodb_list_tables` | DynamoDB tables with key schemas, indexes and item counts | |
| `search_request` | Elasticsearch/OpenSearch REST requests in Dev Tools syntax (`GET /index/_search` + JSON body, `_bulk` NDJSON) | ✓ (writes) |
| `search_list_indices` | Indices with health, document counts, size and aliases | |
| `search_index_mapping` | An index's fields as dotted paths with types | |
| `firestore_query` | Firestore statements (`query users where …`, `get`, `count`, `set`, `update`, `add`, `delete`) | ✓ (writes) |
| `firestore_list_collections` | Firestore collections at the root or under a document | |
| `list_saved_queries`, `save_query`, `run_scenarios` | Documented queries and expectation checks | ✓ |
| `get_recent_activity` | What the user and agents did recently | |

Connection ids come from `list_connections`; mock servers use `mock:<id>`.

## Access rules

- **Mock servers:** read and write. On mocks that run real PostgreSQL, statements that could reach
  the computer as the server's superuser (`COPY`, `DO`, creating functions or extensions, server file
  functions, …) are refused for agents.
- **Saved connections:** per-connection *AI agent access* — `No access`, `Read only` (default) or
  `Read and write`. Refused calls return a clear message to the agent and appear under
  *Activity → Errors*.
- **Read only** is enforced twice: statements must read only (judged with each database's own
  quoting and comment rules), and they run in a read-only transaction (PostgreSQL, MySQL, Oracle,
  TwinLite mocks), with SQLite's `query_only`, ClickHouse's `readonly` setting, or in a transaction
  that is rolled back (SQL Server), so a write that slips past the check is refused or undone.
- Agents cannot read or change settings, the token or passwords.

## Security

The endpoint listens on 127.0.0.1 only, requires the token, and rejects browser requests from other
origins. If the port is taken (for example by a second TwinBase), the status bar shows
**MCP unavailable**; choose another port in Settings and restart. **New token** invalidates existing
HTTP configurations (the stdio bridge picks up the new token automatically).
