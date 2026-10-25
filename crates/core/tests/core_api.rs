//! End-to-end checks of the core API: workspaces, mocks, queries, preview, sample data,
//! scenarios, bundles, access control and the MCP endpoint.

use std::sync::Arc;

use serde_json::{Value, json};
use twinbase_core::api::call;
use twinbase_core::{Actor, App};

async fn app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().join("home"), Some(dir.path().join("ws"))).unwrap();
    (dir, app)
}

async fn ok(app: &Arc<App>, method: &str, params: Value) -> Value {
    match call(app, method, params, &Actor::User).await {
        Ok(v) => v,
        Err(e) => panic!("{method}: {e:?}"),
    }
}

/// A port that is free right now (tests run in parallel, so automatic suggestions could collide).
/// A port outside the OS's ephemeral range: a port the OS hands out for `:0` can be taken by an
/// outgoing connection before the mock binds it, which made parallel tests flaky.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let _ = NEXT.compare_exchange(0, 20_000 + (std::process::id() % 1000) as u16 * 20, Ordering::SeqCst, Ordering::SeqCst);
    loop {
        let p = NEXT.fetch_add(1, Ordering::SeqCst);
        if p >= 45_000 {
            NEXT.store(20_000, Ordering::SeqCst);
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

async fn create_pg_mock(app: &Arc<App>) -> String {
    let m = ok(app, "mock.save", json!({ "mock": { "id": "", "name": "Shop DB", "kind": "postgres", "port": free_port() } })).await;
    let id = m["id"].as_str().unwrap().to_owned();
    ok(app, "mock.start", json!({ "id": id })).await;
    format!("mock:{id}")
}

#[tokio::test(flavor = "multi_thread")]
async fn mock_lifecycle_queries_preview_and_generation() {
    let (_d, app) = app().await;
    let conn = create_pg_mock(&app).await;
    ok(&app, "db.query", json!({ "connection": conn, "sql": "
        CREATE TABLE customers (id serial PRIMARY KEY, email text NOT NULL UNIQUE, name text, city text);
        CREATE TABLE orders (id serial PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total numeric(10,2) NOT NULL, status text DEFAULT 'new');
    " })).await;
    let generated = ok(&app, "db.generate", json!({ "connection": conn, "table": "customers", "count": 30 })).await;
    assert_eq!(generated["inserted"], 30);
    let generated = ok(&app, "db.generate", json!({ "connection": conn, "table": "orders", "count": 80 })).await;
    assert_eq!(generated["inserted"], 80);
    let r = ok(
        &app,
        "db.query",
        json!({ "connection": conn, "sql": "SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id" }),
    )
    .await;
    assert_eq!(r[0]["rows"][0][0], 80);

    // Preview a delete on the twin: the cascade shows up, nothing changes for real.
    let target: i64 = ok(&app, "db.query", json!({ "connection": conn, "sql": "SELECT customer_id FROM orders LIMIT 1" })).await[0]["rows"]
        [0][0]
        .as_i64()
        .unwrap();
    let preview = ok(&app, "db.preview", json!({ "connection": conn, "sql": format!("DELETE FROM customers WHERE id = {target}") })).await;
    assert!(preview["ok"].as_bool().unwrap(), "{preview}");
    let tables = preview["tables"].as_array().unwrap();
    let customers = tables.iter().find(|t| t["table"] == "customers").unwrap();
    assert_eq!(customers["removed"].as_array().unwrap().len(), 1);
    let orders = tables.iter().find(|t| t["table"] == "orders").unwrap();
    assert!(!orders["removed"].as_array().unwrap().is_empty(), "cascade visible in preview");
    let still: i64 =
        ok(&app, "db.query", json!({ "connection": conn, "sql": format!("SELECT count(*) FROM customers WHERE id = {target}") })).await[0]
            ["rows"][0][0]
            .as_i64()
            .unwrap();
    assert_eq!(still, 1);

    // Update preview shows changed cells; commit is guarded by the expected count.
    let preview =
        ok(&app, "db.preview", json!({ "connection": conn, "sql": "UPDATE orders SET status = 'shipped' WHERE total > 100" })).await;
    let expected = preview["expected_affected"].as_u64().unwrap();
    let committed = ok(
        &app,
        "db.commit",
        json!({ "connection": conn, "sql": "UPDATE orders SET status = 'shipped' WHERE total > 100", "expected_affected": expected }),
    )
    .await;
    assert_eq!(committed["affected"].as_u64().unwrap(), expected);
    let mismatch = call(
        &app,
        "db.commit",
        json!({ "connection": conn, "sql": "UPDATE orders SET status = 'x' WHERE total > 100", "expected_affected": expected + 5 }),
        &Actor::User,
    )
    .await;
    assert!(mismatch.is_err());
    let untouched: i64 = ok(&app, "db.query", json!({ "connection": conn, "sql": "SELECT count(*) FROM orders WHERE status = 'x'" })).await
        [0]["rows"][0][0]
        .as_i64()
        .unwrap();
    assert_eq!(untouched, 0);

    // Graph, DDL export, grid paging and staged edits.
    let graph = ok(&app, "db.graph", json!({ "connection": conn })).await;
    assert_eq!(graph["tables"].as_array().unwrap().len(), 2);
    let ddl = ok(&app, "db.ddl", json!({ "connection": conn, "dialect": "mysql" })).await;
    assert!(ddl["sql"].as_str().unwrap().contains("AUTO_INCREMENT"));
    let page = ok(&app, "db.rows", json!({ "connection": conn, "table": "customers", "query": { "limit": 10, "offset": 0 } })).await;
    assert_eq!(page["total"], 30);
    assert_eq!(page["result"]["rows"].as_array().unwrap().len(), 10);
    let applied = ok(&app, "db.apply", json!({ "connection": conn, "table": "customers", "changes": { "updates": [{ "key": { "id": 1 }, "values": { "city": "Lisbon" } }] } })).await;
    assert_eq!(applied["affected"], 1);

    // Fault injection applies live: errors with the chosen code, then off again.
    let mock_id = conn.trim_start_matches("mock:").to_owned();
    ok(
        &app,
        "mock.faults.set",
        json!({ "id": mock_id, "faults": { "enabled": true, "rules": [
        { "matches": "/^select count/", "action": { "type": "error", "code": "40001", "message": "could not serialize access" } }
    ] } }),
    )
    .await;
    let failed =
        call(&app, "db.query", json!({ "connection": conn, "sql": "SELECT count(*) FROM orders" }), &Actor::User).await.unwrap_err();
    assert_eq!(failed.code.as_deref(), Some("40001"), "{failed:?}");
    assert!(call(&app, "db.query", json!({ "connection": conn, "sql": "SELECT 1" }), &Actor::User).await.is_ok());
    let bad = call(
        &app,
        "mock.faults.set",
        json!({ "id": mock_id, "faults": { "enabled": true, "rules": [{ "matches": "/(/", "action": { "type": "drop" } }] } }),
        &Actor::User,
    )
    .await;
    assert!(bad.is_err(), "invalid regex rejected");
    let saved = ok(&app, "mock.list", json!({})).await;
    assert_eq!(saved[0]["faults"]["rules"][0]["action"]["code"], "40001", "faults persist in the mock definition");
    ok(&app, "mock.faults.set", json!({ "id": mock_id, "faults": { "enabled": false } })).await;
    assert!(call(&app, "db.query", json!({ "connection": conn, "sql": "SELECT count(*) FROM orders" }), &Actor::User).await.is_ok());

    // Seed capture + reset brings the data back.
    let seed = ok(&app, "mock.seed.capture", json!({ "id": conn.trim_start_matches("mock:") })).await;
    assert!(seed["sql"].as_str().unwrap().contains("CREATE TABLE"));
    ok(&app, "db.query", json!({ "connection": conn, "sql": "DELETE FROM orders" })).await;
    ok(&app, "mock.reset", json!({ "id": conn.trim_start_matches("mock:") })).await;
    let back: i64 =
        ok(&app, "db.query", json!({ "connection": conn, "sql": "SELECT count(*) FROM orders" })).await[0]["rows"][0][0].as_i64().unwrap();
    assert_eq!(back, 80);
    app.stop_all_mocks().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_queries_scenarios_and_bundles() {
    let (dir, app) = app().await;
    let conn = create_pg_mock(&app).await;
    ok(&app, "db.query", json!({ "connection": conn, "sql": "CREATE TABLE users (id serial primary key, email text unique not null); INSERT INTO users (email) VALUES ('a@x.io')" })).await;
    for (name, sql, expect) in [
        ("one user", "SELECT * FROM users", json!({ "kind": "rows", "count": 1 })),
        ("duplicate email fails", "INSERT INTO users (email) VALUES ('a@x.io')", json!({ "kind": "fails", "code": "23505" })),
        ("no admins", "SELECT * FROM users WHERE email LIKE 'admin%'", json!({ "kind": "empty" })),
    ] {
        ok(&app, "query.save", json!({ "query": { "id": "", "name": name, "collection": "checks", "sql": sql, "docs": "## Why\nEdge case.", "connection": conn, "tags": [], "expect": expect } })).await;
    }
    // Queries saved by collection name land in one real collection.
    let ws = ok(&app, "workspace.get", json!({})).await;
    assert_eq!(ws["collections"].as_array().unwrap().len(), 1);
    assert_eq!(ws["collections"][0]["id"], "checks");
    let by_name = ok(&app, "query.save", json!({ "query": { "id": "", "name": "count", "collection": "Checks", "sql": "SELECT count(*) FROM users", "docs": "", "tags": [] } })).await;
    assert_eq!(by_name["collection"], "checks");
    ok(&app, "query.delete", json!({ "collection": "checks", "id": by_name["id"] })).await;
    let results = ok(&app, "scenario.run", json!({ "collection": "checks" })).await;
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["passed"].as_bool().unwrap()), "{results:?}");

    app.stop_all_mocks().await;
    let bundle = dir.path().join("share.twinbase");
    let mock_id = conn.trim_start_matches("mock:").to_owned();
    ok(&app, "bundle.export", json!({ "path": bundle.to_string_lossy(), "selection": { "mocks": [mock_id], "collections": ["checks"] } }))
        .await;
    // Import into a second workspace and use the data there.
    let other = App::new(dir.path().join("home2"), Some(dir.path().join("ws2"))).unwrap();
    let summary = ok(&other, "bundle.import", json!({ "path": bundle.to_string_lossy() })).await;
    assert_eq!(summary["mocks"][0], mock_id);
    let ws = ok(&other, "workspace.get", json!({})).await;
    assert_eq!(ws["queries"].as_array().unwrap().len(), 3);
    // Importing again into the same workspace gets new ids and a different port.
    let again = ok(&other, "bundle.import", json!({ "path": bundle.to_string_lossy() })).await;
    let again_id = again["mocks"][0].as_str().unwrap().to_owned();
    assert_ne!(again_id, mock_id);
    let mocks = ok(&other, "mock.list", json!({})).await;
    let ports: Vec<u64> = mocks.as_array().unwrap().iter().map(|m| m["port"].as_u64().unwrap()).collect();
    assert_eq!(ports.len(), 2);
    assert_ne!(ports[0], ports[1], "imported mocks must not share a port");
    ok(&other, "mock.delete", json!({ "id": again_id })).await;
    let ws = ok(&other, "workspace.get", json!({})).await;
    // Use a different port in the second workspace (the first app may still hold the old one).
    let mut def = ws["mocks"][0].clone();
    def["port"] = json!(0);
    for k in ["running", "connection_id", "error"] {
        def.as_object_mut().unwrap().remove(k);
    }
    ok(&other, "mock.save", json!({ "mock": def })).await;
    ok(&other, "mock.start", json!({ "id": mock_id })).await;
    let n = ok(&other, "db.query", json!({ "connection": format!("mock:{mock_id}"), "sql": "SELECT count(*) FROM users" })).await;
    assert_eq!(n[0]["rows"][0][0], 1);
    other.stop_all_mocks().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_access_and_mcp_protocol() {
    let (_d, app) = app().await;
    let conn = create_pg_mock(&app).await;
    // A "real" connection pointing at the mock, read-only for agents by default.
    let mock = ok(&app, "mock.list", json!({})).await;
    let port = mock[0]["port"].as_u64().unwrap();
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Staging", "kind": "postgres", "host": "127.0.0.1", "port": port, "user": "x", "database": "app" }, "password": "x" })).await;
    let agent = Actor::Agent { name: "Claude Code".into() };
    let denied = call(&app, "db.query", json!({ "connection": "staging", "sql": "CREATE TABLE t (id int)" }), &agent).await.unwrap_err();
    assert_eq!(denied.kind, "forbidden");
    assert!(call(&app, "db.query", json!({ "connection": "staging", "sql": "SELECT 1" }), &agent).await.is_ok());
    assert!(call(&app, "db.query", json!({ "connection": conn, "sql": "CREATE TABLE t (id int)" }), &agent).await.is_ok());
    let activity = ok(&app, "activity.list", json!({ "limit": 10 })).await;
    assert!(activity.as_array().unwrap().iter().any(|a| a["actor"]["name"] == "Claude Code" && a["ok"] == false));

    // MCP over HTTP.
    let (addr, _task) =
        twinbase_core::http::serve(app.clone(), twinbase_core::http::HttpOptions { port: 0, dev_api: false }).await.unwrap();
    let token = app.settings().mcp.token;
    let url = format!("http://{addr}/mcp");
    let post = |body: Value, session: Option<String>| {
        let url = url.clone();
        let token = token.clone();
        async move {
            let mut cmd = std::process::Command::new("curl");
            cmd.args([
                "-s",
                "-i",
                "-X",
                "POST",
                &url,
                "-H",
                "Content-Type: application/json",
                "-H",
                &format!("Authorization: Bearer {token}"),
                "-d",
                &body.to_string(),
            ]);
            if let Some(s) = session {
                cmd.args(["-H", &format!("Mcp-Session-Id: {s}")]);
            }
            let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    };
    let init = post(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "Codex" }, "capabilities": {} } }), None).await;
    assert!(init.contains("\"protocolVersion\":\"2025-06-18\""), "{init}");
    let session = init
        .lines()
        .find_map(|l| l.to_ascii_lowercase().starts_with("mcp-session-id:").then(|| l.split(':').nth(1).unwrap().trim().to_owned()))
        .unwrap();
    let list = post(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }), Some(session.clone())).await;
    assert!(list.contains("\"run_sql\"") && list.contains("\"preview_sql\""));
    let call_res = post(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "run_sql", "arguments": { "connection": conn, "sql": "INSERT INTO t VALUES (7); SELECT * FROM t" } } }), Some(session.clone())).await;
    assert!(call_res.contains("\"isError\":false"), "{call_res}");
    let unauthorized = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST", &url, "-d", "{}"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&unauthorized.stdout), "401");
    let evil_origin = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            &url,
            "-H",
            "Origin: https://evil.example",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-d",
            "{}",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&evil_origin.stdout), "403");
    let activity = ok(&app, "activity.list", json!({ "limit": 5 })).await;
    assert!(activity.as_array().unwrap().iter().any(|a| a["actor"]["name"] == "Codex"));
    app.stop_all_mocks().await;
}

