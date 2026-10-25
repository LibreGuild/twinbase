//! End-to-end behaviour of the engine in MySQL mode (no network).

use twinbase_engine::{Dialect, Engine, Outcome, Session};

fn run(s: &mut Session, sql: &str) -> Vec<Outcome> {
    let r = s.execute(sql);
    if let Some(e) = r.error {
        panic!("{sql}\n=> {e} (mysql {})", e.mysql_code);
    }
    r.outcomes
}

fn err(s: &mut Session, sql: &str) -> twinbase_engine::EngineError {
    s.execute(sql).error.unwrap_or_else(|| panic!("expected error: {sql}"))
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = run(s, sql);
    let rs = out.last().unwrap().result.as_ref().expect("result set");
    rs.rows
        .iter()
        .map(|r| r.iter().zip(&rs.columns).map(|(v, c)| twinbase_engine::value::format_text(v, &c.ty, Dialect::MySql)).collect())
        .collect()
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql)[0][0].clone().unwrap_or_else(|| "NULL".into())
}

fn session() -> (Engine, Session) {
    let engine = Engine::temporary(Dialect::MySql).unwrap();
    let mut s = engine.session(None, "root").unwrap();
    run(&mut s, "CREATE DATABASE shop; USE shop");
    run(
        &mut s,
        "CREATE TABLE `customers` (
          `id` INT NOT NULL AUTO_INCREMENT,
          `email` VARCHAR(100) NOT NULL,
          `tier` ENUM('free','pro') NOT NULL DEFAULT 'free',
          `is_active` TINYINT(1) NOT NULL DEFAULT 1,
          `created_at` DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
          `updated_at` DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
          PRIMARY KEY (`id`),
          UNIQUE KEY `uniq_email` (`email`)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
        CREATE TABLE purchases (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          customer_id INT NOT NULL,
          amount DECIMAL(10,2) NOT NULL,
          KEY idx_customer (customer_id),
          CONSTRAINT fk_customer FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE
        )",
    );
    (engine, s)
}

#[test]
fn auto_increment_and_last_insert_id() {
    let (_e, mut s) = session();
    let out = run(&mut s, "INSERT INTO customers (email) VALUES ('a@x.io')");
    assert_eq!(out[0].affected, 1);
    assert_eq!(out[0].last_insert_id, Some(1));
    run(&mut s, "INSERT INTO customers (id, email) VALUES (10, 'b@x.io')");
    let out = run(&mut s, "INSERT INTO customers (email) VALUES ('c@x.io')");
    assert_eq!(out[0].last_insert_id, Some(11));
    assert_eq!(one(&mut s, "SELECT LAST_INSERT_ID()"), "11");
    run(&mut s, "INSERT INTO customers (id, email) VALUES (NULL, 'd@x.io')");
    assert_eq!(one(&mut s, "SELECT max(id) FROM customers"), "12");
}

#[test]
fn mysql_semantics() {
    let (_e, mut s) = session();
    run(&mut s, "INSERT INTO customers (email) VALUES ('Ann@X.io')");
    // Case-insensitive comparisons (utf8mb4_0900_ai_ci).
    assert_eq!(one(&mut s, "SELECT count(*) FROM customers WHERE email = 'ann@x.io'"), "1");
    assert_eq!(one(&mut s, "SELECT tier FROM customers"), "free");
    assert_eq!(one(&mut s, "SELECT is_active FROM customers"), "1");
    run(&mut s, "INSERT INTO customers (email, tier) VALUES ('b@x.io', 'PRO')");
    assert_eq!(one(&mut s, "SELECT tier FROM customers WHERE email = 'b@x.io'"), "pro");
    run(&mut s, "INSERT INTO customers (email) VALUES ('ann@x.io') ON DUPLICATE KEY UPDATE tier = VALUES(tier), is_active = 0");
    assert_eq!(one(&mut s, "SELECT is_active FROM customers WHERE id = 1"), "0");
    run(&mut s, "INSERT IGNORE INTO customers (email) VALUES ('ann@x.io')");
    assert_eq!(one(&mut s, "SELECT count(*) FROM customers"), "2");
    assert_eq!(one(&mut s, "SELECT 7 / 2"), "3.5000");
    assert_eq!(one(&mut s, "SELECT IF(1 > 0, 'yes', 'no')"), "yes");
    assert_eq!(one(&mut s, "SELECT CONCAT('a', 'b', 'c')"), "abc");
    assert_eq!(one(&mut s, "SELECT DATE_FORMAT('2024-03-05 14:07:09', '%Y/%m/%d')"), "2024/03/05");
    assert_eq!(one(&mut s, "SELECT DATE_ADD('2024-01-31', INTERVAL 1 DAY)"), "2024-02-01");
    assert_eq!(one(&mut s, "SELECT email FROM customers ORDER BY id LIMIT 1, 1"), "b@x.io");
    assert_eq!(one(&mut s, "SELECT GROUP_CONCAT(email SEPARATOR '|') FROM customers"), "Ann@X.io|b@x.io");
    assert_eq!(one(&mut s, "SELECT 1 FROM dual"), "1");
    let r = run(&mut s, "SELECT COUNT(*) FROM customers");
    assert_eq!(r[0].result.as_ref().unwrap().columns[0].name, "COUNT(*)");
}

