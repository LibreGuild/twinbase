//! Model Context Protocol server: tools that let coding agents drive TwinBase. Calls go through
//! the same API as the UI, so every action shows up in the app's Activity panel.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::api;
use crate::app::App;
use crate::events::{Actor, Event};
use crate::settings::Settings;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "TwinBase is a database client and mock-server app running on the user's machine. \
Use list_connections first: it returns connection ids (mock servers have ids like 'mock:<id>'). \
get_workspace and get_docs return the team's notes on databases, tables and columns; read them before changing a schema, and keep them current with set_docs. \
Mock servers speak real PostgreSQL/MySQL/Redis/MongoDB/DynamoDB/Firestore protocols, so application code can connect to them. \
For MongoDB connections use mongo_query with mongosh syntax (db.users.find({age: {$gt: 30}})); for DynamoDB use dynamodb_request with the DynamoDB API; for Firestore use firestore_query. \
Prefer mock servers for experiments. Real servers are read-only for agents unless the user allowed writes. \
Use preview_sql to see what a data-changing statement would do before running it. Everything you do is shown to the user live.";

struct ToolDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
}

fn conn() -> Value {
    json!({ "type": "string", "description": "Connection id from list_connections (mock servers use 'mock:<id>')." })
}

fn schema_prop() -> Value {
    json!({ "type": "string", "description": "Schema (PostgreSQL) or database (MySQL). Defaults to public / the connection's database." })
}

fn redis_db() -> Value {
    json!({ "type": "integer", "description": "Numbered database (SELECT n); default: the connection's." })
}

fn id_only(desc: &str) -> Value {
    json!({ "type": "object", "properties": { "id": { "type": "string", "description": desc } }, "required": ["id"] })
}

fn tools() -> Vec<ToolDef> {
    let mut all = core_tools();
    all.extend(more_tools());
    all
}

/// Workspace, docs, connection, seed, bundle and history tools.
fn more_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_workspace",
            title: "Workspace overview",
            description: "The open workspace: name, folder, README, and counts of connections, mock servers, saved queries and snapshots.",
            schema: json!({ "type": "object", "properties": {} }),
            read_only: true,
        },
        ToolDef {
            name: "get_docs",
            title: "Read documentation",
            description: "Markdown docs. No target: the workspace README. target only: docs of a connection or mock server plus the list of documented objects. target + object: docs of a table/collection/index and its columns.",
            schema: json!({ "type": "object", "properties": {
                "target": { "type": "string", "description": "Connection id, mock id or 'mock:<id>'." },
                "object": { "type": "string", "description": "e.g. 'public.users' for SQL tables, 'app.orders' for MongoDB collections." }
            } }),
            read_only: true,
        },
        ToolDef {
            name: "set_docs",
            title: "Write documentation",
            description: "Replace Markdown docs (```mermaid diagrams render in the app). Same addressing as get_docs; for objects, `columns` maps column/field names to short descriptions (empty string removes one).",
            schema: json!({ "type": "object", "properties": {
                "target": { "type": "string" }, "object": { "type": "string" },
                "docs": { "type": "string" },
                "columns": { "type": "object", "additionalProperties": { "type": "string" } }
            }, "required": ["docs"] }),
            read_only: false,
        },
        ToolDef {
            name: "redis_databases",
            title: "Redis databases",
            description: "Numbered databases holding keys (from INFO keyspace) and how many the server has.",
            schema: json!({ "type": "object", "properties": { "connection": conn() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "save_connection",
            title: "Save connection",
            description: "Create or update a saved connection (PostgreSQL, MySQL, SQL Server, SQLite, Redis, MongoDB, …). New connections are read-only for agents; agents cannot change a connection's access level.",
            schema: json!({ "type": "object", "properties": {
                "id": { "type": "string", "description": "Existing id to update; omit to create." },
                "name": { "type": "string" },
                "kind": { "type": "string", "enum": ["postgres", "mysql", "mssql", "sqlite", "clickhouse", "cassandra", "oracle", "redis", "mongodb", "dynamodb", "firestore", "elasticsearch", "spanner", "bigquery"] },
                "host": { "type": "string" }, "port": { "type": "integer" }, "user": { "type": "string" },
                "database": { "type": "string" }, "path": { "type": "string", "description": "SQLite file." },
                "password": { "type": "string", "description": "Stored in the OS keychain, never in workspace files." }
            }, "required": ["name", "kind"] }),
            read_only: false,
        },
        ToolDef {
            name: "test_connection",
            title: "Test connection",
            description: "Try to connect with a saved connection and report the server version.",
            schema: id_only("Connection id."),
            read_only: true,
        },
        ToolDef {
            name: "delete_connection",
            title: "Delete connection",
            description: "Remove a saved connection and its stored password (the database itself is untouched). Only connections that allow agent writes.",
            schema: id_only("Connection id."),
            read_only: false,
        },
        ToolDef {
            name: "update_mock_server",
            title: "Update mock server",
            description: "Change a mock server's name, port, database, persistence, auto-start or engine. A running mock restarts.",
            schema: json!({ "type": "object", "properties": {
                "id": { "type": "string" }, "name": { "type": "string" }, "port": { "type": "integer" }, "database": { "type": "string" },
                "persist": { "type": "boolean" }, "auto_start": { "type": "boolean" },
                "engine": { "type": "string", "enum": ["twinlite", "native"] }
            }, "required": ["id"] }),
            read_only: false,
        },
        ToolDef {
            name: "delete_mock_server",
            title: "Delete mock server",
            description: "Stop a mock server and delete its definition, data and seed.",
            schema: id_only("Mock id (without 'mock:')."),
            read_only: false,
        },
        ToolDef {
            name: "get_mock_seed",
            title: "Read mock seed",
            description: "The seed a mock loads on reset: SQL, Redis commands, or JSON for MongoDB/DynamoDB/Firestore.",
            schema: id_only("Mock id."),
            read_only: true,
        },
        ToolDef {
            name: "set_mock_seed",
            title: "Write mock seed",
            description: "Replace a mock's seed (checked first). Reset the mock to load it.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" }, "seed": { "type": "string" } }, "required": ["id", "seed"] }),
            read_only: false,
        },
        ToolDef {
            name: "capture_mock_seed",
            title: "Capture data as seed",
            description: "Turn a running mock's current data into its seed, so resets come back to this state.",
            schema: id_only("Mock id."),
            read_only: false,
        },
        ToolDef {
            name: "get_mock_traffic",
            title: "Mock traffic",
            description: "Recent statements and commands a mock server received from applications, with timings and errors.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" }, "limit": { "type": "integer", "description": "Default 100." } }, "required": ["id"] }),
            read_only: true,
        },
        ToolDef {
            name: "get_query_history",
            title: "Query history",
            description: "Statements recently run on a connection (by the user and agents), newest first.",
            schema: json!({ "type": "object", "properties": { "connection": conn() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "delete_saved_query",
            title: "Delete saved query",
            description: "Delete one saved query.",
            schema: json!({ "type": "object", "properties": { "collection": { "type": "string" }, "id": { "type": "string" } }, "required": ["collection", "id"] }),
            read_only: false,
        },
        ToolDef {
            name: "save_collection",
            title: "Save query collection",
            description: "Create or rename a query collection, with Markdown docs.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" }, "name": { "type": "string" }, "docs": { "type": "string" } }, "required": ["name"] }),
            read_only: false,
        },
        ToolDef {
            name: "delete_collection",
            title: "Delete query collection",
            description: "Delete a collection and every saved query in it.",
            schema: id_only("Collection id."),
            read_only: false,
        },
        ToolDef {
            name: "get_schema_snapshot",
            title: "Read schema snapshot",
            description: "Tables, columns, keys and indexes captured in a snapshot.",
            schema: id_only("Snapshot id from list_schema_snapshots."),
            read_only: true,
        },
        ToolDef {
            name: "delete_schema_snapshot",
            title: "Delete schema snapshot",
            description: "Delete a schema snapshot.",
            schema: id_only("Snapshot id."),
            read_only: false,
        },
        ToolDef {
            name: "export_bundle",
            title: "Export bundle",
            description: "Write a .twinbase bundle (mocks with data, query collections, connections without passwords, snapshots) into the workspace's exports/ folder. Returns the file path.",
            schema: json!({ "type": "object", "properties": {
                "name": { "type": "string", "description": "File name without extension." },
                "mocks": { "type": "array", "items": { "type": "string" } },
                "collections": { "type": "array", "items": { "type": "string" } },
                "connections": { "type": "array", "items": { "type": "string" } },
                "snapshots": { "type": "array", "items": { "type": "string" } },
                "include_data": { "type": "boolean", "description": "Include mock data (default true)." }
            }, "required": ["name"] }),
            read_only: false,
        },
        ToolDef {
            name: "import_bundle",
            title: "Import bundle",
            description: "Import a .twinbase bundle file into the workspace (ids are renamed when they clash). Use inspect first to see what it contains.",
            schema: json!({ "type": "object", "properties": { "path": { "type": "string" }, "inspect": { "type": "boolean", "description": "Only list the contents." } }, "required": ["path"] }),
            read_only: false,
        },
    ]
}

