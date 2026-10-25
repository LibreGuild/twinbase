# Database client and change preview

`crates/client` connects to real databases: PostgreSQL (`tokio-postgres`), MySQL (`mysql_async`),
SQL Server and Azure SQL (TDS through `tiberius`), Oracle (the `oracle` crate on ODPI-C, with
Oracle Instant Client), ClickHouse (its HTTP interface), Cassandra and
ScyllaDB (CQL through the `scylla` driver), Cloud Spanner and BigQuery (their REST APIs),
Elasticsearch and OpenSearch (REST), SQLite (`rusqlite`, local files), Redis (`redis`),
MongoDB (the official `mongodb` driver),
DynamoDB (its HTTP API with AWS Signature Version 4, implemented in `dynamo_client.rs`) and
Firestore (the gRPC API, generated from Google's protos in `crates/firestore-proto`). The same code
is used for mock servers.

## Connections

Saved connections store host, port, user, database, TLS mode, colour tag, read-only flag and AI
access level in the workspace (`connections/<id>.toml`). Passwords are stored on this computer only.
Paste a URL (`postgres://…`, `mysql://…`, `sqlserver://…`, `redis://…`, `mongodb://…`, `mongodb+srv://…`) in the
dialog to fill the fields. MongoDB connections keep a whole connection string in the host field
(several hosts, SRV, options such as `authSource` or `replicaSet`); the password is always taken out
of it and stored on this computer, also when the connection is saved through the API.

TLS modes: **Off**, **Required** (encrypted, certificate not verified — like libpq
`sslmode=require`) and **Verify certificate** (public CA roots and host name).

### Passwords and the OS keychain

Database passwords and SSH secrets are stored in the operating system's keychain (macOS Keychain,
Windows Credential Manager, Secret Service on Linux). Secrets saved by older versions in
`secrets.toml` move into the keychain the first time they are read. When no keychain is available —
or the app directory is isolated with `TWINBASE_HOME` (tests, CI, portable installs) — secrets stay
in `secrets.toml` in the app directory, written with owner-only permissions. Settings → About shows
which store is in use. Secrets never go into the workspace or bundles.

### SSH tunnels

Turn on **Connect through an SSH tunnel** to reach a database in a private network through a
bastion host. Authentication: password, private key file (OpenSSH or PEM, optional passphrase), or
the running ssh-agent. TwinBase forwards a local port through the SSH session, so PostgreSQL,
MySQL and Redis connections work unchanged; TLS still verifies the database's real host name. The
bastion's host key is checked against `~/.ssh/known_hosts`; an unknown key is trusted on first use
and recorded in TwinBase's own `known_hosts` (in the app directory), and a key that changed is
refused, as a possible man-in-the-middle.

## Browsing and editing tables

- Rows load in pages (default 200) with a total count; sort by clicking headers (Shift+click adds a
  sort key); filter with typed filter chips or a raw `WHERE` clause.
- Edits are **staged**: change cells, add, duplicate or delete rows, then **Review** (shows the exact
  SQL) or **Save**. All statements run in one transaction, one statement per edited row; if the
  number of affected rows differs from the number of edited rows (a row was deleted or changed its key
  elsewhere), the whole save is rolled back with error `TB001`.
- Rows are identified by the primary key (SQLite tables without one use `rowid`). Tables with no way
  to identify rows are shown read-only.
- The value inspector shows long text and pretty-printed JSON and edits them.
- Right-click a foreign-key cell to open the referenced row.

## SQL editor

Dialect-aware highlighting and completion from the live schema. ⌘/Ctrl+Enter runs everything or the
selection; Shift+Enter runs the statement under the cursor. Results open in a virtualized grid and
can be copied as CSV, JSON or a Markdown table. History is kept per connection.

## Preview on a local twin

**Preview** runs your statements against a copy of the affected data instead of the server:

1. The target tables are copied into a temporary TwinLite database — rows matching the statement's
   `WHERE` clause (up to 2,000), otherwise a 500-row sample — together with child rows that reference
   them, so `ON DELETE CASCADE` and `SET NULL` show up.
2. The statements run on the twin.
3. TwinBase shows a row-level diff per table: added, removed and changed rows with old → new values.

**Commit** runs the same SQL on the server in one transaction and rolls back if the affected-row count
differs from the preview (when the preview copied complete data). Notes in the preview say what is
not reproduced (triggers, server functions, sampled tables).

## Query plans

