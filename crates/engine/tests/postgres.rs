//! End-to-end behaviour of the engine in PostgreSQL mode (no network).

use twinbase_engine::{Dialect, Engine, Outcome, Param, Session, SqlType};

fn session() -> (Engine, Session) {
    let engine = Engine::temporary(Dialect::Postgres).unwrap();
    let s = engine.session(Some("app"), "dev").unwrap();
    (engine, s)
}

fn run(s: &mut Session, sql: &str) -> Vec<Outcome> {
    let r = s.execute(sql);
    if let Some(e) = r.error {
        panic!("{sql}\n=> {e} {:?}", e.detail);
    }
    r.outcomes
}

fn err(s: &mut Session, sql: &str) -> twinbase_engine::EngineError {
    s.execute(sql).error.unwrap_or_else(|| panic!("expected error: {sql}"))
}

/// Rows as PostgreSQL text values.
fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = run(s, sql);
    let rs = out.last().unwrap().result.as_ref().expect("result set");
    rs.rows
        .iter()
        .map(|r| r.iter().zip(&rs.columns).map(|(v, c)| twinbase_engine::value::format_text(v, &c.ty, Dialect::Postgres)).collect())
        .collect()
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql)[0][0].clone().unwrap_or_else(|| "NULL".into())
}

const SCHEMA: &str = "
CREATE TABLE users (
  id SERIAL PRIMARY KEY,
  email VARCHAR(120) NOT NULL UNIQUE,
  name TEXT,
  active BOOLEAN NOT NULL DEFAULT true,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  score NUMERIC(8,2) DEFAULT 0,
  tags TEXT[],
  profile JSONB
);
CREATE TABLE orders (
  id BIGSERIAL PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  total NUMERIC(10,2) NOT NULL CHECK (total >= 0),
  placed_on DATE NOT NULL DEFAULT CURRENT_DATE
);
CREATE INDEX orders_user_idx ON orders (user_id);
";

#[test]
fn create_insert_select_with_types() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    let out = run(
        &mut s,
        "INSERT INTO users (email, name, tags, profile) VALUES ('a@x.io', 'Ann', '{admin,dev}', '{\"b\":1,\"a\":2}') RETURNING id, active, created_at",
    );
    let rs = out[0].result.as_ref().unwrap();
    assert_eq!(out[0].pg_tag(), "INSERT 0 1");
    assert_eq!(rs.columns[0].ty, SqlType::Int4);
    assert_eq!(rs.columns[1].ty, SqlType::Bool);
    assert_eq!(rs.columns[2].ty, SqlType::TimestampTz);
    let r = rows(&mut s, "SELECT id, email, active, tags, profile, score FROM users");
    assert_eq!(r[0][0].as_deref(), Some("1"));
    assert_eq!(r[0][2].as_deref(), Some("t"));
    assert_eq!(r[0][3].as_deref(), Some("{admin,dev}"));
    assert_eq!(r[0][4].as_deref(), Some(r#"{"a": 2, "b": 1}"#));
    assert_eq!(r[0][5].as_deref(), Some("0.00"));
    run(&mut s, "INSERT INTO users (email) VALUES ('b@x.io'), ('c@x.io')");
    assert_eq!(one(&mut s, "SELECT count(*) FROM users"), "3");
    assert_eq!(one(&mut s, "SELECT max(id) FROM users"), "3");
}

#[test]
fn constraint_errors_use_postgres_codes() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    run(&mut s, "INSERT INTO users (email) VALUES ('a@x.io')");
    let e = err(&mut s, "INSERT INTO users (email) VALUES ('a@x.io')");
    assert_eq!(e.sqlstate, "23505");
    assert_eq!(e.message, "duplicate key value violates unique constraint \"users_email_key\"");
    let e = err(&mut s, "INSERT INTO users (name) VALUES ('x')");
    assert_eq!(e.sqlstate, "23502");
    let e = err(&mut s, "INSERT INTO orders (user_id, total) VALUES (99, 10)");
    assert_eq!(e.sqlstate, "23503");
    assert!(e.message.contains("orders_user_id_fkey"), "{}", e.message);
    let e = err(&mut s, "INSERT INTO orders (user_id, total) VALUES (1, -5)");
    assert_eq!(e.sqlstate, "23514");
    assert!(e.message.contains("orders_total_check"), "{}", e.message);
    let e = err(&mut s, "INSERT INTO users (email, active) VALUES ('z@x.io', 'maybe')");
    assert_eq!(e.sqlstate, "22P02");
    let e = err(&mut s, "SELECT * FROM nope");
    assert_eq!(e.sqlstate, "42P01");
    assert_eq!(e.message, "relation \"nope\" does not exist");
    let e = err(&mut s, "SELECT nope FROM users");
    assert_eq!(e.sqlstate, "42703");
    let e = err(&mut s, "INSERT INTO users (email) VALUES ('".to_owned().as_str());
    assert_eq!(e.sqlstate, "42601");
}

