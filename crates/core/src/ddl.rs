//! Schema DDL generation from introspected tables (for export and for change-preview twins).

use std::collections::HashSet;

use twinbase_client::sqlgen::quote_ident;
use twinbase_client::{SqlDialect, TableSchema};
use twinbase_engine::{Dialect, SqlType};

/// Map an introspected type name into another dialect.
pub fn convert_type(data_type: &str, from: SqlDialect, to: SqlDialect) -> String {
    let src = match from {
        SqlDialect::Mysql => Dialect::MySql,
        _ => Dialect::Postgres,
    };
    if from == to {
        return data_type.to_owned();
    }
    let ty = match from {
        SqlDialect::Clickhouse => SqlType::parse_clickhouse(data_type),
        SqlDialect::Cassandra => SqlType::parse_cql(data_type),
        SqlDialect::Spanner | SqlDialect::Bigquery => SqlType::parse_googlesql(data_type),
        _ => SqlType::parse_simple(data_type, src),
    };
    match to {
        SqlDialect::Postgres => ty.pg_format_type(),
        SqlDialect::Mysql => ty.mysql_column_type(),
        SqlDialect::Mssql => ty.mssql_column_type(),
        SqlDialect::Clickhouse => ty.clickhouse_column_type(),
        SqlDialect::Cassandra => ty.cql_column_type(),
        SqlDialect::Spanner => ty.googlesql_column_type(true),
        SqlDialect::Bigquery => ty.googlesql_column_type(false),
        SqlDialect::Oracle => ty.oracle_column_type(),
        SqlDialect::Sqlite => match ty.storage() {
            twinbase_engine::types::Storage::Integer => "INTEGER".into(),
            twinbase_engine::types::Storage::Real => "REAL".into(),
            twinbase_engine::types::Storage::Blob => "BLOB".into(),
            twinbase_engine::types::Storage::Text => "TEXT".into(),
        },
    }
}

/// A default expression in the target dialect's spelling (booleans, current time).
pub fn default_in(d: SqlDialect, def: &str) -> String {
    let lower = def.trim().to_ascii_lowercase();
    match lower.as_str() {
        "now()" | "current_timestamp()" | "current_timestamp" | "getdate()" | "sysdatetime()" | "sysutcdatetime()" | "getutcdate()"
            if matches!(d, SqlDialect::Spanner | SqlDialect::Bigquery) =>
        {
            "CURRENT_TIMESTAMP()".into()
        }
        "now()" | "current_timestamp()" | "current_timestamp" | "getdate()" | "sysdatetime()" | "sysutcdatetime()" | "getutcdate()"
            if d == SqlDialect::Clickhouse =>
        {
            "now()".into()
        }
        "now()" | "current_timestamp()" | "current_timestamp" | "getdate()" | "sysdatetime()" | "sysutcdatetime()" | "getutcdate()" => {
            "CURRENT_TIMESTAMP".into()
        }
        "true" if matches!(d, SqlDialect::Mssql | SqlDialect::Oracle) => "1".into(),
        "false" if matches!(d, SqlDialect::Mssql | SqlDialect::Oracle) => "0".into(),
        _ => def.to_owned(),
    }
}

/// Defaults that are plain literals (safe to carry across servers).
fn portable_default(d: &str) -> Option<String> {
    let t = d.trim();
    // SQL Server writes Unicode literals as N'…'.
    let t = if (t.starts_with("N'") || t.starts_with("n'")) && t.len() > 2 { &t[1..] } else { t };
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("nextval(") || lower.contains("::regclass") {
        return None;
    }
    // PostgreSQL decorates literal defaults with casts: 'x'::character varying
    let base = match t.find("::") {
        Some(p) if t.starts_with('\'') => &t[..p],
        _ => t,
    };
    let lb = base.to_ascii_lowercase();
    let is_literal = base.starts_with('\'')
        || base.parse::<f64>().is_ok()
        || matches!(
            lb.as_str(),
            "true"
                | "false"
                | "null"
                | "current_timestamp"
                | "current_date"
                | "now()"
                | "current_timestamp()"
                | "getdate()"
                | "sysdatetime()"
                | "sysutcdatetime()"
                | "getutcdate()"
        );
    is_literal.then(|| base.to_owned())
}

/// Order tables so referenced tables come first.
pub fn dependency_order(tables: &[TableSchema]) -> Vec<&TableSchema> {
    let mut done: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    let mut remaining: Vec<&TableSchema> = tables.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        remaining.retain(|t| {
            let ready = t
                .foreign_keys
                .iter()
                .all(|f| f.ref_table == t.name || done.contains(f.ref_table.as_str()) || !tables.iter().any(|x| x.name == f.ref_table));
            if ready {
                done.insert(&t.name);
                out.push(*t);
            }
            !ready
        });
        if remaining.len() == before {
            out.append(&mut remaining);
        }
    }
    out
}

