//! Client drivers against TwinBase mocks (and optionally real servers, see `real_postgres`).

use serde_json::json;
use twinbase_client::{
    ChangeSet, Client, ConnectionConfig, DbKind, Filter, FilterOp, RowQuery, SortKey, TlsMode, apply_changes, fetch_rows,
};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

const PG_SCHEMA: &str = "
CREATE TABLE teams (id serial PRIMARY KEY, name text NOT NULL UNIQUE);
CREATE TABLE members (
  id serial PRIMARY KEY,
  team_id int NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  email varchar(120) NOT NULL UNIQUE,
  active boolean DEFAULT true,
  score numeric(6,2)
);
CREATE INDEX members_team_idx ON members (team_id);
INSERT INTO teams (name) VALUES ('core'), ('web');
INSERT INTO members (team_id, email, score) VALUES (1, 'a@x.io', 10.5), (1, 'b@x.io', 3), (2, 'c@x.io', NULL);
";

fn config(kind: DbKind, port: u16, db: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        kind,
        host: "127.0.0.1".into(),
        port,
        user: "dev".into(),
        password: Some("pw".into()),
        database: db.map(Into::into),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 5,
        ssh: None,
        via: None,
    }
}

async fn exercise_sql(client: &Client, schema: &str) {
    let sql = client.sql().unwrap();
    // Read-only execution: reads work, writes are refused by the server.
    let r = sql.query_read_only("SELECT count(*) FROM members", 10).await.unwrap();
    assert_eq!(r[0].rows.len(), 1);
    let e = sql.query_read_only("UPDATE members SET score = 0", 10).await.unwrap_err();
    match sql.dialect() {
        twinbase_client::SqlDialect::Mysql => assert_eq!(e.code.as_deref(), Some("1792"), "{e:?}"),
        twinbase_client::SqlDialect::Postgres => assert_eq!(e.code.as_deref(), Some("25006"), "{e:?}"),
        _ => assert!(e.message.contains("readonly"), "{e:?}"),
    }
    let still = sql.query("SELECT count(*) FROM members WHERE score = 0", 10).await.unwrap();
    assert_eq!(still[0].rows[0][0], serde_json::json!(0));
    let graph = sql.schema_graph(schema).await.unwrap();
    let members = graph.iter().find(|t| t.name == "members").expect("members table");
    assert_eq!(members.primary_key, vec!["id"]);
    assert_eq!(members.columns.len(), 5);
    assert!(members.columns[0].auto, "serial/auto_increment detected");
    assert!(members.columns.iter().find(|c| c.name == "email").unwrap().unique);
    assert!(!members.columns.iter().find(|c| c.name == "email").unwrap().nullable);
    let fk = &members.foreign_keys[0];
    assert_eq!(
        (fk.columns.clone(), fk.ref_table.clone(), fk.ref_columns.clone()),
        (vec!["team_id".into()], "teams".into(), vec!["id".into()])
    );
    assert!(members.indexes.iter().any(|i| i.columns == vec!["team_id"] && !i.unique));

    let page = fetch_rows(
        sql,
        schema,
        "members",
        &RowQuery {
            limit: 2,
            sort: vec![SortKey { column: "email".into(), descending: true }],
            filters: vec![Filter { column: "email".into(), op: FilterOp::Contains, value: json!("x.io") }],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.total, Some(3));
    assert_eq!(page.result.rows.len(), 2);
    let email_idx = page.result.columns.iter().position(|c| c.name == "email").unwrap();
    assert_eq!(page.result.rows[0][email_idx], json!("c@x.io"));
    assert_eq!(page.key_columns, vec!["id"]);

    let mut cs = ChangeSet::default();
    cs.updates.push(twinbase_client::sqlgen::RowUpdate {
        key: json!({"id": 1}).as_object().unwrap().clone(),
        values: json!({"score": "99.5"}).as_object().unwrap().clone(),
    });
    cs.inserts.push(json!({"team_id": 2, "email": "d@x.io"}).as_object().unwrap().clone());
    cs.deletes.push(json!({"id": 2}).as_object().unwrap().clone());
    let applied = apply_changes(sql, schema, "members", &cs).await.unwrap();
    assert_eq!(applied.affected, 3);
    let r = sql.query("SELECT count(*) AS n FROM members; SELECT score FROM members WHERE id = 1", 10).await.unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].rows[0][0], json!(3));
    assert!(r[1].rows[0][0].to_string().contains("99.5"));

    // A failing change set rolls back completely.
    let mut bad = ChangeSet::default();
    bad.inserts.push(json!({"team_id": 1, "email": "e@x.io"}).as_object().unwrap().clone());
    bad.inserts.push(json!({"team_id": 1, "email": "e@x.io"}).as_object().unwrap().clone());
    let err = apply_changes(sql, schema, "members", &bad).await.unwrap_err();
    assert_eq!(err.statement_index, Some(1));
    let r = sql.query("SELECT count(*) FROM members WHERE email = 'e@x.io'", 10).await.unwrap();
    assert_eq!(r[0].rows[0][0], json!(0));

    // An edit to a row that no longer exists (deleted elsewhere) rolls back the whole save,
    // including edits that would have succeeded. A no-op update still counts as one row.
    let mut stale = ChangeSet::default();
    stale.updates.push(twinbase_client::sqlgen::RowUpdate {
        key: json!({"id": 1}).as_object().unwrap().clone(),
        values: json!({"score": "99.5"}).as_object().unwrap().clone(),
    });
    stale.updates.push(twinbase_client::sqlgen::RowUpdate {
        key: json!({"id": 2}).as_object().unwrap().clone(),
        values: json!({"email": "gone@x.io"}).as_object().unwrap().clone(),
    });
    let err = apply_changes(sql, schema, "members", &stale).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("TB001"), "{err:?}");
    let mut noop = ChangeSet::default();
    noop.updates.push(stale.updates[0].clone());
    assert_eq!(apply_changes(sql, schema, "members", &noop).await.unwrap().affected, 1);

    let truncated = sql.query("SELECT * FROM members", 1).await.unwrap();
    assert!(truncated[0].truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_mock() {
    let m = start(MockConfig {
        kind: MockKind::Postgres,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let client = Client::connect(&config(DbKind::Postgres, m.addr.port(), Some("app"))).await.unwrap();
    client.sql().unwrap().query(PG_SCHEMA, 10).await.unwrap();
    // Big tables: the grid stops counting at the cap instead of scanning everything per page.
    let sql = client.sql().unwrap();
    sql.query("CREATE TABLE big AS SELECT g AS id FROM generate_series(1, 100005) g", 10).await.unwrap();
    let page = fetch_rows(sql, "public", "big", &RowQuery { limit: 10, ..Default::default() }).await.unwrap();
    assert_eq!((page.total, page.total_at_least), (None, Some(twinbase_client::sqlgen::COUNT_CAP)));
    let filtered =
        RowQuery { limit: 10, filters: vec![Filter { column: "id".into(), op: FilterOp::Le, value: json!(42) }], ..Default::default() };
    assert_eq!(fetch_rows(sql, "public", "big", &filtered).await.unwrap().total, Some(42));
    assert!(client.sql().unwrap().server_version().await.unwrap().contains("PostgreSQL"));
    assert_eq!(client.sql().unwrap().schemas().await.unwrap(), vec!["public"]);
    exercise_sql(&client, "public").await;
    m.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_mock() {
    let m = start(MockConfig {
        kind: MockKind::Mysql,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let client = Client::connect(&config(DbKind::Mysql, m.addr.port(), None)).await.unwrap();
    let sql = client.sql().unwrap();
    sql.query("CREATE DATABASE crm; USE crm", 10).await.unwrap();
    sql.query(
        "CREATE TABLE teams (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL UNIQUE);
         CREATE TABLE members (id INT AUTO_INCREMENT PRIMARY KEY, team_id INT NOT NULL, email VARCHAR(120) NOT NULL UNIQUE,
           active TINYINT(1) DEFAULT 1, score DECIMAL(6,2), KEY members_team_idx (team_id),
           CONSTRAINT fk_team FOREIGN KEY (team_id) REFERENCES teams (id) ON DELETE CASCADE);
         INSERT INTO teams (name) VALUES ('core'), ('web');
         INSERT INTO members (team_id, email, score) VALUES (1, 'a@x.io', 10.5), (1, 'b@x.io', 3), (2, 'c@x.io', NULL)",
        10,
    )
    .await
    .unwrap();
    assert!(sql.schemas().await.unwrap().contains(&"crm".to_string()));
    exercise_sql(&client, "crm").await;
    m.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_file() {
    let path = std::env::temp_dir().join(format!("tb-client-{}.db", uuid::Uuid::new_v4()));
    let mut cfg = config(DbKind::Sqlite, 0, None);
    cfg.path = Some(path.to_string_lossy().into_owned());
    let client = Client::connect(&cfg).await.unwrap();
    client
        .sql()
        .unwrap()
        .query(
            "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             CREATE TABLE members (id INTEGER PRIMARY KEY, team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
               email TEXT NOT NULL UNIQUE, active INTEGER DEFAULT 1, score REAL);
             CREATE INDEX members_team_idx ON members (team_id);
             INSERT INTO teams (name) VALUES ('core'), ('web');
             INSERT INTO members (team_id, email, score) VALUES (1, 'a@x.io', 10.5), (1, 'b@x.io', 3), (2, 'c@x.io', NULL)",
            10,
        )
        .await
        .unwrap();
    exercise_sql(&client, "main").await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_mock() {
    let m = start(MockConfig {
        kind: MockKind::Redis,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let client = Client::connect(&config(DbKind::Redis, m.addr.port(), None)).await.unwrap();
    let r = client.redis().unwrap();
    r.command("HSET user:1 name \"Ann Lee\" role admin").await.unwrap();
    r.command("SET session:abc token PX 60000").await.unwrap();
    r.set("tags", &twinbase_client::RedisValue::Set { members: vec!["b".into(), "a".into()] }, None).await.unwrap();
    let (cursor, keys) = r.scan("*", 0, 100, None).await.unwrap();
    assert_eq!(cursor, 0);
    assert_eq!(keys.len(), 3);
    let session = keys.iter().find(|k| k.key == "session:abc").unwrap();
    assert_eq!(session.kind, "string");
    assert!(session.ttl_ms > 0);
    assert_eq!(
        r.get("user:1").await.unwrap(),
        twinbase_client::RedisValue::Hash { fields: vec![("name".into(), "Ann Lee".into()), ("role".into(), "admin".into())] }
    );
    assert_eq!(r.get("tags").await.unwrap(), twinbase_client::RedisValue::Set { members: vec!["a".into(), "b".into()] });
    let (_, only_hashes) = r.scan("*", 0, 100, Some("hash")).await.unwrap();
    assert_eq!(only_hashes.len(), 1);
    assert_eq!(r.delete(&["tags".into()]).await.unwrap(), 1);
    m.stop().await;
}

/// Run with a real server: `TWINBASE_REAL_PG_PORT=55432 cargo test -p twinbase-client real_postgres`.
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres() {
    let Ok(port) = std::env::var("TWINBASE_REAL_PG_PORT") else { return };
    let mut cfg = config(DbKind::Postgres, port.parse().unwrap(), Some("postgres"));
    cfg.user = "postgres".into();
    cfg.password = Some("postgres".into());
    let client = Client::connect(&cfg).await.unwrap();
    let sql = client.sql().unwrap();
    sql.query("DROP SCHEMA IF EXISTS tb_test CASCADE; CREATE SCHEMA tb_test; SET search_path TO tb_test", 10).await.unwrap();
    sql.query(PG_SCHEMA, 10).await.unwrap();
    exercise_sql(&client, "tb_test").await;
    sql.query("DROP SCHEMA tb_test CASCADE", 10).await.unwrap();
}