/// Snapshot v1, evolve the schema to v2, generate a migration from the diff, apply it to a fresh
/// copy of v1 and check the result is identical to v2 — then migrate back down to v1.
async fn migration_round_trip(kind: &str, v1: &str, v2_changes: &str) {
    let (_d, app) = app().await;
    let new_mock = |name: &str| {
        let app = app.clone();
        let name = name.to_owned();
        let kind = kind.to_owned();
        async move {
            let m = ok(&app, "mock.save", json!({ "mock": { "id": "", "name": name, "kind": kind, "port": free_port() } })).await;
            let id = m["id"].as_str().unwrap().to_owned();
            ok(&app, "mock.start", json!({ "id": id })).await;
            format!("mock:{id}")
        }
    };
    let a = new_mock("Evolving").await;
    ok(&app, "db.query", json!({ "connection": a, "sql": v1 })).await;
    let snap = ok(&app, "schema.snapshot.create", json!({ "connection": a, "label": "v1" })).await;
    let snap_id = snap["id"].as_str().unwrap().to_owned();
    assert_eq!(ok(&app, "schema.snapshot.list", json!({})).await.as_array().unwrap().len(), 1);
    ok(&app, "db.query", json!({ "connection": a, "sql": v2_changes })).await;

    let d = ok(&app, "schema.diff", json!({ "from": { "snapshot": snap_id }, "to": { "connection": a } })).await;
    assert_eq!(d["identical"], false);
    let up = d["up_sql"].as_str().unwrap().to_owned();
    let down = d["down_sql"].as_str().unwrap().to_owned();

    let b = new_mock("Copy of v1").await;
    ok(&app, "db.query", json!({ "connection": b, "sql": v1 })).await;
    ok(&app, "db.query", json!({ "connection": b, "sql": up })).await;
    let same = ok(&app, "schema.diff", json!({ "from": { "connection": b }, "to": { "connection": a } })).await;
    assert_eq!(same["identical"], true, "after up ({kind}):\n{up}\nremaining: {}", same["changes"]);

    ok(&app, "db.query", json!({ "connection": b, "sql": down })).await;
    let back = ok(&app, "schema.diff", json!({ "from": { "connection": b }, "to": { "snapshot": snap_id } })).await;
    assert_eq!(back["identical"], true, "after down ({kind}):\n{down}\nremaining: {}", back["changes"]);

    // Snapshots travel in bundles.
    let bundle = _d.path().join("schema.twinbase");
    ok(&app, "bundle.export", json!({ "path": bundle.to_string_lossy(), "selection": { "snapshots": [snap_id] } })).await;
    let other = App::new(_d.path().join("home-b"), Some(_d.path().join("ws-b"))).unwrap();
    let imported = ok(&other, "bundle.import", json!({ "path": bundle.to_string_lossy() })).await;
    assert_eq!(imported["snapshots"].as_array().unwrap().len(), 1);
    let listed = ok(&other, "schema.snapshot.list", json!({})).await;
    assert_eq!(listed[0]["label"], "v1");

    // Ids that could escape the workspace are rejected.
    assert!(call(&app, "schema.snapshot.get", json!({ "id": "../../etc/passwd" }), &Actor::User).await.is_err());
    assert!(call(&app, "collection.delete", json!({ "id": ".." }), &Actor::User).await.is_err());
    app.stop_all_mocks().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_snapshots_diffs_and_migrations_postgres() {
    migration_round_trip(
        "postgres",
        "CREATE TABLE users (id serial PRIMARY KEY, email text NOT NULL, legacy text);
         CREATE TABLE orders (id serial PRIMARY KEY, user_id int NOT NULL REFERENCES users(id), total numeric(10,2));",
        "ALTER TABLE users ADD COLUMN plan text NOT NULL DEFAULT 'free';
         ALTER TABLE users DROP COLUMN legacy;
         ALTER TABLE users ALTER COLUMN email TYPE varchar(320);
         CREATE UNIQUE INDEX users_email_key ON users (email);
         CREATE INDEX orders_total_idx ON orders (total);
         CREATE TABLE coupons (code text PRIMARY KEY, user_id int REFERENCES users(id) ON DELETE CASCADE, percent int DEFAULT 10);",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_snapshots_diffs_and_migrations_mysql() {
    migration_round_trip(
        "mysql",
        "CREATE TABLE users (id INT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(200) NOT NULL, legacy TEXT);
         CREATE TABLE orders (id INT AUTO_INCREMENT PRIMARY KEY, user_id INT NOT NULL, total DECIMAL(10,2), CONSTRAINT orders_user_fk FOREIGN KEY (user_id) REFERENCES users(id));",
        "ALTER TABLE users ADD COLUMN plan VARCHAR(20) NOT NULL DEFAULT 'free';
         ALTER TABLE users DROP COLUMN legacy;
         ALTER TABLE users MODIFY COLUMN email VARCHAR(320) NOT NULL;
         CREATE UNIQUE INDEX users_email_key ON users (email);
         CREATE TABLE coupons (code VARCHAR(40) PRIMARY KEY, user_id INT, percent INT DEFAULT 10, CONSTRAINT coupons_user_fk FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE);",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_plans_on_mocks_and_real_postgres() {
    let (_d, app) = app().await;
    let conn = create_pg_mock(&app).await;
    ok(&app, "db.query", json!({ "connection": conn, "sql": "CREATE TABLE c (id serial primary key, name text); CREATE TABLE o (id serial primary key, c_id int references c(id), total numeric); INSERT INTO c (name) VALUES ('a'); INSERT INTO o (c_id, total) VALUES (1, 5)" })).await;
    let plan = ok(
        &app,
        "db.explain",
        json!({ "connection": conn, "sql": "SELECT * FROM o JOIN c ON c.id = o.c_id WHERE o.total > 1 ORDER BY c.name" }),
    )
    .await;
    assert!(!plan["roots"].as_array().unwrap().is_empty(), "{plan}");
    assert_eq!(plan["format"], "postgres-text");
    // Agents may explain on read-only connections, but ANALYZE of a write needs write access.
    let mock = ok(&app, "mock.list", json!({})).await;
    let port = mock[0]["port"].as_u64().unwrap();
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "RO", "kind": "postgres", "host": "127.0.0.1", "port": port, "user": "x", "database": "app" } })).await;
    let agent = Actor::Agent { name: "Codex".into() };
    assert!(call(&app, "db.explain", json!({ "connection": "ro", "sql": "SELECT * FROM o" }), &agent).await.is_ok());
    assert!(call(&app, "db.explain", json!({ "connection": "ro", "sql": "DELETE FROM o", "analyze": true }), &agent).await.is_err());

    // Real PostgreSQL (docker): JSON plans with actual timings; ANALYZE of a write is rolled back.
    if let Ok(port) = std::env::var("TWINBASE_REAL_PG_PORT") {
        ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Real PG", "kind": "postgres", "host": "127.0.0.1", "port": port.parse::<u16>().unwrap(), "user": "postgres", "database": "postgres", "ai_access": "read-write" }, "password": "postgres" })).await;
        ok(&app, "db.query", json!({ "connection": "real-pg", "sql": "DROP TABLE IF EXISTS tb_explain; CREATE TABLE tb_explain (id serial primary key, v int); INSERT INTO tb_explain (v) SELECT g FROM generate_series(1, 5000) g; ANALYZE tb_explain" })).await;
        let p =
            ok(&app, "db.explain", json!({ "connection": "real-pg", "sql": "SELECT * FROM tb_explain WHERE v > 4990", "analyze": true }))
                .await;
        assert_eq!(p["format"], "postgres-json", "{p}");
        assert_eq!(p["analyzed"], true);
        assert_eq!(p["roots"][0]["actual_rows"], 10.0);
        assert!(p["hints"].as_array().unwrap().iter().any(|h| h.as_str().unwrap().contains("index on (v)")), "{}", p["hints"]);
        let w = ok(&app, "db.explain", json!({ "connection": "real-pg", "sql": "INSERT INTO tb_explain (v) VALUES (1)", "analyze": true }))
            .await;
        assert_eq!(w["analyzed"], true);
        let n = ok(&app, "db.query", json!({ "connection": "real-pg", "sql": "SELECT count(*) FROM tb_explain" })).await;
        assert_eq!(n[0]["rows"][0][0], 5000, "EXPLAIN ANALYZE of a write must be rolled back");
        // A failing ANALYZE leaves no open transaction behind.
        assert!(
            call(
                &app,
                "db.explain",
                json!({ "connection": "real-pg", "sql": "INSERT INTO tb_explain (nope) VALUES (1)", "analyze": true }),
                &Actor::User
            )
            .await
            .is_err()
        );
        ok(&app, "db.query", json!({ "connection": "real-pg", "sql": "DROP TABLE tb_explain" })).await;
    }
    app.stop_all_mocks().await;
}