fn core_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_connections",
            title: "List connections",
            description: "List saved connections and mock servers with their ids, kinds, status and whether agents may write.",
            schema: json!({ "type": "object", "properties": {} }),
            read_only: true,
        },
        ToolDef {
            name: "create_mock_server",
            title: "Create mock server",
            description: "Create and start a local mock database server (PostgreSQL, MySQL, Redis, MongoDB, DynamoDB or Firestore) that applications can connect to with normal drivers and SDKs (Firestore SDKs via FIRESTORE_EMULATOR_HOST).",
            schema: json!({ "type": "object", "properties": {
                "name": { "type": "string" },
                "kind": { "type": "string", "enum": ["postgres", "mysql", "mssql", "redis", "mongodb", "dynamodb", "firestore"] },
                "port": { "type": "integer", "description": "Optional; a free port is chosen when omitted." },
                "database": { "type": "string", "description": "Database name for PostgreSQL/MySQL/MongoDB (default 'app')." },
                "persist": { "type": "boolean", "description": "Keep data in the workspace (default true)." },
                "engine": { "type": "string", "enum": ["twinlite", "native"], "description": "PostgreSQL only: native runs a real PostgreSQL server (needs PostgreSQL installed); default twinlite." }
            }, "required": ["name", "kind"] }),
            read_only: false,
        },
        ToolDef {
            name: "start_mock_server",
            title: "Start mock server",
            description: "Start a mock server by id.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }),
            read_only: false,
        },
        ToolDef {
            name: "stop_mock_server",
            title: "Stop mock server",
            description: "Stop a running mock server.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }),
            read_only: false,
        },
        ToolDef {
            name: "reset_mock_server",
            title: "Reset mock server",
            description: "Delete a mock server's data and reload it from its seed script.",
            schema: json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }),
            read_only: false,
        },
        ToolDef {
            name: "set_mock_faults",
            title: "Inject faults into a mock",
            description: "Make a mock server slow or unreliable to test an application's error handling: add latency, fail matching statements with a chosen error code (SQLSTATE, MySQL number, Redis prefix, MongoDB error code DynamoDB exception name such as ProvisionedThroughputExceededException, or gRPC status such as UNAVAILABLE / ABORTED for Firestore), drop connections, or refuse new connections. Applies immediately. Send enabled=false to turn everything off.",
            schema: json!({ "type": "object", "properties": {
                "id": { "type": "string", "description": "Mock id (without the 'mock:' prefix)." },
                "enabled": { "type": "boolean" },
                "refuse_connections": { "type": "boolean", "description": "Accept and immediately close new connections (database looks down)." },
                "rules": { "type": "array", "description": "Evaluated in order for every statement or command.", "items": { "type": "object", "properties": {
                    "matches": { "type": "string", "description": "Case-insensitive substring, or /regex/; empty matches everything." },
                    "probability": { "type": "number", "minimum": 0, "maximum": 1, "description": "Default 1." },
                    "action": { "type": "object", "description": "{\"type\":\"delay\",\"ms\":500,\"jitter_ms\":100} | {\"type\":\"error\",\"code\":\"40001\",\"message\":\"...\"} | {\"type\":\"drop\"}" }
                }, "required": ["action"] } }
            }, "required": ["id", "enabled"] }),
            read_only: false,
        },
        ToolDef {
            name: "get_connection_snippets",
            title: "Connection snippets",
            description: "Connection strings and code for many languages to connect an application to a mock server.",
            schema: json!({ "type": "object", "properties": { "mock_id": { "type": "string" } }, "required": ["mock_id"] }),
            read_only: true,
        },
        ToolDef {
            name: "list_schemas",
            title: "List schemas",
            description: "List schemas (PostgreSQL) or databases (MySQL) on a connection.",
            schema: json!({ "type": "object", "properties": { "connection": conn() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "list_tables",
            title: "List tables",
            description: "List tables and views.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "describe_table",
            title: "Describe table",
            description: "Columns, types, keys, foreign keys and indexes of a table.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "table": { "type": "string" } }, "required": ["connection", "table"] }),
            read_only: true,
        },
        ToolDef {
            name: "get_schema_graph",
            title: "Schema graph",
            description: "All tables of a schema with columns and relationships (the ER diagram data).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "run_sql",
            title: "Run SQL",
            description: "Run one or more SQL statements (DDL or DML). Results include rows for SELECT/RETURNING. Writes are refused on read-only connections.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "sql": { "type": "string" }, "max_rows": { "type": "integer", "description": "Default 200." } }, "required": ["connection", "sql"] }),
            read_only: false,
        },
        ToolDef {
            name: "read_rows",
            title: "Read rows",
            description: "Read rows from a table with optional filters and ordering.",
            schema: json!({ "type": "object", "properties": {
                "connection": conn(), "schema": schema_prop(), "table": { "type": "string" },
                "limit": { "type": "integer" }, "offset": { "type": "integer" },
                "filters": { "type": "array", "items": { "type": "object", "properties": {
                    "column": { "type": "string" },
                    "op": { "type": "string", "enum": ["eq", "ne", "lt", "le", "gt", "ge", "contains", "starts_with", "ends_with", "is_null", "is_not_null"] },
                    "value": {} }, "required": ["column", "op"] } },
                "order_by": { "type": "array", "items": { "type": "object", "properties": { "column": { "type": "string" }, "descending": { "type": "boolean" } }, "required": ["column"] } }
            }, "required": ["connection", "table"] }),
            read_only: true,
        },
        ToolDef {
            name: "insert_rows",
            title: "Insert rows",
            description: "Insert rows given as objects of column → value, in one transaction.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "table": { "type": "string" }, "rows": { "type": "array", "items": { "type": "object" } } }, "required": ["connection", "table", "rows"] }),
            read_only: false,
        },
        ToolDef {
            name: "update_rows",
            title: "Update rows",
            description: "Update rows identified by key objects (primary key column → value).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "table": { "type": "string" }, "updates": { "type": "array", "items": { "type": "object", "properties": { "key": { "type": "object" }, "values": { "type": "object" } }, "required": ["key", "values"] } } }, "required": ["connection", "table", "updates"] }),
            read_only: false,
        },
        ToolDef {
            name: "delete_rows",
            title: "Delete rows",
            description: "Delete rows identified by key objects (primary key column → value).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "table": { "type": "string" }, "keys": { "type": "array", "items": { "type": "object" } } }, "required": ["connection", "table", "keys"] }),
            read_only: false,
        },
        ToolDef {
            name: "explain_query",
            title: "Explain a query",
            description: "Show the query plan of one statement as a tree (with tuning hints). With analyze=true the statement really runs inside a transaction that is rolled back, giving actual row counts and timings.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "sql": { "type": "string" }, "analyze": { "type": "boolean" } }, "required": ["connection", "sql"] }),
            read_only: true,
        },
        ToolDef {
            name: "preview_sql",
            title: "Preview SQL",
            description: "Run INSERT/UPDATE/DELETE/DDL on a temporary local copy of the affected data and return a row-level diff. Nothing changes on the server.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "sql": { "type": "string" } }, "required": ["connection", "sql"] }),
            read_only: true,
        },
        ToolDef {
            name: "generate_sample_data",
            title: "Generate sample data",
            description: "Insert realistic sample rows into a table (respects types, uniqueness and foreign keys; fill parent tables first).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "table": { "type": "string" }, "count": { "type": "integer", "description": "1-10000, default 25" } }, "required": ["connection", "table"] }),
            read_only: false,
        },
        ToolDef {
            name: "snapshot_schema",
            title: "Snapshot a schema",
            description: "Save the current structure (tables, columns, keys, indexes) of a schema as a named snapshot in the workspace, to compare against later.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "label": { "type": "string" } }, "required": ["connection"] }),
            read_only: false,
        },
        ToolDef {
            name: "list_schema_snapshots",
            title: "List schema snapshots",
            description: "Saved schema snapshots, newest first.",
            schema: json!({ "type": "object", "properties": {} }),
            read_only: true,
        },
        ToolDef {
            name: "diff_schemas",
            title: "Diff schemas and generate a migration",
            description: "Compare two schemas — each a saved snapshot {\"snapshot\": id} or a live connection {\"connection\": id, \"schema\": optional} — and return the changes plus up/down migration SQL that turns `from` into `to`.",
            schema: json!({ "type": "object", "properties": {
                "from": { "type": "object", "description": "{\"snapshot\": \"<id>\"} or {\"connection\": \"<id>\", \"schema\": \"public\"}" },
                "to": { "type": "object" },
                "dialect": { "type": "string", "enum": ["postgres", "mysql", "sqlite", "mssql", "clickhouse", "cassandra", "spanner", "bigquery", "oracle"], "description": "Script dialect (default: the `from` side)." }
            }, "required": ["from", "to"] }),
            read_only: true,
        },
        ToolDef {
            name: "export_schema_ddl",
            title: "Export schema DDL",
            description: "CREATE TABLE statements for a schema, optionally converted to another dialect.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "schema": schema_prop(), "dialect": { "type": "string", "enum": ["postgres", "mysql", "sqlite", "mssql", "clickhouse", "cassandra", "spanner", "bigquery", "oracle"] } }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "redis_command",
            title: "Redis command",
            description: "Run a Redis command line, e.g. HSET user:1 name \"Ann\". SELECT, MULTI/EXEC and SUBSCRIBE are not available; pass db to use another numbered database.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "command": { "type": "string" }, "db": redis_db() }, "required": ["connection", "command"] }),
            read_only: false,
        },
        ToolDef {
            name: "redis_scan",
            title: "Scan Redis keys",
            description: "List keys with type and TTL.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "pattern": { "type": "string" }, "type": { "type": "string" }, "db": redis_db() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "redis_get",
            title: "Get Redis value",
            description: "Read a key's value with its type (strings, hashes, lists, sets, sorted sets, streams, RedisJSON).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "key": { "type": "string" }, "db": redis_db() }, "required": ["connection", "key"] }),
            read_only: true,
        },
        ToolDef {
            name: "mongo_query",
            title: "MongoDB statements",
            description: "Run mongosh-style statements on a MongoDB connection, one per line: db.users.find({age: {$gt: 30}}).sort({age: -1}).limit(10), db.orders.aggregate([...]), db.users.insertOne({...}), db.users.updateMany(filter, {$set: {...}}), db.users.createIndex({email: 1}, {unique: true}), show collections, use otherdb. Values may use ObjectId(\"...\"), ISODate(\"...\") and /regex/. Documents come back as relaxed Extended JSON.",
            schema: json!({ "type": "object", "properties": {
                "connection": conn(),
                "statements": { "type": "string" },
                "database": { "type": "string", "description": "Database to start in (default: the connection's)." },
                "max_documents": { "type": "integer", "description": "Per statement (default 100)." }
            }, "required": ["connection", "statements"] }),
            read_only: false,
        },
        ToolDef {
            name: "mongo_list_collections",
            title: "List MongoDB collections",
            description: "Databases on a MongoDB connection and the collections (with document counts) of one database.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "database": { "type": "string" } }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "mongo_collection_schema",
            title: "MongoDB collection schema",
            description: "Field paths with their BSON types, sampled from a collection's documents, plus its indexes.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "database": { "type": "string" }, "collection": { "type": "string" }, "sample": { "type": "integer", "description": "Documents to sample (default 200)." } }, "required": ["connection", "collection"] }),
            read_only: true,
        },
        ToolDef {
            name: "dynamodb_request",
            title: "DynamoDB request",
            description: "Call a DynamoDB API operation with its JSON request (DynamoDB JSON for values), e.g. operation=Query, request={\"TableName\":\"orders\",\"KeyConditionExpression\":\"pk = :p\",\"ExpressionAttributeValues\":{\":p\":{\"S\":\"c#1\"}}}. Supports CreateTable, DescribeTable, PutItem, GetItem, UpdateItem, DeleteItem, Query, Scan, BatchGetItem, BatchWriteItem, TransactWriteItems, TransactGetItems, UpdateTimeToLive and more.",
            schema: json!({ "type": "object", "properties": {
                "connection": conn(),
                "operation": { "type": "string" },
                "request": { "type": "object" }
            }, "required": ["connection", "operation", "request"] }),
            read_only: false,
        },
        ToolDef {
            name: "dynamodb_list_tables",
            title: "List DynamoDB tables",
            description: "Tables on a DynamoDB connection with key schemas, indexes and item counts.",
            schema: json!({ "type": "object", "properties": { "connection": conn() }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "firestore_query",
            title: "Firestore statements",
            description: "Run Firestore statements, one per line: get users/alice; list users limit 20; query users where age >= 30 and city == \"Lisbon\" order by age desc limit 10; query group orders where status in [\"open\"]; count users where active == true; collections [users/alice]; set users/alice {\"name\": \"Alice\"}; update users/alice {\"age\": 32, \"address.city\": \"Porto\"}; add users {\"name\": \"Bo\"}; delete users/alice. Values are JSON; special values: {\"__timestamp\": \"2024-01-01T00:00:00Z\"}, {\"__ref\": \"users/bob\"}, {\"__geo\": [lat, lng]}, {\"__bytes\": \"base64\"}.",
            schema: json!({ "type": "object", "properties": {
                "connection": conn(),
                "statements": { "type": "string" },
                "max_documents": { "type": "integer", "description": "Per statement (default 100)." }
            }, "required": ["connection", "statements"] }),
            read_only: false,
        },
        ToolDef {
            name: "search_request",
            title: "Elasticsearch / OpenSearch requests",
            description: "Run REST requests on an Elasticsearch or OpenSearch connection in Dev Tools syntax: a line `METHOD /path` followed by an optional JSON body (NDJSON lines for _bulk and _msearch); several requests separated by blank lines. Example: GET /products/_search\n{\"query\": {\"match\": {\"name\": \"lamp\"}}, \"size\": 5}. GET requests and POST to _search, _count, _msearch, _mget, _sql, _query are reads; anything else writes.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "requests": { "type": "string" } }, "required": ["connection", "requests"] }),
            read_only: false,
        },
        ToolDef {
            name: "search_list_indices",
            title: "List Elasticsearch indices",
            description: "Indices with health, document counts, size and aliases; the server's product and version.",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "include_hidden": { "type": "boolean" } }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "search_index_mapping",
            title: "Elasticsearch index mapping",
            description: "The fields of an index (dotted paths with their types, including multi-fields such as name.keyword).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "index": { "type": "string" } }, "required": ["connection", "index"] }),
            read_only: true,
        },
        ToolDef {
            name: "firestore_list_collections",
            title: "List Firestore collections",
            description: "Collection ids at the database root, or under a document path (e.g. users/alice).",
            schema: json!({ "type": "object", "properties": { "connection": conn(), "parent": { "type": "string" } }, "required": ["connection"] }),
            read_only: true,
        },
        ToolDef {
            name: "list_saved_queries",
            title: "List saved queries",
            description: "Saved queries with their documentation, optionally for one collection.",
            schema: json!({ "type": "object", "properties": { "collection": { "type": "string" } } }),
            read_only: true,
        },
        ToolDef {
            name: "save_query",
            title: "Save query",
            description: "Save a SQL query with Markdown documentation into a collection (created when missing). Optional expectation for scenario runs.",
            schema: json!({ "type": "object", "properties": {
                "name": { "type": "string" }, "collection": { "type": "string" }, "sql": { "type": "string" }, "docs": { "type": "string" },
                "connection": { "type": "string" },
                "expect": { "type": "object", "description": "{\"kind\":\"rows\",\"count\":3} | {\"kind\":\"empty\"} | {\"kind\":\"succeeds\"} | {\"kind\":\"fails\",\"code\":\"23505\"}" }
            }, "required": ["name", "sql"] }),
            read_only: false,
        },
        ToolDef {
            name: "run_scenarios",
            title: "Run scenarios",
            description: "Run every saved query in a collection and report which expectations pass.",
            schema: json!({ "type": "object", "properties": { "collection": { "type": "string" }, "connection": { "type": "string" } }, "required": ["collection"] }),
            read_only: false,
        },
        ToolDef {
            name: "get_recent_activity",
            title: "Recent activity",
            description: "What the user and agents did recently in TwinBase.",
            schema: json!({ "type": "object", "properties": { "limit": { "type": "integer" } } }),
            read_only: true,
        },
    ]
}

/// Translate a tool call into API calls.
async fn call_tool(app: &Arc<App>, name: &str, args: Value, actor: &Actor) -> crate::error::Result<Value> {
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    // Boxed: the API's dispatch future is large, and inlining it into every arm here would make
    // this future overflow a runtime worker's stack.
    let call = |method: &'static str, params: Value| Box::pin(api::call(app, method, params, actor));
    match name {
        "list_connections" => {
            let data = app.data();
            let mocks = app.mock_statuses().await;
            Ok(json!({
                "mock_servers": mocks.iter().map(|m| json!({
                    "id": m.def.id, "connection_id": m.connection_id, "name": m.def.name, "kind": m.def.kind, "port": m.def.port,
                    "running": m.running, "database": m.def.database, "agent_access": "read-write"
                })).collect::<Vec<_>>(),
                "connections": data.connections.iter().map(|c| json!({
                    "id": c.id, "name": c.name, "kind": c.kind, "host": c.host, "port": c.port, "database": c.database,
                    "read_only": c.read_only, "agent_access": c.ai_access
                })).collect::<Vec<_>>(),
            }))
        }
        "create_mock_server" => {
            let kind = a("kind");
            let mock = json!({
                "id": "", "name": a("name"), "kind": kind, "port": a("port").as_u64().unwrap_or(0),
                "database": a("database").as_str().unwrap_or("app"), "persist": a("persist").as_bool().unwrap_or(true), "auto_start": false,
                "engine": a("engine").as_str().unwrap_or("twinlite")
            });
            let saved = call("mock.save", json!({ "mock": mock })).await?;
            let id = saved["id"].as_str().unwrap_or_default().to_owned();
            call("mock.start", json!({ "id": id })).await?;
            Ok(json!({ "mock": saved, "connection_id": format!("mock:{id}"), "running": true }))
        }
        "start_mock_server" => call("mock.start", json!({ "id": a("id") })).await,
        "stop_mock_server" => call("mock.stop", json!({ "id": a("id") })).await,
        "reset_mock_server" => call("mock.reset", json!({ "id": a("id") })).await,
        "get_connection_snippets" => call("mock.snippets", json!({ "id": a("mock_id") })).await,
        "set_mock_faults" => {
            let id = a("id").as_str().unwrap_or_default().trim_start_matches("mock:").to_owned();
            let faults = json!({
                "enabled": a("enabled").as_bool().unwrap_or(false),
                "refuse_connections": a("refuse_connections").as_bool().unwrap_or(false),
                "rules": if a("rules").is_null() { json!([]) } else { a("rules") },
            });
            call("mock.faults.set", json!({ "id": id, "faults": faults })).await
        }
        "list_schemas" => call("db.schemas", json!({ "connection": a("connection") })).await,
        "list_tables" => call("db.tables", json!({ "connection": a("connection"), "schema": a("schema") })).await,
        "describe_table" => call("db.describe", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table") })).await,
        "get_schema_graph" => call("db.graph", json!({ "connection": a("connection"), "schema": a("schema") })).await,
        "run_sql" => call("db.query", json!({ "connection": a("connection"), "sql": a("sql"), "max_rows": a("max_rows").as_u64().unwrap_or(200) })).await,
        "read_rows" => {
            let query = json!({
                "limit": a("limit").as_u64().unwrap_or(100), "offset": a("offset").as_u64().unwrap_or(0),
                "filters": if a("filters").is_null() { json!([]) } else { a("filters") },
                "sort": if a("order_by").is_null() { json!([]) } else { a("order_by") },
            });
            call("db.rows", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table"), "query": query })).await
        }
        "insert_rows" => call("db.apply", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table"), "changes": { "inserts": a("rows") } })).await,
        "update_rows" => call("db.apply", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table"), "changes": { "updates": a("updates") } })).await,
        "delete_rows" => call("db.apply", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table"), "changes": { "deletes": a("keys") } })).await,
        "explain_query" => {
            let mut plan = call("db.explain", json!({ "connection": a("connection"), "sql": a("sql"), "analyze": a("analyze").as_bool().unwrap_or(false) })).await?;
            // The tree already carries everything; drop the raw text to keep answers small.
            if let Some(o) = plan.as_object_mut() {
                o.remove("raw");
            }
            Ok(plan)
        }
        "preview_sql" => call("db.preview", json!({ "connection": a("connection"), "schema": a("schema"), "sql": a("sql") })).await,
        "generate_sample_data" => call("db.generate", json!({ "connection": a("connection"), "schema": a("schema"), "table": a("table"), "count": a("count").as_u64().unwrap_or(25) })).await,
        "snapshot_schema" => call("schema.snapshot.create", json!({ "connection": a("connection"), "schema": a("schema"), "label": a("label") })).await,
        "list_schema_snapshots" => call("schema.snapshot.list", json!({})).await,
        "diff_schemas" => {
            let d = call("schema.diff", json!({ "from": a("from"), "to": a("to"), "dialect": a("dialect") })).await?;
            // The arrays duplicate the joined scripts; keep the answer compact for agents.
            Ok(json!({
                "identical": d["identical"], "from": d["from_label"], "to": d["to_label"], "dialect": d["dialect"],
                "changes": d["changes"], "warnings": d["warnings"], "up_sql": d["up_sql"], "down_sql": d["down_sql"],
            }))
        }
        "export_schema_ddl" => call("db.ddl", json!({ "connection": a("connection"), "schema": a("schema"), "dialect": a("dialect") })).await,
        "redis_command" => call("redis.command", json!({ "connection": a("connection"), "command": a("command"), "db": a("db") })).await,
        "redis_scan" => {
            call("redis.scan", json!({ "connection": a("connection"), "pattern": a("pattern").as_str().unwrap_or("*"), "type": a("type").as_str().unwrap_or(""), "count": 1000, "db": a("db") })).await
        }
        "redis_get" => call("redis.get", json!({ "connection": a("connection"), "key": a("key"), "db": a("db") })).await,
        "redis_databases" => call("redis.databases", json!({ "connection": a("connection") })).await,
        "get_workspace" => {
            let data = app.data();
            let readme = call("docs.get", json!({})).await?;
            Ok(json!({
                "name": data.name, "path": data.path, "readme": readme["docs"],
                "connections": data.connections.len(), "mock_servers": data.mocks.len(),
                "collections": data.collections.len(), "saved_queries": data.queries.len(),
                "snapshots": app.workspace().list_snapshots().len(),
            }))
        }
        "get_docs" => {
            let mut p = json!({});
            for k in ["target", "object"] {
                if let Some(v) = args.get(k).filter(|v| v.is_string()) {
                    p[k] = v.clone();
                }
            }
            if p.get("target").is_some() && p.get("object").is_none() { call("docs.list", p).await } else { call("docs.get", p).await }
        }
        "set_docs" => {
            let mut p = json!({ "docs": a("docs").as_str().unwrap_or("") });
            for k in ["target", "object", "columns"] {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    p[k] = v.clone();
                }
            }
            call("docs.set", p).await
        }
        "save_connection" => {
            let data = app.data();
            let existing = a("id").as_str().and_then(|id| data.connections.iter().find(|c| c.id == id)).cloned();
            let mut c = match &existing {
                Some(c) => serde_json::to_value(c)?,
                None => json!({ "id": "", "read_only": false, "ai_access": "read-only" }),
            };
            for k in ["name", "kind", "host", "port", "user", "database", "path"] {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    c[k] = v.clone();
                }
            }
            let mut p = json!({ "connection": c });
            if let Some(pw) = a("password").as_str() {
                p["password"] = json!(pw);
            }
            call("connection.save", p).await
        }
        "test_connection" => call("connection.connect", json!({ "id": a("id") })).await,
        "delete_connection" => {
            let def = app.connection_def(a("id").as_str().unwrap_or_default())?;
            app.check_access(&def, actor, true)?;
            call("connection.delete", json!({ "id": def.id })).await
        }
        "update_mock_server" => {
            let id = a("id").as_str().unwrap_or_default().trim_start_matches("mock:").to_owned();
            let mut m = serde_json::to_value(app.mock_def(&id)?)?;
            for k in ["name", "port", "database", "persist", "auto_start", "engine"] {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    m[k] = v.clone();
                }
            }
            call("mock.save", json!({ "mock": m })).await
        }
        "delete_mock_server" => call("mock.delete", json!({ "id": a("id").as_str().unwrap_or_default().trim_start_matches("mock:") })).await,
        "get_mock_seed" => call("mock.seed.get", json!({ "id": a("id").as_str().unwrap_or_default().trim_start_matches("mock:") })).await,
        "set_mock_seed" => call("mock.seed.set", json!({ "id": a("id").as_str().unwrap_or_default().trim_start_matches("mock:"), "sql": a("seed") })).await,
        "capture_mock_seed" => {
            let id = a("id").as_str().unwrap_or_default().trim_start_matches("mock:").to_owned();
            let captured = call("mock.seed.capture", json!({ "id": id })).await?;
            call("mock.seed.set", json!({ "id": id, "sql": captured["sql"] })).await?;
            Ok(json!({ "id": id, "seed_bytes": captured["sql"].as_str().map(str::len).unwrap_or(0) }))
        }
        "get_mock_traffic" => {
            call("mock.traffic", json!({ "id": a("id").as_str().unwrap_or_default().trim_start_matches("mock:"), "after": 0, "limit": a("limit").as_u64().unwrap_or(100) })).await
        }
        "get_query_history" => call("db.history", json!({ "connection": a("connection") })).await,
        "delete_saved_query" => call("query.delete", json!({ "collection": a("collection"), "id": a("id") })).await,
        "save_collection" => {
            call("collection.save", json!({ "collection": { "id": a("id").as_str().unwrap_or(""), "name": a("name"), "docs": a("docs").as_str().unwrap_or("") } })).await
        }
        "delete_collection" => call("collection.delete", json!({ "id": a("id") })).await,
        "get_schema_snapshot" => call("schema.snapshot.get", json!({ "id": a("id") })).await,
        "delete_schema_snapshot" => call("schema.snapshot.delete", json!({ "id": a("id") })).await,
        "export_bundle" => {
            // Agents write bundles only inside the workspace, never to arbitrary paths.
            let name = crate::model::slugify(a("name").as_str().unwrap_or("bundle"));
            let dir = app.workspace().root.join("exports");
            std::fs::create_dir_all(&dir).map_err(|e| crate::error::CoreError::io(&dir, e))?;
            let path = dir.join(format!("{name}.twinbase"));
            let list = |k: &str| a(k).as_array().cloned().unwrap_or_default();
            let selection = json!({
                "mocks": list("mocks"), "collections": list("collections"), "connections": list("connections"), "snapshots": list("snapshots"),
                "include_data": a("include_data").as_bool().unwrap_or(true),
            });
            let manifest = call("bundle.export", json!({ "selection": selection, "path": path.to_string_lossy() })).await?;
            Ok(json!({ "path": path.to_string_lossy(), "manifest": manifest }))
        }
        "import_bundle" => {
            if a("inspect").as_bool().unwrap_or(false) {
                call("bundle.inspect", json!({ "path": a("path") })).await
            } else {
                call("bundle.import", json!({ "path": a("path") })).await
            }
        }
        "mongo_query" => {
            call("mongo.run", json!({ "connection": a("connection"), "database": a("database"), "command": a("statements"), "max_documents": a("max_documents").as_u64().unwrap_or(100) })).await
        }
        "mongo_list_collections" => {
            let dbs = call("mongo.databases", json!({ "connection": a("connection") })).await?;
            let db = a("database").as_str().map(str::to_owned).or_else(|| dbs["default"].as_str().map(str::to_owned)).unwrap_or_default();
            let collections = call("mongo.collections", json!({ "connection": a("connection"), "database": db })).await?;
            Ok(json!({ "databases": dbs["databases"], "database": db, "collections": collections }))
        }
        "mongo_collection_schema" => {
            call("mongo.schema", json!({ "connection": a("connection"), "database": a("database"), "collection": a("collection"), "sample": a("sample").as_u64().unwrap_or(200) })).await
        }
        "firestore_query" => {
            call("firestore.run", json!({ "connection": a("connection"), "command": a("statements"), "max_documents": a("max_documents").as_u64().unwrap_or(100) })).await
        }
        "search_request" => call("search.run", json!({ "connection": a("connection"), "command": a("requests") })).await,
        "search_list_indices" => call("search.indices", json!({ "connection": a("connection"), "include_hidden": a("include_hidden").as_bool().unwrap_or(false) })).await,
        "search_index_mapping" => call("search.mapping", json!({ "connection": a("connection"), "index": a("index") })).await,
        "firestore_list_collections" => call("firestore.collections", json!({ "connection": a("connection"), "parent": a("parent").as_str().unwrap_or("") })).await,
        "dynamodb_request" => {
            let op = a("operation").as_str().unwrap_or_default().to_owned();
            let script = format!("{op} {}", if a("request").is_null() { json!({}) } else { a("request") });
            let out = call("dynamo.run", json!({ "connection": a("connection"), "command": script })).await?;
            let first = out["outputs"][0].clone();
            match first.get("error") {
                Some(e) => Err(crate::error::CoreError::invalid(e["message"].as_str().unwrap_or("request failed").to_owned())),
                None => Ok(first["response"].clone()),
            }
        }
        "dynamodb_list_tables" => {
            let t = call("dynamo.tables", json!({ "connection": a("connection") })).await?;
            let mut tables = Vec::new();
            for name in t["tables"].as_array().cloned().unwrap_or_default().into_iter().take(50) {
                let d = call("dynamo.describe", json!({ "connection": a("connection"), "table": name })).await?;
                tables.push(json!({
                    "name": name, "key_schema": d["KeySchema"], "attributes": d["AttributeDefinitions"], "item_count": d["ItemCount"],
                    "global_indexes": d["GlobalSecondaryIndexes"].as_array().map(|a| a.iter().map(|i| json!({ "name": i["IndexName"], "key_schema": i["KeySchema"] })).collect::<Vec<_>>()),
                    "local_indexes": d["LocalSecondaryIndexes"].as_array().map(|a| a.iter().map(|i| json!({ "name": i["IndexName"], "key_schema": i["KeySchema"] })).collect::<Vec<_>>()),
                }));
            }
            Ok(json!({ "endpoint": t["endpoint"], "tables": tables }))
        }
        "list_saved_queries" => {
            let data = app.data();
            let col = args.get("collection").and_then(|v| v.as_str());
            Ok(json!({
                "collections": data.collections,
                "queries": data.queries.iter().filter(|q| col.is_none_or(|c| q.collection == c)).collect::<Vec<_>>(),
            }))
        }
        "save_query" => {
            let query = json!({
                "id": "", "name": a("name"), "collection": a("collection").as_str().unwrap_or("agent-queries"), "sql": a("sql"),
                "docs": a("docs").as_str().unwrap_or(""), "connection": a("connection"), "tags": [], "expect": a("expect")
            });
            call("query.save", json!({ "query": query })).await
        }
        "run_scenarios" => call("scenario.run", json!({ "collection": a("collection"), "connection": a("connection") })).await,
        "get_recent_activity" => call("activity.list", json!({ "limit": a("limit").as_u64().unwrap_or(50) })).await,
        other => Err(crate::error::CoreError::not_found(format!("unknown tool '{other}'"))),
    }
}

/// MCP sessions (Streamable HTTP `Mcp-Session-Id`) and the client names behind them.
#[derive(Default)]
pub struct Sessions {
    names: Mutex<HashMap<String, String>>,
}

/// Tool results larger than this are cut (agents' context windows are the limit anyway).
const MAX_TOOL_TEXT: usize = 200_000;
/// Sessions kept; clients that never send DELETE cannot grow the table without bound.
const MAX_SESSIONS: usize = 256;

impl Sessions {
    /// Forget a session (`DELETE /mcp`).
    pub fn end(&self, id: &str) -> bool {
        self.names.lock().remove(id).is_some()
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Handle one JSON-RPC message. Returns `None` for notifications, plus a new session id after
/// `initialize`.
pub async fn handle(app: &Arc<App>, sessions: &Sessions, session: Option<&str>, msg: Value) -> (Option<Value>, Option<String>) {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_owned();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let Some(id) = id else {
        // Notification.
        return (None, None);
    };
    let agent = session.and_then(|s| sessions.names.lock().get(s).cloned()).unwrap_or_else(|| "AI agent".into());
    let actor = Actor::Agent { name: agent };
    match method.as_str() {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSIONS[0]);
            let version = if PROTOCOL_VERSIONS.contains(&requested) { requested } else { PROTOCOL_VERSIONS[0] };
            let client_name = params.pointer("/clientInfo/name").and_then(|v| v.as_str()).unwrap_or("AI agent").to_owned();
            let sid = uuid::Uuid::new_v4().simple().to_string();
            {
                let mut names = sessions.names.lock();
                if names.len() >= MAX_SESSIONS {
                    // Agent names are only labels for the activity log; dropping an old one is harmless.
                    if let Some(old) = names.keys().next().cloned() {
                        names.remove(&old);
                    }
                }
                names.insert(sid.clone(), client_name.clone());
            }
            app.emit(Event::McpClient { name: client_name, connected: true });
            let result = json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "twinbase", "title": "TwinBase", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            });
            (Some(rpc_result(&id, result)), Some(sid))
        }
        "ping" => (Some(rpc_result(&id, json!({}))), None),
        "tools/list" => {
            let list: Vec<Value> = tools()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name, "title": t.title, "description": t.description, "inputSchema": t.schema,
                        "annotations": { "readOnlyHint": t.read_only, "openWorldHint": false }
                    })
                })
                .collect();
            (Some(rpc_result(&id, json!({ "tools": list }))), None)
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = match call_tool(app, &name, args, &actor).await {
                Ok(v) => {
                    let mut text = serde_json::to_string_pretty(&v).unwrap_or_default();
                    if text.len() > MAX_TOOL_TEXT {
                        let mut cut = MAX_TOOL_TEXT;
                        while !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text.truncate(cut);
                        text.push_str("\n… (truncated)");
                        // The structured copy would carry the full payload again.
                        json!({ "content": [{ "type": "text", "text": text }], "isError": false })
                    } else {
                        let structured = if v.is_object() { v } else { json!({ "result": v }) };
                        json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
                    }
                }
                Err(e) => {
                    let mut msg = e.message.clone();
                    if let Some(code) = &e.code {
                        msg.push_str(&format!(" (code {code})"));
                    }
                    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
                }
            };
            (Some(rpc_result(&id, result)), None)
        }
        "resources/list" => (Some(rpc_result(&id, json!({ "resources": [] }))), None),
        "resources/templates/list" => (Some(rpc_result(&id, json!({ "resourceTemplates": [] }))), None),
        "prompts/list" => (Some(rpc_result(&id, json!({ "prompts": [] }))), None),
        _ => (Some(rpc_error(&id, -32601, &format!("method not found: {method}"))), None),
    }
}

