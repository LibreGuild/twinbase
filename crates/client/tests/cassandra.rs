//! Cassandra / ScyllaDB client against a real node: set `TWINBASE_REAL_CASSANDRA_PORT`, e.g. with
//! `docker run -e MAX_HEAP_SIZE=512M -e HEAP_NEWSIZE=128M -p 127.0.0.1:59042:9042 cassandra:5.0`.

use serde_json::json;
use twinbase_client::sqlgen::{ChangeSet, Filter, FilterOp, RowQuery, RowUpdate};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode, apply_changes, fetch_rows};

fn config(keyspace: Option<&str>) -> Option<ConnectionConfig> {
    let port = std::env::var("TWINBASE_REAL_CASSANDRA_PORT").ok()?.parse().ok()?;
    Some(ConnectionConfig {
        kind: DbKind::Cassandra,
        host: "127.0.0.1".into(),
        port,
        user: String::new(),
        password: None,
        database: keyspace.map(Into::into),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 20,
        ssh: None,
        via: None,
    })
}

#[tokio::test]
async fn cassandra_end_to_end() {
    let Some(cfg) = config(None) else { return };
    let admin = Client::connect(&cfg).await.unwrap();
    let a = admin.sql().unwrap();
    let ks = format!("tb_test_{}", std::process::id());
    a.query(&format!("CREATE KEYSPACE IF NOT EXISTS {ks} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"), 10)
        .await
        .unwrap();

    let c = Client::connect(&config(Some(&ks)).unwrap()).await.unwrap();
    let s = c.sql().unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Cassandra 5."), "{}", s.server_version().await.unwrap());
    let r = s
        .query(
            "CREATE TABLE events (user_id uuid, day date, at timestamp, kind text, amount decimal, tags set<text>, attrs map<text, int>, raw blob, big varint, PRIMARY KEY ((user_id, day), at)) WITH CLUSTERING ORDER BY (at DESC);
CREATE INDEX events_kind ON events (kind);
INSERT INTO events (user_id, day, at, kind, amount, tags, attrs, raw, big) VALUES (6f9619ff-8b86-d011-b42d-00c04fc964ff, '2024-05-06', '2024-05-06 10:00:00+0000', 'click', 12.50, {'a', 'b'}, {'x': 1}, 0xcafe, 123456789012345678901234567890);
INSERT INTO events (user_id, day, at, kind, amount) VALUES (6f9619ff-8b86-d011-b42d-00c04fc964ff, '2024-05-06', '2024-05-06 11:00:00+0000', 'view', -0.005);
INSERT INTO events (user_id, day, at, kind) VALUES (00000000-0000-0000-0000-000000000001, '2024-05-07', '2024-05-07 09:00:00+0000', 'click');
SELECT kind, amount, tags, attrs, raw, big, day, at FROM events WHERE user_id = 6f9619ff-8b86-d011-b42d-00c04fc964ff AND day = '2024-05-06';",
            100,
        )
        .await
        .unwrap();
    let last = r.last().unwrap();
    assert_eq!(
        last.rows[0],
        vec![
            json!("view"),
            json!("-0.005"),
            json!(null),
            json!(null),
            json!(null),
            json!(null),
            json!("2024-05-06"),
            json!("2024-05-06 11:00:00.000Z")
        ]
    );
    assert_eq!(
        last.rows[1][..6],
        [json!("click"), json!("12.50"), json!(["a", "b"]), json!({"x": 1}), json!("\\xcafe"), json!("123456789012345678901234567890")]
    );
    assert_eq!(last.columns[2].type_name, "set<text>");

    // Lightweight transactions answer with [applied].
    let r = s.query("INSERT INTO events (user_id, day, at, kind) VALUES (00000000-0000-0000-0000-000000000001, '2024-05-07', '2024-05-07 09:00:00+0000', 'dup') IF NOT EXISTS", 10).await.unwrap();
    assert_eq!(r[0].columns[0].name, "[applied]");
    assert_eq!(r[0].rows[0][0], json!(false));

    // Errors carry the kind and the statement index.
    let e = s.query("SELECT kind FROM events LIMIT 1; SELEC x", 10).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("SyntaxError"), "{e:?}");
    assert_eq!(e.statement_index, Some(1));
    let e = s.query("SELECT * FROM events WHERE kind2 = 'x'", 10).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("Invalid"), "{e:?}");

    // Introspection: partition + clustering key, index.
    assert_eq!(s.schemas().await.unwrap()[0], ks);
    let t = s.schema_graph(&ks).await.unwrap().into_iter().find(|t| t.name == "events").unwrap();
    assert_eq!(t.primary_key, vec!["user_id", "day", "at"]);
    assert_eq!(t.indexes.iter().find(|i| i.name == "partition key").unwrap().columns, vec!["user_id", "day"]);
    assert!(t.indexes.iter().any(|i| i.name == "events_kind" && i.columns == vec!["kind"]));
    assert_eq!(t.columns.iter().find(|c| c.name == "attrs").unwrap().data_type, "map<text, int>");

    // Grid: read-ahead paging, ALLOW FILTERING for filters, typed edits in one batch.
    let page = fetch_rows(s, &ks, "events", &RowQuery { limit: 2, offset: 1, ..Default::default() }).await.unwrap();
    assert_eq!(page.result.rows.len(), 2);
    assert_eq!(page.total, None);
    let f = RowQuery { filters: vec![Filter { column: "kind".into(), op: FilterOp::Eq, value: json!("click") }], ..Default::default() };
    assert_eq!(fetch_rows(s, &ks, "events", &f).await.unwrap().result.rows.len(), 2);
    let key = json!({ "user_id": "6f9619ff-8b86-d011-b42d-00c04fc964ff", "day": "2024-05-06", "at": "2024-05-06 11:00:00.000Z" })
        .as_object()
        .unwrap()
        .clone();
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: key.clone(),
            values: json!({ "amount": "1.25", "tags": ["z"], "raw": "\\xbeef" }).as_object().unwrap().clone(),
        }],
        inserts: vec![
            json!({ "user_id": "00000000-0000-0000-0000-000000000002", "day": "2024-05-08", "at": "2024-05-08 08:00:00Z", "kind": "new" })
                .as_object()
                .unwrap()
                .clone(),
        ],
        deletes: vec![
            json!({ "user_id": "00000000-0000-0000-0000-000000000001", "day": "2024-05-07", "at": "2024-05-07 09:00:00.000Z" })
                .as_object()
                .unwrap()
                .clone(),
        ],
    };
    assert_eq!(apply_changes(s, &ks, "events", &changes).await.unwrap().affected, 3);
    let r = s.query("SELECT amount, tags, raw FROM events WHERE user_id = 6f9619ff-8b86-d011-b42d-00c04fc964ff AND day = '2024-05-06' AND at = '2024-05-06 11:00:00+0000'", 1).await.unwrap();
    assert_eq!(r[0].rows[0], vec![json!("1.25"), json!(["z"]), json!("\\xbeef")]);
    assert_eq!(s.query("SELECT COUNT(*) FROM events", 1).await.unwrap()[0].rows[0][0], json!(3));

    // Truncation.
    let r = s.query("SELECT * FROM system_schema.columns", 5).await.unwrap();
    assert!(r[0].truncated && r[0].rows.len() == 5);
    a.query(&format!("DROP KEYSPACE {ks}"), 10).await.unwrap();
}