/// SSH tunnels through a real OpenSSH bastion (docker): set TWINBASE_TEST_SSH_PORT and
/// TWINBASE_TEST_SSH_KEY (ed25519 key with passphrase "key-pass"); the bastion reaches PostgreSQL
/// as `pg:5432`.
#[tokio::test(flavor = "multi_thread")]
async fn ssh_tunnels_with_password_key_and_host_key_checks() {
    let (Ok(port), Ok(key)) = (std::env::var("TWINBASE_TEST_SSH_PORT"), std::env::var("TWINBASE_TEST_SSH_KEY")) else { return };
    let (_d, app) = app().await;
    let conn = |auth: &str, extra: Value| {
        let mut c = json!({ "id": "", "name": format!("Via SSH {auth}"), "kind": "postgres", "host": "pg", "port": 5432, "user": "postgres", "database": "postgres",
            "ssh": { "host": "127.0.0.1", "port": port.parse::<u16>().unwrap(), "user": "tunnel", "auth": auth } });
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                c["ssh"][k] = v.clone();
            }
        }
        c
    };
    // Password authentication; the bastion's key is trusted on first use and recorded.
    let saved = ok(
        &app,
        "connection.save",
        json!({ "connection": conn("password", json!({})), "password": "postgres", "ssh_secret": "tunnel-test-pw" }),
    )
    .await;
    let id = saved["id"].as_str().unwrap().to_owned();
    let test = ok(&app, "connection.test", json!({ "connection": saved })).await;
    assert_eq!(test["ok"], true, "{test}");
    let r = ok(&app, "db.query", json!({ "connection": id, "sql": "SELECT current_database()" })).await;
    assert_eq!(r[0]["rows"][0][0], "postgres");
    let known = std::fs::read_to_string(_d.path().join("home").join("known_hosts")).unwrap();
    assert!(known.contains("2222"), "{known}");
    let ws = ok(&app, "workspace.get", json!({})).await;
    assert_eq!(ws["connections"][0]["has_ssh_secret"], true);
    assert!(ws["connections"][0]["ssh"].get("secret").is_none(), "secrets never land in the workspace");
    // Wrong password.
    let bad =
        ok(&app, "connection.test", json!({ "connection": conn("password", json!({})), "password": "postgres", "ssh_secret": "nope" }))
            .await;
    assert_eq!(bad["ok"], false);
    assert!(bad["error"].as_str().unwrap().contains("authentication failed"), "{bad}");
    // Key file with a passphrase.
    let with_key = ok(
        &app,
        "connection.test",
        json!({ "connection": conn("key", json!({ "key_path": key })), "password": "postgres", "ssh_secret": "key-pass" }),
    )
    .await;
    assert_eq!(with_key["ok"], true, "{with_key}");
    // A different host key for the same bastion is refused.
    app.disconnect(&id).await;
    let path = _d.path().join("home").join("known_hosts");
    let line = known.lines().find(|l| !l.trim().is_empty()).unwrap();
    let mut parts: Vec<&str> = line.split_whitespace().collect();
    let fake = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    parts[2] = fake;
    std::fs::write(&path, format!("{}\n", parts.join(" "))).unwrap();
    let refused = ok(&app, "connection.test", json!({ "connection": saved })).await;
    assert_eq!(refused["ok"], false);
    assert!(refused["error"].as_str().unwrap().contains("differs"), "{refused}");
}

/// MongoDB mocks: seeds in Extended JSON, the document API, the mongosh-style console,
/// captured seeds and agent access rules.
#[tokio::test(flavor = "multi_thread")]
async fn mongodb_mocks_seeds_documents_and_console() {
    let (_d, app) = app().await;
    let m = ok(
        &app,
        "mock.save",
        json!({ "mock": { "id": "", "name": "Catalog", "kind": "mongodb", "port": free_port(), "database": "shop" } }),
    )
    .await;
    let id = m["id"].as_str().unwrap().to_owned();
    let conn = format!("mock:{id}");
    let bad = call(&app, "mock.seed.set", json!({ "id": id, "sql": "{\"collections\": {\"x\": [1]}}" }), &Actor::User).await;
    assert!(bad.is_err(), "seeds are validated before they are saved");
    let seed = r#"{ "collections": {
        "products": { "indexes": [ { "key": { "sku": 1 }, "name": "sku_1", "unique": true } ],
                      "documents": [ { "_id": { "$oid": "65a1b2c3d4e5f60718293a4b" }, "sku": "A1", "price": 10, "tags": ["new"] },
                                     { "sku": "B2", "price": 25.5, "added": { "$date": "2024-05-01T00:00:00Z" } } ] },
        "orders": [ { "_id": 1, "sku": "A1", "qty": 2 } ] } }"#;
    ok(&app, "mock.seed.set", json!({ "id": id, "sql": seed })).await;
    ok(&app, "mock.start", json!({ "id": id })).await;
    let mocks = ok(&app, "mock.list", json!({})).await;
    assert!(mocks[0]["error"].is_null(), "{mocks}");

    let colls = ok(&app, "mongo.collections", json!({ "connection": conn })).await;
    assert_eq!(colls.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["orders", "products"]);
    let page = ok(
        &app,
        "mongo.find",
        json!({ "connection": conn, "collection": "products", "filter": "{price: {$gt: 5}}", "sort": "{price: -1}", "limit": 10 }),
    )
    .await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["documents"][0]["sku"], "B2");
    assert_eq!(page["documents"][0]["added"]["$date"], "2024-05-01T00:00:00Z");
    assert_eq!(page["documents"][1]["_id"]["$oid"], "65a1b2c3d4e5f60718293a4b");
    ok(&app, "mongo.replace", json!({ "connection": conn, "collection": "products", "id": { "$oid": "65a1b2c3d4e5f60718293a4b" }, "document": "{sku: 'A1', price: 12}" })).await;
    let dup = call(&app, "mongo.insert", json!({ "connection": conn, "collection": "products", "documents": "{sku: 'A1'}" }), &Actor::User)
        .await
        .unwrap_err();
    assert!(dup.message.contains("duplicate key"), "{dup:?}");
    let schema = ok(&app, "mongo.schema", json!({ "connection": conn, "collection": "products" })).await;
    assert!(schema["schema"]["fields"].as_array().unwrap().iter().any(|f| f["path"] == "added"));
    assert_eq!(schema["indexes"].as_array().unwrap().len(), 2);
    let empty_filter =
        call(&app, "mongo.delete", json!({ "connection": conn, "collection": "orders", "filter": "{}", "many": true }), &Actor::User).await;
    assert!(empty_filter.is_err(), "deleting everything needs all=true");

    let out = ok(&app, "mongo.run", json!({ "connection": conn, "command": "db.products.aggregate([{$group: {_id: null, total: {$sum: '$price'}}}])\ndb.orders.updateOne({_id: 1}, {$inc: {qty: 1}})" })).await;
    assert_eq!(out["outputs"][0]["documents"][0]["total"], 37.5);
    assert_eq!(out["outputs"][1]["value"]["modifiedCount"], 1);

    // Agents: read-write on mocks, read-only on a "real" connection by default.
    let port = ok(&app, "mock.list", json!({})).await[0]["port"].as_u64().unwrap();
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Prod Mongo", "kind": "mongodb", "host": "127.0.0.1", "port": port, "user": "", "database": "shop" } })).await;
    let agent = Actor::Agent { name: "Claude Code".into() };
    assert!(call(&app, "mongo.run", json!({ "connection": "prod-mongo", "command": "db.orders.find()" }), &agent).await.is_ok());
    let denied =
        call(&app, "mongo.run", json!({ "connection": "prod-mongo", "command": "db.orders.deleteMany({})" }), &agent).await.unwrap_err();
    assert_eq!(denied.kind, "forbidden");
    let denied = call(
        &app,
        "mongo.aggregate",
        json!({ "connection": "prod-mongo", "collection": "orders", "pipeline": "[{$out: 'copy'}]" }),
        &agent,
    )
    .await
    .unwrap_err();
    assert_eq!(denied.kind, "forbidden");
    assert!(call(&app, "mongo.run", json!({ "connection": conn, "command": "db.orders.deleteMany({})" }), &agent).await.is_ok());

    // Connection strings keep their passwords out of the workspace.
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Atlas", "kind": "mongodb", "host": "mongodb://app:s3cret@db.example.net:27017/shop", "port": 0, "user": "" } })).await;
    let ws = ok(&app, "workspace.get", json!({})).await;
    let atlas = ws["connections"].as_array().unwrap().iter().find(|c| c["name"] == "Atlas").unwrap();
    assert_eq!(atlas["host"], "mongodb://app@db.example.net:27017/shop");
    assert_eq!(atlas["has_password"], true);

    // Capture the current data as the seed, reset, and the captured state comes back.
    let captured = ok(&app, "mock.seed.capture", json!({ "id": id })).await;
    assert!(captured["sql"].as_str().unwrap().contains("\"price\""));
    ok(&app, "mongo.run", json!({ "connection": conn, "command": "db.products.drop()" })).await;
    ok(&app, "mock.reset", json!({ "id": id })).await;
    let page = ok(&app, "mongo.find", json!({ "connection": conn, "collection": "products", "filter": "{sku: 'A1'}" })).await;
    assert_eq!(page["documents"][0]["price"], 12);
    let snippets = ok(&app, "mock.snippets", json!({ "id": id })).await;
    assert!(snippets.to_string().contains(&format!("mongodb://127.0.0.1:{port}/shop")));
    app.stop_all_mocks().await;
}

