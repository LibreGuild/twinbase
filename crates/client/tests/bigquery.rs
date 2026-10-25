//! BigQuery client against the BigQuery emulator: set `TWINBASE_BIGQUERY_EMULATOR_PORT`, e.g. with
//! `docker run -p 127.0.0.1:59050:9050 ghcr.io/goccy/bigquery-emulator --project=tb-test`.

use serde_json::json;
use twinbase_client::sqlgen::{ChangeSet, RowQuery};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode, apply_changes, fetch_rows};

#[tokio::test]
async fn bigquery_end_to_end() {
    let Some(port) = std::env::var("TWINBASE_BIGQUERY_EMULATOR_PORT").ok().and_then(|p| p.parse::<u16>().ok()) else { return };
    let ds = format!("tb_{}", std::process::id());
    // The emulator creates datasets through the REST API (no DDL for them).
    let out = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("http://127.0.0.1:{port}/bigquery/v2/projects/tb-test/datasets"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &json!({ "datasetReference": { "projectId": "tb-test", "datasetId": ds } }).to_string(),
        ])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(&ds));
    let cfg = ConnectionConfig {
        kind: DbKind::Bigquery,
        host: "127.0.0.1".into(),
        port,
        user: String::new(),
        password: None,
        database: Some(format!("tb-test.{ds}")),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 10,
        ssh: None,
        via: None,
    };
    // The emulator mixes up same-named tables across datasets: a unique table name per run.
    let t = format!("events_{}", std::process::id());
    let c = Client::connect(&cfg).await.unwrap();
    let s = c.sql().unwrap();
    assert!(s.server_version().await.unwrap().starts_with("BigQuery project tb-test"));
    let r = s
        .query(
            &format!(
                "CREATE TABLE {t} (id INT64, name STRING, price NUMERIC, ts TIMESTAMP, tags ARRAY<STRING>, rec STRUCT<a INT64, b STRING>);
INSERT INTO {t} (id, name, price, ts, tags, rec) VALUES (1, 'Lamp', 25.5, TIMESTAMP '2024-01-02 03:04:05', ['home', 'light'], STRUCT(7, 'x')), (2, 'Desk', 200, NULL, [], NULL), (3, 'it\\'s', NULL, NULL, NULL, NULL);
SELECT id, name, price, ts, tags, rec FROM {t} ORDER BY id"
            ),
            100,
        )
        .await
        .unwrap();
    let rows = &r[2].rows;
    assert_eq!(
        rows[0],
        vec![
            json!(1),
            json!("Lamp"),
            json!("25.5"),
            json!("2024-01-02 03:04:05 UTC"),
            json!(["home", "light"]),
            json!({ "a": 7, "b": "x" })
        ]
    );
    assert_eq!(rows[2][1], json!("it's"));
    assert_eq!(r[2].columns[4].type_name, "ARRAY<STRING>");
    assert_eq!(r[2].columns[5].type_name, "STRUCT<a INT64, b STRING>");

    // Introspection.
    assert!(s.schemas().await.unwrap().contains(&ds));
    let info = s.describe(&ds, &t).await.unwrap();
    assert_eq!(
        info.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(),
        vec!["INT64", "STRING", "NUMERIC", "TIMESTAMP", "ARRAY<STRING>", "STRUCT<a INT64, b STRING>"]
    );

    // Unfiltered pages come from tabledata.list (free); filtered ones run a query.
    let page = fetch_rows(s, &ds, &t, &RowQuery { limit: 2, ..Default::default() }).await.unwrap();
    assert!(page.result.statement.starts_with("tabledata.list"));
    assert_eq!(page.total, Some(3));
    assert_eq!(page.result.rows.len(), 2);
    let q = RowQuery {
        filters: vec![twinbase_client::sqlgen::Filter {
            column: "name".into(),
            op: twinbase_client::sqlgen::FilterOp::Contains,
            value: json!("LAM"),
        }],
        ..Default::default()
    };
    let filtered = fetch_rows(s, &ds, &t, &q).await.unwrap();
    assert_eq!(filtered.total, Some(1));
    assert_eq!(filtered.result.rows[0][1], json!("Lamp"));
    // The grid does not edit BigQuery tables.
    let edit = ChangeSet { deletes: vec![json!({ "id": 1 }).as_object().unwrap().clone()], ..Default::default() };
    assert!(apply_changes(s, &ds, &t, &edit).await.unwrap_err().message.contains("does not edit BigQuery"));

    let e = s.query(&format!("SELECT 1; SELECT nope FROM {t}"), 10).await.unwrap_err();
    assert_eq!(e.statement_index, Some(1));
    assert!(e.message.contains("nope"), "{e:?}");
    let r = s.query("SELECT x FROM UNNEST(GENERATE_ARRAY(1, 500)) AS x", 50).await.unwrap();
    assert!(r[0].truncated && r[0].rows.len() == 50);
}
