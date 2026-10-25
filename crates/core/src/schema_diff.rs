//! Schema snapshots, structural diffs and migration scripts.
//!
//! A diff compares two lists of introspected tables (from snapshots or live connections) and
//! produces a change list plus `up` (from → to) and `down` (to → from) SQL for a target dialect.
//! Renames cannot be told apart from drop + add, so likely renames are reported as warnings.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use twinbase_client::sqlgen::quote_ident;
use twinbase_client::{ColumnInfo, ForeignKeyInfo, IndexInfo, SqlDialect, TableSchema};
use twinbase_engine::{Dialect, SqlType};

use crate::ddl::{DdlOptions, convert_type, create_table, dependency_order};

/// A saved copy of a schema's structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub label: String,
    /// RFC 3339.
    pub taken_at: String,
    /// Connection id the snapshot was taken from.
    pub connection: String,
    /// Connection name at the time (for display after the connection is gone).
    pub source: String,
    pub dialect: SqlDialect,
    pub schema: String,
    pub tables: Vec<TableSchema>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub label: String,
    pub taken_at: String,
    pub connection: String,
    pub source: String,
    pub dialect: SqlDialect,
    pub schema: String,
    pub tables: usize,
}

impl Snapshot {
    pub fn info(&self) -> SnapshotInfo {
        SnapshotInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            taken_at: self.taken_at.clone(),
            connection: self.connection.clone(),
            source: self.source.clone(),
            dialect: self.dialect,
            schema: self.schema.clone(),
            tables: self.tables.iter().filter(|t| t.kind == "table").count(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    TableAdded { table: String },
    TableRemoved { table: String },
    ColumnAdded { table: String, column: String, data_type: String },
    ColumnRemoved { table: String, column: String },
    ColumnChanged { table: String, column: String, details: Vec<String> },
    PrimaryKeyChanged { table: String, from: Vec<String>, to: Vec<String> },
    ForeignKeyAdded { table: String, columns: Vec<String>, ref_table: String },
    ForeignKeyRemoved { table: String, columns: Vec<String>, ref_table: String },
    IndexAdded { table: String, name: String, columns: Vec<String>, unique: bool },
    IndexRemoved { table: String, name: String, columns: Vec<String>, unique: bool },
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDiff {
    pub dialect: SqlDialect,
    pub changes: Vec<Change>,
    pub up: Vec<String>,
    pub down: Vec<String>,
    pub warnings: Vec<String>,
    pub identical: bool,
}

/// One side of a comparison.
pub struct Side<'a> {
    pub tables: &'a [TableSchema],
    pub dialect: SqlDialect,
}

fn engine_dialect(d: SqlDialect) -> Dialect {
    match d {
        SqlDialect::Mysql => Dialect::MySql,
        _ => Dialect::Postgres,
    }
}

/// Type in canonical form for comparison (aliases such as `int4`/`integer` compare equal).
fn canonical_type(t: &str, from: SqlDialect, target: SqlDialect) -> String {
    let converted = convert_type(t, from, target);
    let ty = match target {
        SqlDialect::Clickhouse => SqlType::parse_clickhouse(&converted),
        SqlDialect::Cassandra => SqlType::parse_cql(&converted),
        SqlDialect::Spanner | SqlDialect::Bigquery => SqlType::parse_googlesql(&converted),
        _ => SqlType::parse_simple(&converted, engine_dialect(target)),
    };
    let s = match target {
        SqlDialect::Mysql => ty.mysql_column_type(),
        SqlDialect::Mssql => ty.mssql_column_type(),
        SqlDialect::Clickhouse => ty.clickhouse_column_type(),
        SqlDialect::Cassandra => ty.cql_column_type(),
        SqlDialect::Spanner => ty.googlesql_column_type(true),
        SqlDialect::Bigquery => ty.googlesql_column_type(false),
        SqlDialect::Oracle => ty.oracle_column_type(),
        SqlDialect::Sqlite => converted,
        SqlDialect::Postgres => ty.pg_format_type(),
    };
    s.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Default value as a comparable literal, ignoring generated defaults of auto columns.
fn canonical_default(c: &ColumnInfo) -> Option<String> {
    if c.auto {
        return None;
    }
    let d = c.default.as_deref()?.trim();
    if d.to_ascii_lowercase().starts_with("nextval(") {
        return None;
    }
    let base = match d.find("::") {
        Some(p) if d.starts_with('\'') || d.starts_with('(') => &d[..p],
        _ => d,
    };
    let base = if base.ends_with("()") { base.trim_start_matches('(') } else { base.trim_matches(|c| c == '(' || c == ')') };
    let base = if (base.starts_with("N'") || base.starts_with("n'")) && base.len() > 2 { &base[1..] } else { base };
    let lower = base.to_ascii_lowercase();
    Some(match lower.as_str() {
        "now()" | "current_timestamp()" | "current_timestamp" | "getdate()" | "sysdatetime()" | "sysutcdatetime()" | "getutcdate()" => {
            "current_timestamp".into()
        }
        "null" => return None,
        _ => base.to_owned(),
    })
}

fn sql_default(c: &ColumnInfo) -> Option<String> {
    canonical_default(c).map(|d| if d == "current_timestamp" { "CURRENT_TIMESTAMP".into() } else { d })
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct FkKey {
    columns: Vec<String>,
    ref_table: String,
    ref_columns: Vec<String>,
    on_delete: String,
}

fn fk_key(f: &ForeignKeyInfo) -> FkKey {
    let od = f.on_delete.clone().unwrap_or_default().to_ascii_uppercase();
    FkKey {
        columns: f.columns.clone(),
        ref_table: f.ref_table.clone(),
        ref_columns: f.ref_columns.clone(),
        on_delete: if od == "NO ACTION" { String::new() } else { od },
    }
}

fn ix_key(i: &IndexInfo) -> (Vec<String>, bool) {
    (i.columns.clone(), i.unique)
}

/// Per-table differences used to generate SQL.
#[derive(Default)]
struct TableDelta<'a> {
    added_cols: Vec<&'a ColumnInfo>,
    removed_cols: Vec<&'a ColumnInfo>,
    changed_cols: Vec<(&'a ColumnInfo, &'a ColumnInfo, Vec<String>)>,
    pk: Option<(Vec<String>, Vec<String>)>,
    fks_added: Vec<&'a ForeignKeyInfo>,
    fks_removed: Vec<&'a ForeignKeyInfo>,
    ix_added: Vec<&'a IndexInfo>,
    ix_removed: Vec<&'a IndexInfo>,
}

impl TableDelta<'_> {
    fn is_empty(&self) -> bool {
        self.added_cols.is_empty()
            && self.removed_cols.is_empty()
            && self.changed_cols.is_empty()
            && self.pk.is_none()
            && self.fks_added.is_empty()
            && self.fks_removed.is_empty()
            && self.ix_added.is_empty()
            && self.ix_removed.is_empty()
    }

    /// SQLite can only add nullable/defaulted columns and add/drop plain indexes in place.
    fn sqlite_needs_rebuild(&self) -> bool {
        !self.removed_cols.is_empty()
            || !self.changed_cols.is_empty()
            || self.pk.is_some()
            || !self.fks_added.is_empty()
            || !self.fks_removed.is_empty()
            || self.ix_removed.iter().any(|i| i.unique)
            || self.added_cols.iter().any(|c| (!c.nullable && sql_default(c).is_none()) || c.primary_key || c.unique)
    }
}

fn tables_only(t: &[TableSchema]) -> BTreeMap<&str, &TableSchema> {
    t.iter().filter(|t| t.kind == "table").map(|t| (t.name.as_str(), t)).collect()
}

/// Compare `from` with `to` and generate SQL in `target`.
pub fn diff(from: Side<'_>, to: Side<'_>, target: SqlDialect) -> SchemaDiff {
    let (changes, up, mut warnings) = one_way(&from, &to, target);
    let (_, down, _) = one_way(&to, &from, target);
    if from.dialect != to.dialect {
        warnings.push(format!(
            "Comparing a {:?} schema with a {:?} schema: types were mapped to {:?} before comparing.",
            from.dialect, to.dialect, target
        ));
    }
    warnings.dedup();
    SchemaDiff { dialect: target, identical: changes.is_empty(), changes, up, down, warnings }
}

fn one_way(from: &Side<'_>, to: &Side<'_>, d: SqlDialect) -> (Vec<Change>, Vec<String>, Vec<String>) {
    let a = tables_only(from.tables);
    let b = tables_only(to.tables);
    let q = |s: &str| quote_ident(d, s);
    let mut changes = Vec::new();
    let mut warnings = Vec::new();

    let added: Vec<&TableSchema> = b.iter().filter(|(n, _)| !a.contains_key(*n)).map(|(_, t)| *t).collect();
    let removed: Vec<&TableSchema> = a.iter().filter(|(n, _)| !b.contains_key(*n)).map(|(_, t)| *t).collect();
    for t in &added {
        changes.push(Change::TableAdded { table: t.name.clone() });
    }
    for t in &removed {
        changes.push(Change::TableRemoved { table: t.name.clone() });
    }
    if removed.len() == 1 && added.len() == 1 {
        let (r, n) = (removed[0], added[0]);
        let same_cols =
            r.columns.iter().map(|c| &c.name).collect::<BTreeSet<_>>() == n.columns.iter().map(|c| &c.name).collect::<BTreeSet<_>>();
        if same_cols {
            warnings.push(format!(
                "Table \"{}\" looks renamed to \"{}\". The script drops and recreates it (data is lost); use ALTER TABLE … RENAME TO instead to keep the rows.",
                r.name, n.name
            ));
        }
    }

    // Per-table deltas for tables on both sides.
    let mut deltas: BTreeMap<&str, TableDelta> = BTreeMap::new();
    for (name, ta) in &a {
        let Some(tb) = b.get(name) else { continue };
        let mut delta = TableDelta::default();
        let cols_a: HashMap<&str, &ColumnInfo> = ta.columns.iter().map(|c| (c.name.as_str(), c)).collect();
        let cols_b: HashMap<&str, &ColumnInfo> = tb.columns.iter().map(|c| (c.name.as_str(), c)).collect();
        for c in &tb.columns {
            match cols_a.get(c.name.as_str()) {
                None => {
                    delta.added_cols.push(c);
                    changes.push(Change::ColumnAdded {
                        table: name.to_string(),
                        column: c.name.clone(),
                        data_type: convert_type(&c.data_type, to.dialect, d),
                    });
                }
                Some(old) => {
                    let mut details = Vec::new();
                    let (ta_, tb_) = (canonical_type(&old.data_type, from.dialect, d), canonical_type(&c.data_type, to.dialect, d));
                    if ta_ != tb_ && !(old.auto && c.auto) {
                        details.push(format!("type {ta_} → {tb_}"));
                    }
                    if old.nullable != c.nullable && !c.primary_key {
                        details.push(if c.nullable { "now nullable".into() } else { "now NOT NULL".into() });
                    }
                    let (da, db) = (canonical_default(old), canonical_default(c));
                    if da != db {
                        details.push(format!("default {} → {}", da.as_deref().unwrap_or("none"), db.as_deref().unwrap_or("none")));
                    }
                    if !details.is_empty() {
                        changes.push(Change::ColumnChanged { table: name.to_string(), column: c.name.clone(), details: details.clone() });
                        delta.changed_cols.push((old, c, details));
                    }
                }
            }
        }
        for c in &ta.columns {
            if !cols_b.contains_key(c.name.as_str()) {
                delta.removed_cols.push(c);
                changes.push(Change::ColumnRemoved { table: name.to_string(), column: c.name.clone() });
            }
        }
        if delta.removed_cols.len() == 1 && delta.added_cols.len() == 1 {
            let (r, n) = (delta.removed_cols[0], delta.added_cols[0]);
            if canonical_type(&r.data_type, from.dialect, d) == canonical_type(&n.data_type, to.dialect, d) {
                warnings.push(format!(
                    "Column {name}.{} looks renamed to {}. The script drops and adds it (data is lost); use ALTER TABLE … RENAME COLUMN instead.",
                    r.name, n.name
                ));
            }
        }
        for r in &delta.removed_cols {
            warnings.push(format!("Dropping column {name}.{} deletes its data.", r.name));
        }
        for c in &delta.added_cols {
            if !c.nullable && sql_default(c).is_none() && !c.auto {
                warnings
                    .push(format!("New NOT NULL column {name}.{} has no default; the script fails if the table already has rows.", c.name));
            }
        }
        if ta.primary_key != tb.primary_key {
            changes.push(Change::PrimaryKeyChanged { table: name.to_string(), from: ta.primary_key.clone(), to: tb.primary_key.clone() });
            delta.pk = Some((ta.primary_key.clone(), tb.primary_key.clone()));
        }
        let fk_a: BTreeMap<FkKey, &ForeignKeyInfo> = ta.foreign_keys.iter().map(|f| (fk_key(f), f)).collect();
        let fk_b: BTreeMap<FkKey, &ForeignKeyInfo> = tb.foreign_keys.iter().map(|f| (fk_key(f), f)).collect();
        for (k, f) in &fk_b {
            if !fk_a.contains_key(k) {
                delta.fks_added.push(f);
                changes.push(Change::ForeignKeyAdded {
                    table: name.to_string(),
                    columns: f.columns.clone(),
                    ref_table: f.ref_table.clone(),
                });
            }
        }
        for (k, f) in &fk_a {
            if !fk_b.contains_key(k) {
                delta.fks_removed.push(f);
                changes.push(Change::ForeignKeyRemoved {
                    table: name.to_string(),
                    columns: f.columns.clone(),
                    ref_table: f.ref_table.clone(),
                });
            }
        }
        let ix_a: BTreeMap<(Vec<String>, bool), &IndexInfo> = ta.indexes.iter().filter(|i| !i.primary).map(|i| (ix_key(i), i)).collect();
        let ix_b: BTreeMap<(Vec<String>, bool), &IndexInfo> = tb.indexes.iter().filter(|i| !i.primary).map(|i| (ix_key(i), i)).collect();
        for (k, i) in &ix_b {
            if !ix_a.contains_key(k) {
                delta.ix_added.push(i);
                changes.push(Change::IndexAdded {
                    table: name.to_string(),
                    name: i.name.clone(),
                    columns: i.columns.clone(),
                    unique: i.unique,
                });
            }
        }
        for (k, i) in &ix_a {
            if !ix_b.contains_key(k) {
                delta.ix_removed.push(i);
                changes.push(Change::IndexRemoved {
                    table: name.to_string(),
                    name: i.name.clone(),
                    columns: i.columns.clone(),
                    unique: i.unique,
                });
            }
        }
        if !delta.is_empty() {
            deltas.insert(name, delta);
        }
    }

    // SQL, in an order that keeps references valid.
    let mut up = Vec::new();
    let opts = DdlOptions { target: d, source: to.dialect, fk_targets: None, include_indexes: true };
    let mut rebuilt: BTreeSet<&str> = BTreeSet::new();
    if d == SqlDialect::Sqlite {
        for (name, delta) in &deltas {
            if delta.sqlite_needs_rebuild() {
                rebuilt.insert(name);
            }
        }
    }
    // ClickHouse and Cassandra have no foreign keys, and their keys cannot change in place; their
    // indexes are not scripted here.
    if matches!(d, SqlDialect::Clickhouse | SqlDialect::Cassandra | SqlDialect::Bigquery) {
        for (name, delta) in &mut deltas {
            if delta.pk.take().is_some() {
                warnings.push(if d == SqlDialect::Bigquery {
                    format!("Primary-key changes on \"{name}\" are not scripted for BigQuery (its keys are not enforced).")
                } else if d == SqlDialect::Cassandra {
                    format!("Cassandra cannot change the primary key of \"{name}\"; create a new table and copy the data.")
                } else {
                    format!("ClickHouse cannot change the sorting key (ORDER BY) of \"{name}\" in place; create a new table and INSERT … SELECT.")
                });
            }
            if !delta.ix_added.is_empty() || !delta.ix_removed.is_empty() || !delta.fks_added.is_empty() || !delta.fks_removed.is_empty() {
                warnings.push(format!(
                    "Index and foreign-key differences on \"{name}\" are not scripted for {}.",
                    match d {
                        SqlDialect::Cassandra => "Cassandra",
                        SqlDialect::Bigquery => "BigQuery",
                        _ => "ClickHouse",
                    }
                ));
            }
            delta.ix_added.clear();
            delta.ix_removed.clear();
            delta.fks_added.clear();
            delta.fks_removed.clear();
        }
    }
    // Spanner keys cannot change in place, and interleaving is part of the table definition.
    if d == SqlDialect::Spanner {
        for (name, delta) in &mut deltas {
            if delta.pk.take().is_some() {
                warnings.push(format!("Spanner cannot change the primary key of \"{name}\"; create a new table and copy the data."));
            }
            if delta.fks_added.iter().chain(&delta.fks_removed).any(|f| f.name.starts_with("INTERLEAVE")) {
                warnings.push(format!("Interleaving of \"{name}\" differs; Spanner cannot change it in place."));
            }
            delta.fks_added.retain(|f| !f.name.starts_with("INTERLEAVE"));
            delta.fks_removed.retain(|f| !f.name.starts_with("INTERLEAVE"));
        }
    }
    // 1. Drop foreign keys and indexes that go away (or change).
    for (name, delta) in &deltas {
        if rebuilt.contains(name) {
            continue;
        }
        for f in &delta.fks_removed {
            up.push(match d {
                SqlDialect::Mysql => format!("ALTER TABLE {} DROP FOREIGN KEY {};", q(name), q(&f.name)),
                _ => format!("ALTER TABLE {} DROP CONSTRAINT {};", q(name), q(&f.name)),
            });
        }
        for i in &delta.ix_removed {
            up.extend(drop_index(d, name, i));
        }
    }
    // 2. New tables, parents first.
    let added_owned: Vec<TableSchema> = added.iter().map(|t| (*t).clone()).collect();
    for t in dependency_order(&added_owned) {
        up.push(create_table(t, &opts));
    }
    // 3. Column and key changes on existing tables.
    for (name, delta) in &deltas {
        if rebuilt.contains(name) {
            up.extend(sqlite_rebuild(b[name], a[name], to.dialect));
            warnings.push(format!(
                "SQLite cannot alter table \"{name}\" in place; the script rebuilds it (copy, drop, rename) inside a transaction."
            ));
            continue;
        }
        for c in &delta.added_cols {
            let add = if matches!(d, SqlDialect::Mssql | SqlDialect::Cassandra | SqlDialect::Oracle) { "ADD" } else { "ADD COLUMN" };
            up.push(format!("ALTER TABLE {} {add} {};", q(name), column_sql(d, c, to.dialect)));
        }
        for (old, new, _) in &delta.changed_cols {
            if d == SqlDialect::Cassandra {
                warnings.push(format!(
                    "Cassandra cannot change column \"{}\" of \"{name}\" from {} to {} in place.",
                    new.name, old.data_type, new.data_type
                ));
                continue;
            }
            up.extend(alter_column(d, name, old, new, from.dialect, to.dialect));
        }
        if let Some((old_pk, new_pk)) = &delta.pk {
            let cols = new_pk.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            match d {
                SqlDialect::Mysql => {
                    let mut parts = Vec::new();
                    if !old_pk.is_empty() {
                        parts.push("DROP PRIMARY KEY".to_owned());
                    }
                    if !new_pk.is_empty() {
                        parts.push(format!("ADD PRIMARY KEY ({cols})"));
                    }
                    up.push(format!("ALTER TABLE {} {};", q(name), parts.join(", ")));
                }
                SqlDialect::Oracle => {
                    if !old_pk.is_empty() {
                        up.push(format!("ALTER TABLE {} DROP PRIMARY KEY;", q(name)));
                    }
                    if !new_pk.is_empty() {
                        up.push(format!("ALTER TABLE {} ADD PRIMARY KEY ({cols});", q(name)));
                    }
                }
                SqlDialect::Mssql => {
                    if !old_pk.is_empty() {
                        let lookup = format!(
                            "SELECT name FROM sys.key_constraints WHERE type = 'PK' AND parent_object_id = OBJECT_ID(N'{}')",
                            q(name).replace('\'', "''")
                        );
                        up.push(mssql_drop_named(&q(name), &lookup));
                    }
                    if !new_pk.is_empty() {
                        up.push(format!("ALTER TABLE {} ADD PRIMARY KEY ({cols});", q(name)));
                    }
                }
                _ => {
                    if !old_pk.is_empty() {
                        up.push(format!("ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};", q(name), q(&format!("{name}_pkey"))));
                    }
                    if !new_pk.is_empty() {
                        up.push(format!("ALTER TABLE {} ADD PRIMARY KEY ({cols});", q(name)));
                    }
                }
            }
        }
        for c in &delta.removed_cols {
            if d == SqlDialect::Mssql && c.default.is_some() {
                // A column with a default constraint cannot be dropped before the constraint.
                up.push(mssql_drop_named(&q(name), &mssql_default_lookup(&q(name), &c.name)));
            }
            up.push(if d == SqlDialect::Cassandra {
                format!("ALTER TABLE {} DROP {};", q(name), q(&c.name))
            } else {
                format!("ALTER TABLE {} DROP COLUMN {};", q(name), q(&c.name))
            });
        }
    }
    // 4. New indexes and foreign keys.
    for (name, delta) in &deltas {
        if rebuilt.contains(name) {
            continue;
        }
        for i in &delta.ix_added {
            let cols = i.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            up.push(format!("CREATE {}INDEX {} ON {} ({cols});", if i.unique { "UNIQUE " } else { "" }, q(&i.name), q(name)));
        }
        for f in &delta.fks_added {
            up.push(add_fk(d, name, f));
        }
    }
    // 5. Dropped tables, children first.
    let removed_owned: Vec<TableSchema> = removed.iter().map(|t| (*t).clone()).collect();
    for t in dependency_order(&removed_owned).into_iter().rev() {
        up.push(format!("DROP TABLE {};", q(&t.name)));
        warnings.push(format!("Dropping table \"{}\" deletes its data.", t.name));
    }
    if d == SqlDialect::Sqlite && !rebuilt.is_empty() {
        up.insert(0, "PRAGMA foreign_keys = OFF;".into());
        up.insert(1, "BEGIN;".into());
        up.push("COMMIT;".into());
        up.push("PRAGMA foreign_keys = ON;".into());
    }
    (changes, up, warnings)
}

fn column_sql(d: SqlDialect, c: &ColumnInfo, source: SqlDialect) -> String {
    if matches!(d, SqlDialect::Spanner | SqlDialect::Bigquery) {
        let mut s = format!("{} {}", quote_ident(d, &c.name), convert_type(&c.data_type, source, d));
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        if let Some(def) = sql_default(c) {
            let def = crate::ddl::default_in(d, &def);
            s.push_str(&if d == SqlDialect::Spanner { format!(" DEFAULT ({def})") } else { format!(" DEFAULT {def}") });
        }
        return s;
    }
    if d == SqlDialect::Clickhouse {
        return crate::ddl::clickhouse_column(c, source);
    }
    if d == SqlDialect::Cassandra {
        return format!("{} {}", quote_ident(d, &c.name), convert_type(&c.data_type, source, d));
    }
    let q = |s: &str| quote_ident(d, s);
    let mut ty = convert_type(&c.data_type, source, d);
    let mut extra = String::new();
    if c.auto {
        match d {
            SqlDialect::Postgres => ty = if ty.to_ascii_lowercase().contains("bigint") { "bigserial".into() } else { "serial".into() },
            SqlDialect::Mysql => extra.push_str(" AUTO_INCREMENT"),
            SqlDialect::Mssql => extra.push_str(" IDENTITY(1,1)"),
            SqlDialect::Oracle => extra.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
            SqlDialect::Sqlite | SqlDialect::Clickhouse | SqlDialect::Cassandra | SqlDialect::Spanner | SqlDialect::Bigquery => {}
        }
    }
    let mut s = format!("{} {ty}", q(&c.name));
    let default = sql_default(c).map(|def| format!(" DEFAULT {}", crate::ddl::default_in(d, &def)));
    // Oracle: the identity clause or DEFAULT directly after the type, then constraints.
    if d == SqlDialect::Oracle {
        s.push_str(&std::mem::take(&mut extra));
        if let Some(def) = &default {
            s.push_str(def);
        }
    }
    if !c.nullable && !(c.auto && d == SqlDialect::Postgres) {
        s.push_str(" NOT NULL");
    }
    if d != SqlDialect::Oracle
        && let Some(def) = &default
    {
        s.push_str(def);
    }
    s.push_str(&extra);
    s
}

fn alter_column(d: SqlDialect, table: &str, old: &ColumnInfo, new: &ColumnInfo, from: SqlDialect, to: SqlDialect) -> Vec<String> {
    let q = |s: &str| quote_ident(d, s);
    let t = q(table);
    let c = q(&new.name);
    match d {
        SqlDialect::Mysql => vec![format!("ALTER TABLE {t} MODIFY COLUMN {};", column_sql(d, new, to))],
        // CQL cannot change a column's type in place (the caller reports it).
        SqlDialect::Cassandra => vec![],
        SqlDialect::Oracle => {
            let mut out = Vec::new();
            let (ta, tb) = (canonical_type(&old.data_type, from, d), canonical_type(&new.data_type, to, d));
            if ta != tb && !(old.auto && new.auto) {
                out.push(format!("ALTER TABLE {t} MODIFY ({c} {});", convert_type(&new.data_type, to, d)));
            }
            if canonical_default(old) != canonical_default(new) {
                out.push(format!(
                    "ALTER TABLE {t} MODIFY ({c} DEFAULT {});",
                    sql_default(new).map(|d2| crate::ddl::default_in(d, &d2)).unwrap_or_else(|| "NULL".into())
                ));
            }
            if old.nullable != new.nullable && !new.primary_key {
                out.push(format!("ALTER TABLE {t} MODIFY ({c} {});", if new.nullable { "NULL" } else { "NOT NULL" }));
            }
            out
        }
        // Spanner: type and nullability in one ALTER COLUMN; defaults separately.
        SqlDialect::Spanner => {
            let mut out = Vec::new();
            let (ta, tb) = (canonical_type(&old.data_type, from, d), canonical_type(&new.data_type, to, d));
            if ta != tb || old.nullable != new.nullable {
                out.push(format!(
                    "ALTER TABLE {t} ALTER COLUMN {c} {}{};",
                    convert_type(&new.data_type, to, d),
                    if new.nullable { "" } else { " NOT NULL" }
                ));
            }
            if canonical_default(old) != canonical_default(new) {
                out.push(match sql_default(new) {
                    Some(def) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT ({});", crate::ddl::default_in(d, &def)),
                    None => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT;"),
                });
            }
            out
        }
        SqlDialect::Bigquery => {
            let mut out = Vec::new();
            let (ta, tb) = (canonical_type(&old.data_type, from, d), canonical_type(&new.data_type, to, d));
            if ta != tb {
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {c} SET DATA TYPE {};", convert_type(&new.data_type, to, d)));
            }
            if old.nullable != new.nullable && new.nullable {
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP NOT NULL;"));
            }
            if canonical_default(old) != canonical_default(new) {
                out.push(match sql_default(new) {
                    Some(def) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT {};", crate::ddl::default_in(d, &def)),
                    None => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT;"),
                });
            }
            out
        }
        SqlDialect::Clickhouse => {
            let mut out = vec![format!("ALTER TABLE {t} MODIFY COLUMN {};", column_sql(d, new, to))];
            if old.default.is_some() && new.default.is_none() {
                out.push(format!("ALTER TABLE {t} MODIFY COLUMN {c} REMOVE DEFAULT;"));
            }
            out
        }
        SqlDialect::Mssql => {
            let mut out = Vec::new();
            let (ta, tb) = (canonical_type(&old.data_type, from, d), canonical_type(&new.data_type, to, d));
            if (ta != tb && !(old.auto && new.auto)) || old.nullable != new.nullable {
                let ty = convert_type(&new.data_type, to, d);
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {c} {ty} {};", if new.nullable { "NULL" } else { "NOT NULL" }));
            }
            if canonical_default(old) != canonical_default(new) {
                // Defaults are named constraints: drop the current one, whatever its name.
                out.push(mssql_drop_named(&t, &mssql_default_lookup(&t, &new.name)));
                if let Some(def) = sql_default(new) {
                    out.push(format!("ALTER TABLE {t} ADD DEFAULT {} FOR {c};", crate::ddl::default_in(d, &def)));
                }
            }
            out
        }
        _ => {
            let mut out = Vec::new();
            let (ta, tb) = (canonical_type(&old.data_type, from, d), canonical_type(&new.data_type, to, d));
            if ta != tb && !(old.auto && new.auto) {
                let ty = convert_type(&new.data_type, to, d);
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty} USING {c}::{ty};"));
            }
            if old.nullable != new.nullable && !new.primary_key {
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {c} {} NOT NULL;", if new.nullable { "DROP" } else { "SET" }));
            }
            if canonical_default(old) != canonical_default(new) {
                out.push(match sql_default(new) {
                    Some(def) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT {def};"),
                    None => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT;"),
                });
            }
            out
        }
    }
}

/// Query for the name of a column's default constraint.
fn mssql_default_lookup(table: &str, column: &str) -> String {
    format!(
        "SELECT d.name FROM sys.default_constraints d JOIN sys.columns c ON c.default_object_id = d.object_id WHERE d.parent_object_id = OBJECT_ID(N'{}') AND c.name = N'{}'",
        table.replace('\'', "''"),
        column.replace('\'', "''")
    )
}

/// Drop the constraint whose name the query `lookup` finds (SQL Server names primary keys and
/// defaults itself unless told otherwise). Runs in its own scope so several can share a batch.
fn mssql_drop_named(table: &str, lookup: &str) -> String {
    let inner = format!("DECLARE @n sysname = ({lookup}); IF @n IS NOT NULL EXEC(N'ALTER TABLE {table} DROP CONSTRAINT [' + @n + N']');");
    format!("EXEC(N'{}');", inner.replace('\'', "''"))
}

fn drop_index(d: SqlDialect, table: &str, i: &IndexInfo) -> Vec<String> {
    let q = |s: &str| quote_ident(d, s);
    match d {
        SqlDialect::Mssql => vec![format!(
            "IF EXISTS (SELECT 1 FROM sys.key_constraints WHERE name = N'{n}' AND parent_object_id = OBJECT_ID(N'{tl}')) ALTER TABLE {t} DROP CONSTRAINT {qi} ELSE DROP INDEX IF EXISTS {qi} ON {t};",
            n = i.name.replace('\'', "''"),
            tl = q(table).replace('\'', "''"),
            t = q(table),
            qi = q(&i.name)
        )],
        SqlDialect::Mysql => vec![format!("DROP INDEX {} ON {};", q(&i.name), q(table))],
        SqlDialect::Oracle => vec![format!("DROP INDEX {};", q(&i.name))],
        SqlDialect::Postgres if i.unique => vec![
            // Unique constraints own their index; plain unique indexes do not. Cover both.
            format!("ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};", q(table), q(&i.name)),
            format!("DROP INDEX IF EXISTS {};", q(&i.name)),
        ],
        _ => vec![format!("DROP INDEX IF EXISTS {};", q(&i.name))],
    }
}

fn add_fk(d: SqlDialect, table: &str, f: &ForeignKeyInfo) -> String {
    let q = |s: &str| quote_ident(d, s);
    let mut s = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        q(table),
        q(&f.name),
        f.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "),
        q(&f.ref_table),
        f.ref_columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
    );
    if let Some(a) = f.on_delete.as_ref().filter(|a| !a.eq_ignore_ascii_case("NO ACTION")) {
        s.push_str(&format!(" ON DELETE {a}"));
    }
    s.push(';');
    s
}