**Explain** in the SQL editor shows the plan of the statement at the cursor (or the selection) as a
tree: each step with its table and index, estimated rows, and a bar for its share of the cost.
**Explain analyze** runs the statement inside a transaction that is always rolled back — even for
`INSERT`/`UPDATE`/`DELETE` — and adds actual rows, loops and time per step. Steps whose estimates are
off by more than 10× are highlighted.

| Database | Format used |
| --- | --- |
| PostgreSQL | `EXPLAIN (FORMAT JSON[, ANALYZE, BUFFERS])`, falling back to the text format |
| MySQL 8 | `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE`, falling back to tabular `EXPLAIN` |
| SQLite | `EXPLAIN QUERY PLAN` |
| SQL Server | `SET SHOWPLAN_ALL` (estimated) / `SET STATISTICS PROFILE` (actual, rolled back) |
| ClickHouse | `EXPLAIN json = 1, indexes = 1` (granules read per index; no actual run) |
| Cassandra | No planner: the statement's shape is checked (full scans, `ALLOW FILTERING`) |
| Oracle | `EXPLAIN PLAN` read from `PLAN_TABLE` (estimates) |
| Spanner | The server's query plan (`PLAN`; `PROFILE` with actual rows, rolled back) |
| BigQuery | A dry run: bytes the query would scan and its on-demand cost |
| Mock servers | TwinLite's plan in the same tree view |

Hints point out full scans with filters (suggesting the columns to index), large misestimates (stale
statistics), sorts that spill to disk, and temporary tables or filesorts. Analyzing a write needs
write access to the connection, also for AI agents.

## SQL Server

SQL Server 2012+ and Azure SQL Database connect with a SQL login (user and password), or — with the
user field empty — a Microsoft Entra (Azure AD) access token in the password field. TLS: **Off**
encrypts only the login, as `Encrypt=no` does in Microsoft's drivers; **Required** encrypts
everything without checking the certificate (`TrustServerCertificate=true`, needed for the
self-signed certificate of a default installation or Docker image); **Verify certificate** checks it
against the operating system's trusted roots (use it for Azure SQL). Azure SQL gateway redirects are
followed.

The explorer shows the schemas of the connection's database (`dbo` first), tables and views with
columns (exact types such as `nvarchar(100)` or `decimal(10,2)`), identity columns, defaults,
indexes and foreign keys. The grid pages with `OFFSET … FETCH`, edits use bracket-quoted names and
`N'…'` literals, and staged changes run in one transaction with the usual affected-row check.

The SQL editor understands T-SQL scripts as SSMS and sqlcmd do: `GO` on its own line separates
batches (`GO 5` repeats one). Inside a batch, statements run one by one so each gets its own result
and row count; a batch that declares variables, contains `BEGIN … END`, or creates a procedure,
function, trigger or view runs as one unit. Temporary tables and open transactions stay with the
connection between runs. Errors show the SQL Server error number (e.g. 2627 for a duplicate key) and
the line. DDL export, the table designer and schema diffs write T-SQL: `IDENTITY(1,1)`,
`ALTER TABLE … ADD`, `ALTER COLUMN`, `sp_rename`, and default constraints dropped by looking up their
server-generated names. **Preview on a local twin** is not available for SQL Server yet (TwinLite
does not run T-SQL); run risky changes inside `BEGIN TRANSACTION … ROLLBACK` or use Explain.

## ClickHouse

ClickHouse connections use the HTTP interface: `host` and port 8123, or 8443 with TLS (Required or
Verify certificate); a URL such as `https://….clickhouse.cloud:8443` in the host field works too
(ClickHouse Cloud). User `default` when left empty; the **Database** field is the default database.
Every statement is one HTTP request in a session, so `USE`, `SET` and temporary tables last between
runs. 64-bit integers beyond JavaScript's safe range and decimals are shown exactly (as text);
arrays, maps and tuples as JSON. A statement with its own `FORMAT` clause shows the raw output.

The explorer lists databases (the connection's first), tables and views with exact types
(`LowCardinality(String)`, `Nullable(…)`, `DateTime64(3)`), defaults, `MATERIALIZED`/`ALIAS`
columns (marked generated), the sorting key as the primary key, and data-skipping indexes. The grid
pages, sorts and filters (case-insensitive `ILIKE`) but is **read-only**: ClickHouse has no
transactions, so staged edits cannot be applied atomically — change data with SQL (`INSERT`,
`ALTER TABLE … UPDATE/DELETE`, lightweight `DELETE FROM`). Errors show ClickHouse's error name (for
example `UNKNOWN_TABLE`). DDL export, the designer and schema diffs write ClickHouse SQL (`MergeTree
ORDER BY` the key, `Nullable` types, `ADD`/`MODIFY`/`DROP COLUMN`); sorting-key, index and
foreign-key differences are reported, not scripted. Explain shows the plan with how many granules
each index selects and flags tables read completely despite a `WHERE`. Preview is not available
(mutations are asynchronous).