/// DynamoDB mocks: table seeds, the item browser API, the request console, captured seeds and
/// agent access rules.
#[tokio::test(flavor = "multi_thread")]
async fn dynamodb_mocks_seeds_items_and_console() {
    let (_d, app) = app().await;
    let m = ok(&app, "mock.save", json!({ "mock": { "id": "", "name": "Orders API", "kind": "dynamodb", "port": free_port() } })).await;
    let id = m["id"].as_str().unwrap().to_owned();
    let conn = format!("mock:{id}");
    let seed = json!({ "tables": [{
        "TableName": "orders", "BillingMode": "PAY_PER_REQUEST",
        "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }],
        "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }, { "AttributeName": "sk", "AttributeType": "S" }],
        "Items": [
            { "pk": { "S": "customer#1" }, "sk": { "S": "order#001" }, "status": { "S": "paid" }, "total": { "N": "25.5" } },
            { "pk": { "S": "customer#1" }, "sk": { "S": "order#002" }, "status": { "S": "open" }, "total": { "N": "10" } },
            { "pk": { "S": "customer#2" }, "sk": { "S": "order#001" }, "status": { "S": "open" }, "total": { "N": "99" } }
        ]
    }] });
    assert!(call(&app, "mock.seed.set", json!({ "id": id, "sql": "{\"tables\": [{\"TableName\": \"x\"}]}" }), &Actor::User).await.is_err());
    ok(&app, "mock.seed.set", json!({ "id": id, "sql": seed.to_string() })).await;
    ok(&app, "mock.start", json!({ "id": id })).await;
    assert!(ok(&app, "mock.list", json!({})).await[0]["error"].is_null());

    let tables = ok(&app, "dynamo.tables", json!({ "connection": conn })).await;
    assert_eq!(tables["tables"], json!(["orders"]));
    // Query with a sort-key condition and a filter on a reserved word ("status") — the API adds #names.
    let r = ok(&app, "dynamo.read", json!({ "connection": conn, "table": "orders", "partition": { "S": "customer#1" }, "sort": { "path": "sk", "op": "begins_with", "value": { "S": "order#" } }, "filters": [{ "path": "status", "op": "=", "value": { "S": "open" } }] })).await;
    assert_eq!(r["operation"], "Query");
    assert_eq!((r["count"].as_u64(), r["scanned"].as_u64()), (Some(1), Some(2)));
    let page = ok(&app, "dynamo.read", json!({ "connection": conn, "table": "orders", "limit": 2 })).await;
    assert_eq!(page["operation"], "Scan");
    assert!(page["last_key"].is_object());
    let rest = ok(&app, "dynamo.read", json!({ "connection": conn, "table": "orders", "limit": 2, "start_key": page["last_key"] })).await;
    assert_eq!(rest["count"], 1);

    let item = json!({ "pk": { "S": "customer#3" }, "sk": { "S": "order#001" }, "total": { "N": "1" } });
    ok(&app, "dynamo.put", json!({ "connection": conn, "table": "orders", "item": item, "expect": "new" })).await;
    let again = call(&app, "dynamo.put", json!({ "connection": conn, "table": "orders", "item": item, "expect": "new" }), &Actor::User)
        .await
        .unwrap_err();
    assert!(again.message.contains("already exists"), "{again:?}");
    let gone = call(
        &app,
        "dynamo.put",
        json!({ "connection": conn, "table": "orders", "item": { "pk": { "S": "nope" }, "sk": { "S": "x" } }, "expect": "exists" }),
        &Actor::User,
    )
    .await
    .unwrap_err();
    assert!(gone.message.contains("no longer exists"), "{gone:?}");
    ok(
        &app,
        "dynamo.delete",
        json!({ "connection": conn, "table": "orders", "keys": [{ "pk": { "S": "customer#3" }, "sk": { "S": "order#001" } }] }),
    )
    .await;

    let out = ok(&app, "dynamo.run", json!({ "connection": conn, "command": "UpdateItem {\"TableName\": \"orders\", \"Key\": {\"pk\": {\"S\": \"customer#2\"}, \"sk\": {\"S\": \"order#001\"}}, \"UpdateExpression\": \"SET #t = #t + :d\", \"ExpressionAttributeNames\": {\"#t\": \"total\"}, \"ExpressionAttributeValues\": {\":d\": {\"N\": \"1\"}}, \"ReturnValues\": \"UPDATED_NEW\"}\nGetItem {\"TableName\": \"orders\", \"Key\": {\"pk\": {\"S\": \"customer#2\"}, \"sk\": {\"S\": \"order#001\"}}}" })).await;
    assert_eq!(out["outputs"][0]["response"]["Attributes"]["total"]["N"], "100");
    assert_eq!(out["outputs"][1]["response"]["Item"]["total"]["N"], "100");

    // Agents: read-only on a saved connection by default.
    let port = ok(&app, "mock.list", json!({})).await[0]["port"].as_u64().unwrap();
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Prod Dynamo", "kind": "dynamodb", "host": "127.0.0.1", "port": port, "user": "any", "database": "us-east-1" }, "password": "any" })).await;
    let agent = Actor::Agent { name: "Claude Code".into() };
    assert!(
        call(&app, "dynamo.run", json!({ "connection": "prod-dynamo", "command": "Scan {\"TableName\": \"orders\"}" }), &agent)
            .await
            .is_ok()
    );
    let denied =
        call(&app, "dynamo.run", json!({ "connection": "prod-dynamo", "command": "DeleteTable {\"TableName\": \"orders\"}" }), &agent)
            .await
            .unwrap_err();
    assert_eq!(denied.kind, "forbidden");

    // Capture the data as the seed, break things, reset: the captured state returns.
    let captured = ok(&app, "mock.seed.capture", json!({ "id": id })).await;
    assert!(captured["sql"].as_str().unwrap().contains("\"100\""));
    ok(&app, "dynamo.table.delete", json!({ "connection": conn, "table": "orders" })).await;
    ok(&app, "mock.reset", json!({ "id": id })).await;
    let r = ok(&app, "dynamo.read", json!({ "connection": conn, "table": "orders", "partition": { "S": "customer#2" } })).await;
    assert_eq!(r["items"][0]["total"]["N"], "100");
    let snippets = ok(&app, "mock.snippets", json!({ "id": id })).await;
    assert!(snippets.to_string().contains(&format!("http://127.0.0.1:{port}")));
    app.stop_all_mocks().await;
}

/// Firestore mocks: document seeds, the document API, statements, captured seeds and access rules.
#[tokio::test(flavor = "multi_thread")]
async fn firestore_mocks_seeds_documents_and_console() {
    let (_d, app) = app().await;
    let m = ok(&app, "mock.save", json!({ "mock": { "id": "", "name": "App Firestore", "kind": "firestore", "port": free_port() } })).await;
    let id = m["id"].as_str().unwrap().to_owned();
    let conn = format!("mock:{id}");
    assert!(
        call(&app, "mock.seed.set", json!({ "id": id, "sql": "{\"documents\": {\"users\": {}}}" }), &Actor::User).await.is_err(),
        "a collection path is not a document"
    );
    let seed = json!({ "documents": {
        "users/ann": { "name": "Ann", "age": 31, "joined": { "__timestamp": "2024-05-01T10:00:00Z" } },
        "users/bob": { "name": "Bob", "age": 25, "best": { "__ref": "users/ann" } },
        "users/ann/orders/o1": { "total": 10.5 }
    } });
    ok(&app, "mock.seed.set", json!({ "id": id, "sql": seed.to_string() })).await;
    ok(&app, "mock.start", json!({ "id": id })).await;
    assert!(ok(&app, "mock.list", json!({})).await[0]["error"].is_null());

    assert_eq!(ok(&app, "firestore.collections", json!({ "connection": conn })).await["collections"], json!(["users"]));
    assert_eq!(
        ok(&app, "firestore.collections", json!({ "connection": conn, "parent": "users/ann" })).await["collections"],
        json!(["orders"])
    );
    let docs = ok(
        &app,
        "firestore.documents",
        json!({ "connection": conn, "query": { "collection": "users", "conditions": [{ "field": "age", "op": ">", "value": 26 }] } }),
    )
    .await;
    assert_eq!(docs["documents"].as_array().unwrap().len(), 1);
    assert_eq!(docs["documents"][0]["data"]["joined"], json!({ "__timestamp": "2024-05-01T10:00:00Z" }));
    let bob = ok(&app, "firestore.get", json!({ "connection": conn, "path": "users/bob" })).await;
    assert_eq!(bob["document"]["data"]["best"], json!({ "__ref": "users/ann" }));
    ok(&app, "firestore.set", json!({ "connection": conn, "path": "users/bob", "data": { "age": 26 }, "merge": true, "expect": "exists" }))
        .await;
    let gone = call(
        &app,
        "firestore.set",
        json!({ "connection": conn, "path": "users/zed", "data": { "a": 1 }, "merge": true, "expect": "exists" }),
        &Actor::User,
    )
    .await
    .unwrap_err();
    assert!(gone.message.contains("no longer exists"), "{gone:?}");
    let added = ok(&app, "firestore.add", json!({ "connection": conn, "collection": "users", "data": { "name": "Cid" } })).await;
    assert_eq!(added["document"]["id"].as_str().unwrap().len(), 20);
    let out =
        ok(&app, "firestore.run", json!({ "connection": conn, "command": "count users\nquery users where age >= 26 order by age" })).await;
    assert_eq!(out["outputs"][0]["value"], 3);
    assert_eq!(out["outputs"][1]["documents"].as_array().unwrap().len(), 2);

    // Agents: read-only on a saved connection by default.
    let port = ok(&app, "mock.list", json!({})).await[0]["port"].as_u64().unwrap();
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Prod Firestore", "kind": "firestore", "host": "127.0.0.1", "port": port, "user": "", "database": "my-project" } })).await;
    let agent = Actor::Agent { name: "Claude Code".into() };
    assert!(call(&app, "firestore.run", json!({ "connection": "prod-firestore", "command": "query users" }), &agent).await.is_ok());
    let denied =
        call(&app, "firestore.run", json!({ "connection": "prod-firestore", "command": "delete users/ann" }), &agent).await.unwrap_err();
    assert_eq!(denied.kind, "forbidden");

    let captured = ok(&app, "mock.seed.capture", json!({ "id": id })).await;
    assert!(captured["sql"].as_str().unwrap().contains("\"Cid\""));
    ok(&app, "firestore.delete", json!({ "connection": conn, "paths": ["users/ann", "users/bob"] })).await;
    ok(&app, "mock.reset", json!({ "id": id })).await;
    let back = ok(&app, "firestore.get", json!({ "connection": conn, "path": "users/bob" })).await;
    assert_eq!(back["document"]["data"]["age"], 26);
    let snippets = ok(&app, "mock.snippets", json!({ "id": id })).await;
    assert!(snippets.to_string().contains(&format!("FIRESTORE_EMULATOR_HOST=127.0.0.1:{port}")));
    app.stop_all_mocks().await;
}