#[test]
fn transactions_and_failed_state() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    run(&mut s, "BEGIN");
    run(&mut s, "INSERT INTO users (email) VALUES ('t@x.io')");
    let e = err(&mut s, "INSERT INTO users (email) VALUES ('t@x.io')");
    assert_eq!(e.sqlstate, "23505");
    let e = err(&mut s, "SELECT 1");
    assert_eq!(e.sqlstate, "25P02");
    let out = run(&mut s, "COMMIT");
    assert_eq!(out[0].tag, "ROLLBACK");
    assert_eq!(one(&mut s, "SELECT count(*) FROM users"), "0");
    // savepoints recover
    run(&mut s, "BEGIN; INSERT INTO users (email) VALUES ('s@x.io'); SAVEPOINT a;");
    err(&mut s, "INSERT INTO users (email) VALUES ('s@x.io')");
    run(&mut s, "ROLLBACK TO SAVEPOINT a; COMMIT");
    assert_eq!(one(&mut s, "SELECT count(*) FROM users"), "1");
    // implicit transaction for multi-statement batches
    let r = s.execute("INSERT INTO users (email) VALUES ('m1@x.io'); INSERT INTO users (email) VALUES ('s@x.io')");
    assert!(r.error.is_some());
    assert_eq!(one(&mut s, "SELECT count(*) FROM users"), "1");
}

#[test]
fn foreign_key_cascade_and_joins() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    run(&mut s, "INSERT INTO users (email, name) VALUES ('a@x.io', 'Ann'), ('b@x.io', 'Bob')");
    run(&mut s, "INSERT INTO orders (user_id, total) VALUES (1, 10.5), (1, 20), (2, 7.25)");
    let r = rows(
        &mut s,
        "SELECT u.name, count(o.id) AS n, sum(o.total) AS total FROM users u LEFT JOIN orders o ON o.user_id = u.id GROUP BY u.name ORDER BY u.name",
    );
    assert_eq!(
        r,
        vec![
            vec![Some("Ann".into()), Some("2".into()), Some("30.50".into())],
            vec![Some("Bob".into()), Some("1".into()), Some("7.25".into())],
        ]
    );
    run(&mut s, "DELETE FROM users WHERE email = 'a@x.io'");
    assert_eq!(one(&mut s, "SELECT count(*) FROM orders"), "1");
}

#[test]
fn upsert_returning_and_update_from() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    run(&mut s, "INSERT INTO users (email, name) VALUES ('a@x.io', 'Ann')");
    let r = rows(
        &mut s,
        "INSERT INTO users (email, name) VALUES ('a@x.io', 'Annie') ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name RETURNING name",
    );
    assert_eq!(r[0][0].as_deref(), Some("Annie"));
    run(&mut s, "INSERT INTO users (email) VALUES ('a@x.io') ON CONFLICT DO NOTHING");
    run(&mut s, "INSERT INTO orders (user_id, total) VALUES (1, 5)");
    run(&mut s, "UPDATE orders SET total = total * 2 FROM users WHERE users.id = orders.user_id AND users.name = 'Annie'");
    assert_eq!(one(&mut s, "SELECT total FROM orders"), "10.00");
    let out = run(&mut s, "UPDATE users SET active = 'false' WHERE email = 'a@x.io'");
    assert_eq!(out[0].pg_tag(), "UPDATE 1");
    assert_eq!(one(&mut s, "SELECT active FROM users"), "f");
    assert_eq!(one(&mut s, "SELECT count(*) FROM users WHERE active = false"), "1");
    assert_eq!(one(&mut s, "SELECT count(*) FROM users WHERE active = 'f'"), "1");
}