#[test]
fn mysql_errors() {
    let (_e, mut s) = session();
    run(&mut s, "INSERT INTO customers (email) VALUES ('a@x.io')");
    let e = err(&mut s, "INSERT INTO customers (email) VALUES ('a@x.io')");
    assert_eq!(e.mysql_code, 1062);
    let e = err(&mut s, "INSERT INTO customers (email) VALUES (NULL)");
    assert_eq!(e.mysql_code, 1048);
    let e = err(&mut s, "INSERT INTO purchases (customer_id, amount) VALUES (99, 1)");
    assert_eq!(e.mysql_code, 1452);
    let e = err(&mut s, "SELECT * FROM missing");
    assert_eq!(e.mysql_code, 1146);
    assert_eq!(e.message, "Table 'shop.missing' doesn't exist");
    let e = err(&mut s, "INSERT INTO customers (email, tier) VALUES ('z@x.io', 'gold')");
    assert_eq!(e.mysql_code, 1265);
    let e = err(&mut s, "CREATE TABLE customers (id int)");
    assert_eq!(e.mysql_code, 1050);
}

#[test]
fn show_describe_and_variables() {
    let (_e, mut s) = session();
    assert_eq!(rows(&mut s, "SHOW TABLES"), vec![vec![Some("customers".into())], vec![Some("purchases".into())]]);
    let d = rows(&mut s, "DESCRIBE customers");
    assert_eq!(
        d[0],
        vec![Some("id".into()), Some("int".into()), Some("NO".into()), Some("PRI".into()), None, Some("auto_increment".into())]
    );
    assert_eq!(d[2][1].as_deref(), Some("enum('free','pro')"));
    assert_eq!(d[5][5].as_deref(), Some("DEFAULT_GENERATED on update CURRENT_TIMESTAMP"));
    let create = rows(&mut s, "SHOW CREATE TABLE purchases");
    assert!(create[0][1].as_deref().unwrap().contains("CONSTRAINT `fk_customer` FOREIGN KEY"));
    assert!(one(&mut s, "SELECT @@version").starts_with("8.0"));
    assert_eq!(one(&mut s, "SELECT @@session.auto_increment_increment"), "1");
    run(&mut s, "SET NAMES utf8mb4; SET autocommit = 1, sql_mode = 'STRICT_ALL_TABLES'");
    assert_eq!(one(&mut s, "SELECT @@sql_mode"), "STRICT_ALL_TABLES");
    assert_eq!(one(&mut s, "SELECT DATABASE()"), "shop");
    let r = rows(
        &mut s,
        "SELECT COLUMN_NAME, COLUMN_KEY FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'shop' AND TABLE_NAME = 'purchases' ORDER BY ORDINAL_POSITION",
    );
    assert_eq!(r[0], vec![Some("id".into()), Some("PRI".into())]);
    assert_eq!(r[1], vec![Some("customer_id".into()), Some("MUL".into())]);
    let dbs = rows(&mut s, "SHOW DATABASES");
    assert!(dbs.contains(&vec![Some("shop".into())]));
}

#[test]
fn autocommit_off_and_ddl_implicit_commit() {
    let (e, mut s) = session();
    run(&mut s, "SET autocommit = 0");
    run(&mut s, "INSERT INTO customers (email) VALUES ('t@x.io')");
    let mut other = e.session(Some("shop"), "root").unwrap();
    assert_eq!(one(&mut other, "SELECT count(*) FROM customers"), "0");
    run(&mut s, "ROLLBACK");
    assert_eq!(one(&mut s, "SELECT count(*) FROM customers"), "0");
    run(&mut s, "INSERT INTO customers (email) VALUES ('u@x.io')");
    run(&mut s, "COMMIT");
    assert_eq!(one(&mut other, "SELECT count(*) FROM customers"), "1");
}

#[test]
fn alter_and_on_update() {
    let (_e, mut s) = session();
    run(&mut s, "INSERT INTO customers (email) VALUES ('a@x.io')");
    run(&mut s, "UPDATE customers SET updated_at = '2000-01-01 00:00:00'");
    run(&mut s, "UPDATE customers SET tier = 'pro'");
    assert_ne!(one(&mut s, "SELECT updated_at FROM customers"), "2000-01-01 00:00:00");
    run(&mut s, "ALTER TABLE customers ADD COLUMN nickname VARCHAR(30) NULL AFTER email");
    assert_eq!(err(&mut s, "ALTER TABLE customers MODIFY COLUMN nickname VARCHAR(50) NOT NULL").mysql_code, 1138);
    run(&mut s, "UPDATE customers SET nickname = 'nick'");
    run(&mut s, "ALTER TABLE customers MODIFY COLUMN nickname VARCHAR(50) NOT NULL DEFAULT ''");
    run(&mut s, "ALTER TABLE customers CHANGE nickname handle VARCHAR(50)");
    run(&mut s, "ALTER TABLE customers ADD INDEX idx_handle (handle)");
    run(&mut s, "ALTER TABLE customers DROP INDEX idx_handle");
    run(&mut s, "ALTER TABLE purchases DROP FOREIGN KEY fk_customer");
    run(&mut s, "INSERT INTO purchases (customer_id, amount) VALUES (77, 2)");
    run(&mut s, "TRUNCATE TABLE purchases");
    let out = run(&mut s, "INSERT INTO purchases (customer_id, amount) VALUES (1, 2)");
    assert_eq!(out[0].last_insert_id, Some(1));
}

#[test]
fn read_only_transactions_refuse_writes() {
    let (_e, mut s) = session();
    run(&mut s, "START TRANSACTION READ ONLY");
    let e = err(&mut s, "INSERT INTO customers (email) VALUES ('ro@x.io')");
    assert_eq!(e.mysql_code, 1792, "{e}");
    run(&mut s, "ROLLBACK");
    run(&mut s, "INSERT INTO customers (email) VALUES ('rw@x.io')");
}