## Cassandra and ScyllaDB

Connect to any node (CQL, port 9042) with or without a user and password; TLS uses the same modes as
other connections. The **Keyspace** field picks the default keyspace. TwinBase talks to every node
through the address you entered — nodes often advertise addresses private to their network
(Docker, Kubernetes, NAT, an SSH bastion), and the node you reach coordinates requests for the rest.
The server version tells Cassandra and ScyllaDB apart.

The explorer lists keyspaces, tables and materialized views; a table shows its partition key and
clustering columns (as the primary key, in order), secondary indexes, and CQL types such as
`set<text>` or `map<text, int>`. Values: `decimal` and `varint` exactly (as text), timestamps in UTC,
blobs as hex, collections and user-defined types as JSON. The grid reads ahead for paging (CQL has no
OFFSET; up to 10,000 rows deep), shows no total (counting scans the cluster), adds `ALLOW FILTERING`
when you filter on non-key columns, and keeps the server's partition order (sorting needs `ORDER BY`
on clustering columns in a query). Staged edits are written with literals typed by column (`uuid`,
`blob`, collections) in one logged batch: applied together, without isolation; Cassandra writes are
upserts, so there is no affected-row check. Lightweight transactions show their `[applied]` row.
Explain flags full scans and `ALLOW FILTERING`. DDL export writes `PRIMARY KEY ((partition…),
clustering…)`; schema diffs add and drop columns and report what CQL cannot change (types, keys).
Preview is not available.

## Oracle

Oracle connections need **Oracle Instant Client** (Basic or Basic Light) on the computer — TwinBase
does not ship it. Set its folder in Settings › Data, or set `TWINBASE_ORACLE_CLIENT`; otherwise
TwinBase looks in `~/Downloads/instantclient_*`, `~/instantclient_*`, `/opt/oracle` and
`/usr/lib/oracle/*/client64/lib`. The connection uses Easy Connect: host, port (1521) and **service
name** (for example `FREEPDB1` or `ORCLPDB1`); TLS uses `tcps://` (Required skips the server
certificate name check).

Scripts follow SQL*Plus: statements end with `;` (removed before sending, as Oracle requires), and
PL/SQL blocks — `BEGIN … END;`, `DECLARE …`, `CREATE OR REPLACE PROCEDURE/FUNCTION/PACKAGE/
TRIGGER/TYPE` — run to a `/` on its own line. Statements autocommit; staged grid edits run in one
transaction with the affected-row check. The explorer lists the schemas of non-Oracle-maintained
users (yours first) with tables, views, column types as Oracle writes them (`VARCHAR2(100 CHAR)`,
`NUMBER(10,2)`), identity columns, defaults, keys, foreign keys and indexes. Paging uses `OFFSET …
FETCH`; filters compare case-insensitively. Errors show their `ORA-` code. DDL export and schema
diffs write Oracle DDL (`GENERATED BY DEFAULT AS IDENTITY`, `ALTER TABLE … MODIFY (…)`). Explain
shows the optimizer's plan from `PLAN_TABLE`.