/// A PostgreSQL mock on the native engine (real PostgreSQL), when `TWINBASE_PG_BIN` points to
/// server binaries: seeds, queries, explain and capture work like on TwinLite mocks.
#[tokio::test(flavor = "multi_thread")]
async fn native_postgres_engine_mocks() {
    let Some(bin) = std::env::var_os("TWINBASE_PG_BIN") else { return };
    let (_d, app) = app().await;
    ok(&app, "settings.update", json!({ "settings": { "postgres_bin": bin.to_string_lossy() } })).await;
    let found = ok(&app, "mock.native.detect", json!({})).await;
    assert!(found["found"]["version"].as_str().unwrap().starts_with('1'));
    let m = ok(
        &app,
        "mock.save",
        json!({ "mock": { "id": "", "name": "Real PG", "kind": "postgres", "engine": "native", "port": free_port(), "database": "shop" } }),
    )
    .await;
    let id = m["id"].as_str().unwrap().to_owned();
    let conn = format!("mock:{id}");
    ok(&app, "mock.seed.set", json!({ "id": id, "sql": "CREATE TABLE products (id serial PRIMARY KEY, name text NOT NULL, tags text[]); INSERT INTO products (name, tags) VALUES ('lamp', ARRAY['home']), ('desk', ARRAY['office', 'home']);" })).await;
    ok(&app, "mock.start", json!({ "id": id })).await;
    assert!(ok(&app, "mock.list", json!({})).await[0]["error"].is_null());
    let r = ok(&app, "db.query", json!({ "connection": conn, "sql": "SELECT name FROM products WHERE 'home' = ANY(tags) ORDER BY name" }))
        .await;
    assert_eq!(r[0]["rows"], json!([["desk"], ["lamp"]]));
    let tables = ok(&app, "db.tables", json!({ "connection": conn })).await;
    assert!(tables.to_string().contains("products"));
    let plan = ok(&app, "db.explain", json!({ "connection": conn, "sql": "SELECT * FROM products WHERE id = 1" })).await;
    assert!(plan.to_string().contains("products"));
    // Agents work as the server's superuser here, so statements that reach the computer are refused.
    let agent = Actor::Agent { name: "Codex".into() };
    assert!(call(&app, "db.query", json!({ "connection": conn, "sql": "CREATE TABLE notes (id int)" }), &agent).await.is_ok());
    for sql in [
        "COPY products TO PROGRAM 'cat > /dev/null'",
        "SELECT pg_read_file('/etc/hosts')",
        "DO $$ BEGIN EXECUTE 'SELECT 1'; END $$",
        "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        "SELECT query_to_xml('select 1', true, false, '')",
        "ALTER SYSTEM SET work_mem = '8MB'",
    ] {
        let e = call(&app, "db.query", json!({ "connection": conn, "sql": sql }), &agent).await.unwrap_err();
        assert_eq!(e.kind, "forbidden", "{sql}: {e:?}");
    }
    let captured = ok(&app, "mock.seed.capture", json!({ "id": id })).await;
    assert!(captured["sql"].as_str().unwrap().contains("CREATE TABLE"));
    ok(&app, "db.query", json!({ "connection": conn, "sql": "DELETE FROM products" })).await;
    ok(&app, "mock.reset", json!({ "id": id })).await;
    let r = ok(&app, "db.query", json!({ "connection": conn, "sql": "SELECT count(*) FROM products" })).await;
    assert_eq!(r[0]["rows"][0][0], 2);
    app.stop_all_mocks().await;
}

/// SQL Server through the core API (docker `mcr.microsoft.com/mssql/server`): set
/// TWINBASE_REAL_MSSQL_PORT and TWINBASE_REAL_MSSQL_PASSWORD (the `sa` password).
#[tokio::test(flavor = "multi_thread")]
async fn schema_snapshots_diffs_and_migrations_mssql() {
    migration_round_trip(
        "mssql",
        "CREATE TABLE users (id INT IDENTITY(1,1) PRIMARY KEY, email NVARCHAR(200) NOT NULL, legacy NVARCHAR(MAX));
         CREATE TABLE orders (id INT IDENTITY PRIMARY KEY, user_id INT NOT NULL, total DECIMAL(10,2), CONSTRAINT orders_user_fk FOREIGN KEY (user_id) REFERENCES users(id));",
        "ALTER TABLE users ADD [plan] NVARCHAR(20) NOT NULL DEFAULT 'free';
         ALTER TABLE users DROP COLUMN legacy;
         ALTER TABLE users ALTER COLUMN email NVARCHAR(320) NOT NULL;
         CREATE UNIQUE INDEX users_email_key ON users (email);
         CREATE TABLE coupons (code NVARCHAR(40) PRIMARY KEY, user_id INT, [percent] INT DEFAULT 10, CONSTRAINT coupons_user_fk FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE);",
    )
    .await;
}

