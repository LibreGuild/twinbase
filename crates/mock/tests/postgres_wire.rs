//! The PostgreSQL mock driven by a real driver (tokio-postgres): simple and extended protocol,
//! binary parameters and results, transactions and SQLSTATE errors.

use tokio_postgres::{NoTls, SimpleQueryMessage};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

async fn connect(port: u16) -> tokio_postgres::Client {
    let (client, conn) =
        tokio_postgres::connect(&format!("host=127.0.0.1 port={port} user=dev password=secret dbname=app"), NoTls).await.expect("connect");
    tokio::spawn(conn);
    client
}

async fn mock() -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Postgres,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn simple_and_extended_queries() {
    let m = mock().await;
    let c = connect(m.addr.port()).await;
    c.batch_execute(
        "CREATE TABLE users (id serial primary key, email text not null unique, active boolean default true,
          score numeric(6,2), created_at timestamptz default now(), born date, uid uuid default gen_random_uuid(), meta jsonb, tags text[])",
    )
    .await
    .unwrap();

    // Typed binary parameters and results.
    let born = chrono::NaiveDate::from_ymd_opt(1990, 5, 17).unwrap();
    let row = c
        .query_one(
            "INSERT INTO users (email, active, born, meta, tags) VALUES ($1, $2, $3, $4, $5) RETURNING id, uid, created_at",
            &[&"a@x.io", &false, &born, &serde_json::json!({"k": [1, 2]}), &vec!["x".to_string(), "y z".to_string()]],
        )
        .await
        .unwrap();
    let id: i32 = row.get(0);
    let _uid: uuid::Uuid = row.get(1);
    let created: chrono::DateTime<chrono::Utc> = row.get(2);
    assert_eq!(id, 1);
    assert!((chrono::Utc::now() - created).num_seconds().abs() < 60);

    let row = c.query_one("SELECT email, active, born, meta, tags, score IS NULL FROM users WHERE id = $1", &[&id]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), "a@x.io");
    assert!(!row.get::<_, bool>(1));
    assert_eq!(row.get::<_, chrono::NaiveDate>(2), born);
    assert_eq!(row.get::<_, serde_json::Value>(3), serde_json::json!({"k": [1, 2]}));
    assert_eq!(row.get::<_, Vec<String>>(4), vec!["x".to_string(), "y z".to_string()]);
    assert!(row.get::<_, bool>(5));

    let n: i64 = c.query_one("SELECT count(*) FROM users WHERE active = $1", &[&false]).await.unwrap().get(0);
    assert_eq!(n, 1);

    // Simple protocol returns text.
    let msgs = c.simple_query("SELECT id, active, born FROM users").await.unwrap();
    let SimpleQueryMessage::Row(r) = &msgs[1] else { panic!("expected row: {msgs:?}") };
    assert_eq!(r.get(1), Some("f"));
    assert_eq!(r.get(2), Some("1990-05-17"));
    m.stop().await;
}

#[tokio::test]
async fn transactions_and_errors() {
    let m = mock().await;
    let mut c = connect(m.addr.port()).await;
    c.batch_execute("CREATE TABLE t (id int primary key, v text)").await.unwrap();
    {
        let tx = c.transaction().await.unwrap();
        tx.execute("INSERT INTO t VALUES (1, 'a')", &[]).await.unwrap();
        tx.rollback().await.unwrap();
    }
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
    assert_eq!(n, 0);
    {
        let tx = c.transaction().await.unwrap();
        tx.execute("INSERT INTO t VALUES ($1, $2)", &[&1i32, &"a"]).await.unwrap();
        tx.commit().await.unwrap();
    }
    let err = c.execute("INSERT INTO t VALUES (1, 'dup')", &[]).await.unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code().code(), "23505");
    assert_eq!(db.message(), "duplicate key value violates unique constraint \"t_pkey\"");
    let err = c.query("SELECT * FROM missing", &[]).await.unwrap_err();
    assert_eq!(err.as_db_error().unwrap().code().code(), "42P01");
    // The connection still works after errors.
    let v: String = c.query_one("SELECT v FROM t WHERE id = 1", &[]).await.unwrap().get(0);
    assert_eq!(v, "a");
    let affected = c.execute("UPDATE t SET v = $1 WHERE id = $2", &[&"b", &1i32]).await.unwrap();
    assert_eq!(affected, 1);
    m.stop().await;
}

