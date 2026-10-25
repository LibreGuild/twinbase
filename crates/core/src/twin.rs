//! Change preview on a local twin: copy the affected tables and rows into a temporary TwinLite
//! database, run the statements there, and diff the result. Nothing touches the real server.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use sqlparser::ast::{FromTable, Statement, TableFactor, TableObject};
use sqlparser::dialect::{MsSqlDialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use twinbase_client::sqlgen::{literal, quote_ident};
use twinbase_client::{SqlClient, SqlDialect, TableSchema};
use twinbase_engine::{Dialect, Engine, Session, value};

use crate::ddl::{DdlOptions, create_table, dependency_order};
use crate::error::{CoreError, Result};

const TARGET_ROWS: usize = 2000;
const SAMPLE_ROWS: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct StatementPreview {
    pub sql: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedRow {
    pub key: Vec<serde_json::Value>,
    pub before: Vec<serde_json::Value>,
    pub after: Vec<serde_json::Value>,
    /// Indexes of columns whose value changed.
    pub changed: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableDiff {
    pub table: String,
    pub columns: Vec<String>,
    pub key_columns: Vec<String>,
    pub added: Vec<Vec<serde_json::Value>>,
    pub removed: Vec<Vec<serde_json::Value>>,
    pub changed: Vec<ChangedRow>,
    /// Only a sample of the table was copied.
    pub sampled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewResult {
    pub statements: Vec<StatementPreview>,
    pub tables: Vec<TableDiff>,
    pub notes: Vec<String>,
    /// Total affected rows reported by the twin (used to guard the real commit).
    pub expected_affected: Option<u64>,
    pub ok: bool,
}

struct Target {
    table: String,
    /// WHERE condition usable on the real server to fetch exactly the affected rows.
    filter: Option<String>,
}

fn parse_targets(sql: &str, dialect: SqlDialect) -> Result<(Vec<Target>, bool)> {
    let parsed = match dialect {
        SqlDialect::Mysql => Parser::parse_sql(&MySqlDialect {}, sql),
        SqlDialect::Mssql => Parser::parse_sql(&MsSqlDialect {}, sql),
        _ => Parser::parse_sql(&PostgreSqlDialect {}, sql),
    }
    .map_err(|e| CoreError::invalid(format!("cannot parse statement for preview: {e}")))?;
    let mut targets = Vec::new();
    let mut ddl = false;
    let last = |n: &sqlparser::ast::ObjectName| {
        n.0.last().map(|p| p.to_string().trim_matches(['"', '`', '[', ']']).to_owned()).unwrap_or_default()
    };
    for stmt in &parsed {
        match stmt {
            Statement::Insert(ins) => {
                if let TableObject::TableName(n) = &ins.table {
                    targets.push(Target { table: last(n), filter: None });
                }
            }
            Statement::Update(u) => {
                if let TableFactor::Table { name, alias, .. } = &u.table.relation {
                    let simple = u.from.is_none() && alias.is_none() && u.table.joins.is_empty();
                    targets.push(Target {
                        table: last(name),
                        filter: if simple { u.selection.as_ref().map(|s| s.to_string()) } else { None },
                    });
                }
            }
            Statement::Delete(d) => {
                let tables = match &d.from {
                    FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
                };
                if let Some(TableFactor::Table { name, alias, .. }) = tables.first().map(|t| &t.relation) {
                    let simple = alias.is_none() && d.using.is_none() && tables.len() == 1;
                    targets.push(Target {
                        table: last(name),
                        filter: if simple { d.selection.as_ref().map(|s| s.to_string()) } else { None },
                    });
                }
            }
            Statement::CreateTable(_) | Statement::AlterTable(_) | Statement::Drop { .. } | Statement::CreateIndex(_) => {
                ddl = true;
                if let Statement::AlterTable(a) = stmt {
                    targets.push(Target { table: last(&a.name), filter: Some("1 = 0".into()) });
                }
            }
            Statement::Query(_) => {}
            other => {
                return Err(CoreError::invalid(format!(
                    "preview supports INSERT, UPDATE, DELETE and DDL statements, not: {}",
                    other.to_string().chars().take(60).collect::<String>()
                )));
            }
        }
    }
    Ok((targets, ddl))
}

/// The engine dialect and the SQL dialect of the twin for a server's dialect.
fn twin_dialects(dialect: SqlDialect) -> (Dialect, SqlDialect) {
    match dialect {
        SqlDialect::Mysql => (Dialect::MySql, SqlDialect::Mysql),
        SqlDialect::Mssql => (Dialect::MsSql, SqlDialect::Mssql),
        _ => (Dialect::Postgres, SqlDialect::Postgres),
    }
}

fn twin_session(dialect: SqlDialect, schema: &str) -> Result<(Engine, Session)> {
    let d = twin_dialects(dialect).0;
    let engine = Engine::temporary(d)?;
    let mut s = engine.session(Some("twin"), "twin")?;
    let setup = match d {
        Dialect::MySql => format!("CREATE DATABASE {q}; USE {q}", q = quote_ident(SqlDialect::Mysql, schema)),
        // Checked by the caller: only dbo.
        Dialect::MsSql => String::new(),
        _ if schema == "public" || schema == "main" => String::new(),
        _ => format!("CREATE SCHEMA {q}; SET search_path TO {q}", q = quote_ident(SqlDialect::Postgres, schema)),
    };
    if !setup.is_empty()
        && let Some(e) = s.execute(&setup).error
    {
        return Err(e.into());
    }
    Ok((engine, s))
}

fn run(s: &mut Session, sql: &str) -> Result<()> {
    match s.execute(sql).error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Column names and rows (as display JSON) keyed by primary key.
type Snapshot = (Vec<String>, BTreeMap<String, Vec<serde_json::Value>>);

/// Snapshot of a twin table.
fn snapshot(s: &mut Session, dialect: SqlDialect, t: &TableSchema) -> Result<Snapshot> {
    let d = twin_dialects(dialect).0;
    let out = s.execute(&format!("SELECT * FROM {}", quote_ident(dialect, &t.name)));
    if let Some(e) = out.error {
        return Err(e.into());
    }
    let rs = out.outcomes.into_iter().last().and_then(|o| o.result).ok_or_else(|| CoreError::invalid("no snapshot"))?;
    let columns: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();
    let key_idx: Vec<usize> = if t.primary_key.is_empty() {
        (0..columns.len()).collect()
    } else {
        t.primary_key.iter().filter_map(|k| columns.iter().position(|c| c == k)).collect()
    };
    let mut rows = BTreeMap::new();
    for (n, r) in rs.rows.iter().enumerate() {
        let cells: Vec<serde_json::Value> = r
            .iter()
            .zip(&rs.columns)
            .map(|(v, c)| value::format_text(v, &c.ty, d).map(serde_json::Value::String).unwrap_or(serde_json::Value::Null))
            .collect();
        let mut key = serde_json::to_string(&key_idx.iter().map(|i| &cells[*i]).collect::<Vec<_>>()).unwrap_or_default();
        if t.primary_key.is_empty() {
            key.push_str(&format!("#{n}"));
        }
        rows.insert(key, cells);
    }
    Ok((columns, rows))
}

/// Preview `sql` against a twin of `schema` built from `client`.
pub async fn preview(client: &dyn SqlClient, schema: &str, sql: &str) -> Result<PreviewResult> {
    let dialect = client.dialect();
    if dialect == SqlDialect::Mssql && !schema.eq_ignore_ascii_case("dbo") {
        return Err(CoreError::invalid(
            "Change preview for SQL Server works on the dbo schema. Use Explain, or run the change inside BEGIN TRANSACTION … ROLLBACK.",
        ));
    }
    if dialect == SqlDialect::Oracle {
        return Err(CoreError::invalid(
            "Change preview is not available for Oracle yet: the local twin cannot run Oracle SQL. Use Explain, or run the change and ROLLBACK.",
        ));
    }
    if matches!(dialect, SqlDialect::Spanner | SqlDialect::Bigquery) {
        return Err(CoreError::invalid(
            "Change preview is not available for Spanner and BigQuery yet: the local twin cannot run GoogleSQL. Use Explain to check a statement first.",
        ));
    }
    if dialect == SqlDialect::Cassandra {
        return Err(CoreError::invalid(
            "Change preview is not available for Cassandra: CQL writes are upserts without transactions to roll back.",
        ));
    }
    if dialect == SqlDialect::Clickhouse {
        return Err(CoreError::invalid(
            "Change preview is not available for ClickHouse: its mutations (ALTER TABLE … UPDATE/DELETE) run asynchronously and have no transactions to roll back.",
        ));
    }
    let (targets, has_ddl) = parse_targets(sql, dialect)?;
    let graph = client.schema_graph(schema).await?;
    let find = |name: &str| graph.iter().find(|t| t.name.eq_ignore_ascii_case(name));
    let mut notes = vec!["Triggers, stored functions and server-specific behaviour are not reproduced in the preview.".to_owned()];
    let target_names: HashSet<String> = targets.iter().filter_map(|t| find(&t.table).map(|x| x.name.clone())).collect();
    // Children of targets show ON DELETE / ON UPDATE effects.
    let mut involved: Vec<TableSchema> = graph.iter().filter(|t| target_names.contains(&t.name)).cloned().collect();
    for t in &graph {
        if !target_names.contains(&t.name) && t.foreign_keys.iter().any(|f| target_names.contains(&f.ref_table)) {
            involved.push(t.clone());
        }
    }
    if involved.iter().any(|t| target_names.contains(&t.name) && !t.foreign_keys.iter().all(|f| target_names.contains(&f.ref_table))) {
        notes.push("Foreign keys from the changed tables to other tables are checked when you commit, not in the preview.".into());
    }
    let involved_names: HashSet<String> = involved.iter().map(|t| t.name.clone()).collect();
    let (_engine, mut twin) = twin_session(dialect, schema)?;
    let twin_dialect = twin_dialects(dialect).1;
    // Row-limited reads on the real server.
    let limited = |rest: String, n: usize| {
        if dialect == SqlDialect::Mssql { format!("SELECT TOP {n} * FROM {rest}") } else { format!("SELECT * FROM {rest} LIMIT {n}") }
    };
    // Create tables: FKs are kept only from children to changed tables.
    for t in dependency_order(&involved) {
        let fk_targets: HashSet<String> = if target_names.contains(&t.name) { HashSet::new() } else { target_names.clone() };
        let opts = DdlOptions { target: twin_dialect, source: dialect, fk_targets: Some(fk_targets), include_indexes: false };
        run(&mut twin, &create_table(t, &opts))?;
    }
    // Copy rows.
    let mut sampled_tables = HashSet::new();
    let mut copied_keys: BTreeMap<String, Vec<Vec<serde_json::Value>>> = BTreeMap::new();
    for t in dependency_order(&involved) {
        let qualified = twinbase_client::sqlgen::qualified(dialect, schema, &t.name);
        let select = if let Some(target) = targets.iter().find(|x| x.table.eq_ignore_ascii_case(&t.name)) {
            match &target.filter {
                Some(f) => limited(format!("{qualified} WHERE {f}"), TARGET_ROWS),
                None => {
                    sampled_tables.insert(t.name.clone());
                    limited(qualified.clone(), SAMPLE_ROWS)
                }
            }
        } else {
            // A child: rows referencing copied parent rows.
            let mut conds = Vec::new();
            for fk in t.foreign_keys.iter().filter(|f| target_names.contains(&f.ref_table) && f.columns.len() == 1) {
                if let Some(keys) = copied_keys.get(&format!("{}.{}", fk.ref_table, fk.ref_columns[0])) {
                    let vals: Vec<String> =
                        keys.iter().filter_map(|k| k.first()).filter(|v| !v.is_null()).map(|v| literal(dialect, v)).collect();
                    if !vals.is_empty() {
                        conds.push(format!("{} IN ({})", quote_ident(dialect, &fk.columns[0]), vals.join(", ")));
                    }
                }
            }
            if conds.is_empty() {
                continue;
            }
            limited(format!("{qualified} WHERE {}", conds.join(" OR ")), TARGET_ROWS)
        };
        let mut res = client.query(&select, TARGET_ROWS).await?;
        let Some(rs) = res.pop() else { continue };
        if rs.truncated || (target_names.contains(&t.name) && rs.rows.len() >= TARGET_ROWS) {
            sampled_tables.insert(t.name.clone());
        }
        for (i, col) in rs.columns.iter().enumerate() {
            copied_keys.entry(format!("{}.{}", t.name, col.name)).or_default().extend(rs.rows.iter().map(|r| vec![r[i].clone()]));
        }
        for chunk in rs.rows.chunks(200) {
            let cols = rs.columns.iter().map(|c| quote_ident(twin_dialect, &c.name)).collect::<Vec<_>>().join(", ");
            let values = chunk
                .iter()
                .map(|r| format!("({})", r.iter().map(|v| literal(twin_dialect, v)).collect::<Vec<_>>().join(", ")))
                .collect::<Vec<_>>()
                .join(", ");
            run(&mut twin, &format!("INSERT INTO {} ({cols}) VALUES {values}", quote_ident(twin_dialect, &t.name)))?;
        }
    }
    if !sampled_tables.is_empty() {
        notes.push(format!(
            "Only a sample of rows was copied for: {}. Counts in the preview may differ from the real server.",
            sampled_tables.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if has_ddl {
        notes.push("Schema changes are applied to an empty copy of the affected tables; the preview shows whether they succeed.".into());
    }
    // Before snapshots.
    let mut before = BTreeMap::new();
    for t in &involved {
        before.insert(t.name.clone(), snapshot(&mut twin, twin_dialect, t)?);
    }
    // Run statements.
    let mut statements = Vec::new();
    let mut total: u64 = 0;
    let mut ok = true;
    for stmt in twinbase_client::split_statements(sql, dialect) {
        if !ok {
            statements.push(StatementPreview { sql: stmt, affected: None, error: Some("not run (an earlier statement failed)".into()) });
            continue;
        }
        let r = twin.execute(&stmt);
        match r.error {
            Some(e) => {
                ok = false;
                statements.push(StatementPreview { sql: stmt, affected: None, error: Some(e.message) });
            }
            None => {
                let affected: u64 =
                    r.outcomes.iter().filter(|o| matches!(o.tag.as_str(), "INSERT" | "UPDATE" | "DELETE")).map(|o| o.affected).sum();
                total += affected;
                statements.push(StatementPreview { sql: stmt, affected: Some(affected), error: None });
            }
        }
    }
    // Diff.
    let mut tables = Vec::new();
    for t in &involved {
        let (columns, old) = before.remove(&t.name).unwrap_or_default();
        let (_, new) = match snapshot(&mut twin, twin_dialect, t) {
            Ok(v) => v,
            Err(_) => (columns.clone(), BTreeMap::new()),
        };
        let key_idx: Vec<usize> = t.primary_key.iter().filter_map(|k| columns.iter().position(|c| c == k)).collect();
        let mut diff = TableDiff {
            table: t.name.clone(),
            columns: columns.clone(),
            key_columns: t.primary_key.clone(),
            added: vec![],
            removed: vec![],
            changed: vec![],
            sampled: sampled_tables.contains(&t.name),
        };
        for (k, row) in &old {
            match new.get(k) {
                None => diff.removed.push(row.clone()),
                Some(after) if after != row => diff.changed.push(ChangedRow {
                    key: key_idx.iter().map(|i| row[*i].clone()).collect(),
                    before: row.clone(),
                    after: after.clone(),
                    changed: (0..row.len()).filter(|i| row.get(*i) != after.get(*i)).collect(),
                }),
                _ => {}
            }
        }
        for (k, row) in &new {
            if !old.contains_key(k) {
                diff.added.push(row.clone());
            }
        }
        if !diff.added.is_empty() || !diff.removed.is_empty() || !diff.changed.is_empty() {
            tables.push(diff);
        }
    }
    let _ = involved_names;
    let dml = !has_ddl && statements.iter().all(|s| s.error.is_none());
    Ok(PreviewResult { statements, tables, notes, expected_affected: (dml && ok && sampled_tables.is_empty()).then_some(total), ok })
}
