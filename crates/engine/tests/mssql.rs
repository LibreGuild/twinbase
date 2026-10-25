//! The engine in SQL Server (T-SQL) mode, as the TDS mock drives it (no network).

use twinbase_engine::{Dialect, Engine, Outcome, Session};

fn run(s: &mut Session, sql: &str) -> Vec<Outcome> {
    let r = s.execute(sql);
    if let Some(e) = r.error {
        panic!("{sql}\n=> {e} ({})", e.sqlstate);
    }
    r.outcomes
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = run(s, sql);
    let rs = out.last().unwrap().result.as_ref().expect("result set");
    rs.rows
        .iter()
        .map(|r| r.iter().zip(&rs.columns).map(|(v, c)| twinbase_engine::value::format_text(v, &c.ty, Dialect::MsSql)).collect())
        .collect()
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql)[0][0].clone().unwrap_or_else(|| "NULL".into())
}

fn session() -> (Engine, Session) {
    let engine = Engine::temporary(Dialect::MsSql).unwrap();
    let mut s = engine.session(None, "sa").unwrap();
    run(
        &mut s,
        "CREATE TABLE [dbo].[Customers] (Id INT IDENTITY(1,1) NOT NULL PRIMARY KEY, Name NVARCHAR(100) NOT NULL UNIQUE, Credit DECIMAL(10,2) NOT NULL DEFAULT 0, Joined DATETIME2 DEFAULT GETDATE(), Ref UNIQUEIDENTIFIER DEFAULT NEWID(), Vip BIT NOT NULL DEFAULT 0);
         CREATE TABLE Orders (Id BIGINT IDENTITY PRIMARY KEY, CustomerId INT NOT NULL REFERENCES Customers(Id) ON DELETE CASCADE, Total MONEY);
         CREATE INDEX IX_Orders_Customer ON Orders (CustomerId)",
    );
    (engine, s)
}

#[test]
fn identity_top_offset_fetch_and_names() {
    let (_e, mut s) = session();
    run(&mut s, "INSERT INTO customers (name, credit) VALUES (N'Zoë', 12.5), (N'Bo', 3), (N'Al', 7)");
    assert_eq!(one(&mut s, "SELECT SCOPE_IDENTITY()"), "3");
    assert_eq!(one(&mut s, "SELECT @@IDENTITY"), "3");
    // Names match regardless of case and brackets.
    assert_eq!(
        rows(&mut s, "SELECT TOP 2 [name] FROM [DBO].[CUSTOMERS] ORDER BY Credit DESC"),
        vec![vec![Some("Zoë".into())], vec![Some("Al".into())]]
    );
    assert_eq!(rows(&mut s, "SELECT TOP (1) Name FROM Customers ORDER BY Id"), vec![vec![Some("Zoë".into())]]);
    assert_eq!(rows(&mut s, "SELECT Name FROM Customers ORDER BY Id OFFSET 1 ROWS FETCH NEXT 1 ROWS ONLY"), vec![vec![Some("Bo".into())]]);
    assert_eq!(rows(&mut s, "SELECT Name FROM Customers ORDER BY Id OFFSET 2 ROWS"), vec![vec![Some("Al".into())]]);
    assert_eq!(one(&mut s, "SELECT ISNULL(NULL, N'x') + N'y'"), "xy");
    assert_eq!(one(&mut s, "SELECT LEN(N'ab  ')"), "2");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Customers WHERE Ref IS NOT NULL AND Joined IS NOT NULL"), "3");
    // Explicit ids (IDENTITY_INSERT) move the identity on, as in SQL Server.
    run(
        &mut s,
        "SET IDENTITY_INSERT Customers ON; INSERT INTO Customers (Id, Name) VALUES (10, N'Ten'); SET IDENTITY_INSERT Customers OFF",
    );
    run(&mut s, "INSERT INTO Customers (Name) VALUES (N'Next')");
    assert_eq!(one(&mut s, "SELECT SCOPE_IDENTITY()"), "11");
}

