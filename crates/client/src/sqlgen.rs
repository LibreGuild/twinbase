//! SQL generation for the data grid: paged selects, filters, and staged edits.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{SqlDialect, TableSchema};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    StartsWith,
    EndsWith,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowQuery {
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default)]
    pub sort: Vec<SortKey>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Extra raw WHERE condition typed by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_sql: Option<String>,
}

fn default_limit() -> u64 {
    200
}

impl Default for RowQuery {
    fn default() -> Self {
        Self { offset: 0, limit: default_limit(), sort: vec![], filters: vec![], where_sql: None }
    }
}

/// Staged edits from the grid. Keys identify rows (primary key values).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChangeSet {
    #[serde(default)]
    pub updates: Vec<RowUpdate>,
    #[serde(default)]
    pub inserts: Vec<Map<String, Value>>,
    #[serde(default)]
    pub deletes: Vec<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowUpdate {
    pub key: Map<String, Value>,
    pub values: Map<String, Value>,
}

pub fn quote_ident(d: SqlDialect, s: &str) -> String {
    match d {
        SqlDialect::Mysql => format!("`{}`", s.replace('`', "``")),
        SqlDialect::Mssql => format!("[{}]", s.replace(']', "]]")),
        SqlDialect::Clickhouse | SqlDialect::Spanner | SqlDialect::Bigquery => format!("`{}`", s.replace('\\', "\\\\").replace('`', "\\`")),
        _ => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

pub fn qualified(d: SqlDialect, schema: &str, table: &str) -> String {
    if (d == SqlDialect::Sqlite && (schema.is_empty() || schema == "main")) || (d == SqlDialect::Spanner && schema.is_empty()) {
        return quote_ident(d, table);
    }
    format!("{}.{}", quote_ident(d, schema), quote_ident(d, table))
}

pub fn string_literal(d: SqlDialect, s: &str) -> String {
    if matches!(d, SqlDialect::Spanner | SqlDialect::Bigquery) {
        // GoogleSQL escapes quotes with a backslash (no doubling).
        return format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n"));
    }
    let mut out = s.replace('\'', "''");
    if matches!(d, SqlDialect::Mysql | SqlDialect::Clickhouse) {
        out = out.replace('\\', "\\\\");
    }
    if d == SqlDialect::Mssql {
        // N'…' keeps non-Latin text intact in nvarchar columns and comparisons.
        return format!("N'{out}'");
    }
    format!("'{out}'")
}

pub fn literal(d: SqlDialect, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match d {
            SqlDialect::Postgres | SqlDialect::Cassandra | SqlDialect::Spanner | SqlDialect::Bigquery => {
                if *b {
                    "TRUE".into()
                } else {
                    "FALSE".into()
                }
            }
            _ => {
                if *b {
                    "1".into()
                } else {
                    "0".into()
                }
            }
        },
        Value::Number(n) => n.to_string(),
        Value::String(s) => string_literal(d, s),
        other => string_literal(d, &other.to_string()),
    }
}

fn like_escape(d: SqlDialect, s: &str) -> String {
    let s = s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    // `[` starts a character class in T-SQL patterns.
    if d == SqlDialect::Mssql { s.replace('[', "\\[") } else { s }
}

fn condition(d: SqlDialect, f: &Filter) -> String {
    let col = quote_ident(d, &f.column);
    let text_col = match d {
        SqlDialect::Postgres => format!("CAST({col} AS TEXT)"),
        SqlDialect::Mssql => format!("CAST({col} AS NVARCHAR(MAX))"),
        SqlDialect::Clickhouse => format!("toString({col})"),
        // GoogleSQL has no ILIKE: compare lower-cased text with a lower-cased pattern.
        SqlDialect::Spanner | SqlDialect::Bigquery => format!("LOWER(CAST({col} AS STRING))"),
        SqlDialect::Oracle => format!("LOWER(TO_CHAR({col}))"),
        _ => col.clone(),
    };
    let like = match d {
        SqlDialect::Postgres | SqlDialect::Clickhouse => "ILIKE",
        _ => "LIKE",
    };
    let escape = match d {
        SqlDialect::Mysql | SqlDialect::Clickhouse | SqlDialect::Spanner | SqlDialect::Bigquery => "",
        _ => " ESCAPE '\\'",
    };
    let text = match &f.value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let text = if matches!(d, SqlDialect::Spanner | SqlDialect::Bigquery | SqlDialect::Oracle) { text.to_lowercase() } else { text };
    match f.op {
        FilterOp::Eq => format!("{col} = {}", literal(d, &f.value)),
        FilterOp::Ne => format!("{col} <> {}", literal(d, &f.value)),
        FilterOp::Lt => format!("{col} < {}", literal(d, &f.value)),
        FilterOp::Le => format!("{col} <= {}", literal(d, &f.value)),
        FilterOp::Gt => format!("{col} > {}", literal(d, &f.value)),
        FilterOp::Ge => format!("{col} >= {}", literal(d, &f.value)),
        FilterOp::Contains => format!("{text_col} {like} {}{escape}", string_literal(d, &format!("%{}%", like_escape(d, &text)))),
        FilterOp::StartsWith => format!("{text_col} {like} {}{escape}", string_literal(d, &format!("{}%", like_escape(d, &text)))),
        FilterOp::EndsWith => format!("{text_col} {like} {}{escape}", string_literal(d, &format!("%{}", like_escape(d, &text)))),
        FilterOp::IsNull => format!("{col} IS NULL"),
        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
    }
}

fn where_clause(d: SqlDialect, q: &RowQuery) -> String {
    let mut parts: Vec<String> = q.filters.iter().map(|f| condition(d, f)).collect();
    if let Some(w) = q.where_sql.as_ref().filter(|w| !w.trim().is_empty()) {
        parts.push(format!("({w})"));
    }
    if parts.is_empty() { String::new() } else { format!(" WHERE {}", parts.join(" AND ")) }
}

pub fn select_page(d: SqlDialect, schema: &str, table: &str, q: &RowQuery, row_id: Option<&str>, info: &TableSchema) -> String {
    let mut cols = String::new();
    if let Some(r) = row_id {
        cols.push_str(&format!("{r} AS {}, ", quote_ident(d, &format!("__{r}"))));
    }
    cols.push('*');
    let mut sql = format!("SELECT {cols} FROM {}{}", qualified(d, schema, table), where_clause(d, q));
    let mut sort: Vec<String> =
        q.sort.iter().map(|s| format!("{}{}", quote_ident(d, &s.column), if s.descending { " DESC" } else { "" })).collect();
    if sort.is_empty() {
        // Stable paging: order by the primary key when the user did not choose an order.
        sort = info.primary_key.iter().map(|c| quote_ident(d, c)).collect();
    }
    if d == SqlDialect::Cassandra {
        // Rows come in token order; filters on non-key columns need ALLOW FILTERING. The caller
        // skips `offset` rows itself (CQL has no OFFSET).
        sql.push_str(&format!(" LIMIT {}", q.offset.min(10_000) + q.limit));
        if !q.filters.is_empty() || q.where_sql.as_ref().is_some_and(|w| !w.trim().is_empty()) {
            sql.push_str(" ALLOW FILTERING");
        }
        return sql;
    }
    if d == SqlDialect::Oracle {
        if !sort.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", sort.join(", ")));
        }
        sql.push_str(&format!(" OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", q.offset, q.limit));
        return sql;
    }
    if d == SqlDialect::Mssql {
        // OFFSET … FETCH needs an ORDER BY.
        let order = if sort.is_empty() { "(SELECT NULL)".to_owned() } else { sort.join(", ") };
        sql.push_str(&format!(" ORDER BY {order} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", q.offset, q.limit));
        return sql;
    }
    if !sort.is_empty() {
        sql.push_str(&format!(" ORDER BY {}", sort.join(", ")));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", q.limit, q.offset));
    sql
}

/// Rows the grid counts at most: beyond this the total shows as "N+" (counting every row of a big
/// table on every page would scan it each time).
pub const COUNT_CAP: u64 = 100_000;

/// Count matching rows, stopping after `COUNT_CAP + 1`.
pub fn count_rows(d: SqlDialect, schema: &str, table: &str, q: &RowQuery) -> String {
    let source = format!("{}{}", qualified(d, schema, table), where_clause(d, q));
    let n = COUNT_CAP + 1;
    match d {
        SqlDialect::Mssql => format!("SELECT COUNT_BIG(*) FROM (SELECT TOP ({n}) 1 AS x FROM {source}) AS tb_count"),
        SqlDialect::Oracle => format!("SELECT COUNT(*) FROM (SELECT 1 FROM {source} FETCH FIRST {n} ROWS ONLY)"),
        _ => format!("SELECT count(*) FROM (SELECT 1 FROM {source} LIMIT {n}) AS tb_count"),
    }
}

fn key_match(d: SqlDialect, key: &Map<String, Value>) -> String {
    key.iter()
        .map(|(k, v)| {
            let col = if k == "__rowid" { "rowid".to_owned() } else { quote_ident(d, k) };
            if v.is_null() { format!("{col} IS NULL") } else { format!("{col} = {}", literal(d, v)) }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A CQL literal for a value of a column type (`uuid`, `blob`, collections and numbers are not
/// quoted; text, timestamps, dates and inet are).
pub fn cql_literal(v: &Value, ty: &str) -> String {
    let t = ty.trim().to_ascii_lowercase();
    let inner = |t: &str, open: &str| t.strip_prefix(open).and_then(|r| r.strip_suffix('>')).map(str::to_owned);
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let numeric =
                matches!(t.as_str(), "int" | "bigint" | "smallint" | "tinyint" | "varint" | "decimal" | "float" | "double" | "counter");
            let uuid = (t == "uuid" || t == "timeuuid") && s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if uuid || (numeric && s.parse::<f64>().is_ok()) {
                s.clone()
            } else if t == "blob" && s.starts_with("\\x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()) {
                format!("0x{}", &s[2..])
            } else if t == "boolean" && (s == "true" || s == "false") {
                s.clone()
            } else {
                string_literal(SqlDialect::Cassandra, s)
            }
        }
        Value::Array(xs) => {
            let frozen = t.strip_prefix("frozen<").and_then(|r| r.strip_suffix('>')).unwrap_or(&t).to_owned();
            if let Some(e) = inner(&frozen, "set<") {
                format!("{{{}}}", xs.iter().map(|x| cql_literal(x, &e)).collect::<Vec<_>>().join(", "))
            } else {
                let e = inner(&frozen, "list<").unwrap_or_default();
                format!("[{}]", xs.iter().map(|x| cql_literal(x, &e)).collect::<Vec<_>>().join(", "))
            }
        }
        Value::Object(m) => {
            let frozen = t.strip_prefix("frozen<").and_then(|r| r.strip_suffix('>')).unwrap_or(&t).to_owned();
            let value_type = inner(&frozen, "map<").and_then(|kv| kv.split_once(',').map(|(_, v)| v.trim().to_owned())).unwrap_or_default();
            format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, x)| format!("{}: {}", string_literal(SqlDialect::Cassandra, k), cql_literal(x, &value_type)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// A GoogleSQL literal for a value of a Spanner column type (`INT64`, `NUMERIC`, `BYTES(MAX)`,
/// `JSON`, `TIMESTAMP`, `ARRAY<…>`).
pub fn googlesql_literal(v: &Value, ty: &str) -> String {
    let t = ty.trim().to_ascii_uppercase();
    let base = t.split('(').next().unwrap_or(&t).to_owned();
    let d = SqlDialect::Spanner;
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if *b {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Number(n) if base == "NUMERIC" => format!("NUMERIC {}", string_literal(d, &n.to_string())),
        Value::Number(n) => n.to_string(),
        Value::String(s) => match base.as_str() {
            "INT64" if s.parse::<i64>().is_ok() => s.clone(),
            "FLOAT64" | "FLOAT32" if s.parse::<f64>().is_ok() => s.clone(),
            "NUMERIC" => format!("NUMERIC {}", string_literal(d, s)),
            "BYTES" if s.starts_with("\\x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()) => {
                format!("FROM_HEX({})", string_literal(d, &s[2..]))
            }
            "TIMESTAMP" | "DATE" | "JSON" => format!("{base} {}", string_literal(d, s)),
            _ => string_literal(d, s),
        },
        Value::Array(xs) => {
            let e = t.strip_prefix("ARRAY<").and_then(|r| r.strip_suffix('>')).unwrap_or("").to_owned();
            format!("[{}]", xs.iter().map(|x| googlesql_literal(x, &e)).collect::<Vec<_>>().join(", "))
        }
        Value::Object(_) if base == "JSON" => format!("JSON {}", string_literal(d, &v.to_string())),
        Value::Object(_) => string_literal(d, &v.to_string()),
    }
}

impl ChangeSet {
    /// Statements with literals typed by column, for CQL ([`cql_literal`]) and GoogleSQL
    /// ([`googlesql_literal`]).
    pub fn to_sql_typed(&self, d: SqlDialect, schema: &str, table: &str, types: &std::collections::HashMap<String, String>) -> Vec<String> {
        let t = qualified(d, schema, table);
        let lit = |k: &str, v: &Value| {
            let ty = types.get(k).map(String::as_str).unwrap_or("");
            if d == SqlDialect::Cassandra { cql_literal(v, if ty.is_empty() { "text" } else { ty }) } else { googlesql_literal(v, ty) }
        };
        let key = |m: &Map<String, Value>| {
            m.iter().map(|(k, v)| format!("{} = {}", quote_ident(d, k), lit(k, v))).collect::<Vec<_>>().join(" AND ")
        };
        let mut out = Vec::new();
        for k in &self.deletes {
            out.push(format!("DELETE FROM {t} WHERE {}", key(k)));
        }
        for u in self.updates.iter().filter(|u| !u.values.is_empty()) {
            let set = u.values.iter().map(|(k, v)| format!("{} = {}", quote_ident(d, k), lit(k, v))).collect::<Vec<_>>().join(", ");
            out.push(format!("UPDATE {t} SET {set} WHERE {}", key(&u.key)));
        }
        for row in self.inserts.iter().filter(|r| !r.is_empty()) {
            let cols = row.keys().map(|k| quote_ident(d, k)).collect::<Vec<_>>().join(", ");
            let vals = row.iter().map(|(k, v)| lit(k, v)).collect::<Vec<_>>().join(", ");
            out.push(format!("INSERT INTO {t} ({cols}) VALUES ({vals})"));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
    }

    /// Statements in apply order: deletes, updates, inserts.
    pub fn to_sql(&self, d: SqlDialect, schema: &str, table: &str) -> Vec<String> {
        let t = qualified(d, schema, table);
        let mut out = Vec::new();
        for key in &self.deletes {
            out.push(format!("DELETE FROM {t} WHERE {}", key_match(d, key)));
        }
        for u in &self.updates {
            if u.values.is_empty() {
                continue;
            }
            let set = u.values.iter().map(|(k, v)| format!("{} = {}", quote_ident(d, k), literal(d, v))).collect::<Vec<_>>().join(", ");
            out.push(format!("UPDATE {t} SET {set} WHERE {}", key_match(d, &u.key)));
        }
        for row in &self.inserts {
            if row.is_empty() {
                out.push(match d {
                    SqlDialect::Mysql => format!("INSERT INTO {t} () VALUES ()"),
                    _ => format!("INSERT INTO {t} DEFAULT VALUES"),
                });
                continue;
            }
            let cols = row.keys().map(|k| quote_ident(d, k)).collect::<Vec<_>>().join(", ");
            let vals = row.values().map(|v| literal(d, v)).collect::<Vec<_>>().join(", ");
            out.push(format!("INSERT INTO {t} ({cols}) VALUES ({vals})"));
        }
        out
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn oracle_paging_and_patterns() {
        let info = TableSchema {
            schema: "APP".into(),
            name: "T".into(),
            kind: "table".into(),
            columns: vec![],
            primary_key: vec!["ID".into()],
            foreign_keys: vec![],
            indexes: vec![],
        };
        let q = RowQuery {
            offset: 50,
            limit: 25,
            filters: vec![Filter { column: "NAME".into(), op: FilterOp::Contains, value: json!("Ab_") }],
            ..Default::default()
        };
        assert_eq!(
            select_page(SqlDialect::Oracle, "APP", "T", &q, None, &info),
            r#"SELECT * FROM "APP"."T" WHERE LOWER(TO_CHAR("NAME")) LIKE '%ab\_%' ESCAPE '\' ORDER BY "ID" OFFSET 50 ROWS FETCH NEXT 25 ROWS ONLY"#
        );
    }

    #[test]
    fn googlesql_literals_follow_column_types() {
        assert_eq!(googlesql_literal(&json!("9007199254740993"), "INT64"), "9007199254740993");
        assert_eq!(googlesql_literal(&json!("1.20"), "NUMERIC"), "NUMERIC '1.20'");
        assert_eq!(googlesql_literal(&json!("\\xca"), "BYTES(MAX)"), "FROM_HEX('ca')");
        assert_eq!(googlesql_literal(&json!({"k": 1}), "JSON"), r#"JSON '{"k":1}'"#);
        assert_eq!(googlesql_literal(&json!("2024-01-01T00:00:00Z"), "TIMESTAMP"), "TIMESTAMP '2024-01-01T00:00:00Z'");
        assert_eq!(googlesql_literal(&json!(["a", "it's"]), "ARRAY<STRING(MAX)>"), r#"['a', 'it\'s']"#);
        assert_eq!(googlesql_literal(&json!(true), "BOOL"), "TRUE");
        assert_eq!(qualified(SqlDialect::Spanner, "", "t"), "`t`");
    }

    #[test]
    fn cql_literals_follow_column_types() {
        assert_eq!(cql_literal(&json!("6f9619ff-8b86-d011-b42d-00c04fc964ff"), "uuid"), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
        assert_eq!(cql_literal(&json!("6f9619ff-8b86-d011-b42d-00c04fc964ff"), "text"), "'6f9619ff-8b86-d011-b42d-00c04fc964ff'");
        assert_eq!(cql_literal(&json!("\\xcafe"), "blob"), "0xcafe");
        assert_eq!(cql_literal(&json!("12.50"), "decimal"), "12.50");
        assert_eq!(cql_literal(&json!(["a", "b"]), "set<text>"), "{'a', 'b'}");
        assert_eq!(cql_literal(&json!([1, 2]), "frozen<list<int>>"), "[1, 2]");
        assert_eq!(cql_literal(&json!({"k": 1}), "map<text, int>"), "{'k': 1}");
        assert_eq!(cql_literal(&json!(true), "boolean"), "true");
        let info = TableSchema {
            schema: "ks".into(),
            name: "t".into(),
            kind: "table".into(),
            columns: vec![],
            primary_key: vec!["id".into()],
            foreign_keys: vec![],
            indexes: vec![],
        };
        let q = RowQuery {
            offset: 400,
            limit: 100,
            filters: vec![Filter { column: "n".into(), op: FilterOp::Gt, value: json!(3) }],
            ..Default::default()
        };
        assert_eq!(
            select_page(SqlDialect::Cassandra, "ks", "t", &q, None, &info),
            r#"SELECT * FROM "ks"."t" WHERE "n" > 3 LIMIT 500 ALLOW FILTERING"#
        );
    }

    #[test]
    fn sql_server_paging_quoting_and_patterns() {
        let info = TableSchema {
            schema: "dbo".into(),
            name: "t".into(),
            kind: "table".into(),
            columns: vec![],
            primary_key: vec!["id".into()],
            foreign_keys: vec![],
            indexes: vec![],
        };
        let q = RowQuery {
            offset: 20,
            limit: 10,
            filters: vec![Filter { column: "na]me".into(), op: FilterOp::Contains, value: json!("50%[x]") }],
            ..Default::default()
        };
        assert_eq!(
            select_page(SqlDialect::Mssql, "dbo", "t", &q, None, &info),
            r#"SELECT * FROM [dbo].[t] WHERE CAST([na]]me] AS NVARCHAR(MAX)) LIKE N'%50\%\[x]%' ESCAPE '\' ORDER BY [id] OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"#
        );
        let no_pk = TableSchema { primary_key: vec![], ..info };
        assert!(
            select_page(SqlDialect::Mssql, "dbo", "t", &RowQuery::default(), None, &no_pk).contains("ORDER BY (SELECT NULL) OFFSET 0 ROWS")
        );
        let mut cs = ChangeSet::default();
        cs.inserts.push(Map::new());
        cs.updates.push(RowUpdate {
            key: json!({"id": 1}).as_object().unwrap().clone(),
            values: json!({"ok": true, "name": "Zoë"}).as_object().unwrap().clone(),
        });
        assert_eq!(
            cs.to_sql(SqlDialect::Mssql, "dbo", "t"),
            vec!["UPDATE [dbo].[t] SET [ok] = 1, [name] = N'Zoë' WHERE [id] = 1", "INSERT INTO [dbo].[t] DEFAULT VALUES"]
        );
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn generates_dialect_specific_sql() {
        let mut cs = ChangeSet::default();
        cs.updates.push(RowUpdate {
            key: json!({"id": 1}).as_object().unwrap().clone(),
            values: json!({"name": "O'Neil"}).as_object().unwrap().clone(),
        });
        cs.inserts.push(json!({"name": "x\\y"}).as_object().unwrap().clone());
        cs.deletes.push(json!({"id": 2}).as_object().unwrap().clone());
        assert_eq!(
            cs.to_sql(SqlDialect::Postgres, "public", "users"),
            vec![
                r#"DELETE FROM "public"."users" WHERE "id" = 2"#,
                r#"UPDATE "public"."users" SET "name" = 'O''Neil' WHERE "id" = 1"#,
                r#"INSERT INTO "public"."users" ("name") VALUES ('x\y')"#,
            ]
        );
        assert_eq!(cs.to_sql(SqlDialect::Mysql, "shop", "users")[2], r#"INSERT INTO `shop`.`users` (`name`) VALUES ('x\\y')"#);
    }

    #[test]
    fn filters_and_paging() {
        let q = RowQuery {
            offset: 20,
            limit: 10,
            sort: vec![SortKey { column: "name".into(), descending: true }],
            filters: vec![Filter { column: "email".into(), op: FilterOp::Contains, value: json!("50%") }],
            where_sql: None,
        };
        let info = TableSchema {
            schema: "public".into(),
            name: "t".into(),
            kind: "table".into(),
            columns: vec![],
            primary_key: vec![],
            foreign_keys: vec![],
            indexes: vec![],
        };
        assert_eq!(
            select_page(SqlDialect::Postgres, "public", "t", &q, None, &info),
            r#"SELECT * FROM "public"."t" WHERE CAST("email" AS TEXT) ILIKE '%50\%%' ESCAPE '\' ORDER BY "name" DESC LIMIT 10 OFFSET 20"#
        );
    }
}
