# Development and testing

## Prerequisites

Rust (stable, edition 2024, 1.90+) and Node.js 22+. Docker is optional (tests against real databases).

- **macOS:** Xcode command line tools (`xcode-select --install`).
- **Windows:** Visual Studio Build Tools with "Desktop development with C++", and WebView2 (already on
  Windows 10 and 11).
- **Linux (Debian/Ubuntu):** `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev patchelf file`.

## Build

```sh
cargo build                       # all library crates and the CLI
cd apps/desktop && npm ci
npx tauri dev                     # desktop app with hot reload
npm run package                   # installers for this system, with the twinbase CLI inside
npm run package -- --bundles app  # macOS: just TwinBase.app
cargo build --release -p twinbase-cli   # the command line alone
```

The root `Cargo.toml` makes `cargo build`/`cargo test` skip the desktop shell by default; build it
with the Tauri CLI or `cargo build -p twinbase-desktop`.

## Tests

```sh
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings   # what CI checks first
cargo test                                               # engine, mocks, client, core, CLI
TWINBASE_REAL_PG_PORT=55432 cargo test -p twinbase-client # also run the client suite against real PostgreSQL
cd apps/desktop && npm test && npm run typecheck          # UI unit tests (Markdown, SQL classification, DDL generator)
```

What the suites cover:

- **Engine:** PostgreSQL and MySQL semantics, types, constraints and error codes, transactions,
  ALTER TABLE, catalogs, persistence.
- **Mocks:** real drivers over TCP (`tokio-postgres`, `mysql_async`, `redis-rs`), authentication,
  persistence, RESP3, pub/sub. `psql` was verified manually against the catalogs.
- **Client:** one suite run against the PostgreSQL mock, the MySQL mock, a SQLite file, the Redis
  mock and (optionally) real PostgreSQL — introspection, paging, staged edits, rollback rules.
- **Core:** end-to-end API flows — mocks, queries, preview with cascades, guarded commits, sample
  data, seeds and reset, scenarios, bundles (including re-import port handling), agent access
  rules, MCP over HTTP with token and origin checks.

## Running the UI in a browser

Useful for UI work and browser automation:

```sh
TWINBASE_HOME=/tmp/tb-home cargo run -p twinbase-cli -- serve --workspace /tmp/tb-ws --port 47900 --dev-api
cd apps/desktop && npx vite
# open http://127.0.0.1:5173/?token=<token printed by serve>&core=47900
```

Native file dialogs become path prompts in this mode.

## Conventions

- One API (`twinbase_core::api::call`) for UI, MCP and CLI; add features there first.
- Every mutation is recorded as Activity with its actor.
- Secrets never go into workspace files or bundles.
- Code is formatted with `cargo fmt` (`rustfmt.toml`) and TypeScript is `strict`.

## Continuous integration and releases

Every pull request and push to `main` runs `.github/workflows/ci.yml`: formatting, Clippy and the
tests on Linux, the tests on Windows, the UI typecheck, unit tests and build, and a dependency
review on pull requests. Tests that need real databases (Docker containers, emulators, a
PostgreSQL installation) run only when their `TWINBASE_*` environment variables are set, so CI
covers the engine, every mock with real drivers, the client against the mocks and the core API.
Installers are built only for releases; see [releasing.md](releasing.md).