*Status:* the Oracle driver was built and unit-tested, but not run against an Oracle database in
development (that needs Oracle's licensed Instant Client and database images); the end-to-end suite
in `crates/client/tests/oracle.rs` runs when one is configured.

## Google Cloud: Spanner and BigQuery

Both connect to Google Cloud when the endpoint is empty, or to their emulators (`host:port`: the
Spanner emulator's REST port 9020, the BigQuery emulator's 9050) with no credentials. Credentials,
in this order: a service-account key file chosen in the dialog (TwinBase signs short-lived tokens
with it; the key stays in its file), an access token in the password field, or Application Default
Credentials — `GOOGLE_APPLICATION_CREDENTIALS`, or the user credentials `gcloud auth
application-default login` writes, whose refresh token TwinBase exchanges at Google's OAuth endpoint.

**Spanner** (GoogleSQL databases; the database field is `project/instance/database`). Queries run as
strong single-use reads; each DML statement runs in its own read-write transaction and shows its
exact row count; consecutive `CREATE`/`ALTER`/`DROP` statements are sent together as one schema
update and TwinBase waits for it. The explorer shows tables with their keys, unique and secondary
indexes, foreign keys, and interleaving (a child table references its parent through the parent's
key; DDL export writes `INTERLEAVE IN PARENT … ON DELETE CASCADE`). The grid pages, sorts and filters
(case-insensitively); staged edits run in one read-write transaction with the affected-row check,
with literals typed by column (`NUMERIC '1.5'`, `FROM_HEX(…)`, `JSON '…'`, arrays). Explain shows
Spanner's own plan; Explain analyze profiles the statement inside a transaction that is rolled
back. Schema diffs add, drop and alter columns and indexes and report key or interleaving changes.

**BigQuery** (the database field is `project`, or `project.dataset` to make a dataset the default).
Datasets are the schemas; tables show nested `STRUCT` and `ARRAY` types, unenforced primary and
foreign keys, and clustering columns. Browsing a table without filters or sorting reads it with
`tabledata.list`, which is not billed; filtered or sorted pages and the SQL editor run query jobs,
which are billed by bytes scanned — **Explain** runs a dry run first and shows the bytes and the
on-demand cost. Results from the query cache are marked. The grid is read-only: change data with
DML statements. Preview on a local twin is not available for Spanner or BigQuery yet.

## Elasticsearch and OpenSearch

One connection kind covers both (the product and version are detected). Endpoint: `host` and port
9200, HTTPS when TLS is on, or a URL in the host field (Elastic Cloud, Amazon OpenSearch Service with
basic auth). Authentication: user and password (basic), an API key in the password field with the
user left empty (`id:key` or the base64 form Elasticsearch prints), or none.

The explorer lists indices with document counts and aliases (system indices on request). An index
tab searches with a query string in Lucene syntax (`title:rust AND year:>2019`) or a query DSL object
(`{"match": {"title": "rust"}}`), sorts by any sortable field from the mapping (keyword, numeric,
date, …), pages with from/size (up to 10,000 hits — beyond that use `search_after` in the console),
and shows documents as JSON or a table. **Fields** lists the mapping as dotted paths with types,
including multi-fields such as `title.keyword`. Documents are edited as JSON: saving sends the
sequence number and primary term it was loaded with, so a document someone changed meanwhile is not
overwritten; a new document with an explicit id fails if it exists; writes wait for a refresh, so
they are searchable at once.

Query tabs on these connections are a console in Kibana Dev Tools syntax:

```text
GET /books/_search
{ "query": { "match": { "title": "rust" } }, "size": 5 }

POST /_bulk
{ "index": { "_index": "books", "_id": "4" } }
{ "title": "Zero to Production", "year": 2022 }

POST /_sql?format=json
{ "query": "SELECT title FROM books ORDER BY year DESC LIMIT 3" }
```

Bodies are JSON (`"""…"""` strings allowed); `_bulk` and `_msearch` take NDJSON lines. Each request
gets a result tab with its status; search responses show their hits as documents (or the raw JSON).
The first failing request stops the script. `GET` requests and `POST` to `_search`, `_count`,
`_msearch`, `_mget`, `_sql`, `_query` (ES|QL), `_explain`, `_validate`, `_field_caps` and
`_analyze` are reads — allowed on read-only connections and for read-only agents; everything else is
a write.

## MongoDB

A MongoDB connection (or mock) opens as a tree of databases and collections. Each collection tab has:

- **Documents:** filter, sort and projection in mongosh syntax (`{ age: { $gte: 18 } }`,
  `ObjectId("…")`, `ISODate("…")`, `/regex/i`), pages of 20–500 documents with the total count, a
  JSON view with collapsible values and a table view of top-level fields. Documents can be edited,
  duplicated, inserted (an array inserts several) and deleted. Edits replace the document by `_id`
  and fail if it was deleted meanwhile; editing is off while a projection hides fields.
- **Aggregation:** a pipeline editor with stage snippets; results in the JSON or table view.
- **Indexes:** list, create (unique, named) and drop.
- **Schema:** field paths (nested objects, `[]` for array elements) with their BSON types and how
  often each appears, sampled from up to 300 documents.

Query tabs on MongoDB connections are a **mongosh-style console**: one statement per line (or
across lines inside brackets), e.g.

```js
use shop
db.orders.find({ status: "paid" }).sort({ total: -1 }).limit(10)
db.orders.aggregate([{ $group: { _id: "$customer", spent: { $sum: "$total" } } }])
db.orders.updateMany({ status: "new" }, { $set: { status: "paid" } })
db.orders.createIndex({ customer: 1 })
show collections
```

Supported: `find`/`findOne` (with `.sort`, `.limit`, `.skip`, `.projection`, `.count`, `.explain`,
`.hint`, `.maxTimeMS`), `aggregate`, `countDocuments`, `estimatedDocumentCount`, `distinct`,
`insertOne`/`insertMany`, `updateOne`/`updateMany` (with `upsert` and `arrayFilters`),
`replaceOne`, `deleteOne`/`deleteMany`, `findOneAndUpdate`/`findOneAndReplace`/`findOneAndDelete`,
`createIndex`, `getIndexes`, `dropIndex(es)`, `drop`, `renameCollection`, `stats`, and
`db.runCommand`, `db.adminCommand`, `db.getCollectionNames`, `db.createCollection`,
`db.dropDatabase`, `db.getSiblingDB`, `show dbs`, `show collections`, `use`. Each statement gets its
own result tab; a failing statement stops the run. Console scripts can be saved and documented like
SQL queries.

Agents and read-only connections: statements are classified before they run. Reads (`find`,
`aggregate` without `$out`/`$merge`, `countDocuments`, read-only `runCommand`s) are allowed on
read-only connections; anything else is a write.

## DynamoDB

A DynamoDB connection has an **endpoint** (empty for AWS — `dynamodb.<region>.amazonaws.com` over
TLS — or a URL such as `http://localhost:8000`), a **region**, and an **access key ID / secret**
(stored like passwords). Without an access key, TwinBase uses `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, then `~/.aws/credentials` (profile from
`AWS_PROFILE`, else `default`).

Each table opens with:

- **Items:** leave the partition key empty to *Scan*, fill it to *Query*; with a sort key, choose a
  condition (`=`, `<`, `between`, `begins with`, …) and the direction; switch to an index; add
  filters (attribute path, operator, typed value). TwinBase writes the expressions with `#name` and
  `:value` placeholders, so reserved words just work. Pages follow `LastEvaluatedKey` (Next / Prev).
  Items show as plain values (JSON or table view); the editor uses DynamoDB JSON so types (sets,
  numbers, binary) are exact. Saving an edit fails if the item was deleted meanwhile; a new item
  fails if its key already exists; keys cannot change.
- **Table:** item count, size, billing mode, TTL, key schema, indexes and the raw `DescribeTable`.

Query tabs on DynamoDB connections are a **request console**: each request starts with an
operation name followed by its JSON, for example

```text
Query {
  "TableName": "orders",
  "KeyConditionExpression": "pk = :p",
  "ExpressionAttributeValues": { ":p": { "S": "customer#1" } }
}
UpdateItem { "TableName": "orders", "Key": { … }, "UpdateExpression": "SET #t = :t", … }
```

Responses with items show them as plain values; other responses as JSON. Read operations (`Query`,
`Scan`, `GetItem`, `BatchGetItem`, `TransactGetItems`, `Describe*`, `List*`) are allowed on read-only
connections and for read-only agents; everything else counts as a write.

## Firestore

A Firestore connection has an **endpoint** (empty for Google Cloud — `firestore.googleapis.com` over
TLS — or `host:port` for the Firestore emulator or a TwinBase mock), a **project id** (`project`, or
`project/database` for a named database) and, for Google Cloud, a **service-account key file**
(otherwise `GOOGLE_APPLICATION_CREDENTIALS`). TwinBase signs short-lived self-signed JWTs with the
key (audience `https://firestore.googleapis.com/`), so no token exchange is needed; the key stays in
its file and is never copied into the workspace. Emulators get the token `owner` (or the password of
a TwinBase mock).

The explorer lists root collections. A collection tab shows documents (JSON with timestamps,
references and geo points rendered as values, or a table), `where` conditions, ordering and paging,
each document's subcollections (as a breadcrumb path), and an editor for JSON documents with the
markers `{"__timestamp": …}`, `{"__ref": "users/bob"}`, `{"__geo": [lat, lng]}`, `{"__bytes": …}`.
Saving an edit fails if the document was deleted meanwhile; a new document with an explicit id fails
if it exists.

Query tabs on Firestore connections run **statements**, one per line:

```text
query users where age >= 30 and city == "Lisbon" order by age desc limit 10
query group orders where status in ["open", "paid"]
count users where active == true
get users/alice
collections users/alice
set users/alice {"name": "Alice", "age": 31}
update users/alice {"age": 32, "address.city": "Porto"}
add users {"name": "Bo"}
delete users/alice
```

Values are JSON. `update` merges the given fields (dotted keys address nested fields) and fails when
the document does not exist; `set` replaces the document. Reads are allowed on read-only connections
and for read-only agents; `set`, `update`, `add` and `delete` are writes.