/// The SQLite table-rebuild procedure for changes `ALTER TABLE` cannot make.
fn sqlite_rebuild(new: &TableSchema, old: &TableSchema, source: SqlDialect) -> Vec<String> {
    let d = SqlDialect::Sqlite;
    let q = |s: &str| quote_ident(d, s);
    let tmp = format!("{}__twinbase_new", new.name);
    let mut renamed = new.clone();
    renamed.name = tmp.clone();
    let opts = DdlOptions { target: d, source, fk_targets: None, include_indexes: false };
    let common: Vec<String> = new.columns.iter().filter(|c| old.columns.iter().any(|o| o.name == c.name)).map(|c| q(&c.name)).collect();
    let mut out = vec![create_table(&renamed, &opts)];
    if !common.is_empty() {
        let cols = common.join(", ");
        out.push(format!("INSERT INTO {} ({cols}) SELECT {cols} FROM {};", q(&tmp), q(&new.name)));
    }
    out.push(format!("DROP TABLE {};", q(&new.name)));
    out.push(format!("ALTER TABLE {} RENAME TO {};", q(&tmp), q(&new.name)));
    for i in new.indexes.iter().filter(|i| !i.primary && !i.unique) {
        let cols = i.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        out.push(format!("CREATE INDEX {} ON {} ({cols});", q(&i.name), q(&new.name)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            default: None,
            primary_key: false,
            unique: false,
            auto: false,
        }
    }

    fn table(name: &str, cols: Vec<ColumnInfo>) -> TableSchema {
        let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        TableSchema {
            schema: "public".into(),
            name: name.into(),
            kind: "table".into(),
            columns: cols,
            primary_key: pk,
            foreign_keys: vec![],
            indexes: vec![],
        }
    }

    fn id() -> ColumnInfo {
        ColumnInfo {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
            default: Some("nextval('x_id_seq'::regclass)".into()),
            primary_key: true,
            unique: false,
            auto: true,
        }
    }

    fn pg(t: &[TableSchema]) -> Side<'_> {
        Side { tables: t, dialect: SqlDialect::Postgres }
    }

    #[test]
    fn identical_schemas_produce_nothing_even_with_type_aliases() {
        let a = vec![table("users", vec![id(), col("email", "character varying(255)"), col("n", "int4")])];
        let b = vec![table("users", vec![id(), col("email", "varchar(255)"), col("n", "integer")])];
        let d = diff(pg(&a), pg(&b), SqlDialect::Postgres);
        assert!(d.identical, "{:?}", d.changes);
        assert!(d.up.is_empty() && d.down.is_empty());
    }

    #[test]
    fn column_and_table_changes_generate_up_and_down() {
        let users_a = table("users", vec![id(), col("email", "text"), col("legacy", "text")]);
        let mut email_b = col("email", "varchar(320)");
        email_b.nullable = false;
        let mut plan = col("plan", "text");
        plan.default = Some("'free'::text".into());
        let mut users_b = table("users", vec![id(), email_b, plan]);
        users_b.indexes.push(IndexInfo { name: "users_email_key".into(), columns: vec!["email".into()], unique: true, primary: false });
        let mut orders = table("orders", vec![id(), col("user_id", "integer")]);
        orders.foreign_keys.push(ForeignKeyInfo {
            name: "orders_user_id_fkey".into(),
            columns: vec!["user_id".into()],
            ref_schema: "public".into(),
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
        });
        let a = vec![users_a];
        let b = vec![users_b, orders];
        let d = diff(pg(&a), pg(&b), SqlDialect::Postgres);
        let up = d.up.join("\n").replace('"', "");
        assert!(up.contains("CREATE TABLE orders"), "{up}");
        assert!(up.contains("REFERENCES users (id) ON DELETE CASCADE"), "{up}");
        assert!(up.contains("ALTER TABLE users ADD COLUMN plan text DEFAULT 'free';"), "{up}");
        assert!(up.contains("ALTER TABLE users ALTER COLUMN email TYPE varchar(320) USING email::varchar(320);"), "{up}");
        assert!(up.contains("ALTER TABLE users ALTER COLUMN email SET NOT NULL;"), "{up}");
        assert!(up.contains("ALTER TABLE users DROP COLUMN legacy;"), "{up}");
        assert!(up.contains("CREATE UNIQUE INDEX users_email_key ON users (email);"), "{up}");
        let down = d.down.join("\n").replace('"', "");
        assert!(down.contains("DROP TABLE orders;"), "{down}");
        assert!(down.contains("ALTER TABLE users ADD COLUMN legacy text;"), "{down}");
        assert!(down.contains("DROP CONSTRAINT IF EXISTS users_email_key"), "{down}");
        assert!(d.warnings.iter().any(|w| w.contains("legacy")));
        // Up then down is the identity on the change list.
        assert_eq!(d.changes.iter().filter(|c| matches!(c, Change::TableAdded { .. })).count(), 1);
    }

    #[test]
    fn renames_are_flagged() {
        let a = vec![table("people", vec![id(), col("full_name", "text")])];
        let b = vec![table("people", vec![id(), col("name", "text")])];
        let d = diff(pg(&a), pg(&b), SqlDialect::Postgres);
        assert!(d.warnings.iter().any(|w| w.contains("looks renamed")), "{:?}", d.warnings);
    }

    #[test]
    fn mysql_uses_modify_and_drop_foreign_key() {
        let mut a_t = table("t", vec![id(), col("n", "int")]);
        a_t.foreign_keys.push(ForeignKeyInfo {
            name: "t_fk".into(),
            columns: vec!["n".into()],
            ref_schema: "app".into(),
            ref_table: "t".into(),
            ref_columns: vec!["id".into()],
            on_delete: None,
        });
        let mut n = col("n", "bigint");
        n.nullable = false;
        let b_t = table("t", vec![id(), n]);
        let (a, b) = (vec![a_t], vec![b_t]);
        let d = diff(Side { tables: &a, dialect: SqlDialect::Mysql }, Side { tables: &b, dialect: SqlDialect::Mysql }, SqlDialect::Mysql);
        assert_eq!(d.up, vec!["ALTER TABLE `t` DROP FOREIGN KEY `t_fk`;", "ALTER TABLE `t` MODIFY COLUMN `n` bigint NOT NULL;"]);
    }

    #[test]
    fn sqlite_rebuilds_tables_it_cannot_alter() {
        let a = vec![table("t", vec![id(), col("a", "text"), col("b", "text")])];
        let mut a2 = col("a", "integer");
        a2.nullable = false;
        let b = vec![table("t", vec![id(), a2])];
        let d =
            diff(Side { tables: &a, dialect: SqlDialect::Sqlite }, Side { tables: &b, dialect: SqlDialect::Sqlite }, SqlDialect::Sqlite);
        let up = d.up.join("\n").replace('"', "");
        assert!(up.starts_with("PRAGMA foreign_keys = OFF;\nBEGIN;"), "{up}");
        assert!(up.contains("INSERT INTO t__twinbase_new (id, a) SELECT id, a FROM t;"), "{up}");
        assert!(up.contains("ALTER TABLE t__twinbase_new RENAME TO t;"), "{up}");
    }
}