#[test]
fn output_rowcount_and_errors() {
    let (_e, mut s) = session();
    let out = run(&mut s, "INSERT INTO Customers (Name) OUTPUT INSERTED.Id, INSERTED.Name VALUES (N'New')");
    let rs = out[0].result.as_ref().unwrap();
    assert_eq!(rs.rows.len(), 1);
    run(&mut s, "INSERT INTO Orders (CustomerId, Total) VALUES (1, 10), (1, 20)");
    run(&mut s, "UPDATE Orders SET Total = Total + 1 WHERE CustomerId = 1");
    assert_eq!(one(&mut s, "SELECT @@ROWCOUNT"), "2");
    let out = run(&mut s, "DELETE FROM Orders OUTPUT DELETED.Total WHERE Total > 20");
    assert_eq!(out[0].result.as_ref().unwrap().rows.len(), 1);
    let e = s.execute("INSERT INTO Customers (Name) VALUES (N'New')").error.unwrap();
    assert_eq!(e.sqlstate, "23505");
    let e = s.execute("SELECT * FROM Nope").error.unwrap();
    assert_eq!(e.sqlstate, "42P01");
    run(&mut s, "SET NOCOUNT ON; SET ANSI_NULLS ON; SET XACT_ABORT ON");
    run(&mut s, "BEGIN TRANSACTION; DELETE FROM Orders; ROLLBACK TRANSACTION");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Orders"), "1");
}

#[test]
fn system_views_and_functions() {
    let (_e, mut s) = session();
    assert_eq!(one(&mut s, "SELECT SCHEMA_NAME()"), "dbo");
    assert_eq!(one(&mut s, "SELECT SCHEMA_ID(N'dbo')"), "1");
    assert_eq!(one(&mut s, "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64))"), "16.0.1000.6");
    assert_ne!(one(&mut s, "SELECT OBJECT_ID(N'[dbo].[Orders]')"), "NULL");
    let r = rows(
        &mut s,
        "SELECT o.name, c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, dc.definition, c.is_identity \
         FROM sys.objects o JOIN sys.columns c ON c.object_id = o.object_id JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
         WHERE o.schema_id = SCHEMA_ID(N'dbo') AND o.type IN ('U', 'V') AND o.name = 'Customers' ORDER BY c.column_id",
    );
    assert_eq!(
        r[0],
        vec![
            Some("Customers".into()),
            Some("Id".into()),
            Some("int".into()),
            Some("4".into()),
            Some("10".into()),
            Some("0".into()),
            Some("0".into()),
            None,
            Some("1".into())
        ]
    );
    assert_eq!(r[1][2..4], [Some("nvarchar".into()), Some("200".into())]);
    assert_eq!(r[2][7], Some("(0)".into()));
    let fk = rows(
        &mut s,
        "SELECT fk.name, fk.delete_referential_action_desc FROM sys.foreign_keys fk JOIN sys.objects o ON o.object_id = fk.parent_object_id WHERE o.name = 'Orders'",
    );
    assert_eq!(fk[0][1], Some("CASCADE".into()));
    let ix = rows(
        &mut s,
        "SELECT i.name, i.is_unique, i.is_primary_key FROM sys.indexes i JOIN sys.objects o ON o.object_id = i.object_id WHERE o.name = 'Orders' ORDER BY i.index_id",
    );
    assert_eq!(ix.len(), 2);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = 'dbo'"), "2");
    // Default constraints: named, added with ADD … FOR, dropped by name.
    run(&mut s, "ALTER TABLE Orders ADD CONSTRAINT DF_Orders_Total DEFAULT 5 FOR Total");
    assert_eq!(one(&mut s, "SELECT name FROM sys.default_constraints WHERE parent_object_id = OBJECT_ID(N'Orders')"), "DF_Orders_Total");
    run(&mut s, "ALTER TABLE Orders DROP CONSTRAINT DF_Orders_Total; ALTER TABLE Customers DROP CONSTRAINT DF__Customers__Vip");
    assert_eq!(
        one(
            &mut s,
            "SELECT COUNT(*) FROM sys.default_constraints WHERE parent_object_id IN (OBJECT_ID(N'Orders'), OBJECT_ID(N'Customers'))"
        ),
        "3"
    );
    run(&mut s, "ALTER TABLE Customers ALTER COLUMN Name NVARCHAR(200)");
    assert_eq!(one(&mut s, "SELECT is_nullable FROM sys.columns WHERE object_id = OBJECT_ID(N'Customers') AND name = 'Name'"), "1");
}