/// The SQL Server mock (TDS on TwinLite) browsed and edited through TwinBase's own SQL Server
/// driver, as the desktop app does.
#[tokio::test(flavor = "multi_thread")]
async fn sql_server_mocks() {
    let (_d, app) = app().await;
    let m = ok(&app, "mock.save", json!({ "mock": { "id": "", "name": "Shop MS", "kind": "mssql", "port": free_port() } })).await;
    let id = m["id"].as_str().unwrap().to_owned();
    ok(&app, "mock.start", json!({ "id": id })).await;
    let c = format!("mock:{id}");
    ok(&app, "db.query", json!({ "connection": c, "sql": "CREATE TABLE customers (id int IDENTITY(1,1) PRIMARY KEY, email nvarchar(200) NOT NULL UNIQUE, name nvarchar(100), joined datetime2 DEFAULT SYSDATETIME());
GO
CREATE TABLE orders (id bigint IDENTITY PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total decimal(10,2) NOT NULL, status nvarchar(20) DEFAULT N'new');
CREATE INDEX ix_orders_customer ON orders (customer_id);" })).await;
    assert_eq!(ok(&app, "db.generate", json!({ "connection": c, "table": "customers", "count": 30 })).await["inserted"], 30);
    assert_eq!(ok(&app, "db.generate", json!({ "connection": c, "table": "orders", "count": 50 })).await["inserted"], 50);
    let tables = ok(&app, "db.tables", json!({ "connection": c })).await;
    assert_eq!(tables.as_array().unwrap().len(), 2, "{tables}");
    assert_eq!(tables[0]["schema"], "dbo", "{tables}");
    let desc = ok(&app, "db.describe", json!({ "connection": c, "table": "orders" })).await;
    let cols: Vec<&str> = desc["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(cols, ["id", "customer_id", "total", "status"], "{desc}");
    let graph = ok(&app, "db.graph", json!({ "connection": c })).await;
    let orders = graph["tables"].as_array().unwrap().iter().find(|t| t["name"] == "orders").unwrap().clone();
    assert_eq!(orders["foreign_keys"][0]["ref_table"], "customers", "{orders}");
    let r = ok(&app, "db.query", json!({ "connection": c, "sql": "SELECT TOP 3 o.id, c.email FROM orders o JOIN customers c ON c.id = o.customer_id ORDER BY o.id" })).await;
    assert_eq!(r[0]["rows"].as_array().unwrap().len(), 3, "{r}");
    let page = ok(&app, "db.rows", json!({ "connection": c, "table": "customers", "query": { "limit": 10, "offset": 5 } })).await;
    assert_eq!(page["total"], 30, "{page}");
    assert_eq!(page["key_columns"], json!(["id"]));
    let applied = ok(
        &app,
        "db.apply",
        json!({ "connection": c, "table": "customers", "changes": {
        "updates": [{ "key": { "id": 1 }, "values": { "name": "Ana" } }],
        "inserts": [{ "email": "new@example.com", "name": "New" }],
        "deletes": [{ "id": 2 }]
    } }),
    )
    .await;
    assert_eq!(applied["affected"], 3, "{applied}");
    let ddl = ok(&app, "db.ddl", json!({ "connection": c })).await;
    assert!(ddl["sql"].as_str().unwrap().contains("IDENTITY(1,1)"), "{}", ddl["sql"]);

    // Preview runs the statement on a twin and shows the cascade.
    let preview = ok(&app, "db.preview", json!({ "connection": c, "sql": "DELETE FROM customers WHERE id = 1" })).await;
    assert!(preview["ok"].as_bool().unwrap(), "{preview}");
    let n = ok(&app, "db.query", json!({ "connection": c, "sql": "SELECT COUNT(*) FROM customers WHERE id = 1" })).await;
    assert_eq!(n[0]["rows"][0][0], 1);

    // Seed capture and reset.
    let seed = ok(&app, "mock.seed.capture", json!({ "id": id })).await;
    let seed_sql = seed["sql"].as_str().unwrap();
    assert!(seed_sql.contains("CREATE TABLE") && seed_sql.contains("SET IDENTITY_INSERT [customers] ON;"), "{seed}");
    let count = ok(&app, "db.query", json!({ "connection": c, "sql": "SELECT COUNT(*) FROM orders" })).await[0]["rows"][0][0].clone();
    ok(&app, "db.query", json!({ "connection": c, "sql": "DELETE FROM orders" })).await;
    ok(&app, "mock.reset", json!({ "id": id })).await;
    let back = ok(&app, "db.query", json!({ "connection": c, "sql": "SELECT COUNT(*) FROM orders" })).await;
    assert_eq!(back[0]["rows"][0][0], count, "{back}");
    let after = ok(
        &app,
        "db.query",
        json!({ "connection": c, "sql": "INSERT INTO customers (email) OUTPUT INSERTED.id VALUES (N'after@example.com')" }),
    )
    .await;
    assert!(after[0]["rows"][0][0].as_i64().unwrap() > 30, "identity continues after the seeded ids: {after}");
    let snippets = ok(&app, "mock.snippets", json!({ "id": id })).await;
    assert!(snippets.to_string().contains("TrustServerCertificate"), "{snippets}");
    app.stop_all_mocks().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_server_connections() {
    let (Ok(port), Ok(pw)) = (std::env::var("TWINBASE_REAL_MSSQL_PORT"), std::env::var("TWINBASE_REAL_MSSQL_PASSWORD")) else { return };
    let port: u16 = port.parse().unwrap();
    let (_d, app) = app().await;
    let db = format!("tb_core_{}", std::process::id());
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "MS master", "kind": "mssql", "host": "127.0.0.1", "port": port, "user": "sa", "database": "master", "tls": "require" }, "password": pw })).await;
    ok(&app, "db.query", json!({ "connection": "ms-master", "sql": format!("IF DB_ID(N'{db}') IS NOT NULL DROP DATABASE [{db}]\nGO\nCREATE DATABASE [{db}]") })).await;
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "MS shop", "kind": "mssql", "host": "127.0.0.1", "port": port, "user": "sa", "database": db, "ai_access": "read-only" }, "password": pw })).await;
    let c = "ms-shop";
    let test = ok(&app, "connection.test", json!({ "connection": { "id": "x", "name": "x", "kind": "mssql", "host": "127.0.0.1", "port": port, "user": "sa", "database": db }, "password": pw })).await;
    assert_eq!(test["ok"], true, "{test}");
    assert!(test["version"].as_str().unwrap().starts_with("SQL Server"), "{test}");
    ok(&app, "db.query", json!({ "connection": c, "sql": "CREATE TABLE products (id int IDENTITY PRIMARY KEY, name nvarchar(80) NOT NULL, price decimal(8,2) NOT NULL DEFAULT 0, active bit NOT NULL DEFAULT 1);
CREATE TABLE reviews (id int IDENTITY PRIMARY KEY, product_id int NOT NULL REFERENCES products(id), stars tinyint);
INSERT INTO products (name, price) SELECT CONCAT(N'item ', n), n * 1.5 FROM (SELECT TOP 3000 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) n FROM sys.all_objects a CROSS JOIN sys.all_objects b) x;
INSERT INTO reviews (product_id, stars) VALUES (1, 5), (2, 3);" })).await;
    let tables = ok(&app, "db.tables", json!({ "connection": c })).await;
    assert_eq!(tables[0]["schema"], "dbo", "{tables}");
    let page = ok(&app, "db.rows", json!({ "connection": c, "table": "products", "query": { "limit": 5, "offset": 10, "filters": [{ "column": "name", "op": "starts_with", "value": "item 1" }] } })).await;
    assert_eq!(page["total"], 1111, "{page}");
    assert_eq!(page["key_columns"], json!(["id"]));
    let applied = ok(&app, "db.apply", json!({ "connection": c, "table": "products", "changes": { "updates": [{ "key": { "id": 1 }, "values": { "price": 9.99, "active": false } }] } })).await;
    assert_eq!(applied["affected"], 1);

    // Plans: estimated (nothing runs) and actual (writes rolled back).
    let est = ok(&app, "db.explain", json!({ "connection": c, "sql": "SELECT * FROM products WHERE price > 100" })).await;
    assert_eq!(est["format"], "mssql", "{est}");
    assert!(est["roots"][0]["est_rows"].as_f64().is_some(), "{est}");
    assert!(est["hints"].as_array().unwrap().iter().any(|h| h.as_str().unwrap().contains("index on (price)")), "{}", est["hints"]);
    let act = ok(&app, "db.explain", json!({ "connection": c, "sql": "DELETE FROM reviews WHERE stars < 4", "analyze": true })).await;
    assert_eq!(act["analyzed"], true, "{act}");
    let n = ok(&app, "db.query", json!({ "connection": c, "sql": "SELECT COUNT(*) FROM reviews" })).await;
    assert_eq!(n[0]["rows"][0][0], 2, "the analyzed DELETE must be rolled back");
    // The session is back to normal: statements run and report row counts.
    let r = ok(&app, "db.query", json!({ "connection": c, "sql": "UPDATE products SET active = 1 WHERE id = 1" })).await;
    assert_eq!(r[0]["affected"], 1, "{r}");

    // DDL export in T-SQL and converted to PostgreSQL.
    let ddl = ok(&app, "db.ddl", json!({ "connection": c })).await;
    let ddl = ddl["sql"].as_str().unwrap();
    assert!(ddl.contains("[id] int NOT NULL IDENTITY(1,1)") && ddl.contains("[name] nvarchar(80) NOT NULL"), "{ddl}");
    let pg = ok(&app, "db.ddl", json!({ "connection": c, "dialect": "postgres" })).await;
    assert!(pg["sql"].as_str().unwrap().contains("\"price\" numeric(8,2) NOT NULL DEFAULT 0"), "{}", pg["sql"]);

    // Agents on a read-only connection: reads yes, writes and SELECT INTO no.
    let agent = Actor::Agent { name: "Codex".into() };
    assert!(call(&app, "db.query", json!({ "connection": c, "sql": "SELECT TOP 1 name FROM products" }), &agent).await.is_ok());
    assert!(call(&app, "db.query", json!({ "connection": c, "sql": "DELETE FROM reviews" }), &agent).await.is_err());
    assert!(call(&app, "db.query", json!({ "connection": c, "sql": "SELECT * INTO copy FROM products" }), &agent).await.is_err());
    // Schema diff between two schemas, applied with the generated T-SQL, converges.
    ok(&app, "db.query", json!({ "connection": c, "sql": "CREATE SCHEMA next\nGO\nCREATE TABLE next.products (id int IDENTITY PRIMARY KEY, name nvarchar(120) NULL, price decimal(10,2) NOT NULL DEFAULT 1, sku nvarchar(20) NULL);\nCREATE INDEX ix_products_sku ON next.products (sku);\nCREATE TABLE next.reviews (id int IDENTITY PRIMARY KEY, product_id int NOT NULL REFERENCES next.products(id), stars tinyint);" })).await;
    let diff =
        ok(&app, "schema.diff", json!({ "from": { "connection": c, "schema": "dbo" }, "to": { "connection": c, "schema": "next" } })).await;
    let up = diff["up_sql"].as_str().unwrap().to_owned();
    assert!(up.contains("ALTER TABLE [products] ADD [sku]") && up.contains("DROP COLUMN [active]"), "{up}");
    ok(&app, "db.query", json!({ "connection": c, "sql": up })).await;
    let again =
        ok(&app, "schema.diff", json!({ "from": { "connection": c, "schema": "dbo" }, "to": { "connection": c, "schema": "next" } })).await;
    assert_eq!(again["up_sql"], "", "{}", again["up_sql"]);

    // Preview runs on a T-SQL twin built from the real server.
    let preview = ok(&app, "db.preview", json!({ "connection": c, "sql": "DELETE FROM reviews WHERE stars < 4" })).await;
    assert!(preview["ok"].as_bool().unwrap(), "{preview}");
    assert_eq!(preview["expected_affected"], 1, "{preview}");

    ok(&app, "db.query", json!({ "connection": "ms-master", "sql": format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE\nGO\nDROP DATABASE [{db}]") })).await;
}

/// ClickHouse through the core API (docker `clickhouse/clickhouse-server`): set
/// TWINBASE_REAL_CLICKHOUSE_PORT (HTTP) and TWINBASE_REAL_CLICKHOUSE_PASSWORD (user `default`).
#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_connections() {
    let (Ok(port), Ok(pw)) = (std::env::var("TWINBASE_REAL_CLICKHOUSE_PORT"), std::env::var("TWINBASE_REAL_CLICKHOUSE_PASSWORD")) else {
        return;
    };
    let port: u16 = port.parse().unwrap();
    let (_d, app) = app().await;
    let db = format!("tb_core_{}", std::process::id());
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "CH", "kind": "clickhouse", "host": "127.0.0.1", "port": port, "user": "default", "ai_access": "read-only" }, "password": pw })).await;
    let c = "ch";
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}; CREATE TABLE {db}.hits (id UInt64, url String, ms Float64, at DateTime DEFAULT now(), ref Nullable(String)) ENGINE = MergeTree ORDER BY id; INSERT INTO {db}.hits (id, url, ms) SELECT number, concat('/p/', toString(number % 50)), number % 300 FROM numbers(200000)") })).await;
    let tables = ok(&app, "db.tables", json!({ "connection": c, "schema": db })).await;
    assert_eq!(tables[0]["name"], "hits", "{tables}");
    let page = ok(&app, "db.rows", json!({ "connection": c, "schema": db, "table": "hits", "query": { "limit": 10, "filters": [{ "column": "url", "op": "eq", "value": "/p/7" }] } })).await;
    assert_eq!(page["total"], 4000, "{page}");
    let e = call(
        &app,
        "db.apply",
        json!({ "connection": c, "schema": db, "table": "hits", "changes": { "deletes": [{ "id": 1 }] } }),
        &Actor::User,
    )
    .await
    .unwrap_err();
    assert!(e.message.contains("no transactions"), "{e:?}");

    let plan = ok(&app, "db.explain", json!({ "connection": c, "sql": format!("SELECT count() FROM {db}.hits WHERE ms > 250") })).await;
    assert_eq!(plan["format"], "clickhouse", "{plan}");
    assert!(plan["hints"].as_array().unwrap().iter().any(|h| h.as_str().unwrap().contains("read completely")), "{plan}");
    let keyed = ok(&app, "db.explain", json!({ "connection": c, "sql": format!("SELECT * FROM {db}.hits WHERE id = 42") })).await;
    assert!(keyed["hints"].as_array().unwrap().is_empty(), "{keyed}");

    // DDL export in ClickHouse SQL and converted to PostgreSQL.
    let ddl = ok(&app, "db.ddl", json!({ "connection": c, "schema": db })).await;
    let ddl = ddl["sql"].as_str().unwrap();
    assert!(
        ddl.contains("`ref` Nullable(String)") && ddl.contains("ENGINE = MergeTree ORDER BY (`id`)") && ddl.contains("DEFAULT now()"),
        "{ddl}"
    );
    let pg = ok(&app, "db.ddl", json!({ "connection": c, "schema": db, "dialect": "postgres" })).await;
    let pg = pg["sql"].as_str().unwrap();
    assert!(
        pg.contains("\"id\" numeric(39,0) NOT NULL") && pg.contains("\"ms\" double precision NOT NULL") && pg.contains("\"ref\" text,"),
        "{pg}"
    );

    // Schema diff between two databases, applied with the generated ClickHouse SQL, converges.
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("CREATE DATABASE {db}_next; CREATE TABLE {db}_next.hits (id UInt64, url LowCardinality(String), ms Float32, at DateTime DEFAULT now(), country String DEFAULT 'xx') ENGINE = MergeTree ORDER BY id") })).await;
    let diff = ok(
        &app,
        "schema.diff",
        json!({ "from": { "connection": c, "schema": db }, "to": { "connection": c, "schema": format!("{db}_next") } }),
    )
    .await;
    let up = diff["up_sql"].as_str().unwrap().to_owned();
    assert!(
        up.contains("ADD COLUMN `country` String DEFAULT 'xx'")
            && up.contains("DROP COLUMN `ref`")
            && up.contains("MODIFY COLUMN `ms` Float32"),
        "{up}"
    );
    let up = up.replace("ALTER TABLE `hits`", &format!("ALTER TABLE {db}.hits"));
    ok(&app, "db.query", json!({ "connection": c, "sql": up })).await;
    let again = ok(
        &app,
        "schema.diff",
        json!({ "from": { "connection": c, "schema": db }, "to": { "connection": c, "schema": format!("{db}_next") } }),
    )
    .await;
    assert_eq!(again["up_sql"], "", "{}", again["up_sql"]);

    // Agents on a read-only connection.
    let agent = Actor::Agent { name: "Codex".into() };
    assert!(call(&app, "db.query", json!({ "connection": c, "sql": format!("SELECT count() FROM {db}.hits") }), &agent).await.is_ok());
    assert!(
        call(&app, "db.query", json!({ "connection": c, "sql": format!("ALTER TABLE {db}.hits DELETE WHERE id = 1") }), &agent)
            .await
            .is_err()
    );
    assert!(
        call(
            &app,
            "db.query",
            json!({ "connection": c, "sql": format!("INSERT INTO {db}.hits (id, url, ms) VALUES (1, 'x', 1)") }),
            &agent
        )
        .await
        .is_err()
    );
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("DROP DATABASE {db}; DROP DATABASE {db}_next") })).await;
}

