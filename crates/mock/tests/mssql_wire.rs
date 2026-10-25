//! The SQL Server mock driven by a real TDS driver (tiberius): TLS negotiation, batches,
//! parameterized RPC (sp_executesql), typed values, OUTPUT, transactions and error numbers.

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

async fn mock(auth: AuthMode) -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Mssql,
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

async fn connect(port: u16, password: &str, encryption: EncryptionLevel) -> tiberius::Result<Client<Compat<TcpStream>>> {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(port);
    cfg.database("app");
    cfg.authentication(AuthMethod::sql_server("sa", password));
    cfg.encryption(encryption);
    cfg.trust_cert();
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(cfg, tcp.compat_write()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_parameters_and_types() {
    let m = mock(AuthMode::Any).await;
    let mut c = connect(m.addr.port(), "x", EncryptionLevel::Off).await.unwrap();
    c.simple_query(
        "CREATE TABLE dbo.Products (Id INT IDENTITY(1,1) PRIMARY KEY, Name NVARCHAR(80) NOT NULL, Price DECIMAL(8,2), \
         InStock BIT NOT NULL DEFAULT 1, Added DATETIME2 DEFAULT SYSDATETIME(), Released DATE, Ref UNIQUEIDENTIFIER DEFAULT NEWID(), Ratio FLOAT)",
    )
    .await
    .unwrap()
    .into_results()
    .await
    .unwrap();
    // Parameterized statements go through sp_executesql.
    let r = c
        .execute(
            "INSERT INTO Products (Name, Price, Released, Ratio) VALUES (@P1, @P2, @P3, @P4)",
            &[&"Lämp", &bigdecimal::BigDecimal::from(19), &chrono::NaiveDate::from_ymd_opt(2024, 2, 3).unwrap(), &0.25f64],
        )
        .await
        .unwrap();
    assert_eq!(r.total(), 1);
    c.execute(
        "INSERT INTO Products (Name, Price, InStock) VALUES (@P1, @P2, @P3)",
        &[&"Desk's", &Option::<bigdecimal::BigDecimal>::None, &false],
    )
    .await
    .unwrap();

    let rows = c
        .query("SELECT Id, Name, Price, InStock, Released, Ref, Ratio, Added FROM Products WHERE Id >= @P1 ORDER BY Id", &[&1i32])
        .await
        .unwrap()
        .into_first_result()
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<i32, _>(0), Some(1));
    assert_eq!(rows[0].get::<&str, _>(1), Some("Lämp"));
    assert_eq!(rows[0].get::<bigdecimal::BigDecimal, _>(2).map(|d| d.to_string()), Some("19.00".into()));
    assert_eq!(rows[0].get::<bool, _>(3), Some(true));
    assert_eq!(rows[0].get::<chrono::NaiveDate, _>(4), chrono::NaiveDate::from_ymd_opt(2024, 2, 3));
    assert!(rows[0].get::<uuid::Uuid, _>(5).is_some());
    assert_eq!(rows[0].get::<f64, _>(6), Some(0.25));
    assert!(rows[0].get::<chrono::NaiveDateTime, _>(7).is_some());
    assert_eq!(rows[1].get::<&str, _>(1), Some("Desk's"));
    assert_eq!(rows[1].get::<bigdecimal::BigDecimal, _>(2), None);
    assert_eq!(rows[1].get::<bool, _>(3), Some(false));

    // Several result sets and counts in one batch; OUTPUT returns rows.
    let sets = c
        .simple_query(
            "SELECT TOP 1 Name FROM Products ORDER BY Id DESC; UPDATE Products SET Price = 5 WHERE Price IS NULL; SELECT @@ROWCOUNT AS n",
        )
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0][0].get::<&str, _>(0), Some("Desk's"));
    assert_eq!(sets[1][0].get::<i32, _>(0), Some(1));
    let out = c
        .query("INSERT INTO Products (Name) OUTPUT INSERTED.Id VALUES (@P1)", &[&"Chair"])
        .await
        .unwrap()
        .into_first_result()
        .await
        .unwrap();
    assert_eq!(out[0].get::<i32, _>(0), Some(3));

    // Errors carry SQL Server numbers; the connection stays usable.
    let e = c.execute("INSERT INTO Products (Id, Name) VALUES (1, N'dup')", &[]).await.unwrap_err();
    assert_eq!(e.code(), Some(2627), "{e}");
    let e = match c.simple_query("SELECT * FROM Missing").await {
        Ok(stream) => stream.into_results().await.unwrap_err(),
        Err(e) => e,
    };
    assert_eq!(e.code(), Some(208), "{e}");
    let n = c.simple_query("SELECT COUNT(*) FROM Products").await.unwrap().into_row().await.unwrap().unwrap();
    assert_eq!(n.get::<i32, _>(0), Some(3));
    m.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn transactions_tls_and_auth() {
    let m = mock(AuthMode::Password { user: "sa".into(), password: "pw".into() }).await;
    let e = connect(m.addr.port(), "bad", EncryptionLevel::Off).await.expect_err("login must fail");
    assert_eq!(e.code(), Some(18456), "{e}");
    // Full TLS and no TLS both work.
    for level in [EncryptionLevel::Required, EncryptionLevel::NotSupported] {
        let mut c = connect(m.addr.port(), "pw", level).await.unwrap();
        c.simple_query("IF 1 = 0 SELECT 1").await.ok();
        drop(c);
    }
    let mut c = connect(m.addr.port(), "pw", EncryptionLevel::Required).await.unwrap();
    c.simple_query("CREATE TABLE t (id INT PRIMARY KEY)").await.unwrap().into_results().await.unwrap();
    c.simple_query("BEGIN TRANSACTION; INSERT INTO t VALUES (1); ROLLBACK").await.unwrap().into_results().await.unwrap();
    c.simple_query("BEGIN TRAN").await.unwrap().into_results().await.unwrap();
    c.execute("INSERT INTO t VALUES (@P1)", &[&2i32]).await.unwrap();
    c.simple_query("COMMIT").await.unwrap().into_results().await.unwrap();
    let n = c.simple_query("SELECT COUNT(*), MAX(id) FROM t").await.unwrap().into_row().await.unwrap().unwrap();
    assert_eq!((n.get::<i32, _>(0), n.get::<i32, _>(1)), (Some(1), Some(2)));
    m.stop().await;
}
