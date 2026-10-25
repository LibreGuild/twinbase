# Workspaces and bundles

## Workspace folder

A workspace is a folder of plain files designed to be committed to Git and shared:

```text
<workspace>/
  twinbase.toml                      workspace manifest (name, format version)
  .gitignore                         ignores mock data by default
  connections/<id>.toml              saved connections — never passwords
  mocks/<id>/mock.toml               mock server definition
  mocks/<id>/seed.sql                seed script (SQL, or Redis commands one per line)
  mocks/<id>/seed.json               seed of a MongoDB (Extended JSON), DynamoDB (tables) or Firestore (documents) mock
  mocks/<id>/data/                   mock data when "Keep data" is on
  queries/<collection>/_collection.toml   collection name and Markdown docs
  queries/<collection>/<id>.toml          saved query: SQL, docs, tags, connection, expectation
  snapshots/<id>.json                     schema snapshot (tables, columns, keys, indexes)
```

A good place is `.twinbase/` inside your project repository: the team then shares mock definitions,
seeds and documented queries with the code. Passwords, the MCP token and settings stay in the app
directory on each computer.

Example saved query:

```toml
id = "top-rated-products"
name = "Top rated products"
collection = "product-insights"
connection = "mock:e-commerce-db"
tags = ["reports"]
docs = """
## Purpose
Products ranked by average review rating.
"""
sql = """
SELECT p.name, round(avg(r.rating), 2) AS avg_rating
FROM products p JOIN reviews r ON r.product_id = p.id
GROUP BY p.name ORDER BY avg_rating DESC LIMIT 10;
"""

[expect]
kind = "rows"
count = 10
```

## Expectations and scenarios

A saved query can carry an expectation: `rows` (exact count), `empty`, `succeeds`, or `fails`
(optionally with an error code such as `23505`). **Run scenarios** on a collection executes every
query and reports pass/fail — a lightweight test suite for a schema or a backend's data assumptions.
Queries without an expectation pass when they succeed.

## Schema snapshots and migrations

**Schema snapshots** (Command palette → *Schema snapshots and diff*, the diagram's **Compare**
button, or the `snapshot_schema` MCP tool) save a schema's structure as JSON. **Compare** any two
sources — snapshots or live connections, even across PostgreSQL/MySQL/SQLite — to get:

- a change list per table (tables, columns, types, nullability, defaults, primary keys, foreign
  keys, indexes);
- an **up** migration that turns *From* into *To*, and a **down** migration that reverses it, in the
  dialect of the *From* side or a dialect you choose;
- warnings for data loss (dropped tables/columns), likely renames (drop + add of the same shape —
  edit the script to `RENAME` to keep data), and `NOT NULL` columns without defaults.

Statements are ordered so references stay valid (drop old foreign keys and indexes, create new
tables parents first, alter columns, add indexes and foreign keys, drop tables children first).
For SQLite, changes `ALTER TABLE` cannot make are written as the standard table rebuild (create,
copy, drop, rename) in a transaction with foreign keys off. Open a script in the SQL editor to
preview it on a local twin before running it.

## Bundles (`.twinbase`)

A bundle is a zip file with a `manifest.json` and the selected parts of a workspace:

```text
manifest.json                    format, version, creation time, source workspace, contents
mocks/<id>/mock.toml, seed.sql, data/…
queries/<collection>/…
connections/<id>.toml            only when selected; never passwords
snapshots/<id>.json              schema snapshots, when selected
```

- SQL mock data is snapshotted consistently with SQLite `VACUUM INTO`, even while the mock runs.
- Importing never overwrites: items whose ids exist get new ids (mocks are renamed
  "… (imported)"), and mocks whose ports are already used in the workspace get a free port.