/// Elasticsearch through the core API (docker): set TWINBASE_REAL_ES_PORT and
/// TWINBASE_REAL_ES_PASSWORD (user `elastic`).
#[tokio::test(flavor = "multi_thread")]
async fn elasticsearch_connections() {
    let (Ok(port), Ok(pw)) = (std::env::var("TWINBASE_REAL_ES_PORT"), std::env::var("TWINBASE_REAL_ES_PASSWORD")) else { return };
    let port: u16 = port.parse().unwrap();
    let (_d, app) = app().await;
    let idx = format!("tb-core-{}", std::process::id());
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "ES", "kind": "elasticsearch", "host": "127.0.0.1", "port": port, "user": "elastic", "ai_access": "read-only" }, "password": pw })).await;
    let test = ok(&app, "connection.test", json!({ "connection": { "id": "x", "name": "x", "kind": "elasticsearch", "host": "127.0.0.1", "port": port, "user": "elastic" }, "password": pw })).await;
    assert!(test["version"].as_str().unwrap().starts_with("Elasticsearch 8."), "{test}");
    let run = ok(&app, "search.run", json!({ "connection": "es", "command": format!("PUT /{idx}\n{{\"mappings\":{{\"properties\":{{\"title\":{{\"type\":\"text\"}},\"year\":{{\"type\":\"integer\"}}}}}}}}\n\nPOST /_bulk?refresh=true\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"a\"}}}}\n{{\"title\":\"The Rust Book\",\"year\":2018}}\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"b\"}}}}\n{{\"title\":\"Rust in Action\",\"year\":2021}}\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"c\"}}}}\n{{\"title\":\"Go Programming\",\"year\":2015}}") })).await;
    assert_eq!(run["outputs"].as_array().unwrap().len(), 2, "{run}");
    let indices = ok(&app, "search.indices", json!({ "connection": "es" })).await;
    assert!(indices["indices"].as_array().unwrap().iter().any(|i| i["name"] == idx.as_str()), "{indices}");
    let docs =
        ok(&app, "search.documents", json!({ "connection": "es", "index": idx, "query": "title:rust", "sort": [{ "year": "desc" }] }))
            .await;
    assert_eq!(docs["total"], 2, "{docs}");
    assert_eq!(docs["hits"][0]["_id"], "b");
    let dsl = ok(
        &app,
        "search.documents",
        json!({ "connection": "es", "index": idx, "query": { "range": { "year": { "lt": 2020 } } }, "size": 1 }),
    )
    .await;
    assert_eq!(dsl["total"], 2);
    assert_eq!(dsl["hits"].as_array().unwrap().len(), 1);
    let hit = &docs["hits"][0];
    let saved = ok(&app, "search.put", json!({ "connection": "es", "index": idx, "id": "b", "document": { "title": "Rust in Action (2nd ed.)", "year": 2024 }, "if_seq_no": hit["_seq_no"], "if_primary_term": hit["_primary_term"] })).await;
    assert_eq!(saved["result"], "updated");
    let stale = call(&app, "search.put", json!({ "connection": "es", "index": idx, "id": "b", "document": { "title": "x" }, "if_seq_no": hit["_seq_no"], "if_primary_term": hit["_primary_term"] }), &Actor::User).await.unwrap_err();
    assert!(stale.message.contains("changed since it was loaded"), "{stale:?}");
    assert_eq!(ok(&app, "search.delete", json!({ "connection": "es", "index": idx, "ids": ["c"] })).await["deleted"], 1);
    let fields = ok(&app, "search.mapping", json!({ "connection": "es", "index": idx })).await;
    assert!(fields["fields"].as_array().unwrap().iter().any(|f| f["path"] == "year" && f["type"] == "integer"), "{fields}");

    // Agents: searches pass on a read-only connection, writes do not.
    let agent = Actor::Agent { name: "Codex".into() };
    assert!(
        call(&app, "search.run", json!({ "connection": "es", "command": format!("POST /{idx}/_search\n{{\"size\":1}}") }), &agent)
            .await
            .is_ok()
    );
    assert!(
        call(&app, "search.run", json!({ "connection": "es", "command": format!("POST /{idx}/_doc\n{{\"title\":\"x\"}}") }), &agent)
            .await
            .is_err()
    );
    assert!(call(&app, "search.delete", json!({ "connection": "es", "index": idx, "ids": ["a"] }), &agent).await.is_err());
    ok(&app, "search.run", json!({ "connection": "es", "command": format!("DELETE /{idx}") })).await;
}

/// Cassandra through the core API (docker `cassandra:5.0`): set TWINBASE_REAL_CASSANDRA_PORT.
#[tokio::test(flavor = "multi_thread")]
async fn cassandra_connections() {
    let Ok(port) = std::env::var("TWINBASE_REAL_CASSANDRA_PORT") else { return };
    let port: u16 = port.parse().unwrap();
    let (_d, app) = app().await;
    let ks = format!("tb_core_{}", std::process::id());
    ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "Cass", "kind": "cassandra", "host": "127.0.0.1", "port": port, "user": "", "ai_access": "read-only" } })).await;
    let c = "cass";
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}; CREATE TABLE {ks}.orders (customer text, placed timestamp, id uuid, total decimal, items list<text>, PRIMARY KEY (customer, placed)) WITH CLUSTERING ORDER BY (placed DESC); INSERT INTO {ks}.orders (customer, placed, id, total, items) VALUES ('ann', '2024-01-02 10:00:00+0000', uuid(), 25.50, ['lamp']); INSERT INTO {ks}.orders (customer, placed, id, total) VALUES ('ann', '2024-01-03 10:00:00+0000', uuid(), 5); INSERT INTO {ks}.orders (customer, placed, id, total) VALUES ('bob', '2024-01-01 10:00:00+0000', uuid(), 99)") })).await;
    let tables = ok(&app, "db.tables", json!({ "connection": c, "schema": ks })).await;
    assert_eq!(tables[0]["name"], "orders", "{tables}");
    let page = ok(&app, "db.rows", json!({ "connection": c, "schema": ks, "table": "orders", "query": { "limit": 10 } })).await;
    assert_eq!(page["result"]["rows"].as_array().unwrap().len(), 3, "{page}");
    assert_eq!(page["key_columns"], json!(["customer", "placed"]));
    let row = page["result"]["rows"].as_array().unwrap().iter().find(|r| r[0] == "bob").unwrap().clone();
    let applied = ok(&app, "db.apply", json!({ "connection": c, "schema": ks, "table": "orders", "changes": { "updates": [{ "key": { "customer": "bob", "placed": row[1] }, "values": { "total": "100.00", "items": ["desk"] } }] } })).await;
    assert_eq!(applied["affected"], 1, "{applied}");
    let r = ok(&app, "db.query", json!({ "connection": c, "sql": format!("SELECT total, items FROM {ks}.orders WHERE customer = 'bob'") }))
        .await;
    assert_eq!(r[0]["rows"][0], json!(["100.00", ["desk"]]));

    // DDL export in CQL (partition and clustering key) and converted to PostgreSQL.
    let ddl = ok(&app, "db.ddl", json!({ "connection": c, "schema": ks })).await;
    let ddl = ddl["sql"].as_str().unwrap();
    assert!(ddl.contains("\"items\" list<text>") && ddl.contains("PRIMARY KEY (\"customer\", \"placed\")"), "{ddl}");
    let pg = ok(&app, "db.ddl", json!({ "connection": c, "schema": ks, "dialect": "postgres" })).await;
    assert!(pg["sql"].as_str().unwrap().contains("\"placed\" timestamp with time zone NOT NULL"), "{}", pg["sql"]);
    let plan =
        ok(&app, "db.explain", json!({ "connection": c, "sql": format!("SELECT * FROM {ks}.orders WHERE total > 10 ALLOW FILTERING") }))
            .await;
    assert!(plan["hints"][0].as_str().unwrap().contains("ALLOW FILTERING"), "{plan}");

    // Schema diff (add/drop columns) applied and converging.
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("CREATE KEYSPACE {ks}_next WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}; CREATE TABLE {ks}_next.orders (customer text, placed timestamp, id uuid, total decimal, note text, PRIMARY KEY (customer, placed))") })).await;
    let diff = ok(
        &app,
        "schema.diff",
        json!({ "from": { "connection": c, "schema": ks }, "to": { "connection": c, "schema": format!("{ks}_next") } }),
    )
    .await;
    let up = diff["up_sql"].as_str().unwrap().to_owned();
    assert!(up.contains("ALTER TABLE \"orders\" ADD \"note\" text") && up.contains("ALTER TABLE \"orders\" DROP \"items\""), "{up}");
    ok(&app, "db.query", json!({ "connection": c, "sql": up.replace("ALTER TABLE \"orders\"", &format!("ALTER TABLE {ks}.orders")) }))
        .await;
    let again = ok(
        &app,
        "schema.diff",
        json!({ "from": { "connection": c, "schema": ks }, "to": { "connection": c, "schema": format!("{ks}_next") } }),
    )
    .await;
    assert_eq!(again["up_sql"], "", "{}", again["up_sql"]);

    let agent = Actor::Agent { name: "Codex".into() };
    assert!(
        call(&app, "db.query", json!({ "connection": c, "sql": format!("SELECT * FROM {ks}.orders WHERE customer = 'ann'") }), &agent)
            .await
            .is_ok()
    );
    assert!(
        call(&app, "db.query", json!({ "connection": c, "sql": format!("DELETE FROM {ks}.orders WHERE customer = 'ann'") }), &agent)
            .await
            .is_err()
    );
    let e = call(&app, "db.preview", json!({ "connection": c, "sql": "DELETE FROM x" }), &Actor::User).await.unwrap_err();
    assert!(e.message.contains("not available for Cassandra"));
    ok(&app, "db.query", json!({ "connection": c, "sql": format!("DROP KEYSPACE {ks}; DROP KEYSPACE {ks}_next") })).await;
}