#[test]
fn expressions_and_functions() {
    let (_e, mut s) = session();
    assert_eq!(one(&mut s, "SELECT 1 + 1"), "2");
    assert_eq!(one(&mut s, "SELECT 'abc' ILIKE 'A%'"), "t");
    assert_eq!(one(&mut s, "SELECT 'abc' LIKE 'A%'"), "f");
    assert_eq!(one(&mut s, "SELECT upper('x') || lower('Y')"), "Xy");
    assert_eq!(one(&mut s, "SELECT '42'::int + 1"), "43");
    assert_eq!(one(&mut s, "SELECT CAST('2024-01-31' AS date) + 1"), "2024-02-01");
    assert_eq!(one(&mut s, "SELECT '2024-01-31 10:00:00'::timestamp + interval '1 day 2 hours'"), "2024-02-01 12:00:00");
    assert_eq!(one(&mut s, "SELECT date_trunc('month', '2024-03-15 10:11:12'::timestamp)"), "2024-03-01 00:00:00");
    assert_eq!(one(&mut s, "SELECT extract(year from '2024-03-15'::date)"), "2024");
    assert_eq!(one(&mut s, "SELECT coalesce(NULL, 'x')"), "x");
    assert_eq!(one(&mut s, "SELECT greatest(1, NULL, 3)"), "3");
    assert_eq!(one(&mut s, "SELECT 2 = ANY(ARRAY[1,2,3])"), "t");
    assert_eq!(one(&mut s, "SELECT '{\"a\":{\"b\":1}}'::jsonb -> 'a' ->> 'b'"), "1");
    assert_eq!(one(&mut s, "SELECT '{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":1}'"), "t");
    assert_eq!(one(&mut s, "SELECT 'foo123' ~ '[0-9]+$'"), "t");
    assert_eq!(one(&mut s, "SELECT length(gen_random_uuid()::text)"), "36");
    assert_eq!(one(&mut s, "SELECT split_part('a,b,c', ',', 2)"), "b");
    assert_eq!(one(&mut s, "SELECT string_agg(x, '-') FROM (VALUES ('a'), ('b')) AS t(x)"), "a-b");
    let r = run(&mut s, "SELECT count(*), now(), 'x'::text AS label");
    let cols: Vec<_> = r[0].result.as_ref().unwrap().columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
    assert_eq!(cols[0], ("count".into(), SqlType::Int8));
    assert_eq!(cols[1], ("now".into(), SqlType::TimestampTz));
    assert_eq!(cols[2], ("label".into(), SqlType::Text));
    assert_eq!(rows(&mut s, "SELECT 1 AS a WHERE false"), Vec::<Vec<Option<String>>>::new());
}

#[test]
fn settings_and_catalogs() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    assert_eq!(one(&mut s, "SHOW server_version"), "16.4");
    run(&mut s, "SET application_name = 'tests'");
    assert_eq!(one(&mut s, "SELECT current_setting('application_name')"), "tests");
    assert!(one(&mut s, "SELECT version()").starts_with("PostgreSQL 16"));
    assert_eq!(one(&mut s, "SELECT current_schema()"), "public");
    assert_eq!(one(&mut s, "SELECT current_database()"), "app");
    let r = rows(&mut s, "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name");
    assert_eq!(r, vec![vec![Some("orders".into())], vec![Some("users".into())]]);
    let r = rows(
        &mut s,
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = 'users' ORDER BY ordinal_position LIMIT 2",
    );
    assert_eq!(r[1], vec![Some("email".into()), Some("character varying".into()), Some("NO".into())]);
    let r = rows(
        &mut s,
        "SELECT c.relname FROM pg_catalog.pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public' AND c.relkind = 'r' ORDER BY 1",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(one(&mut s, "SELECT 'users'::regclass::oid = (SELECT oid FROM pg_class WHERE relname = 'users')"), "t");
    let r = rows(
        &mut s,
        "SELECT kcu.column_name, ccu.table_name FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = tc.constraint_name JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name WHERE tc.constraint_type = 'FOREIGN KEY'",
    );
    assert_eq!(r, vec![vec![Some("user_id".into()), Some("users".into())]]);
}

#[test]
fn alter_table_operations() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    run(&mut s, "INSERT INTO users (email) VALUES ('a@x.io'), ('b@x.io')");
    run(&mut s, "INSERT INTO orders (user_id, total) VALUES (1, 3)");
    run(&mut s, "ALTER TABLE users ADD COLUMN nickname VARCHAR(20) NOT NULL DEFAULT 'anon'");
    assert_eq!(one(&mut s, "SELECT nickname FROM users WHERE id = 1"), "anon");
    run(&mut s, "ALTER TABLE users RENAME COLUMN nickname TO handle");
    run(&mut s, "ALTER TABLE users ALTER COLUMN handle DROP DEFAULT");
    let e = err(&mut s, "ALTER TABLE users ALTER COLUMN name SET NOT NULL");
    assert_eq!(e.sqlstate, "23502");
    run(&mut s, "UPDATE users SET name = 'n'");
    run(&mut s, "ALTER TABLE users ALTER COLUMN name SET NOT NULL");
    let e = err(&mut s, "INSERT INTO users (email, handle) VALUES ('c@x.io', 'h')");
    assert_eq!(e.sqlstate, "23502");
    run(&mut s, "ALTER TABLE users ADD CONSTRAINT users_handle_len CHECK (length(handle) > 1)");
    run(&mut s, "ALTER TABLE users DROP COLUMN score");
    run(&mut s, "ALTER TABLE orders ALTER COLUMN total TYPE INTEGER");
    assert_eq!(one(&mut s, "SELECT total FROM orders"), "3");
    // parent rename keeps the foreign key and its data
    run(&mut s, "ALTER TABLE users RENAME TO members");
    assert_eq!(one(&mut s, "SELECT count(*) FROM members"), "2");
    let e = err(&mut s, "INSERT INTO orders (user_id, total) VALUES (42, 1)");
    assert_eq!(e.sqlstate, "23503");
    // dropping a referenced table requires CASCADE and never deletes child rows
    let e = err(&mut s, "DROP TABLE members");
    assert_eq!(e.sqlstate, "2BP01");
    run(&mut s, "DROP TABLE members CASCADE");
    assert_eq!(one(&mut s, "SELECT count(*) FROM orders"), "1");
    run(&mut s, "INSERT INTO orders (user_id, total) VALUES (42, 1)");
}

