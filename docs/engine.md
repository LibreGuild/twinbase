# TwinLite engine

TwinLite (`crates/engine`) runs PostgreSQL-, MySQL- and SQL Server (T-SQL)-flavoured SQL on SQLite. It is what makes mock
servers small (a few MB of memory) while behaving like the real servers for typical application and
ORM workloads.

## Storage

- **PostgreSQL dialect:** each database is a directory; each schema is a SQLite file
  (`<db>/<schema>.sqlite`) attached to the session, so `schema.table` names work as in PostgreSQL.
- **MySQL dialect:** each database is one SQLite file.
- **SQL Server dialect:** one database (whatever name clients use); each schema (`dbo`, …) is a
  SQLite file, and names match case-insensitively as with SQL Server's default collation.
- Table definitions that SQLite cannot express (exact column types, enums, view definitions, index
  predicates) are kept as JSON in a `_tb_meta` table; sequences in `_tb_seq`.
- WAL mode, `BEGIN IMMEDIATE` transactions, and an engine-wide sequence registry (sequences are
  non-transactional, as in PostgreSQL).

## Translation

Statements are parsed with `sqlparser` in the client's dialect, rewritten (name resolution, function
mapping, casts through a `tb_cast(value, type)` function, set-returning functions, array subscripts,
`RETURNING`, upserts) and executed by SQLite. Utility statements (`SET`, `SHOW`, `BEGIN`, `USE`,
`DESCRIBE`, MySQL `SHOW …`) are handled directly.

## What works (tested)

PostgreSQL dialect:

- `CREATE/ALTER/DROP TABLE` (add/drop/rename columns, change types, defaults, `NOT NULL`,
  constraints), indexes (unique, partial), views, schemas, enums (`CREATE TYPE … AS ENUM`),
  `TRUNCATE`.
- `serial`/`bigserial`/identity, sequences, `nextval`/`currval`/`setval`.
- Types: integers, `numeric(p,s)` (scale kept), floats, `boolean`, text types, `uuid`, `date`,
  `time`, `timestamp[tz]`, `interval`, `json`/`jsonb` (`->`, `->>`), `bytea`, arrays.
- `INSERT … ON CONFLICT … DO UPDATE/NOTHING`, `RETURNING`, `UPDATE … FROM`, CTEs, joins, aggregates
  (`string_agg`, …), window functions, `generate_series`, `unnest`, `ARRAY(subquery)`.
- Foreign keys with `ON DELETE CASCADE/SET NULL/RESTRICT`, unique and check constraints, with
  PostgreSQL SQLSTATE codes (`23505`, `23503`, `23502`, `42P01`, …).
- Transactions including the failed-transaction state (`25P02`) and savepoints.
- `pg_catalog` and `information_schema` views sufficient for `psql` meta-commands, ORMs and
  introspection tools; `regclass` casts.

MySQL dialect:

- `AUTO_INCREMENT`, `LAST_INSERT_ID()`, `ON DUPLICATE KEY UPDATE`, `ON UPDATE CURRENT_TIMESTAMP`,
  `SHOW TABLES/DATABASES/COLUMNS/CREATE TABLE/VARIABLES`, `DESCRIBE`, `USE`, autocommit and implicit
  commit on DDL, MySQL error numbers (`1062`, `1452`, `1146`, …).

SQL Server (T-SQL) dialect:

- Batches without semicolons and with `GO`, local variables, `IF`/`WHILE`/`BEGIN … END`,
  `TRY … CATCH`, `THROW`/`RAISERROR`/`PRINT`/`RETURN`, dynamic SQL (`EXEC (…)`, `sp_executesql`).
  Control flow runs statement by statement in `tsql.rs`; loops stop after 100,000 iterations.
- `IDENTITY`, `TOP`, `OFFSET … FETCH`, `OUTPUT INSERTED/DELETED` (as `RETURNING`), `N'…'`, string
  `+`, T-SQL functions and `@@` variables, `COUNT` as `int`, unnamed computed columns.
- `sys.*` catalog views built from the catalog, named default constraints, `ALTER COLUMN`.
- SQL Server error numbers, derived from the SQLSTATE (see `tsql::error_number`).

Division by zero fails (SQLSTATE `22012`) in the PostgreSQL and SQL Server dialects, as on the real
servers; MySQL returns NULL.

## Limits

TwinLite is for prototyping and tests, not production storage. Not emulated: stored
procedures/functions written in PL languages, triggers, extensions (PostGIS, pgvector…), row-level
security, `LISTEN/NOTIFY`, full-text search operators, and exact query plans (`EXPLAIN` output is
TwinLite's own). Performance characteristics are SQLite's.