/// Cloud Spanner and BigQuery through the core API, on their emulators: set
/// TWINBASE_SPANNER_EMULATOR_PORT (REST) and TWINBASE_BIGQUERY_EMULATOR_PORT.
#[tokio::test(flavor = "multi_thread")]
async fn google_cloud_sql_connections() {
    let (_d, app) = app().await;
    let curl = |url: String, body: Value| async move {
        let out = tokio::process::Command::new("curl")
            .args(["-s", "-X", "POST", &url, "-H", "Content-Type: application/json", "-d", &body.to_string()])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    if let Some(port) = std::env::var("TWINBASE_SPANNER_EMULATOR_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        let base = format!("http://127.0.0.1:{port}/v1/projects/tb-test/instances");
        curl(base.clone(), json!({ "instanceId": "tb", "instance": { "config": "emulator-config", "displayName": "tb", "nodeCount": 1 } }))
            .await;
        let (a, b) = (format!("core{}a", std::process::id()), format!("core{}b", std::process::id()));
        for db in [&a, &b] {
            curl(format!("{base}/tb/databases"), json!({ "createStatement": format!("CREATE DATABASE `{db}`") })).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        for (name, db) in [("Span A", &a), ("Span B", &b)] {
            ok(&app, "connection.save", json!({ "connection": { "id": "", "name": name, "kind": "spanner", "host": "127.0.0.1", "port": port, "user": "", "database": format!("tb-test/tb/{db}"), "ai_access": "read-only" } })).await;
        }
        ok(&app, "db.query", json!({ "connection": "span-a", "sql": "CREATE TABLE Items (Id INT64 NOT NULL, Name STRING(80) NOT NULL, Price NUMERIC) PRIMARY KEY (Id); INSERT INTO Items (Id, Name, Price) VALUES (1, 'Lamp', NUMERIC '25.5'), (2, 'Desk', NUMERIC '200')" })).await;
        ok(&app, "db.query", json!({ "connection": "span-b", "sql": "CREATE TABLE Items (Id INT64 NOT NULL, Name STRING(MAX), Price NUMERIC, Stock INT64) PRIMARY KEY (Id); CREATE INDEX ItemsByName ON Items (Name)" })).await;
        let tables = ok(&app, "db.tables", json!({ "connection": "span-a" })).await;
        assert_eq!(tables[0]["name"], "Items", "{tables}");
        let page = ok(&app, "db.rows", json!({ "connection": "span-a", "table": "Items", "query": { "limit": 10, "sort": [{ "column": "Price", "descending": true }] } })).await;
        assert_eq!(page["result"]["rows"][0][1], "Desk", "{page}");
        let applied = ok(&app, "db.apply", json!({ "connection": "span-a", "table": "Items", "changes": { "updates": [{ "key": { "Id": 1 }, "values": { "Price": "19.99" } }] } })).await;
        assert_eq!(applied["affected"], 1);
        let plan = ok(&app, "db.explain", json!({ "connection": "span-a", "sql": "SELECT * FROM Items WHERE Price > 10" })).await;
        assert_eq!(plan["format"], "spanner", "{plan}");
        let ddl = ok(&app, "db.ddl", json!({ "connection": "span-a" })).await;
        assert!(
            ddl["sql"].as_str().unwrap().contains("`Name` STRING(80) NOT NULL")
                && ddl["sql"].as_str().unwrap().contains(") PRIMARY KEY (`Id`);"),
            "{ddl}"
        );
        ok(&app, "db.query", json!({ "connection": "span-a", "sql": "CREATE TABLE Notes (Id INT64 NOT NULL, NoteId INT64 NOT NULL, Body STRING(MAX)) PRIMARY KEY (Id, NoteId), INTERLEAVE IN PARENT Items ON DELETE CASCADE" })).await;
        let ddl = ok(&app, "db.ddl", json!({ "connection": "span-a" })).await;
        assert!(
            ddl["sql"].as_str().unwrap().contains(") PRIMARY KEY (`Id`, `NoteId`),\n  INTERLEAVE IN PARENT `Items` ON DELETE CASCADE;"),
            "{ddl}"
        );
        ok(&app, "db.query", json!({ "connection": "span-a", "sql": "DROP TABLE Notes" })).await;
        let pg = ok(&app, "db.ddl", json!({ "connection": "span-a", "dialect": "postgres" })).await;
        assert!(pg["sql"].as_str().unwrap().contains("\"Name\" character varying(80) NOT NULL"), "{pg}");
        let diff = ok(&app, "schema.diff", json!({ "from": { "connection": "span-a" }, "to": { "connection": "span-b" } })).await;
        let up = diff["up_sql"].as_str().unwrap().to_owned();
        assert!(
            up.contains("ALTER TABLE `Items` ADD COLUMN `Stock` INT64")
                && up.contains("ALTER COLUMN `Name` STRING(MAX)")
                && up.contains("CREATE INDEX `ItemsByName`"),
            "{up}"
        );
        ok(&app, "db.query", json!({ "connection": "span-a", "sql": up })).await;
        let again = ok(&app, "schema.diff", json!({ "from": { "connection": "span-a" }, "to": { "connection": "span-b" } })).await;
        assert_eq!(again["up_sql"], "", "{}", again["up_sql"]);
        let agent = Actor::Agent { name: "Codex".into() };
        assert!(call(&app, "db.query", json!({ "connection": "span-a", "sql": "SELECT * FROM Items" }), &agent).await.is_ok());
        assert!(call(&app, "db.query", json!({ "connection": "span-a", "sql": "DELETE FROM Items WHERE TRUE" }), &agent).await.is_err());
    }
    if let Some(port) = std::env::var("TWINBASE_BIGQUERY_EMULATOR_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        let ds = format!("core_{}", std::process::id());
        // (The emulator mixes up tables of the same name in different datasets: unique names.)
        let t = format!("sales_{}", std::process::id());
        curl(
            format!("http://127.0.0.1:{port}/bigquery/v2/projects/tb-test/datasets"),
            json!({ "datasetReference": { "projectId": "tb-test", "datasetId": ds } }),
        )
        .await;
        ok(&app, "connection.save", json!({ "connection": { "id": "", "name": "BQ", "kind": "bigquery", "host": "127.0.0.1", "port": port, "user": "", "database": format!("tb-test.{ds}") } })).await;
        ok(&app, "db.query", json!({ "connection": "bq", "sql": format!("CREATE TABLE {t} (id INT64, region STRING, amount FLOAT64); INSERT INTO {t} VALUES (1, 'eu', 10.5), (2, 'us', 3), (3, 'eu', 7)") })).await;
        let tables = ok(&app, "db.tables", json!({ "connection": "bq" })).await;
        assert!(tables.as_array().unwrap().iter().any(|x| x["name"] == t.as_str() && x["schema"] == ds.as_str()), "{tables}");
        let page = ok(&app, "db.rows", json!({ "connection": "bq", "table": t, "query": { "limit": 10 } })).await;
        assert_eq!(page["total"], 3, "{page}");
        let r = ok(
            &app,
            "db.query",
            json!({ "connection": "bq", "sql": format!("SELECT region, SUM(amount) AS total FROM {t} GROUP BY region ORDER BY region") }),
        )
        .await;
        assert_eq!(r[0]["rows"], json!([["eu", 17.5], ["us", 3.0]]));
        let ddl = ok(&app, "db.ddl", json!({ "connection": "bq" })).await;
        assert!(ddl["sql"].as_str().unwrap().contains(&format!("CREATE TABLE `{t}` (\n  `id` INT64,")), "{ddl}");
        let e = call(&app, "db.preview", json!({ "connection": "bq", "sql": "DELETE FROM x WHERE TRUE" }), &Actor::User).await.unwrap_err();
        assert!(e.message.contains("not available for Spanner and BigQuery"));
    }
    app.stop_all_mocks().await;
}

/// Oracle without a server: DDL converted from a PostgreSQL mock, and the message when Oracle
/// Instant Client is missing (the end-to-end suite is crates/client/tests/oracle.rs).
#[tokio::test(flavor = "multi_thread")]
async fn oracle_ddl_and_missing_client() {
    let (_d, app) = app().await;
    let conn = create_pg_mock(&app).await;
    ok(&app, "db.query", json!({ "connection": conn, "sql": "CREATE TABLE accounts (id serial PRIMARY KEY, email varchar(200) NOT NULL UNIQUE, balance numeric(12,2) NOT NULL DEFAULT 0, active boolean DEFAULT true, created timestamptz)" })).await;
    let ddl = ok(&app, "db.ddl", json!({ "connection": conn, "dialect": "oracle" })).await;
    let ddl = ddl["sql"].as_str().unwrap();
    for part in [
        "\"id\" NUMBER(10) GENERATED BY DEFAULT AS IDENTITY NOT NULL",
        "\"email\" VARCHAR2(200 CHAR) NOT NULL",
        "\"balance\" NUMBER(12,2) DEFAULT 0 NOT NULL",
        "\"active\" NUMBER(3) DEFAULT 1",
        "\"created\" TIMESTAMP WITH TIME ZONE",
    ] {
        assert!(ddl.contains(part), "{part} missing in\n{ddl}");
    }
    if std::env::var_os("TWINBASE_ORACLE_CLIENT").is_none() {
        let t = ok(&app, "connection.test", json!({ "connection": { "id": "x", "name": "x", "kind": "oracle", "host": "127.0.0.1", "port": 1, "user": "app", "database": "FREEPDB1" }, "password": "x" })).await;
        assert_eq!(t["ok"], false);
        let e = t["error"].as_str().unwrap();
        assert!(e.contains("Instant Client") || e.contains("ORA-") || e.contains("DPI-"), "{e}");
    }
    app.stop_all_mocks().await;
}
