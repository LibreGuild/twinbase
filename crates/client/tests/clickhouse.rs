//! ClickHouse client against a real server: set `TWINBASE_REAL_CLICKHOUSE_PORT` (HTTP interface) and
//! `TWINBASE_REAL_CLICKHOUSE_PASSWORD` (user `default`), e.g. with
//! `docker run -e CLICKHOUSE_PASSWORD=… -p 127.0.0.1:58123:8123 clickhouse/clickhouse-server`.

use serde_json::json;
use twinbase_client::sqlgen::{ChangeSet, Filter, FilterOp, RowQuery, SortKey};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode, apply_changes, fetch_rows};

fn config(database: Option<&str>) -> Option<ConnectionConfig> {
    let port = std::env::var("TWINBASE_REAL_CLICKHOUSE_PORT").ok()?.parse().ok()?;
    Some(ConnectionConfig {
        kind: DbKind::Clickhouse,
        host: "127.0.0.1".into(),
        port,
        user: "default".into(),
        password: std::env::var("TWINBASE_REAL_CLICKHOUSE_PASSWORD").ok(),
        database: database.map(Into::into),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 10,
        ssh: None,
        via: None,
    })
}

#[tokio::test]
async fn clickhouse_end_to_end() {
    let Some(cfg) = config(None) else { return };
    let admin = Client::connect(&cfg).await.unwrap();
    let a = admin.sql().unwrap();
    let db = format!("tb_test_{}", std::process::id());
    a.query(&format!("DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}"), 10).await.unwrap();

    let c = Client::connect(&config(Some(&db)).unwrap()).await.unwrap();
    let s = c.sql().unwrap();
    assert!(s.server_version().await.unwrap().starts_with("ClickHouse "));
    let r = s
        .query(
            "CREATE TABLE events (id UInt64, ts DateTime64(3), user LowCardinality(String), amount Decimal(10, 2) DEFAULT 0, tags Array(String), note Nullable(String), day Date MATERIALIZED toDate(ts), INDEX ix_user user TYPE set(100) GRANULARITY 4) ENGINE = MergeTree ORDER BY (user, id);
INSERT INTO events (id, ts, user, amount, tags, note) VALUES (1, '2024-01-01 10:00:00.5', 'ann', 12.5, ['a', 'b'], NULL), (2, '2024-01-02 11:00:00', 'bob', 3, [], 'it''s'), (18446744073709551615, '2024-01-03 12:00:00', 'ann', 99.99, ['x'], 'big');
SELECT user, count() AS n, sum(amount) AS total, max(id) AS top FROM events GROUP BY user ORDER BY user;",
            100,
        )
        .await
        .unwrap();
    assert_eq!(r[1].affected, Some(3), "{:?}", r[1]);
    assert_eq!(
        r[2].rows,
        vec![
            vec![json!("ann"), json!(2), json!("112.49"), json!("18446744073709551615")],
            vec![json!("bob"), json!(1), json!("3"), json!(2)]
        ]
    );
    assert_eq!(r[2].columns[2].type_name, "Decimal(38, 2)");

    // Session state survives between calls (temporary tables, settings).
    s.query("CREATE TEMPORARY TABLE scratch (v UInt8); INSERT INTO scratch VALUES (7)", 10).await.unwrap();
    assert_eq!(s.query("SELECT v FROM scratch", 10).await.unwrap()[0].rows, vec![vec![json!(7)]]);

    // Errors carry ClickHouse's code name and the statement index.
    let e = s.query("SELECT 1; SELECT * FROM nope", 10).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("UNKNOWN_TABLE"), "{e:?}");
    assert_eq!(e.statement_index, Some(1));
    let e = s.query("SELECT toInt8('x')", 10).await.unwrap_err();
    assert!(e.code.is_some(), "{e:?}");

    // Introspection.
    assert!(s.schemas().await.unwrap().contains(&db));
    let t = &s.schema_graph(&db).await.unwrap()[0];
    assert_eq!(t.name, "events");
    assert_eq!(t.primary_key, vec!["user", "id"]);
    let col = |n: &str| t.columns.iter().find(|c| c.name == n).unwrap();
    assert_eq!(col("user").data_type, "LowCardinality(String)");
    assert!(col("note").nullable && !col("id").nullable);
    assert_eq!(col("amount").default.as_deref(), Some("0"));
    assert!(col("day").auto);
    assert!(t.indexes.iter().any(|i| i.name == "ix_user"));

    // Grid paging and filters; staged edits are refused (no transactions).
    let page = fetch_rows(
        s,
        &db,
        "events",
        &RowQuery { limit: 2, offset: 0, sort: vec![SortKey { column: "ts".into(), descending: true }], ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(page.total, Some(3));
    assert_eq!(page.result.rows.len(), 2);
    let f =
        RowQuery { filters: vec![Filter { column: "note".into(), op: FilterOp::Contains, value: json!("IT'S") }], ..Default::default() };
    assert_eq!(fetch_rows(s, &db, "events", &f).await.unwrap().total, Some(1));
    let edit = ChangeSet { deletes: vec![json!({"id": 1}).as_object().unwrap().clone()], ..Default::default() };
    assert!(apply_changes(s, &db, "events", &edit).await.unwrap_err().message.contains("no transactions"));

    // Truncation keeps the session usable; other FORMATs come back as text.
    let r = s.query("SELECT number FROM system.numbers LIMIT 100000", 50).await.unwrap();
    assert!(r[0].truncated && r[0].rows.len() == 50);
    let r = s.query("SELECT 1 AS a, 'x' AS b FORMAT TSV", 10).await.unwrap();
    assert_eq!(r[0].rows, vec![vec![json!("1\tx")]]);

    a.query(&format!("DROP DATABASE {db}"), 10).await.unwrap();
}

#[tokio::test]
async fn wrong_password_is_a_clear_error() {
    let Some(mut cfg) = config(None) else { return };
    cfg.password = Some("nope".into());
    let e = Client::connect(&cfg).await.err().unwrap();
    assert_eq!(e.code.as_deref(), Some("REQUIRED_PASSWORD"), "{e:?}");
    assert!(e.message.starts_with("default: Authentication failed") && !e.message.contains('\n'), "{e:?}");
}