#[test]
fn extended_protocol_prepare_and_params() {
    let (_e, mut s) = session();
    run(&mut s, SCHEMA);
    let p = s.prepare("INSERT INTO users (email, active, created_at) VALUES ($1, $2, $3) RETURNING id").unwrap();
    assert_eq!(p.param_types, vec![SqlType::Varchar { len: Some(120) }, SqlType::Bool, SqlType::TimestampTz]);
    assert_eq!(p.columns[0].ty, SqlType::Int4);
    s.execute_prepared(&p, &[Param::Text("p@x.io".into()), Param::Bool(true), Param::Text("2024-05-01T10:00:00Z".into())]).unwrap();
    let q = s.prepare("SELECT email, created_at FROM users WHERE id = $1 AND created_at >= $2 LIMIT $3").unwrap();
    assert_eq!(q.param_types, vec![SqlType::Int4, SqlType::TimestampTz, SqlType::Int8]);
    let out = s.execute_prepared(&q, &[Param::Int(1), Param::Text("2024-05-01 00:00:00+00".into()), Param::Text("5".into())]).unwrap();
    let rs = out.result.unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        twinbase_engine::value::format_text(&rs.rows[0][1], &rs.columns[1].ty, Dialect::Postgres).unwrap(),
        "2024-05-01 10:00:00+00"
    );
}

