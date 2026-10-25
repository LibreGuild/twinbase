# CLI

The `twinbase` binary runs TwinBase without a window. It ships inside the desktop bundle and builds
with `cargo build --release -p twinbase-cli`.

| Command | What it does |
| --- | --- |
| `twinbase mock <postgres\|mysql\|mssql\|redis\|mongodb\|dynamodb\|firestore> [--port N] [--host H] [--data DIR] [--seed FILE] [--database NAME] [--user U --password P] [--native [BIN_DIR]]` | Start one mock server (`--native`: a PostgreSQL mock on a real PostgreSQL installation). Prints `{"kind","address","port"}` as JSON on the first line (port 0 = pick a free one). Without `--data` the data is temporary. The seed runs only when the data is new. |
| `twinbase serve [--workspace DIR] [--port N] [--all-mocks] [--dev-api]` | Run the core headless: auto-start mocks (or all), serve MCP. `--dev-api` also exposes the JSON API for the browser UI. |
| `twinbase mcp [--url URL] [--token T]` | stdio MCP bridge to the running app (what agents launch). |
| `twinbase mcp-setup` | Print configuration for supported agents. |
| `twinbase export -o FILE [--mock ID]… [--collection ID]… [--workspace DIR] [--no-data]` | Write a `.twinbase` bundle. |
| `twinbase import FILE [--workspace DIR]` | Import a bundle. |

`TWINBASE_HOME` overrides the app directory (settings, secrets), useful for CI and tests. Logs go to
stderr (`RUST_LOG=info` for more).

## Mocks in tests and CI

```sh
twinbase mock postgres --port 55433 --seed db/schema.sql > mock.json &
mock_pid=$!
until [ -s mock.json ]; do sleep 0.1; done          # first line = {"kind","address","port"}
DATABASE_URL="postgres://test:test@127.0.0.1:55433/app" npm test
kill $mock_pid
```

Use `--port 0` and read `port` from the JSON line when several jobs share a machine.

MongoDB works the same way; the seed is Extended JSON (see [mock-servers.md](mock-servers.md)):

```sh
twinbase mock mongodb --port 0 --database shop --seed test/seed.json > mock.json &
MONGODB_URI="mongodb://127.0.0.1:$(jq .port mock.json)/shop" npm test
```

And DynamoDB (seed = `{"tables": [...]}`); with `--user/--password` requests must be signed with
that access key ID and secret:

```sh
twinbase mock dynamodb --port 0 --seed test/tables.json > mock.json &
AWS_ENDPOINT_URL_DYNAMODB="http://127.0.0.1:$(jq .port mock.json)" AWS_REGION=us-east-1 \
  AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test npm test
```

And Firestore (seed = `{"documents": {...}}`), for SDKs in emulator mode:

```sh
twinbase mock firestore --port 8080 --seed test/documents.json &
FIRESTORE_EMULATOR_HOST=127.0.0.1:8080 GCLOUD_PROJECT=demo-test npm test
```