/// Where an agent keeps its MCP servers, for one-click installs.
struct ConfigFile {
    path: std::path::PathBuf,
    /// Top-level key holding the servers (`mcpServers`, `servers`, `mcp`).
    key: &'static str,
    entry: Value,
}

fn home() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The per-user "application support" folder of desktop apps.
fn app_support(app: &str) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support").join(app)
    } else if cfg!(windows) {
        dirs::config_dir().unwrap_or_else(home).join(app)
    } else {
        home().join(".config").join(app)
    }
}

/// Every agent we know how to set up: copy-paste config, a one-line command where the agent has
/// one, and the config file TwinBase can write for you.
fn agent_setups(settings: &Settings) -> Vec<(Value, Option<ConfigFile>)> {
    let url = format!("http://127.0.0.1:{}/mcp", settings.mcp.port);
    let bearer = format!("Bearer {}", settings.mcp.token);
    let headers = json!({ "Authorization": bearer });
    let cli = cli_path();
    let stdio = json!({ "command": cli, "args": ["mcp"] });
    let pretty = |v: Value| serde_json::to_string_pretty(&v).unwrap_or_default();
    let quoted_cli = if cli.contains(' ') { format!("\"{cli}\"") } else { cli.clone() };
    let cursor_cfg = json!({ "url": url, "headers": headers });
    let cursor_link = format!(
        "cursor://anysphere.cursor-deeplink/mcp/install?name=twinbase&config={}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, cursor_cfg.to_string())
    );
    let vscode_entry = json!({ "type": "http", "url": url, "headers": headers });
    let agent = |id: &str, name: &str, detect: std::path::PathBuf, kind: &str, steps: &str, config: String, command: Option<String>| json!({ "id": id, "name": name, "kind": kind, "steps": steps, "config": config, "command": command, "detected": detect.exists() });
    vec![
        (
            agent("claude-code", "Claude Code", home().join(".claude"), "command", "One command in a terminal (adds TwinBase for every project):",
                format!("claude mcp add --transport http --scope user twinbase {url} --header \"Authorization: {bearer}\""),
                Some(format!("claude mcp add --transport http --scope user twinbase {url} --header \"Authorization: {bearer}\""))),
            None,
        ),
        (
            agent("codex", "Codex", home().join(".codex"), "command", "One command in a terminal, or add the TOML to ~/.codex/config.toml:",
                format!("[mcp_servers.twinbase]\ncommand = {}\nargs = [\"mcp\"]", toml::Value::String(cli.clone())),
                Some(format!("codex mcp add twinbase -- {quoted_cli} mcp"))),
            None,
        ),
        (
            agent("gemini", "Gemini CLI", home().join(".gemini"), "json", "One command, or Install to add it to ~/.gemini/settings.json:",
                pretty(json!({ "mcpServers": { "twinbase": { "httpUrl": url, "headers": headers } } })),
                Some(format!("gemini mcp add --scope user --transport http --header \"Authorization: {bearer}\" twinbase {url}"))),
            Some(ConfigFile { path: home().join(".gemini/settings.json"), key: "mcpServers", entry: json!({ "httpUrl": url, "headers": headers }) }),
        ),
        (
            agent("antigravity", "Antigravity", home().join(".gemini"), "json", "Install writes the shared Antigravity config (~/.gemini/config/mcp_config.json); or Agent panel → MCP Servers → View raw config:",
                pretty(json!({ "mcpServers": { "twinbase": stdio } })), None),
            Some(ConfigFile { path: home().join(".gemini/config/mcp_config.json"), key: "mcpServers", entry: stdio.clone() }),
        ),
        (
            agent("cursor", "Cursor", home().join(".cursor"), "json", "Open the link to install in Cursor, use Install, or add to ~/.cursor/mcp.json:",
                pretty(json!({ "mcpServers": { "twinbase": cursor_cfg } })), None),
            Some(ConfigFile { path: home().join(".cursor/mcp.json"), key: "mcpServers", entry: cursor_cfg.clone() }),
        ),
        (
            agent("vscode", "VS Code + Copilot", app_support("Code").join("User"), "json", "One command, Install (user mcp.json), or add to .vscode/mcp.json in a project:",
                pretty(json!({ "servers": { "twinbase": vscode_entry } })),
                Some(format!("code --add-mcp '{}'", json!({ "name": "twinbase", "type": "http", "url": url, "headers": headers })))),
            Some(ConfigFile { path: app_support("Code").join("User/mcp.json"), key: "servers", entry: vscode_entry.clone() }),
        ),
        (
            agent("copilot-cli", "Copilot CLI", home().join(".copilot"), "json", "Install, or add to ~/.copilot/mcp-config.json:",
                pretty(json!({ "mcpServers": { "twinbase": { "type": "http", "url": url, "headers": headers, "tools": ["*"] } } })), None),
            Some(ConfigFile { path: home().join(".copilot/mcp-config.json"), key: "mcpServers", entry: json!({ "type": "http", "url": url, "headers": headers, "tools": ["*"] }) }),
        ),
        (
            agent("claude-desktop", "Claude Desktop", app_support("Claude"), "json", "Install, or Settings → Developer → Edit Config and add:",
                pretty(json!({ "mcpServers": { "twinbase": stdio } })), None),
            Some(ConfigFile { path: app_support("Claude").join("claude_desktop_config.json"), key: "mcpServers", entry: stdio.clone() }),
        ),
        (
            agent("windsurf", "Windsurf", home().join(".codeium/windsurf"), "json", "Install, or add to ~/.codeium/windsurf/mcp_config.json:",
                pretty(json!({ "mcpServers": { "twinbase": { "serverUrl": url, "headers": headers } } })), None),
            Some(ConfigFile { path: home().join(".codeium/windsurf/mcp_config.json"), key: "mcpServers", entry: json!({ "serverUrl": url, "headers": headers }) }),
        ),
        (
            agent("kiro", "Kiro", home().join(".kiro"), "json", "Install, or add to ~/.kiro/settings/mcp.json:",
                pretty(json!({ "mcpServers": { "twinbase": { "url": url, "headers": headers } } })), None),
            Some(ConfigFile { path: home().join(".kiro/settings/mcp.json"), key: "mcpServers", entry: json!({ "url": url, "headers": headers }) }),
        ),
        (
            agent("opencode", "opencode", home().join(".config/opencode"), "json", "Install, or add to ~/.config/opencode/opencode.json:",
                pretty(json!({ "mcp": { "twinbase": { "type": "remote", "url": url, "headers": headers, "enabled": true } } })), None),
            Some(ConfigFile { path: home().join(".config/opencode/opencode.json"), key: "mcp", entry: json!({ "type": "remote", "url": url, "headers": headers, "enabled": true }) }),
        ),
        (
            agent("zed", "Zed", app_support("Zed"), "json", "Add to Zed's settings.json (Zed → Settings → Open Settings):",
                pretty(json!({ "context_servers": { "twinbase": { "source": "custom", "command": cli, "args": ["mcp"], "env": {} } } })), None),
            None,
        ),
        (
            agent("cline", "Cline / Roo", home().join(".vscode"), "json", "MCP Servers → Configure → add to the servers file:",
                pretty(json!({ "mcpServers": { "twinbase": { "type": "streamableHttp", "url": url, "headers": headers } } })), None),
            None,
        ),
        (
            agent("stdio", "Any agent (stdio)", home(), "json", "For agents that start MCP servers as programs. The bridge reads the token itself:",
                pretty(json!({ "mcpServers": { "twinbase": stdio } })), None),
            None,
        ),
        (
            agent("http", "Any agent (HTTP)", home(), "text", "Streamable HTTP endpoint and header:", format!("URL: {url}\nHeader: Authorization: {bearer}"), None),
            None,
        ),
    ]
    .into_iter()
    .map(|(mut info, file)| {
        if let Some(f) = &file {
            info["install"] = json!({ "path": f.path.to_string_lossy(), "installed": installed(f) });
        }
        if info["id"] == "cursor" {
            info["link"] = json!(cursor_link);
        }
        (info, file)
    })
    .collect()
}

