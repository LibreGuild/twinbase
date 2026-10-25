//! Cloud Spanner client against the emulator: set `TWINBASE_SPANNER_EMULATOR_PORT` (its REST port),
//! e.g. with `docker run -p 127.0.0.1:59020:9020 gcr.io/cloud-spanner-emulator/emulator`.

use serde_json::json;
use twinbase_client::sqlgen::{ChangeSet, Filter, FilterOp, RowQuery, RowUpdate, SortKey};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode, apply_changes, fetch_rows};

/// Create the emulator instance and database over REST (the emulator needs no credentials).
async fn create_database(port: u16, db: &str) {
    let base = format!("http://127.0.0.1:{port}/v1/projects/tb-test/instances");
    let post = |url: String, body: serde_json::Value| async move {
        let out = tokio::process::Command::new("curl").args(["-s", "-X", "POST", &url, "-d", &body.to_string()]).output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    post(base.clone(), json!({ "instanceId": "tb", "instance": { "config": "emulator-config", "displayName": "tb", "nodeCount": 1 } }))
        .await;
    let r = post(format!("{base}/tb/databases"), json!({ "createStatement": format!("CREATE DATABASE `{db}`") })).await;
    assert!(r.contains("operations"), "{r}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

fn config(port: u16, db: &str) -> ConnectionConfig {
    ConnectionConfig {
        kind: DbKind::Spanner,
        host: "127.0.0.1".into(),
        port,
        user: String::new(),
        password: None,
        database: Some(format!("tb-test/tb/{db}")),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 10,
        ssh: None,
        via: None,
    }
}

#[tokio::test]
async fn spanner_end_to_end() {
    let Some(port) = std::env::var("TWINBASE_SPANNER_EMULATOR_PORT").ok().and_then(|p| p.parse::<u16>().ok()) else { return };
    let db = format!("tb{}", std::process::id());
    create_database(port, &db).await;
    let c = Client::connect(&config(port, &db)).await.unwrap();
    let s = c.sql().unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Cloud Spanner tb-test/tb"), "{}", s.server_version().await.unwrap());
    let r = s
        .query(
            "CREATE TABLE Singers (SingerId INT64 NOT NULL, Name STRING(100) NOT NULL, Rating NUMERIC, Photo BYTES(MAX), Tags ARRAY<STRING(MAX)>, Info JSON, Joined TIMESTAMP) PRIMARY KEY (SingerId);
CREATE TABLE Albums (SingerId INT64 NOT NULL, AlbumId INT64 NOT NULL, Title STRING(MAX)) PRIMARY KEY (SingerId, AlbumId), INTERLEAVE IN PARENT Singers ON DELETE CASCADE;
CREATE UNIQUE INDEX SingersByName ON Singers (Name);
INSERT INTO Singers (SingerId, Name, Rating, Photo, Tags, Info, Joined) VALUES (1, 'Marc', NUMERIC '4.50', b'\\xca\\xfe', ['rock', 'pop'], JSON '{\"label\": \"x\"}', TIMESTAMP '2024-01-02T03:04:05Z'), (2, 'Cat\\'s', NULL, NULL, [], NULL, NULL), (9007199254740993, 'Big', NULL, NULL, NULL, NULL, NULL);
INSERT INTO Albums (SingerId, AlbumId, Title) VALUES (1, 1, 'First'), (1, 2, 'Second');
UPDATE Singers SET Rating = NUMERIC '5' WHERE SingerId = 2;
SELECT SingerId, Name, Rating, Photo, Tags, Info, Joined FROM Singers ORDER BY SingerId",
            100,
        )
        .await
        .unwrap();
    assert_eq!(r[0].message.as_deref(), Some("Schema updated (3 statements)"), "{:?}", r[0]);
    assert_eq!(r[1].affected, Some(3));
    assert_eq!(r[3].affected, Some(1));
    let rows = &r.last().unwrap().rows;
    assert_eq!(
        rows[0],
        vec![
            json!(1),
            json!("Marc"),
            json!("4.5"),
            json!("\\xcafe"),
            json!(["rock", "pop"]),
            json!({ "label": "x" }),
            json!("2024-01-02T03:04:05Z")
        ]
    );
    assert_eq!(rows[1][1], json!("Cat's"));
    assert_eq!(rows[2][0], json!("9007199254740993"));
    assert_eq!(r.last().unwrap().columns[4].type_name, "ARRAY<STRING>");

    // Introspection: keys, unique index, interleaving as a reference to the parent.
    assert_eq!(s.schemas().await.unwrap(), vec![""]);
    let graph = s.schema_graph("").await.unwrap();
    let singers = graph.iter().find(|t| t.name == "Singers").unwrap();
    assert_eq!(singers.primary_key, vec!["SingerId"]);
    assert!(singers.columns.iter().find(|c| c.name == "Name").unwrap().unique);
    assert_eq!(singers.columns.iter().find(|c| c.name == "Photo").unwrap().data_type, "BYTES(MAX)");
    let albums = graph.iter().find(|t| t.name == "Albums").unwrap();
    assert_eq!(albums.primary_key, vec!["SingerId", "AlbumId"]);
    let parent = &albums.foreign_keys[0];
    assert_eq!((parent.ref_table.as_str(), parent.on_delete.as_deref()), ("Singers", Some("CASCADE")));

    // Grid: paging, case-insensitive contains, typed staged edits in one transaction.
    let page = fetch_rows(
        s,
        "",
        "Singers",
        &RowQuery { limit: 2, offset: 1, sort: vec![SortKey { column: "Name".into(), descending: false }], ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(page.total, Some(3));
    assert_eq!(page.result.rows[0][1], json!("Cat's"));
    let f = RowQuery { filters: vec![Filter { column: "Name".into(), op: FilterOp::Contains, value: json!("MAR") }], ..Default::default() };
    assert_eq!(fetch_rows(s, "", "Singers", &f).await.unwrap().total, Some(1));
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: json!({ "SingerId": "9007199254740993" }).as_object().unwrap().clone(),
            values: json!({ "Rating": "3.25", "Photo": "\\xbeef", "Tags": ["x"] }).as_object().unwrap().clone(),
        }],
        inserts: vec![json!({ "SingerId": 3, "Name": "New", "Joined": "2025-01-01T00:00:00Z" }).as_object().unwrap().clone()],
        deletes: vec![],
    };
    assert_eq!(apply_changes(s, "", "Singers", &changes).await.unwrap().affected, 2);
    let r = s.query("SELECT Rating, TO_HEX(Photo), Tags FROM Singers WHERE SingerId = 9007199254740993", 1).await.unwrap();
    assert_eq!(r[0].rows[0], vec![json!("3.25"), json!("beef"), json!(["x"])]);
    let stale = ChangeSet { deletes: vec![json!({ "SingerId": 42 }).as_object().unwrap().clone()], ..Default::default() };
    assert_eq!(apply_changes(s, "", "Singers", &stale).await.unwrap_err().code.as_deref(), Some("TB001"));

    // Rolled-back runs and errors.
    let r = s.query_rolled_back("DELETE FROM Singers WHERE TRUE", 10).await.unwrap();
    assert_eq!(r[0].affected, Some(4));
    assert_eq!(s.query("SELECT COUNT(*) FROM Singers", 1).await.unwrap()[0].rows[0][0], json!(4));
    let e = s.query("SELECT 1; SELECT * FROM Nope", 10).await.unwrap_err();
    assert_eq!(e.statement_index, Some(1));
    // (The emulator's REST gateway reports execution errors as "failed to marshal error message";
    // Cloud Spanner returns the status and message, which `error_from` keeps.)
    assert!(s.query("INSERT INTO Singers (SingerId, Name) VALUES (1, 'Dup')", 10).await.is_err());
    assert_eq!(s.query("SELECT COUNT(*) FROM Singers", 1).await.unwrap()[0].rows[0][0], json!(4), "the failed insert was rolled back");
    assert!(s.native_plan("SELECT * FROM Singers", false).await.unwrap().is_some());
}