#[test]
fn schemas_enums_views_and_persistence() {
    let engine = Engine::temporary(Dialect::Postgres).unwrap();
    {
        let mut s = engine.session(Some("app"), "dev").unwrap();
        run(&mut s, "CREATE SCHEMA billing");
        run(&mut s, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
        run(&mut s, "CREATE TABLE billing.invoices (id serial primary key, m mood, amount numeric(6,2))");
        run(&mut s, "INSERT INTO billing.invoices (m, amount) VALUES ('happy', 12.5)");
        let e = err(&mut s, "INSERT INTO billing.invoices (m) VALUES ('angry')");
        assert_eq!(e.message, "invalid input value for enum mood: \"angry\"");
        run(&mut s, "CREATE VIEW billing.big AS SELECT id, amount FROM billing.invoices WHERE amount > 10");
        assert_eq!(one(&mut s, "SELECT count(*) FROM billing.big"), "1");
        run(&mut s, "SET search_path TO billing, public");
        assert_eq!(one(&mut s, "SELECT amount FROM invoices"), "12.50");
    }
    // A new session sees committed state and continues sequences.
    let mut s = engine.session(Some("app"), "dev").unwrap();
    run(&mut s, "INSERT INTO billing.invoices (m, amount) VALUES ('sad', 1)");
    assert_eq!(one(&mut s, "SELECT max(id) FROM billing.invoices"), "2");
    // Other sessions observe DDL committed by this one.
    let mut other = engine.session(Some("app"), "dev").unwrap();
    run(&mut s, "CREATE TABLE public.late (x int)");
    run(&mut other, "INSERT INTO late VALUES (1)");
}

#[test]
fn long_non_ascii_names_and_division_by_zero() {
    let engine = Engine::temporary(Dialect::Postgres).unwrap();
    let mut s = engine.session(None, "postgres").unwrap();
    // Generated constraint names are cut to 63 bytes on a character boundary.
    let table = "ü".repeat(40);
    let r = s.execute(&format!("CREATE TABLE \"{table}\" (\"{table}\" int UNIQUE, n int CHECK (n > 0))"));
    assert!(r.error.is_none(), "{:?}", r.error);
    let r = s.execute("SELECT 1 / 0");
    assert_eq!(r.error.unwrap().sqlstate, "22012");
    let r = s.execute("SELECT 7 % 0");
    assert_eq!(r.error.unwrap().sqlstate, "22012");
}

#[test]
fn read_only_transactions_refuse_writes() {
    let engine = Engine::temporary(Dialect::Postgres).unwrap();
    let mut s = engine.session(None, "postgres").unwrap();
    assert!(s.execute("CREATE TABLE t (id serial PRIMARY KEY, v text)").error.is_none());
    assert!(s.execute("BEGIN TRANSACTION READ ONLY").error.is_none());
    let e = s.execute("INSERT INTO t (v) VALUES ('x')").error.unwrap();
    assert_eq!(e.sqlstate, "25006", "{e}");
    assert!(s.execute("ROLLBACK").error.is_none());
    s.execute("BEGIN READ ONLY");
    // Catalog views still work (they are rebuilt in memory).
    let r = s.execute("SELECT count(*) FROM information_schema.tables");
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(s.execute("CREATE TABLE u (id int)").error.unwrap().sqlstate, "25006");
    s.execute("COMMIT");
    // Writes work again afterwards.
    assert!(s.execute("INSERT INTO t (v) VALUES ('y')").error.is_none());
}

#[test]
fn create_table_as_keeps_column_types() {
    let engine = Engine::temporary(Dialect::Postgres).unwrap();
    let mut s = engine.session(None, "postgres").unwrap();
    assert!(s.execute("CREATE TABLE big AS SELECT g AS id FROM generate_series(1, 200) g").error.is_none());
    let r = s.execute("SELECT count(*) FROM big WHERE id <= 42");
    let rs = r.outcomes.last().unwrap().result.as_ref().unwrap();
    assert_eq!(rs.rows[0][0], twinbase_engine::Value::Integer(42));
    let r = s.execute("SELECT data_type FROM information_schema.columns WHERE table_name = 'big'");
    let rs = r.outcomes.last().unwrap().result.as_ref().unwrap();
    assert_eq!(rs.rows[0][0], twinbase_engine::Value::Text("integer".into()));
}