fn read_json_config(path: &std::path::Path) -> crate::error::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|_| {
            crate::error::CoreError::invalid(format!(
                "{} is not plain JSON (comments?), so TwinBase won't rewrite it. Add the snippet by hand.",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(crate::error::CoreError::io(path, e)),
    }
}

fn installed(f: &ConfigFile) -> bool {
    read_json_config(&f.path).ok().and_then(|v| v.get(f.key).and_then(|s| s.get("twinbase")).cloned()).is_some_and(|e| e == f.entry)
}

/// Add (or update) the `twinbase` server in an agent's config file. The first time a file is
/// changed, the original is kept next to it as `<name>.twinbase-backup`.
pub fn install(settings: &Settings, agent: &str) -> crate::error::Result<Value> {
    let (_, file) = agent_setups(settings)
        .into_iter()
        .find(|(info, _)| info["id"] == agent)
        .ok_or_else(|| crate::error::CoreError::not_found(format!("unknown agent '{agent}'")))?;
    let f = file.ok_or_else(|| crate::error::CoreError::invalid("this agent is set up with a command or by hand"))?;
    write_entry(&f)
}

fn write_entry(f: &ConfigFile) -> crate::error::Result<Value> {
    let mut doc = read_json_config(&f.path)?;
    if !doc.is_object() {
        return Err(crate::error::CoreError::invalid(format!("{} does not hold a JSON object", f.path.display())));
    }
    let servers = doc.as_object_mut().unwrap().entry(f.key).or_insert_with(|| json!({}));
    if !servers.is_object() {
        return Err(crate::error::CoreError::invalid(format!("\"{}\" in {} is not an object", f.key, f.path.display())));
    }
    servers.as_object_mut().unwrap().insert("twinbase".into(), f.entry.clone());
    if let Some(dir) = f.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::error::CoreError::io(dir, e))?;
    }
    let backup = f.path.with_file_name(format!("{}.twinbase-backup", f.path.file_name().unwrap_or_default().to_string_lossy()));
    if f.path.exists() && !backup.exists() {
        std::fs::copy(&f.path, &backup).map_err(|e| crate::error::CoreError::io(&backup, e))?;
    }
    let tmp = f.path.with_extension("twinbase-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n").map_err(|e| crate::error::CoreError::io(&tmp, e))?;
    std::fs::rename(&tmp, &f.path).map_err(|e| crate::error::CoreError::io(&f.path, e))?;
    Ok(json!({ "path": f.path.to_string_lossy(), "backup": backup.exists().then(|| backup.to_string_lossy().into_owned()) }))
}

