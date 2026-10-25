//! SQL Server client against a real server. Set `TWINBASE_REAL_MSSQL_PORT` (and
//! `TWINBASE_REAL_MSSQL_PASSWORD` for the `sa` login) to run, e.g. with
//! `docker run -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=… -p 127.0.0.1:51433:1433 mcr.microsoft.com/mssql/server:2022-latest`.

use serde_json::json;
use twinbase_client::sqlgen::{ChangeSet, Filter, FilterOp, RowQuery, RowUpdate, SortKey};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode, apply_changes, fetch_rows};

fn config(tls: TlsMode, database: Option<&str>) -> Option<ConnectionConfig> {
    let port = std::env::var("TWINBASE_REAL_MSSQL_PORT").ok()?.parse().ok()?;
    Some(ConnectionConfig {
        kind: DbKind::Mssql,
        host: "127.0.0.1".into(),
        port,
        user: "sa".into(),
        password: std::env::var("TWINBASE_REAL_MSSQL_PASSWORD").ok(),
        database: database.map(Into::into),
        path: None,
        tls,
        timeout_secs: 20,
        ssh: None,
        via: None,
    })
}

#[tokio::test]
async fn sql_server_end_to_end() {
    let Some(cfg) = config(TlsMode::Disable, Some("master")) else { return };
    let admin = Client::connect(&cfg).await.unwrap();
    let a = admin.sql().unwrap();
    let db = format!("tb_test_{}", std::process::id());
    a.query(&format!("IF DB_ID(N'{db}') IS NOT NULL DROP DATABASE [{db}]\nGO\nCREATE DATABASE [{db}]"), 10).await.unwrap();

    // TLS without verification (the container has a self-signed certificate).
    let cfg = config(TlsMode::Require, Some(&db)).unwrap();
    let c = Client::connect(&cfg).await.unwrap();
    let s = c.sql().unwrap();
    assert!(s.server_version().await.unwrap().starts_with("SQL Server 1"));

    let script = "CREATE TABLE customers (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(100) NOT NULL UNIQUE, joined datetime2 DEFAULT SYSUTCDATETIME(), credit decimal(10,2) DEFAULT 0, vip bit NOT NULL DEFAULT 0, ref uniqueidentifier DEFAULT NEWID());
CREATE TABLE orders (id bigint IDENTITY PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total money, note varchar(max), data varbinary(16));
CREATE INDEX ix_orders_customer ON orders (customer_id);
GO
CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
GO
CREATE PROCEDURE add_customer @name nvarchar(100) AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO customers (name) VALUES (@name);
  SELECT SCOPE_IDENTITY() AS id;
END
GO
INSERT INTO customers (name, credit, vip) VALUES (N'Zoë', 12.5, 1), (N'Bo', 0, 0);
EXEC add_customer N'李';
INSERT INTO orders (customer_id, total, note, data) VALUES (1, 250, 'big', 0x0102), (1, 20, NULL, NULL), (2, 120.5, 'x', NULL);
UPDATE customers SET credit = credit + 1 WHERE vip = 0;
SELECT name, credit, vip FROM customers ORDER BY id;";
    let r = s.query(script, 100).await.unwrap();
    let inserted = r.iter().find(|q| q.statement.starts_with("INSERT INTO customers")).unwrap();
    assert_eq!(inserted.affected, Some(2));
    let updated = r.iter().find(|q| q.statement.starts_with("UPDATE")).unwrap();
    assert_eq!(updated.affected, Some(2));
    let proc = r.iter().find(|q| q.statement.starts_with("EXEC")).unwrap();
    assert_eq!(proc.rows, vec![vec![json!("3")]]);
    let last = r.last().unwrap();
    assert_eq!(
        last.rows,
        vec![
            vec![json!("Zoë"), json!("12.50"), json!(true)],
            vec![json!("Bo"), json!("1.00"), json!(false)],
            vec![json!("李"), json!("1.00"), json!(false)]
        ]
    );
    assert_eq!(last.columns[2].type_name, "bit");

    // Variables and temp tables: a batch with DECLARE runs whole; temp tables live in the session.
    let r = s.query("DECLARE @n int = (SELECT COUNT(*) FROM orders); SELECT @n * 2 AS twice;", 10).await.unwrap();
    assert_eq!(r[0].rows, vec![vec![json!(6)]]);
    s.query("CREATE TABLE #scratch (v int); INSERT INTO #scratch VALUES (7)", 10).await.unwrap();
    assert_eq!(s.query("SELECT v FROM #scratch", 10).await.unwrap()[0].rows, vec![vec![json!(7)]]);

    // Types.
    let r = s.query("SELECT CAST(1 AS tinyint), CAST(9007199254740993 AS bigint), CAST('2024-05-06 07:08:09.5' AS datetime2), CAST('2024-05-06' AS date), CAST('2024-05-06T07:08:09+02:00' AS datetimeoffset), CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier), 0xCAFE, CAST(1.5 AS float), NULL", 10).await.unwrap();
    assert_eq!(
        r[0].rows[0],
        vec![
            json!(1),
            json!("9007199254740993"),
            json!("2024-05-06 07:08:09.5"),
            json!("2024-05-06"),
            json!("2024-05-06 07:08:09 +02:00"),
            json!("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
            json!("\\xcafe"),
            json!(1.5),
            json!(null)
        ]
    );

    // Errors carry the SQL Server error number and the statement index.
    let e = s.query("SELECT 1; INSERT INTO customers (name) VALUES (N'Bo');", 10).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("2627"), "{e:?}");
    assert_eq!(e.statement_index, Some(1));
    let e = s.query("SELECT * FROM nope", 10).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("208"));

    // Introspection.
    assert_eq!(s.schemas().await.unwrap()[0], "dbo");
    let tables = s.tables("dbo").await.unwrap();
    assert_eq!(
        tables.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect::<Vec<_>>(),
        vec![("big_orders", "view"), ("customers", "table"), ("orders", "table")]
    );
    let graph = s.schema_graph("dbo").await.unwrap();
    let cust = graph.iter().find(|t| t.name == "customers").unwrap();
    assert_eq!(cust.primary_key, vec!["id"]);
    let col = |n: &str| cust.columns.iter().find(|c| c.name == n).unwrap();
    assert!(col("id").auto && col("id").primary_key);
    assert_eq!(col("name").data_type, "nvarchar(100)");
    assert!(col("name").unique && !col("name").nullable);
    assert_eq!(col("credit").data_type, "decimal(10,2)");
    assert_eq!(col("credit").default.as_deref(), Some("0"));
    assert_eq!(col("joined").default.as_deref(), Some("sysutcdatetime()"));
    let orders = graph.iter().find(|t| t.name == "orders").unwrap();
    assert_eq!(orders.foreign_keys[0].ref_table, "customers");
    assert_eq!(orders.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert!(orders.indexes.iter().any(|i| i.name == "ix_orders_customer" && !i.unique));
    assert_eq!(orders.columns.iter().find(|c| c.name == "note").unwrap().data_type, "varchar(max)");

    // Grid paging, filters and staged edits.
    let page = fetch_rows(
        s,
        "dbo",
        "customers",
        &RowQuery { limit: 2, offset: 1, sort: vec![SortKey { column: "name".into(), descending: false }], ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(page.total, Some(3));
    assert_eq!(page.result.rows.len(), 2);
    let f = RowQuery { filters: vec![Filter { column: "name".into(), op: FilterOp::Contains, value: json!("o") }], ..Default::default() };
    assert_eq!(fetch_rows(s, "dbo", "customers", &f).await.unwrap().total, Some(2));
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: json!({"id": 2}).as_object().unwrap().clone(),
            values: json!({"vip": true, "credit": 5}).as_object().unwrap().clone(),
        }],
        inserts: vec![json!({"name": "New"}).as_object().unwrap().clone()],
        deletes: vec![json!({"id": 3}).as_object().unwrap().clone()],
    };
    assert_eq!(apply_changes(s, "dbo", "customers", &changes).await.unwrap().affected, 3);
    // A stale edit (row already gone) rolls everything back.
    let stale = ChangeSet {
        deletes: vec![json!({"id": 3}).as_object().unwrap().clone(), json!({"id": 1}).as_object().unwrap().clone()],
        ..Default::default()
    };
    assert_eq!(apply_changes(s, "dbo", "customers", &stale).await.unwrap_err().code.as_deref(), Some("TB001"));
    assert_eq!(s.query("SELECT COUNT(*) FROM customers", 1).await.unwrap()[0].rows[0][0], json!(3));

    // Rolled-back runs leave no trace.
    let r = s.query_rolled_back("DELETE FROM orders", 10).await.unwrap();
    assert_eq!(r[0].affected, Some(3));
    assert_eq!(s.query("SELECT COUNT(*) FROM orders", 1).await.unwrap()[0].rows[0][0], json!(3));

    // Large results are truncated but the connection stays usable.
    let r = s.query("SELECT TOP 500 a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b", 100).await.unwrap();
    assert!(r[0].truncated && r[0].rows.len() == 100);
    assert_eq!(s.query("SELECT 42", 1).await.unwrap()[0].rows[0][0], json!(42));

    drop(c);
    a.query(&format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE\nGO\nDROP DATABASE [{db}]"), 10).await.unwrap();
}

#[tokio::test]
async fn wrong_password_is_a_clear_error() {
    let Some(mut cfg) = config(TlsMode::Disable, None) else { return };
    cfg.password = Some("not-the-password".into());
    let e = Client::connect(&cfg).await.err().unwrap();
    assert_eq!(e.code.as_deref(), Some("18456"), "{e:?}");
}
