# Mock servers

A mock server is a real network server that speaks the PostgreSQL, MySQL, SQL Server (TDS), Redis or
MongoDB wire protocol, the DynamoDB HTTP API, or the Firestore gRPC API. Applications, ORMs, migration tools and CLIs connect to it with their usual drivers; only
the host and port differ from production. Data lives in TwinLite (SQL mocks) or an in-memory store
with snapshots (Redis, MongoDB, DynamoDB, Firestore).

## Creating one

- **Desktop app:** Explorer → *Mock servers* → **+**, or a schema template on the Home tab.
- **Agent:** the `create_mock_server` MCP tool.
- **Headless:** `twinbase mock postgres|mysql|mssql|redis|mongodb|dynamodb|firestore` (see [cli.md](cli.md)).

Each mock has:

| Setting | Meaning |
| --- | --- |
| Port | Fixed per mock (a free one is suggested: 15432+, 13306+, 11433+, 16379+, 27018+, 18000+, 18080+). |
| Listen on | `127.0.0.1` (this computer) or `0.0.0.0` (LAN, containers, other devices). |
| Authentication | *Accept any* user/password (default, so existing configs work) or an exact user and password. MongoDB mocks in *accept any* mode take no credentials at all (see below). |
| Database | Database created for PostgreSQL/MySQL clients and the seeded MongoDB database (default `app`). PostgreSQL clients may also create more with `CREATE DATABASE`. |
| Keep data | Persist data in the workspace (`mocks/<id>/data/`), otherwise a temporary directory per run. |
| Start when TwinBase opens | Auto-start. |

Several mocks of any kind can run at the same time.

## Connecting your application

The **Connect** tab of a mock shows ready-to-paste connection strings and code for many stacks
(environment variables, `psql`/`mysql`/`redis-cli`, Node.js, Python, Go, Java, .NET, Rust, Prisma,
SQLAlchemy, Django, Rails and more).

## Seeds and reset

The **seed script** (`mocks/<id>/seed.sql`, or Redis commands one per line; MongoDB mocks use
`mocks/<id>/seed.json`, DynamoDB mocks a table list and Firestore mocks documents by path in the same
file) runs when the mock starts
with empty data and on every **Reset**. *Capture current data* writes the current schema and rows as
the new seed (PostgreSQL sequences are advanced past the copied ids). Seeds travel with exported
bundles, so a teammate gets the same starting point.

## Live traffic

The **Traffic** tab streams every statement each client sends, with duration, row counts and errors,
plus connects/disconnects. It is the quickest way to see what an ORM actually generates.

## Fault injection

The **Faults** tab (or the `set_mock_faults` MCP tool, or `mock.faults` in `mock.toml`) makes a
mock slow or unreliable so you can test retries, timeouts and error handling:

- **Rules** run in order for every statement (or Redis command). Each has a match — empty for
  everything, a case-insensitive substring, or `/regex/` — a probability, and an action:
  **Delay** (milliseconds plus random jitter), **Error** (any SQLSTATE such as `40P01`, MySQL error
  number such as `1213`, or Redis prefix such as `READONLY`) or **Drop** (close the connection; on
  PostgreSQL the client sees `57P01`, as when a backend is terminated).
- **Refuse new connections** simulates a database that is down while existing connections keep
  working.
- Presets: slow database, latency spikes, deadlocks on writes, read-only, flaky connections,
  database down.

Changes apply immediately to connected clients and every injected fault appears in the Traffic tab
as `[fault]`. The explorer marks mocks with active faults with ⚡.

## Compatibility

Verified in automated tests with real clients:

- **PostgreSQL:** `tokio-postgres` (simple and extended protocol, prepared statements, typed
  parameters), `psql` 16 including `\dt`, `\d table` (columns, indexes, foreign keys, "Referenced
  by"), `\d+`, `\di`, `\dn`, `\l`.
- **MySQL:** `mysql_async` (text and binary protocol, prepared statements, `mysql_native_password`).
- **SQL Server:** `tiberius` (batches, `sp_executesql` parameters, typed values, TLS for the login
  only, full TLS and no TLS) and Microsoft's `sqlcmd` 18 (ODBC Driver 18) from the
  `mssql/server:2022` image, with mandatory and optional encryption; TwinBase's own SQL Server
  client browses, edits, previews and migrates SQL Server mocks like real servers.
- **Redis:** `redis-rs` with RESP2 and RESP3 (`HELLO 3`), `AUTH`, pub/sub, `MULTI`/`EXEC`, raw RESP
  clients.
- **MongoDB:** the official Rust driver (CRUD, cursors with `getMore`, aggregation, indexes,
  SCRAM-SHA-1 and SCRAM-SHA-256) and `mongosh` 2 from the `mongo:7` image. The same client test suite
  runs against the mock and against a real MongoDB 7 and must give identical results.
- **DynamoDB:** boto3 1.43 (resource and client APIs, waiters, `batch_writer`, paginators) and
  TwinBase's own SigV4 client; the client test suite runs against the mock and against Amazon's
  DynamoDB Local 2.5 with identical results. The SigV4 verifier is checked against the worked
  example in the AWS documentation.
- **Firestore:** the official Python client `google-cloud-firestore` 2.34 through
  `FIRESTORE_EMULATOR_HOST` (filters, `Or`, `in`, `array_contains`, cursors, count/sum/avg,
  `Increment`/`ArrayUnion`/`SERVER_TIMESTAMP`, transactions, `on_snapshot` listeners, collection groups)
  and gRPC tests with the generated client.

SQL coverage is described in [engine.md](engine.md). Things a TwinLite mock does **not** do: TLS on
the mock port (SQL Server mocks excepted: TDS clients expect it), replication, stored procedures/triggers/PL code, extensions, and server-side cursors
beyond what drivers use for ordinary queries (a PostgreSQL mock on the real engine does all of these
except TLS and replication — see below). Error codes follow the real servers (SQLSTATE, MySQL error
numbers) so application error handling can be tested.

## SQL Server notes

A SQL Server mock speaks TDS 7.4 on top of TwinLite in T-SQL mode:

- **Login:** SQL authentication (any user and password, or the configured one; a wrong one gets error
  18456). TLS is negotiated as SQL Server does — the login only for `Encrypt=False`/`no`, the whole
  session for `Encrypt=True`/`mandatory`, none for clients without TLS. The certificate is
  self-signed, so clients need `TrustServerCertificate=True` (`-C` for `sqlcmd`). TDS 8 strict
  encryption (`Encrypt=Strict`) is not supported.
- **One database:** the mock serves a single database whatever name the client connects to (`USE`
  is accepted); `DB_NAME()` reports the requested name. Unqualified names resolve in `dbo`.
- **Requests:** SQL batches, RPC calls `sp_executesql`, `sp_prepare`, `sp_prepexec`, `sp_execute`
  and `sp_unprepare` (parameters of every common type are bound as typed values), transaction
  requests (`BEGIN/COMMIT/ROLLBACK` from drivers) and attention (cancel).
- **T-SQL:** statements without semicolons, `GO` in seeds, `TOP`, `OFFSET … FETCH`, `IDENTITY`
  (also `SET IDENTITY_INSERT` and explicit ids), `OUTPUT INSERTED/DELETED`, `N'…'` strings,
  string `+`, `@@ROWCOUNT`/`@@IDENTITY`/`SCOPE_IDENTITY()`, `GETDATE()`/`SYSDATETIME()`, `NEWID()`,
  `ISNULL`, `LEN`, `OBJECT_ID`, `SCHEMA_NAME`, `SERVERPROPERTY`, local variables (`DECLARE`, `SET`,
  `SELECT @v = …`), `IF … ELSE`, `WHILE`, `BEGIN … END`, `TRY … CATCH` with `ERROR_MESSAGE()` and
  friends, `THROW`, `RAISERROR`, `PRINT` (shown as messages), `RETURN`, dynamic SQL with `EXEC (…)`
  and `sp_executesql`, `ALTER TABLE … ALTER COLUMN`, named default constraints (`ADD CONSTRAINT …
  DEFAULT … FOR`, `DROP CONSTRAINT`), and the `sys.*` catalog views tools read (`sys.tables`,
  `sys.columns`, `sys.types`, `sys.indexes`, `sys.foreign_keys`, `sys.default_constraints`, …) next to
  `INFORMATION_SCHEMA`.
- **Errors** carry SQL Server numbers: 2627 (duplicate key), 547 (foreign key or check), 515 (NULL),
  208 (unknown object), 207 (unknown column), 102 (syntax), 137 (undeclared variable), 8134 (divide
  by zero), 1205 (deadlock), the number given to `THROW`. Fault rules take error numbers (`1205`,
  `40613`, …).
- **Not emulated:** stored procedures, functions and triggers you create, temporary tables
  (`#t`), table variables, cursors, `MERGE`, `GOTO`, query plans (`SET SHOWPLAN_*`), and collations
  (comparisons follow TwinLite's rules).

## Real PostgreSQL engine

A PostgreSQL mock can run on **Real PostgreSQL** instead of TwinLite (Engine in the mock dialog,
`"engine": "native"` in `mock.toml`, `twinbase mock postgres --native [BIN_DIR]`). TwinBase then
starts a real `postgres` server from a PostgreSQL installation on your computer — the folder set in
Settings › Data › PostgreSQL bin folder, `TWINBASE_PG_BIN`, or one found automatically (Homebrew,
Postgres.app, `PATH`, `/usr/lib/postgresql/*`, `/usr/pgsql-*`, the Windows installer); the newest
version wins. TwinBase does not ship PostgreSQL.

- The server listens only on a private unix socket (`listen_addresses` empty, socket directory with
  mode 0700), so nothing but TwinBase can reach it. On Windows (no unix sockets) it listens on
  127.0.0.1 at a random port without a password, so other programs on the same computer could reach
  it while the mock runs.
- The mock port is a protocol-aware proxy in front of it: it applies the mock's authentication (any
  credentials, or one user and password — clients send a cleartext password as with TwinLite mocks),
  creates missing roles and databases on first use, records every statement with its row count,
  duration and SQLSTATE in **Traffic**, and applies **faults** (errors are delivered in protocol order,
  so drivers and transactions stay consistent; the statement is not run).
- Everything PostgreSQL does works: PL/pgSQL, triggers, extensions shipped with the installation,
  exact error messages, `EXPLAIN ANALYZE`, `LISTEN/NOTIFY`, `COPY`.
- Data: persistent mocks keep the cluster in `mocks/<id>/data/pgdata` (ignored by Git; the seed is
  what you share). Reset deletes the cluster and runs the seed again. Clusters are created with
  `initdb` on first start (a few seconds), with `fsync` off for speed — a mock, not a production
  database. Stopping the mock stops the server (`pg_ctl stop -m fast`).
- Switching engines starts from the seed; data kept by the other engine is not converted.
- Clients connecting as `postgres` are the server's superuser, as with a local installation, so
  only trusted programs should connect. AI agents (MCP) are refused statements that could reach
  the computer from there: `COPY`, `DO`, `CALL`, `LOAD`, creating functions, procedures, languages,
  extensions, triggers or rules, `ALTER SYSTEM`/`ROLE`, and server-file or query-string functions
  (`pg_read_file`, `lo_export`, `query_to_xml`, `dblink`, …). Put extensions and functions in the
  seed instead.
- Bundles never carry the PostgreSQL cluster (its configuration can run programs), and imported
  mocks start on TwinLite until you switch them back.

## Redis notes

Strings, hashes, lists, sets, sorted sets, key expiry, `SCAN`/`KEYS` with patterns and types,
pub/sub and transactions are supported. Persistent Redis mocks write a JSON snapshot on change and on
stop; exports use the last snapshot.

## MongoDB notes

The MongoDB mock speaks the OP_MSG protocol (and the legacy OP_QUERY handshake) and reports itself as
MongoDB 7.0, a standalone server. Supported:

- **Commands:** `find`, `getMore`, `killCursors`, `insert`, `update` (including upserts, `arrayFilters`,
  the positional operators `$`, `$[]`, `$[name]` and pipeline-style updates), `delete`,
  `findAndModify`, `aggregate`, `count`, `distinct`, `create`, `drop`, `dropDatabase`,
  `renameCollection`, `listDatabases`, `listCollections`, `createIndexes`, `listIndexes`,
  `dropIndexes`, `collStats`, `dbStats`, `explain`, `buildInfo`, `serverStatus`, the handshake and
  session commands drivers send.
- **Queries:** comparison, logical, element, array (`$all`, `$elemMatch`, `$size`), `$regex`, `$mod`,
  `$type`, `$expr`, dotted paths through arrays, projections with `$slice` / `$elemMatch`, sorting
  with MongoDB's cross-type ordering.
- **Updates:** `$set`, `$unset`, `$inc`, `$mul`, `$min`, `$max`, `$rename`, `$setOnInsert`,
  `$currentDate`, `$push` (with `$each`, `$position`, `$slice`, `$sort`), `$addToSet`, `$pull`,
  `$pullAll`, `$pop`.
- **Aggregation:** `$match`, `$project`, `$addFields`/`$set`, `$unset`, `$group` (with `$sum`,
  `$avg`, `$min`, `$max`, `$first`, `$last`, `$push`, `$addToSet`, `$count`, `$mergeObjects`),
  `$sort`, `$skip`, `$limit`, `$count`, `$unwind`, `$lookup` (equality and pipeline forms),
  `$unionWith`, `$replaceRoot`/`$replaceWith`, `$sortByCount`, `$sample`, `$facet`, `$bucket`, `$out`
  and `$merge`, and about a hundred expression operators (arithmetic, string, array, date,
  conditional, type conversion, `$map`/`$filter`/`$reduce`, `$let`).
- **Indexes:** unique and sparse indexes are enforced (duplicate-key errors carry `keyPattern` and
  `keyValue` like the real server); other index types are stored and listed.

Not supported: transactions (a standalone MongoDB rejects them too), change streams, text and
geospatial search, collations, capped and time-series collections, and wire compression.

**Authentication.** With *User & password* the mock speaks SCRAM-SHA-256 and SCRAM-SHA-1. SCRAM
proves the password to both sides, so a mock cannot accept *any* password: in *accept any* mode
clients connect without credentials (`mongodb://127.0.0.1:27018/app`), and a client that sends some
gets an explanatory error.

**Seeds** are Extended JSON. Each collection is either an array of documents or an object with
`indexes` and `documents`:

```json
{
  "collections": {
    "users": {
      "indexes": [{ "key": { "email": 1 }, "name": "email_1", "unique": true }],
      "documents": [{ "_id": { "$oid": "65a1b2c3d4e5f60718293a4b" }, "email": "ann@example.com", "joined": { "$date": "2024-05-01T00:00:00Z" } }]
    },
    "orders": [{ "_id": 1, "user": "ann@example.com", "total": 25.5 }]
  }
}
```

Seeds are validated when saved. *Capture current data* writes the mock's database in canonical
Extended JSON, so types (`NumberLong`, dates, ObjectIds) survive. Persistent MongoDB mocks write a
BSON snapshot (`data/mongo.bson`) on change and on stop; an unreadable snapshot is moved aside, never
overwritten.

**Faults** match the traffic text, which is written like the shell: `db.orders.find({...})`,
`db.orders.insertOne({...})`, `db.orders.updateMany(...)`, `db.orders.aggregate([...])`. Error rules
take a MongoDB error code (for example `11000`, `112` write conflict, `50` time limit, `189` primary
stepped down — drivers retry that one).

## DynamoDB notes

The DynamoDB mock is an HTTP server for the JSON 1.0 protocol (`X-Amz-Target: DynamoDB_20120810.*`),
so SDKs and the AWS CLI work with `--endpoint-url http://127.0.0.1:<port>` (or
`AWS_ENDPOINT_URL_DYNAMODB`). It returns the same error types and messages as the service
(`ConditionalCheckFailedException`, `ValidationException`, `TransactionCanceledException` with
`CancellationReasons`, …) and an `x-amz-crc32` header that SDKs check.

- **Operations:** `CreateTable`, `DescribeTable`, `ListTables`, `UpdateTable` (indexes, billing,
  deletion protection), `DeleteTable`, `PutItem`, `GetItem`, `UpdateItem`, `DeleteItem`, `Query`,
  `Scan` (with `Segment`/`TotalSegments`), `BatchGetItem`, `BatchWriteItem`, `TransactWriteItems`,
  `TransactGetItems`, `UpdateTimeToLive`/`DescribeTimeToLive` (expired items are removed every few
  seconds), tags, `DescribeLimits`, `DescribeContinuousBackups`.
- **Expressions:** condition, filter, key-condition, projection and update expressions with every
  comparator, `BETWEEN`, `IN`, `AND`/`OR`/`NOT`, `attribute_exists`, `attribute_not_exists`,
  `attribute_type`, `begins_with`, `contains`, `size`; `SET` (with `+`/`-`, `if_not_exists`,
  `list_append`), `REMOVE`, `ADD`, `DELETE`; nested paths and list indexes.
- **Strictness on purpose**, so code that works here works on AWS: reserved words (`status`,
  `name`, `count`, `date`, … — the full list) must use `#name` placeholders; unused
  `ExpressionAttributeNames`/`Values` are rejected; key attributes are type-checked and cannot be
  updated; numbers keep DynamoDB's 38-digit precision; items are limited to 400 KB; `Query`/`Scan`
  pages stop at `Limit` items or 1 MB with `LastEvaluatedKey`; global secondary indexes are sparse
  and apply their projection; consistent reads on a GSI are refused.
- **Not supported:** legacy parameters (`KeyConditions`, `Expected`, `AttributeUpdates`, …), PartiQL
  (`ExecuteStatement`), streams, backups and global tables.

**Authentication.** In *accept any* mode every access key and secret works. With *User & password*
the user is the access key ID and the password the secret: requests must carry a valid AWS
Signature Version 4, exactly as on AWS.

**Seeds** list tables as `CreateTable` requests plus their items in DynamoDB JSON:

```json
{
  "tables": [{
    "TableName": "orders",
    "BillingMode": "PAY_PER_REQUEST",
    "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }],
    "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }, { "AttributeName": "sk", "AttributeType": "S" }],
    "TimeToLiveAttribute": "expires",
    "Items": [{ "pk": { "S": "customer#1" }, "sk": { "S": "order#001" }, "total": { "N": "25.5" } }]
  }]
}
```

Persistent DynamoDB mocks write `data/dynamodb.json` on change and on stop. **Faults** match
`<Operation> <table> …` (for example `/^PutItem orders/`) and take an exception name as the error
code — `ProvisionedThroughputExceededException` and `ThrottlingException` make SDKs back off and
retry, as they would on AWS.

## Firestore notes

The Firestore mock serves the `google.firestore.v1.Firestore` gRPC service, the protocol the
Firebase Admin SDKs, `google-cloud-firestore` libraries and the Firebase web/mobile SDKs (in their
emulator mode) use. Point them at it with `FIRESTORE_EMULATOR_HOST=127.0.0.1:<port>` or
`connectFirestoreEmulator(db, "127.0.0.1", <port>)`. Like a single emulator, every project id sees
the same data (database ids are separate).

- **Documents:** get, list, create (auto ids), set / update with field masks, delete, batch get,
  batch write, preconditions (`exists`, `update_time`), field transforms (`SERVER_TIMESTAMP`,
  `Increment`, `Maximum`, `Minimum`, `ArrayUnion`, `ArrayRemove`), 1 MiB document limit.
- **Queries:** `==`, `!=`, `<`, `<=`, `>`, `>=`, `in`, `not-in`, `array-contains`,
  `array-contains-any`, `is null`, `is NaN`, `and`/`or` composites, ordering with Firestore's
  cross-type value order (and the implicit ordering by inequality fields and document id),
  `startAt`/`startAfter`/`endAt`/`endBefore` cursors, offset, limit, field projections, collection
  groups, subcollection listing; `count`, `sum` and `avg` aggregations.
- **Transactions:** read-write transactions are optimistic: a commit fails with `ABORTED` when a
  document it read changed meanwhile, and the SDKs retry — the same contention behaviour as
  production. Read-only transactions refuse writes.
- **Realtime listeners:** `Listen` streams for document and query targets send the initial
  documents, `CURRENT`, then a change for every commit (added / modified / removed / deleted) with
  consistent snapshot markers, so `onSnapshot` works.
- **Not supported:** security rules (requests are not evaluated against rules; Admin-SDK
  behaviour), point-in-time reads (`read_time`), vector search, pipelines, the legacy streaming
  `Write` RPC, and partition queries (one partition).

**Seeds** list documents by path in plain JSON; special values use markers:

```json
{
  "documents": {
    "users/alice": { "name": "Alice", "joined": { "__timestamp": "2024-01-01T00:00:00Z" }, "best": { "__ref": "users/bob" } },
    "users/alice/orders/o1": { "total": 25.5, "where": { "__geo": [38.7, -9.1] } }
  }
}
```

(`{"__bytes": "<base64>"}` and `{"__double": "NaN"}` exist too.) Persistent Firestore mocks write
`data/firestore.json`. **Faults** match `GetDocument …`, `RunQuery users where …`, `Commit set …`,
`Listen …` and take a gRPC status as the error code (`UNAVAILABLE` and `ABORTED` are retried by the
SDKs; `RESOURCE_EXHAUSTED`, `PERMISSION_DENIED`, `DEADLINE_EXCEEDED`, …). A dropped connection ends the
call with `UNAVAILABLE`.

**Authentication.** In *accept any* mode every token works (SDKs send `Bearer owner` to emulators).
With *User & password*, requests must send the password as the bearer token.