/// Copy-paste setup for popular coding agents.
pub fn install_instructions(settings: &Settings) -> Value {
    let url = format!("http://127.0.0.1:{}/mcp", settings.mcp.port);
    let agents: Vec<Value> = agent_setups(settings).into_iter().map(|(info, _)| info).collect();
    json!({ "url": url, "token": settings.mcp.token, "cli": cli_path(), "enabled": settings.mcp.enabled, "agents": agents,
        "note": "TwinBase must be open (or `twinbase serve` running) for agents to connect." })
}

/// The `twinbase` CLI next to this executable, or the bare name when not found.
pub fn cli_path() -> String {
    let exe = std::env::current_exe().ok();
    let name = if cfg!(windows) { "twinbase.exe" } else { "twinbase" };
    if let Some(dir) = exe.as_ref().and_then(|e| e.parent()) {
        for candidate in [dir.join(name), dir.join("../Resources").join(name)] {
            if candidate.exists() {
                return candidate.canonicalize().unwrap_or(candidate).to_string_lossy().into_owned();
            }
        }
    }
    name.to_owned()
}

/// Bridge stdio JSON-RPC to the running app's HTTP endpoint (`twinbase mcp`).
pub async fn stdio_bridge(url: &str, token: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut session: Option<String> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg.get("id").cloned();
        match post_json(url, token, session.as_deref(), &line).await {
            Ok((status, sid, body)) => {
                if let Some(s) = sid {
                    session = Some(s);
                }
                if let Some(id) = &id {
                    let out = if (200..300).contains(&status) && !body.trim().is_empty() {
                        body
                    } else {
                        rpc_error(id, -32603, &format!("TwinBase returned HTTP {status}")).to_string()
                    };
                    stdout.write_all(out.trim().as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
            Err(e) => {
                if let Some(id) = id {
                    let out = rpc_error(
                        &id,
                        -32603,
                        &format!(
                            "TwinBase is not running or not reachable at {url}: {e}. Open the TwinBase app (or run `twinbase serve`)."
                        ),
                    );
                    stdout.write_all(out.to_string().as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
    }
    Ok(())
}

/// Minimal HTTP/1.1 POST over TCP (keeps the CLI free of an HTTP client dependency).
async fn post_json(url: &str, token: &str, session: Option<&str>, body: &str) -> anyhow::Result<(u16, Option<String>, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow::anyhow!("only http:// URLs are supported"))?;
    let (hostport, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let mut stream = tokio::net::TcpStream::connect(hostport).await?;
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(s) = session {
        req.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    // Chunk sizes count bytes, so the body is decoded only after de-chunking.
    let (head, body) = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => (&raw[..p], &raw[p + 4..]),
        None => (&raw[..], &[][..]),
    };
    let head = String::from_utf8_lossy(head);
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sid = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("mcp-session-id").then(|| v.trim().to_owned())
    });
    let chunked =
        head.lines().any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked"));
    let body = if chunked { dechunk(body) } else { body.to_vec() };
    Ok((status, sid, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(p) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&rest[..p])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok())
            .unwrap_or(0);
        let after = &rest[p + 2..];
        if size == 0 || after.len() < size {
            break;
        }
        out.extend_from_slice(&after[..size]);
        rest = after[size..].strip_prefix(b"\r\n").unwrap_or(&after[size..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunks_bytes_split_inside_characters() {
        // "é" (2 bytes) is split across two chunks.
        let body = b"3\r\nab\xc3\r\n2;ext=1\r\n\xa9c\r\n0\r\n\r\n";
        assert_eq!(String::from_utf8(dechunk(body)).unwrap(), "abéc");
    }

    #[test]
    fn sessions_are_bounded_and_can_end() {
        let s = Sessions::default();
        for i in 0..(MAX_SESSIONS + 10) {
            let mut names = s.names.lock();
            if names.len() >= MAX_SESSIONS {
                let old = names.keys().next().cloned().unwrap();
                names.remove(&old);
            }
            names.insert(i.to_string(), "x".into());
        }
        assert_eq!(s.names.lock().len(), MAX_SESSIONS);
        let any = s.names.lock().keys().next().cloned().unwrap();
        assert!(s.end(&any) && !s.end(&any));
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn merges_into_existing_config_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#).unwrap();
        let f = ConfigFile { path: path.clone(), key: "mcpServers", entry: json!({ "url": "http://127.0.0.1:1/mcp" }) };
        assert!(!installed(&f));
        let r = write_entry(&f).unwrap();
        assert!(r["backup"].is_string());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x", "other servers are kept");
        assert_eq!(v["theme"], "dark");
        assert!(installed(&f));
        // Files with comments are refused rather than rewritten.
        std::fs::write(&path, "{ // hi\n }").unwrap();
        assert!(write_entry(&f).is_err());
        // A missing file is created.
        let fresh = ConfigFile { path: dir.path().join("new/dir/settings.json"), key: "servers", entry: json!(1) };
        write_entry(&fresh).unwrap();
        assert!(installed(&fresh));
    }
}
