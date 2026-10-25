//! The native PostgreSQL engine behind the traffic/fault proxy. Needs PostgreSQL server binaries:
//! set `TWINBASE_PG_BIN` to their bin folder (skipped otherwise).

use tokio_postgres::NoTls;
use twinbase_mock::{AuthMode, FaultAction, FaultConfig, FaultRule, MockConfig, MockKind, NativeEngine, TrafficKind, start};

fn bins() -> Option<std::path::PathBuf> {
    std::env::var_os("TWINBASE_PG_BIN").map(Into::into)
}

#[tokio::test]
async fn real_postgres_through_the_proxy() {
    let Some(bin) = bins() else { return };
    let dir = tempfile::tempdir().unwrap();
    let cfg = |auth: AuthMode| MockConfig {
        kind: MockKind::Postgres,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: Some(dir.path().to_path_buf()),
        auth,
        faults: Default::default(),
        native: Some(NativeEngine { bin: Some(bin.clone()) }),
    };
    let m = start(cfg(AuthMode::Any)).await.unwrap();
    let port = m.addr.port();
    // Any user and database work: they are created on first use.
    let (c, conn) =
        tokio_postgres::connect(&format!("host=127.0.0.1 port={port} user=app password=whatever dbname=shop"), NoTls).await.unwrap();
    tokio::spawn(conn);
    let v: String = c.query_one("SELECT version()", &[]).await.unwrap().get(0);
    assert!(v.starts_with("PostgreSQL 1"), "{v}");
    c.batch_execute("CREATE TABLE t (id serial PRIMARY KEY, n int); INSERT INTO t (n) SELECT g FROM generate_series(1, 5) g;")
        .await
        .unwrap();
    let rows = c.query("SELECT n FROM t WHERE n > $1 ORDER BY n", &[&2i32]).await.unwrap();
    assert_eq!(rows.len(), 3);
    // jsonb, window functions, a real PL/pgSQL function: things TwinLite does not emulate.
    c.batch_execute("CREATE FUNCTION twice(x int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN x * 2; END $$").await.unwrap();
    let r = c.query_one("SELECT twice(21), jsonb_build_object('a', 1)->>'a', sum(n) OVER () FROM t LIMIT 1", &[]).await.unwrap();
    assert_eq!(r.get::<_, i32>(0), 42);

    // Faults: a simple-protocol error and an extended-protocol error keep the session usable.
    m.faults
        .set(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![FaultRule {
                enabled: true,
                name: "deadlock".into(),
                matches: "update t".into(),
                probability: 1.0,
                action: FaultAction::Error { code: Some("40P01".into()), message: Some("deadlock detected (injected)".into()) },
            }],
        })
        .unwrap();
    let e = c.batch_execute("UPDATE t SET n = 0").await.unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code().code(), "40P01");
    let e = c.execute("UPDATE t SET n = $1", &[&0i32]).await.unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code().code(), "40P01");
    m.faults.set(FaultConfig::default()).unwrap();
    assert_eq!(
        c.query_one("SELECT count(*) FROM t WHERE n = 0", &[]).await.unwrap().get::<_, i64>(0),
        0,
        "the failed statements did not run"
    );
    let n = c.execute("UPDATE t SET n = n + 1", &[]).await.unwrap();
    assert_eq!(n, 5);

    // COPY and LISTEN/NOTIFY pass through the proxy.
    use futures::SinkExt;
    let sink = c.copy_in("COPY t (n) FROM STDIN").await.unwrap();
    futures::pin_mut!(sink);
    sink.send(bytes::Bytes::from_static(b"100\n200\n")).await.unwrap();
    assert_eq!(sink.finish().await.unwrap(), 2);
    c.batch_execute("DELETE FROM t WHERE n >= 100").await.unwrap();

    let events = m.traffic.since(0, 500);
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, TrafficKind::Connect { user: Some(u), database: Some(d), .. } if u == "app" && d == "shop"))
    );
    assert!(
        events.iter().any(|e| matches!(&e.kind, TrafficKind::Query { text, rows: 3, error: None, .. } if text.contains("WHERE n > $1")))
    );
    assert!(events.iter().any(|e| matches!(&e.kind, TrafficKind::Query { text, rows: 5, .. } if text == "UPDATE t SET n = n + 1")));
    drop(c);
    m.stop().await;

    // Data persists; a password-protected mock checks the password.
    let m = start(cfg(AuthMode::Password { user: "app".into(), password: "pw".into() })).await.unwrap();
    let port = m.addr.port();
    assert!(tokio_postgres::connect(&format!("host=127.0.0.1 port={port} user=app password=nope dbname=shop"), NoTls).await.is_err());
    let (c, conn) = tokio_postgres::connect(&format!("host=127.0.0.1 port={port} user=app password=pw dbname=shop"), NoTls).await.unwrap();
    tokio::spawn(conn);
    assert_eq!(c.query_one("SELECT sum(n) FROM t", &[]).await.unwrap().get::<_, i64>(0), 20);
    drop(c);
    m.stop().await;
}

#[test]
fn detects_a_bin_folder() {
    let Some(bin) = bins() else { return };
    let found = twinbase_mock::native_pg::probe(&bin).unwrap();
    assert!(found.version.starts_with("1"), "{found:?}");
    assert!(twinbase_mock::native_pg::probe(std::path::Path::new("/nonexistent")).is_none());
}