#[tokio::test]
async fn password_auth_and_persistence() {
    let dir = std::env::temp_dir().join(format!("tb-pg-{}", uuid::Uuid::new_v4()));
    let config = MockConfig {
        kind: MockKind::Postgres,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: Some(dir.clone()),
        auth: AuthMode::Password { user: "dev".into(), password: "secret".into() },
        faults: Default::default(),
        native: None,
    };
    let m = start(config.clone()).await.unwrap();
    let port = m.addr.port();
    let c = connect(port).await;
    c.batch_execute("CREATE TABLE notes (id serial primary key, body text); INSERT INTO notes (body) VALUES ('hello')").await.unwrap();
    let bad = tokio_postgres::connect(&format!("host=127.0.0.1 port={port} user=dev password=wrong dbname=app"), NoTls).await;
    assert!(bad.is_err());
    drop(c);
    m.stop().await;
    // Data survives a restart.
    let m = start(config).await.unwrap();
    let c = connect(m.addr.port()).await;
    let body: String = c.query_one("SELECT body FROM notes", &[]).await.unwrap().get(0);
    assert_eq!(body, "hello");
    let id: i32 = c.query_one("INSERT INTO notes (body) VALUES ('again') RETURNING id", &[]).await.unwrap().get(0);
    assert_eq!(id, 2);
    m.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn fault_injection_errors_delays_drops_and_refusals() {
    use twinbase_mock::{FaultAction, FaultConfig, FaultRule};
    let m = mock().await;
    let c = connect(m.addr.port()).await;
    c.batch_execute("CREATE TABLE t (id int)").await.unwrap();
    let rule = |matches: &str, action| FaultRule { enabled: true, name: String::new(), matches: matches.into(), probability: 1.0, action };
    m.faults
        .set(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![
                rule("slow", FaultAction::Delay { ms: 150, jitter_ms: 0 }),
                rule("insert", FaultAction::Error { code: Some("40P01".into()), message: Some("deadlock detected".into()) }),
            ],
        })
        .unwrap();
    // Errors carry the chosen SQLSTATE, in both simple and extended protocol.
    let e = c.batch_execute("INSERT INTO t VALUES (1)").await.unwrap_err();
    assert_eq!(e.code().unwrap().code(), "40P01");
    let e = c.execute("INSERT INTO t VALUES ($1)", &[&2i32]).await.unwrap_err();
    assert_eq!(e.code().unwrap().code(), "40P01");
    // Delays apply only to matching statements.
    let started = std::time::Instant::now();
    c.simple_query("SELECT 'slow'").await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(150));
    // Drop terminates the connection.
    m.faults.set(FaultConfig { enabled: true, refuse_connections: false, rules: vec![rule("boom", FaultAction::Drop)] }).unwrap();
    assert!(c.simple_query("SELECT 'boom'").await.is_err());
    assert!(c.is_closed() || c.simple_query("SELECT 1").await.is_err());
    // Refusing connections makes new clients fail; existing configuration can be switched off live.
    m.faults.set(FaultConfig { enabled: true, refuse_connections: true, rules: vec![] }).unwrap();
    let refused =
        tokio_postgres::connect(&format!("host=127.0.0.1 port={} user=dev dbname=app", m.addr.port()), tokio_postgres::NoTls).await;
    assert!(refused.is_err());
    m.faults.set(FaultConfig::default()).unwrap();
    let c2 = connect(m.addr.port()).await;
    c2.batch_execute("INSERT INTO t VALUES (3)").await.unwrap();
    let errors: Vec<_> = m
        .traffic
        .since(0, 500)
        .into_iter()
        .filter(|e| matches!(&e.kind, twinbase_mock::TrafficKind::Query { error: Some(err), .. } if err.starts_with("[fault]")))
        .collect();
    assert!(errors.len() >= 4, "faults are visible in the traffic log");
    m.stop().await;
}