#[test]
fn batches_variables_and_control_flow() {
    let (_e, mut s) = session();
    // GO batches and statements without semicolons.
    run(
        &mut s,
        "CREATE TABLE Logs (Id INT IDENTITY PRIMARY KEY, Msg NVARCHAR(100))\nINSERT INTO Logs (Msg) VALUES (N'a')\nGO\nINSERT INTO Logs (Msg) VALUES (N'b')",
    );
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs"), "2");
    // Variables, IF/ELSE, WHILE, SELECT @v = …
    run(
        &mut s,
        "DECLARE @i INT = 0, @last NVARCHAR(100);
         WHILE @i < 3
         BEGIN
             SET @i = @i + 1
             INSERT INTO Logs (Msg) VALUES (CONCAT(N'loop ', @i))
         END
         SELECT @last = Msg FROM Logs ORDER BY Id
         IF @last = N'loop 3' INSERT INTO Logs (Msg) VALUES (N'done') ELSE INSERT INTO Logs (Msg) VALUES (N'wrong')
         IF NOT EXISTS (SELECT 1 FROM Logs WHERE Msg = N'zzz') BEGIN INSERT INTO Logs (Msg) VALUES (N'no zzz'); END",
    );
    assert_eq!(
        rows(&mut s, "SELECT Msg FROM Logs WHERE Id > 2 ORDER BY Id").into_iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(),
        ["loop 1", "loop 2", "loop 3", "done", "no zzz"]
    );
    // The classic upsert: UPDATE, then INSERT when nothing matched.
    run(&mut s, "UPDATE Logs SET Msg = N'x' WHERE Msg = N'nope'; IF @@ROWCOUNT = 0 INSERT INTO Logs (Msg) VALUES (N'inserted')");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs WHERE Msg = N'inserted'"), "1");
    // Dynamic SQL and the constraint-dropping pattern TwinBase's own migrations use.
    run(&mut s, "DECLARE @t sysname = N'Logs'; EXEC(N'DELETE FROM ' + @t + N' WHERE Msg = N''a''')");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs WHERE Msg = N'a'"), "0");
    run(&mut s, "EXEC sp_executesql N'INSERT INTO Logs (Msg) VALUES (@m)', N'@m nvarchar(100)', @m = N'via sp_executesql'");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs WHERE Msg = N'via sp_executesql'"), "1");
    run(
        &mut s,
        "EXEC(N'DECLARE @n sysname = (SELECT d.name FROM sys.default_constraints d JOIN sys.columns c ON c.default_object_id = d.object_id WHERE d.parent_object_id = OBJECT_ID(N''[Customers]'') AND c.name = N''Credit''); IF @n IS NOT NULL EXEC(N''ALTER TABLE [Customers] DROP CONSTRAINT ['' + @n + N'']'');');",
    );
    // RETURN ends the batch; THROW and RAISERROR fail it with the given message.
    run(&mut s, "INSERT INTO Logs (Msg) VALUES (N'before'); RETURN; INSERT INTO Logs (Msg) VALUES (N'after')");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs WHERE Msg = N'after'"), "0");
    let e = s.execute("IF 1 = 1 THROW 50001, N'custom failure', 1").error.unwrap();
    assert_eq!((e.message.as_str(), e.mssql_code), ("custom failure", Some(50001)));
    let e = s.execute("RAISERROR(N'bad %s', 16, 1, N'thing')").error.unwrap();
    assert_eq!(e.message, "bad thing");
    let e = s.execute("SELECT @nope").error.unwrap();
    assert_eq!(e.mssql_code, Some(137), "{e}");
    // TRY … CATCH sees the error; a bare THROW re-raises it.
    run(
        &mut s,
        "INSERT INTO Customers (Name, Credit) VALUES (N'Bo', 1); BEGIN TRY\n INSERT INTO Customers (Name, Credit) VALUES (N'Bo', 1)\nEND TRY\nBEGIN CATCH\n INSERT INTO Logs (Msg) VALUES (CONCAT(ERROR_NUMBER(), N': caught'))\nEND CATCH",
    );
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM Logs WHERE Msg = N'2627: caught'"), "1");
    let e = s.execute("BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH THROW; END CATCH").error.unwrap();
    assert_eq!(e.sqlstate, "22012", "{e}");
    let e = s.execute("DECLARE @i INT = 0; WHILE 1 = 1 SET @i = @i + 1").error.unwrap();
    assert!(e.message.contains("stopped after"), "{e}");
}

#[test]
fn result_types_match_sql_server() {
    let (_e, mut s) = session();
    let ty = |s: &mut Session, sql: &str| s.execute(sql).outcomes.last().unwrap().result.as_ref().unwrap().columns[0].ty.clone();
    assert_eq!(ty(&mut s, "SELECT @@ROWCOUNT"), twinbase_engine::SqlType::Int4);
    assert_eq!(ty(&mut s, "SELECT COUNT(*) FROM Orders"), twinbase_engine::SqlType::Int4);
    assert_eq!(ty(&mut s, "SELECT COUNT_BIG(*) FROM Orders"), twinbase_engine::SqlType::Int8);
    assert_eq!(ty(&mut s, "SELECT CAST(1 AS int)"), twinbase_engine::SqlType::Int4);
}