pub struct DdlOptions {
    pub target: SqlDialect,
    pub source: SqlDialect,
    /// Only emit foreign keys whose referenced table is in this set (None = all).
    pub fk_targets: Option<HashSet<String>>,
    pub include_indexes: bool,
}

/// A column definition for ClickHouse: nullability is part of the type, no constraints.
pub fn clickhouse_column(c: &twinbase_client::ColumnInfo, source: SqlDialect) -> String {
    let d = SqlDialect::Clickhouse;
    let ty = convert_type(&c.data_type, source, d);
    let ty = if c.nullable && !c.primary_key && !ty.starts_with("Nullable(") { format!("Nullable({ty})") } else { ty };
    let mut s = format!("{} {ty}", quote_ident(d, &c.name));
    if !c.auto
        && let Some(def) = c.default.as_deref().and_then(portable_default)
    {
        s.push_str(&format!(" DEFAULT {}", default_in(d, &def)));
    }
    s
}

/// ClickHouse tables: MergeTree sorted by the primary key; foreign keys and unique constraints do
/// not exist there.
fn create_table_clickhouse(t: &TableSchema, o: &DdlOptions) -> String {
    let q = |s: &str| quote_ident(SqlDialect::Clickhouse, s);
    let cols: Vec<String> = t.columns.iter().map(|c| clickhouse_column(c, o.source)).collect();
    let order = if t.primary_key.is_empty() {
        "tuple()".to_owned()
    } else {
        format!("({})", t.primary_key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "))
    };
    format!("CREATE TABLE {} (\n  {}\n) ENGINE = MergeTree ORDER BY {order};", q(&t.name), cols.join(",\n  "))
}

/// CQL tables: `PRIMARY KEY ((partition…), clustering…)`; no constraints, defaults or foreign keys.
fn create_table_cql(t: &TableSchema, o: &DdlOptions) -> String {
    let d = SqlDialect::Cassandra;
    let q = |s: &str| quote_ident(d, s);
    let mut parts: Vec<String> = t.columns.iter().map(|c| format!("{} {}", q(&c.name), convert_type(&c.data_type, o.source, d))).collect();
    let key: Vec<String> =
        if t.primary_key.is_empty() { t.columns.first().map(|c| vec![c.name.clone()]).unwrap_or_default() } else { t.primary_key.clone() };
    // Introspected CQL tables carry their partition key; others partition by the first key column.
    let partition: Vec<String> = t
        .indexes
        .iter()
        .find(|i| i.primary && i.name == "partition key")
        .map(|i| i.columns.clone())
        .unwrap_or_else(|| key.iter().take(1).cloned().collect());
    let clustering: Vec<String> = key.iter().filter(|k| !partition.contains(k)).cloned().collect();
    let pk = partition.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let mut pk = if partition.len() > 1 { format!("({pk})") } else { pk };
    for c in &clustering {
        pk.push_str(&format!(", {}", q(c)));
    }
    parts.push(format!("PRIMARY KEY ({pk})"));
    let mut out = format!("CREATE TABLE {} (\n  {}\n);", q(&t.name), parts.join(",\n  "));
    if o.include_indexes {
        for ix in t.indexes.iter().filter(|i| !i.primary) {
            out.push_str(&format!(
                "\nCREATE INDEX {} ON {} ({});",
                q(&ix.name),
                q(&t.name),
                ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out
}

/// GoogleSQL tables: Spanner puts the primary key after the column list (and needs one); BigQuery
/// declares it NOT ENFORCED. Foreign keys are kept for Spanner only.
fn create_table_googlesql(t: &TableSchema, o: &DdlOptions) -> String {
    let d = o.target;
    let q = |s: &str| quote_ident(d, s);
    let mut parts: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", q(&c.name), convert_type(&c.data_type, o.source, d));
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if !c.auto
                && let Some(def) = c.default.as_deref().and_then(portable_default)
            {
                let def = default_in(d, &def);
                s.push_str(&if d == SqlDialect::Spanner { format!(" DEFAULT ({def})") } else { format!(" DEFAULT {def}") });
            }
            s
        })
        .collect();
    if d == SqlDialect::Spanner {
        for fk in t.foreign_keys.iter().filter(|f| !f.name.starts_with("INTERLEAVE")) {
            if o.fk_targets.as_ref().is_some_and(|set| !set.contains(&fk.ref_table)) {
                continue;
            }
            parts.push(format!(
                "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
                q(&fk.name),
                fk.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "),
                q(&fk.ref_table),
                fk.ref_columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    } else if !t.primary_key.is_empty() {
        parts.push(format!("PRIMARY KEY ({}) NOT ENFORCED", t.primary_key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")));
    }
    let mut out = format!("CREATE TABLE {} (\n  {}\n)", q(&t.name), parts.join(",\n  "));
    if d == SqlDialect::Spanner {
        let key = if t.primary_key.is_empty() {
            t.columns.first().map(|c| vec![c.name.clone()]).unwrap_or_default()
        } else {
            t.primary_key.clone()
        };
        out.push_str(&format!(" PRIMARY KEY ({})", key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")));
        if o.source == SqlDialect::Spanner
            && let Some(parent) = t.foreign_keys.iter().find(|f| f.name.starts_with("INTERLEAVE"))
        {
            out.push_str(&format!(
                ",\n  INTERLEAVE IN PARENT {}{}",
                q(&parent.ref_table),
                if parent.on_delete.is_some() { " ON DELETE CASCADE" } else { "" }
            ));
        }
    }
    out.push(';');
    if o.include_indexes && d == SqlDialect::Spanner {
        for ix in t.indexes.iter().filter(|i| !i.primary) {
            out.push_str(&format!(
                "\nCREATE {}INDEX {} ON {} ({});",
                if ix.unique { "UNIQUE " } else { "" },
                q(&ix.name),
                q(&t.name),
                ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out
}

pub fn create_table(t: &TableSchema, o: &DdlOptions) -> String {
    let d = o.target;
    if d == SqlDialect::Clickhouse {
        return create_table_clickhouse(t, o);
    }
    if d == SqlDialect::Cassandra {
        return create_table_cql(t, o);
    }
    if matches!(d, SqlDialect::Spanner | SqlDialect::Bigquery) {
        return create_table_googlesql(t, o);
    }
    let q = |s: &str| quote_ident(d, s);
    let single_auto_pk = t.primary_key.len() == 1 && t.columns.iter().any(|c| c.auto && c.primary_key);
    let mut parts = Vec::new();
    for c in &t.columns {
        let mut ty = convert_type(&c.data_type, o.source, d);
        let mut extra = String::new();
        if c.auto {
            match d {
                SqlDialect::Postgres => {
                    let lower = ty.to_ascii_lowercase();
                    ty = if lower.contains("bigint") {
                        "bigserial".into()
                    } else if lower.contains("smallint") {
                        "smallserial".into()
                    } else {
                        "serial".into()
                    };
                }
                SqlDialect::Mysql => extra.push_str(" AUTO_INCREMENT"),
                SqlDialect::Mssql => extra.push_str(" IDENTITY(1,1)"),
                SqlDialect::Oracle => extra.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                SqlDialect::Sqlite | SqlDialect::Clickhouse | SqlDialect::Cassandra | SqlDialect::Spanner | SqlDialect::Bigquery => {}
            }
        }
        let mut s = format!("{} {ty}", q(&c.name));
        // Oracle wants DEFAULT before NOT NULL; the others accept either order.
        let default = if c.auto {
            None
        } else {
            c.default.as_deref().and_then(portable_default).map(|def| format!(" DEFAULT {}", default_in(d, &def)))
        };
        // Oracle: the identity clause or DEFAULT directly after the type, then constraints.
        if d == SqlDialect::Oracle {
            s.push_str(&std::mem::take(&mut extra));
            if let Some(def) = &default {
                s.push_str(def);
            }
        }
        if !c.nullable && !(d == SqlDialect::Postgres && c.auto) {
            s.push_str(" NOT NULL");
        }
        if d != SqlDialect::Oracle
            && let Some(def) = &default
        {
            s.push_str(def);
        }
        if d == SqlDialect::Sqlite && single_auto_pk && c.primary_key {
            s = format!("{} INTEGER PRIMARY KEY", q(&c.name));
        }
        s.push_str(&extra);
        parts.push(s);
    }
    if !t.primary_key.is_empty() && !(d == SqlDialect::Sqlite && single_auto_pk) {
        parts.push(format!("PRIMARY KEY ({})", t.primary_key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")));
    }
    for ix in t.indexes.iter().filter(|i| i.unique && !i.primary) {
        parts.push(format!("CONSTRAINT {} UNIQUE ({})", q(&ix.name), ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")));
    }
    for fk in &t.foreign_keys {
        if o.fk_targets.as_ref().is_some_and(|set| !set.contains(&fk.ref_table)) {
            continue;
        }
        let mut s = format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            q(&fk.name),
            fk.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "),
            q(&fk.ref_table),
            fk.ref_columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
        );
        if let Some(a) = &fk.on_delete {
            s.push_str(&format!(" ON DELETE {a}"));
        }
        parts.push(s);
    }
    let mut out = format!("CREATE TABLE {} (\n  {}\n);", q(&t.name), parts.join(",\n  "));
    if o.include_indexes {
        for ix in t.indexes.iter().filter(|i| !i.unique && !i.primary) {
            out.push_str(&format!(
                "\nCREATE INDEX {} ON {} ({});",
                q(&ix.name),
                q(&t.name),
                ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out
}

/// DDL for a whole schema, parents before children.
pub fn schema_ddl(tables: &[TableSchema], source: SqlDialect, target: SqlDialect) -> String {
    let opts = DdlOptions { target, source, fk_targets: None, include_indexes: true };
    dependency_order(&tables.iter().filter(|t| t.kind == "table").cloned().collect::<Vec<_>>())
        .into_iter()
        .map(|t| create_table(t, &opts))
        .collect::<Vec<_>>()
        .join("\n\n")
}
