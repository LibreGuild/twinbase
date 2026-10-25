//! The MySQL mock driven by a real driver (mysql_async): text and binary protocol, typed values,
//! LAST_INSERT_ID, transactions and MySQL error codes.

use mysql_async::prelude::*;
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

async fn mock(auth: AuthMode) -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Mysql,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap()
}

fn opts(port: u16, db: Option<&str>, pass: &str) -> mysql_async::Opts {
    mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some(pass))
        .db_name(db)
        .prefer_socket(false)
        .into()
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_and_prepared_statements() {
    let m = mock(AuthMode::Any).await;
    let mut c = mysql_async::Conn::new(opts(m.addr.port(), None, "x")).await.unwrap();
    c.query_drop("CREATE DATABASE shop").await.unwrap();
    c.query_drop("USE shop").await.unwrap();
    c.query_drop(
        "CREATE TABLE products (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(80) NOT NULL, price DECIMAL(8,2), \
         in_stock TINYINT(1) DEFAULT 1, added DATETIME DEFAULT CURRENT_TIMESTAMP, released DATE)",
    )
    .await
    .unwrap();
    c.exec_drop("INSERT INTO products (name, price, released) VALUES (?, ?, ?)", ("Lamp", 19.5, "2024-02-03")).await.unwrap();
    assert_eq!(c.last_insert_id(), Some(1));
    c.exec_drop("INSERT INTO products (name, price) VALUES (?, ?)", ("Desk", 120)).await.unwrap();
    assert_eq!(c.last_insert_id(), Some(2));

    // Binary protocol with typed results.
    let rows: Vec<(i32, String, String, i8, Option<chrono::NaiveDate>)> =
        c.exec("SELECT id, name, price, in_stock, released FROM products WHERE price > ? ORDER BY id", (10,)).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "Lamp");
    assert_eq!(rows[0].2, "19.50");
    assert_eq!(rows[0].3, 1);
    assert_eq!(rows[0].4, chrono::NaiveDate::from_ymd_opt(2024, 2, 3));
    assert_eq!(rows[1].4, None);

    // Text protocol.
    let names: Vec<String> = c.query("SELECT name FROM products ORDER BY name").await.unwrap();
    assert_eq!(names, vec!["Desk", "Lamp"]);
    let count: Option<i64> = c.query_first("SELECT COUNT(*) FROM products").await.unwrap();
    assert_eq!(count, Some(2));
    let added: Option<chrono::NaiveDateTime> = c.query_first("SELECT added FROM products WHERE id = 1").await.unwrap();
    assert!(added.is_some());
    let version: Option<String> = c.query_first("SELECT @@version").await.unwrap();
    assert!(version.unwrap().starts_with("8.0"));

    // Errors keep MySQL codes.
    let err = c.query_drop("INSERT INTO products (id, name) VALUES (1, 'dup')").await.unwrap_err();
    match err {
        mysql_async::Error::Server(e) => assert_eq!(e.code, 1062),
        other => panic!("unexpected {other:?}"),
    }
    let err = c.query_drop("SELECT * FROM nope").await.unwrap_err();
    match err {
        mysql_async::Error::Server(e) => assert_eq!(e.code, 1146),
        other => panic!("unexpected {other:?}"),
    }

    // Transactions.
    let mut tx = c.start_transaction(mysql_async::TxOpts::default()).await.unwrap();
    tx.query_drop("DELETE FROM products").await.unwrap();
    tx.rollback().await.unwrap();
    let count: Option<i64> = c.query_first("SELECT COUNT(*) FROM products").await.unwrap();
    assert_eq!(count, Some(2));
    c.disconnect().await.unwrap();
    m.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn password_auth_and_database_selection() {
    let m = mock(AuthMode::Password { user: "root".into(), password: "pw".into() }).await;
    let mut c = mysql_async::Conn::new(opts(m.addr.port(), None, "pw")).await.unwrap();
    c.query_drop("CREATE DATABASE app").await.unwrap();
    c.disconnect().await.unwrap();
    assert!(mysql_async::Conn::new(opts(m.addr.port(), None, "wrong")).await.is_err());
    let mut c = mysql_async::Conn::new(opts(m.addr.port(), Some("app"), "pw")).await.unwrap();
    let db: Option<String> = c.query_first("SELECT DATABASE()").await.unwrap();
    assert_eq!(db.as_deref(), Some("app"));
    assert!(mysql_async::Conn::new(opts(m.addr.port(), Some("missing"), "pw")).await.is_err());
    c.disconnect().await.unwrap();
    m.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fault_injection_uses_mysql_error_numbers() {
    use twinbase_mock::{FaultAction, FaultConfig, FaultRule};
    let m = mock(AuthMode::Any).await;
    let mut c = mysql_async::Conn::new(opts(m.addr.port(), None, "x")).await.unwrap();
    m.faults
        .set(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![FaultRule {
                enabled: true,
                name: "deadlocks".into(),
                matches: "update".into(),
                probability: 1.0,
                action: FaultAction::Error { code: Some("1213".into()), message: Some("Deadlock found when trying to get lock".into()) },
            }],
        })
        .unwrap();
    c.query_drop("CREATE DATABASE f").await.unwrap();
    let err = c.query_drop("UPDATE f.t SET x = 1").await.unwrap_err();
    match err {
        mysql_async::Error::Server(e) => assert_eq!(e.code, 1213),
        other => panic!("unexpected {other:?}"),
    }
    m.faults.set(FaultConfig { enabled: true, refuse_connections: true, rules: vec![] }).unwrap();
    assert!(mysql_async::Conn::new(opts(m.addr.port(), None, "x")).await.is_err());
}
