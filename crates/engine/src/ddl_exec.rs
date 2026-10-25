//! Applying DDL plans to SQLite while keeping the catalog metadata in sync.
//!
//! Table changes never drop and recreate a table that other tables reference: SQLite would run
//! foreign-key actions on the implicit delete. Instead, definitions are rewritten in place in
//! `sqlite_schema` (the documented "writable_schema" procedure) and columns are added/dropped
//! with native `ALTER TABLE`. Only primary-key changes use a full copy, and only when nothing
//! references the table.

use rusqlite::params;

use crate::Dialect;
use crate::catalog::{
    self, CheckDef, EnumDef, ForeignKeyDef, Identity, IndexDef, KeyDef, TableDef, ViewDef, quote_ident, quote_literal, render_create_index,
    render_create_table, unique_index,
};
use crate::engine::remove_schema_file;
use crate::error::{EngineError, Result, state};
use crate::functions::SeqState;
use crate::session::{Outcome, Session, TxState, sqlite_err};
use crate::translate::{AlterOp, CommentTarget, ConstraintDef, DropKind, NewSequence, Plan};
use crate::types::SqlType;

fn notice_command(tag: &str) -> Outcome {
    Outcome { tag: tag.to_owned(), affected: 0, result: None, last_insert_id: None, tx: crate::session::TxEffect::None, message: None }
}

impl Session {
    pub(crate) fn run_ddl(&mut self, plan: Plan) -> Result<Outcome> {
        let tag = match &plan {
            Plan::CreateTable { as_query: Some(_), .. } if self.dialect == Dialect::Postgres => "SELECT",
            Plan::CreateTable { .. } => "CREATE TABLE",
            Plan::AlterTable { .. } => "ALTER TABLE",
            Plan::Drop { kind, .. } => match kind {
                DropKind::Table => "DROP TABLE",
                DropKind::View => "DROP VIEW",
                DropKind::Index => "DROP INDEX",
                DropKind::Sequence => "DROP SEQUENCE",
                DropKind::Type => "DROP TYPE",
                DropKind::Schema => "DROP SCHEMA",
                DropKind::Database => "DROP DATABASE",
            },
            Plan::CreateIndex { .. } => "CREATE INDEX",
            Plan::CreateView { .. } => "CREATE VIEW",
            Plan::CreateSchema { .. } => "CREATE SCHEMA",
            Plan::CreateDatabase { .. } => "CREATE DATABASE",
            Plan::CreateEnum { .. } => "CREATE TYPE",
            Plan::AlterEnumAddValue { .. } => "ALTER TYPE",
            Plan::CreateSequence { .. } => "CREATE SEQUENCE",
            Plan::Truncate { .. } => "TRUNCATE TABLE",
            Plan::Comment { .. } => "COMMENT",
            _ => "",
        }
        .to_owned();
        // MySQL commits the open transaction before any DDL statement.
        if self.dialect == Dialect::MySql && self.tx == TxState::Active {
            self.commit_internal()?;
        }
        // Schema files cannot be attached/detached inside a transaction.
        let needs_layout = matches!(
            plan,
            Plan::CreateSchema { .. } | Plan::CreateDatabase { .. } | Plan::Drop { kind: DropKind::Schema | DropKind::Database, .. }
        );
        if needs_layout {
            if self.tx != TxState::Idle {
                return Err(EngineError::new(
                    state::ACTIVE_TRANSACTION,
                    format!("{tag} cannot run inside a transaction block in the TwinLite engine"),
                ));
            }
            let affected = self.apply_layout(plan)?;
            self.reload_catalog()?;
            self.publish_catalog_change();
            return Ok(Outcome { affected, ..notice_command(&tag) });
        }
        self.conn.execute_batch("SAVEPOINT _tb_ddl").map_err(sqlite_err)?;
        let result = self.apply_ddl(plan);
        match result {
            Ok(affected) => {
                self.conn.execute_batch("RELEASE _tb_ddl").map_err(sqlite_err)?;
                self.flush_sequences()?;
                self.reload_catalog()?;
                if self.tx == TxState::Idle {
                    self.publish_catalog_change();
                } else {
                    self.ddl_in_tx = true;
                }
                Ok(Outcome { affected, ..notice_command(&tag) })
            }
            Err(e) => {
                let _ = self.conn.execute_batch("PRAGMA writable_schema = OFF; ROLLBACK TO _tb_ddl; RELEASE _tb_ddl");
                let _ = self.reload_catalog();
                Err(e)
            }
        }
    }

    fn apply_layout(&mut self, plan: Plan) -> Result<u64> {
        match plan {
            Plan::CreateSchema { name, if_not_exists } => {
                if self.attached.contains(&name) {
                    return if if_not_exists {
                        Ok(0)
                    } else {
                        Err(EngineError::new(state::DUPLICATE_SCHEMA, format!("schema \"{name}\" already exists")))
                    };
                }
                self.ensure_schema_file(&name)?;
                Ok(0)
            }
            Plan::CreateDatabase { name, if_not_exists } => match self.dialect {
                Dialect::MySql => {
                    if self.attached.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
                        return if if_not_exists {
                            Ok(0)
                        } else {
                            Err(EngineError::new(state::DUPLICATE_DATABASE, format!("Can't create database '{name}'; database exists"))
                                .with_mysql(1007))
                        };
                    }
                    self.ensure_schema_file(&name)?;
                    Ok(1)
                }
                _ => match self.engine.create_database(&name) {
                    Err(_) if if_not_exists => Ok(0),
                    other => other.map(|_| 0),
                },
            },
            Plan::Drop { kind: DropKind::Schema, names, if_exists, cascade } => {
                for (name, _) in names {
                    if !self.attached.contains(&name) {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::new(state::UNDEFINED_SCHEMA, format!("schema \"{name}\" does not exist")));
                    }
                    let cat = self.catalog();
                    let non_empty =
                        cat.schema(&name).is_some_and(|sd| !sd.tables.is_empty() || !sd.views.is_empty() || !sd.enums.is_empty());
                    if non_empty && !cascade {
                        return Err(EngineError::new(
                            state::DEPENDENT_OBJECTS_STILL_EXIST,
                            format!("cannot drop schema {name} because other objects depend on it"),
                        )
                        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
                    }
                    self.detach_and_remove(&name)?;
                }
                Ok(0)
            }
            Plan::Drop { kind: DropKind::Database, names, if_exists, .. } => {
                for (name, _) in names {
                    match self.dialect {
                        Dialect::MySql => {
                            let Some(actual) = self.attached.iter().find(|a| a.eq_ignore_ascii_case(&name)).cloned() else {
                                if if_exists {
                                    continue;
                                }
                                return Err(EngineError::new(
                                    state::INVALID_CATALOG_NAME,
                                    format!("Can't drop database '{name}'; database doesn't exist"),
                                )
                                .with_mysql(1008));
                            };
                            self.detach_and_remove(&actual)?;
                            if self.database.eq_ignore_ascii_case(&actual) {
                                self.database.clear();
                                self.shared.lock().database.clear();
                            }
                        }
                        _ => {
                            if name == self.database {
                                return Err(EngineError::new("55006", "cannot drop the currently open database"));
                            }
                            match self.engine.drop_database(&name) {
                                Err(_) if if_exists => {}
                                other => other?,
                            }
                        }
                    }
                }
                self.update_search_path();
                Ok(0)
            }
            _ => Ok(0),
        }
    }

    fn detach_and_remove(&mut self, name: &str) -> Result<()> {
        let path = {
            let mut stmt = self.conn.prepare("SELECT file FROM pragma_database_list WHERE name = ?1").map_err(sqlite_err)?;
            stmt.query_row([name], |r| r.get::<_, String>(0)).map_err(sqlite_err)?
        };
        self.conn.execute_batch(&format!("DETACH DATABASE {}", quote_ident(name))).map_err(sqlite_err)?;
        self.attached.retain(|a| a != name);
        remove_schema_file(std::path::Path::new(&path))?;
        self.engine.inner.layout_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.update_search_path();
        Ok(())
    }

    fn apply_ddl(&mut self, plan: Plan) -> Result<u64> {
        match plan {
            Plan::CreateTable { schema, def, sequences, as_query, .. } => {
                if let Some(q) = as_query {
                    return self.create_table_as(&schema, def, q);
                }
                self.create_table(&schema, &def, &sequences)?;
                Ok(0)
            }
            Plan::AlterTable { schema, table, ops, .. } => {
                for op in ops {
                    self.alter(&schema, &table, op)?;
                }
                Ok(0)
            }
            Plan::Drop { kind, names, if_exists, cascade } => {
                self.drop_objects(kind, names, if_exists, cascade)?;
                Ok(0)
            }
            Plan::CreateIndex { schema, table, def, if_not_exists } => {
                let mut td = self.table_def(&schema, &table)?;
                if self.index_name_taken(&schema, &def.name) {
                    if if_not_exists {
                        return Ok(0);
                    }
                    return Err(if self.dialect == Dialect::MySql {
                        EngineError::new(state::DUPLICATE_OBJECT, format!("Duplicate key name '{}'", def.name)).with_mysql(1061)
                    } else {
                        EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{}\" already exists", def.name))
                    });
                }
                self.exec(&render_create_index(&schema, &td.name, &def)).map_err(|e| unique_build_error(e, &def.name, self.dialect))?;
                td.indexes.push(def);
                catalog::save_table(&self.conn, &schema, &td)?;
                Ok(0)
            }
            Plan::CreateView { schema, name, definition, plan, columns, or_replace, if_not_exists } => {
                let cat = self.catalog();
                if cat.table(&schema, &name).is_some() {
                    return Err(EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
                }
                if cat.view(&schema, &name).is_some() {
                    if if_not_exists {
                        return Ok(0);
                    }
                    if !or_replace {
                        return Err(EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
                    }
                    self.exec_mapped(&format!("DROP VIEW {}.{}", quote_ident(&schema), quote_ident(&name)))?;
                }
                let cols = if columns.is_empty() { String::new() } else { format!(" ({})", catalog::join_idents(&columns)) };
                self.exec(&format!("CREATE VIEW {}.{}{cols} AS {}", quote_ident(&schema), quote_ident(&name), plan.sql))
                    .map_err(|e| self.map_sqlite(e, Some(&plan)))?;
                let view_cols =
                    self.query_columns(&format!("SELECT * FROM {}.{}", quote_ident(&schema), quote_ident(&name)), &plan.projection)?;
                let def = ViewDef { name: name.clone(), definition, sql: plan.sql, columns: view_cols };
                catalog::save_meta(&self.conn, &schema, "view", &name, &serde_json::to_string(&def).expect("view serializes"))?;
                Ok(0)
            }
            Plan::CreateEnum { schema, name, values } => {
                let cat = self.catalog();
                if cat.schema(&schema).is_some_and(|s| s.enums.contains_key(&name)) {
                    return Err(EngineError::new(state::DUPLICATE_OBJECT, format!("type \"{name}\" already exists")));
                }
                let def = EnumDef { name: name.clone(), values };
                catalog::save_meta(&self.conn, &schema, "enum", &name, &serde_json::to_string(&def).expect("enum serializes"))?;
                Ok(0)
            }
            Plan::AlterEnumAddValue { schema, name, value, if_not_exists } => {
                let cat = self.catalog();
                let mut def = cat
                    .schema(&schema)
                    .and_then(|s| s.enums.get(&name).cloned())
                    .ok_or_else(|| EngineError::new(state::UNDEFINED_OBJECT, format!("type \"{name}\" does not exist")))?;
                if def.values.contains(&value) {
                    if if_not_exists {
                        return Ok(0);
                    }
                    return Err(EngineError::new(state::DUPLICATE_OBJECT, format!("enum label \"{value}\" already exists")));
                }
                def.values.push(value);
                catalog::save_meta(&self.conn, &schema, "enum", &name, &serde_json::to_string(&def).expect("enum serializes"))?;
                // Columns of this type cache the label list.
                let cat = self.catalog();
                for sd in cat.schemas.values() {
                    for t in sd.tables.values() {
                        let mut td = t.clone();
                        let mut changed = false;
                        for c in &mut td.columns {
                            if let SqlType::Enum { name: n, values } = &mut c.ty
                                && *n == name
                            {
                                *values = def.values.clone();
                                changed = true;
                            }
                        }
                        if changed {
                            catalog::save_table(&self.conn, &sd.name, &td)?;
                        }
                    }
                }
                Ok(0)
            }
            Plan::CreateSequence { seq, if_not_exists } => {
                let key = format!("{}.{}", seq.schema, seq.name);
                let cat = self.catalog();
                if cat.schema(&seq.schema).is_some_and(|s| s.sequences.contains_key(&seq.name)) {
                    if if_not_exists {
                        return Ok(0);
                    }
                    return Err(EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{}\" already exists", seq.name)));
                }
                let _ = key;
                self.create_sequence(&seq)?;
                Ok(0)
            }
            Plan::Truncate { tables, restart_identity, cascade } => {
                self.truncate(tables, restart_identity, cascade)?;
                Ok(0)
            }
            Plan::Comment { target, text } => {
                match target {
                    CommentTarget::Table(s, t) => {
                        let mut td = self.table_def(&s, &t)?;
                        td.comment = text;
                        catalog::save_table(&self.conn, &s, &td)?;
                    }
                    CommentTarget::Column(s, t, c) => {
                        let mut td = self.table_def(&s, &t)?;
                        let col = td.column_mut(&c).ok_or_else(|| {
                            EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{c}\" of relation \"{t}\" does not exist"))
                        })?;
                        col.comment = text;
                        catalog::save_table(&self.conn, &s, &td)?;
                    }
                    CommentTarget::Other => {}
                }
                Ok(0)
            }
            _ => Ok(0),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------------------

    pub(crate) fn exec(&self, sql: &str) -> std::result::Result<(), rusqlite::Error> {
        self.conn.execute_batch(sql)
    }

    fn exec_mapped(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql).map_err(|e| self.map_sqlite(e, None))
    }

    fn table_def(&self, schema: &str, table: &str) -> Result<TableDef> {
        self.catalog()
            .table(schema, table)
            .cloned()
            .ok_or_else(|| EngineError::new(state::UNDEFINED_TABLE, format!("relation \"{table}\" does not exist")))
    }

    fn index_name_taken(&self, schema: &str, name: &str) -> bool {
        let cat = self.catalog();
        cat.schema(schema).is_some_and(|sd| {
            sd.tables.values().any(|t| {
                t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(name))
                    || t.uniques.iter().any(|u| u.name.eq_ignore_ascii_case(name))
                    || (self.dialect == Dialect::Postgres && t.primary_key.as_ref().is_some_and(|p| p.name.eq_ignore_ascii_case(name)))
            }) || (self.dialect == Dialect::Postgres && (sd.tables.contains_key(name) || sd.views.contains_key(name)))
        })
    }

    /// Column names/types of a query (used for views and CREATE TABLE AS).
    fn query_columns(&self, sql: &str, hints: &[Option<SqlType>]) -> Result<Vec<(String, SqlType)>> {
        let stmt = self.conn.prepare(sql).map_err(sqlite_err)?;
        let cat = self.catalog();
        let mut out = Vec::new();
        for (i, m) in stmt.columns_with_metadata().enumerate() {
            let from_origin = match (m.database_name(), m.table_name(), m.origin_name()) {
                (Some(db), Some(t), Some(o)) => cat.table(db, t).and_then(|td| td.column(o)).map(|c| c.ty.clone()),
                _ => None,
            };
            out.push((m.name().to_owned(), from_origin.or_else(|| hints.get(i).cloned().flatten())));
        }
        drop(stmt);
        // Columns whose type the statement does not tell (e.g. from generate_series): judge by the
        // first values, so numbers do not become text.
        if out.iter().any(|(_, t)| t.is_none()) {
            let mut sample = self.conn.prepare(&format!("SELECT * FROM ({sql}) LIMIT 100")).map_err(sqlite_err)?;
            let n = out.len();
            let mut seen: Vec<Option<SqlType>> = vec![None; n];
            let mut rows = sample.query([]).map_err(sqlite_err)?;
            while let Some(row) = rows.next().map_err(sqlite_err)? {
                for (i, slot) in seen.iter_mut().enumerate() {
                    let ty = match row.get_ref(i).map_err(sqlite_err)? {
                        rusqlite::types::ValueRef::Integer(v) if self.dialect == Dialect::Postgres && i32::try_from(v).is_ok() => {
                            Some(SqlType::Int4)
                        }
                        rusqlite::types::ValueRef::Integer(_) => Some(SqlType::Int8),
                        rusqlite::types::ValueRef::Real(_) => Some(SqlType::Float8),
                        rusqlite::types::ValueRef::Text(_) => Some(SqlType::Text),
                        rusqlite::types::ValueRef::Blob(_) => Some(SqlType::Bytea),
                        rusqlite::types::ValueRef::Null => None,
                    };
                    *slot = match (slot.take(), ty) {
                        (None, t) => t,
                        // A wider value widens the column (int, then bigint, then float; text wins).
                        (Some(SqlType::Int4), Some(SqlType::Int8)) => Some(SqlType::Int8),
                        (Some(SqlType::Int4 | SqlType::Int8), Some(SqlType::Float8)) => Some(SqlType::Float8),
                        (Some(_), Some(SqlType::Text)) => Some(SqlType::Text),
                        (cur, _) => cur,
                    };
                }
            }
            for ((_, t), s) in out.iter_mut().zip(seen) {
                if t.is_none() {
                    *t = s;
                }
            }
        }
        Ok(out.into_iter().map(|(n, t)| (n, t.unwrap_or(SqlType::Text))).collect())
    }

    fn create_sequence(&mut self, seq: &NewSequence) -> Result<()> {
        self.conn
            .execute(
                &format!(
                    "INSERT OR REPLACE INTO {}._tb_seq (name, last_value, is_called, increment, start_value, owner) VALUES (?1, ?2, 0, ?3, ?2, ?4)",
                    quote_ident(&seq.schema)
                ),
                params![seq.name, seq.start, seq.increment, seq.owner],
            )
            .map_err(sqlite_err)?;
        self.engine.inner.sequences.replace(
            &format!("{}.{}", seq.schema, seq.name),
            SeqState {
                last_value: seq.start,
                is_called: false,
                increment: seq.increment,
                start_value: seq.start,
                min_value: None,
                max_value: None,
            },
        );
        Ok(())
    }

    fn drop_sequence(&mut self, schema: &str, name: &str) -> Result<()> {
        self.conn.execute(&format!("DELETE FROM {}._tb_seq WHERE name = ?1", quote_ident(schema)), [name]).map_err(sqlite_err)?;
        self.engine.inner.sequences.remove(&format!("{schema}.{name}"));
        Ok(())
    }

    fn create_table(&mut self, schema: &str, def: &TableDef, sequences: &[NewSequence]) -> Result<()> {
        for s in sequences {
            self.create_sequence(s)?;
        }
        self.exec_mapped(&render_create_table(schema, def, self.dialect, &def.name, true))?;
        for u in &def.uniques {
            self.exec_mapped(&render_create_index(schema, &def.name, &unique_index(u)))?;
        }
        for i in &def.indexes {
            self.exec_mapped(&render_create_index(schema, &def.name, i))?;
        }
        catalog::save_table(&self.conn, schema, def)
    }

    fn create_table_as(&mut self, schema: &str, mut def: TableDef, q: crate::translate::SqlPlan) -> Result<u64> {
        let columns = self.query_columns(&q.sql, &q.projection)?;
        def.columns = columns
            .into_iter()
            .map(|(name, ty)| catalog::ColumnDef {
                name,
                type_text: ty.display(self.dialect),
                ty,
                nullable: true,
                default: None,
                identity: None,
                comment: None,
                on_update_now: false,
                unsigned: false,
            })
            .collect();
        self.exec_mapped(&render_create_table(schema, &def, self.dialect, &def.name, true))?;
        let cols = catalog::join_idents(&def.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
        let n = self
            .conn
            .execute(&format!("INSERT INTO {}.{} ({cols}) SELECT * FROM ({})", quote_ident(schema), quote_ident(&def.name), q.sql), [])
            .map_err(|e| self.map_sqlite(e, Some(&q)))?;
        catalog::save_table(&self.conn, schema, &def)?;
        Ok(n as u64)
    }

    /// Replace the stored `CREATE TABLE` text of a table with the rendering of `def`.
    fn rewrite_definition(&self, schema: &str, def: &TableDef) -> Result<()> {
        let sql = render_create_table(schema, def, self.dialect, &def.name, false);
        let q = quote_ident(schema);
        let version: i64 = self.conn.query_row(&format!("PRAGMA {q}.schema_version"), [], |r| r.get(0)).map_err(sqlite_err)?;
        self.exec("PRAGMA writable_schema = ON").map_err(sqlite_err)?;
        let updated =
            self.conn.execute(&format!("UPDATE {q}.sqlite_schema SET sql = ?1 WHERE type = 'table' AND name = ?2"), params![sql, def.name]);
        let bump = self.exec(&format!("PRAGMA {q}.schema_version = {}", version + 1));
        self.exec("PRAGMA writable_schema = OFF").map_err(sqlite_err)?;
        updated.map_err(sqlite_err)?;
        bump.map_err(sqlite_err)?;
        Ok(())
    }

    /// Copy a table into a new definition (only used when no other table references it).
    fn rebuild_table(&mut self, schema: &str, old: &TableDef, new: &TableDef) -> Result<()> {
        let cat = self.catalog();
        if !cat.referencing(schema, &old.name).is_empty() {
            return Err(EngineError::unsupported(format!(
                "changing the primary key of \"{}\" while other tables reference it is not supported by the TwinLite engine",
                old.name
            )));
        }
        let tmp = format!("_tb_new_{}", new.name);
        self.exec_mapped(&render_create_table(schema, new, self.dialect, &tmp, true))?;
        let cols: Vec<String> = new.columns.iter().filter(|c| old.column(&c.name).is_some()).map(|c| c.name.clone()).collect();
        let list = catalog::join_idents(&cols);
        self.exec(&format!(
            "INSERT INTO {q}.{} ({list}) SELECT {list} FROM {q}.{}",
            quote_ident(&tmp),
            quote_ident(&old.name),
            q = quote_ident(schema)
        ))
        .map_err(|e| self.map_sqlite(e, None))?;
        self.exec_mapped(&format!("DROP TABLE {}.{}", quote_ident(schema), quote_ident(&old.name)))?;
        self.exec_mapped(&format!("ALTER TABLE {}.{} RENAME TO {}", quote_ident(schema), quote_ident(&tmp), quote_ident(&new.name)))?;
        self.rewrite_definition(schema, new)?;
        for u in &new.uniques {
            self.exec_mapped(&render_create_index(schema, &new.name, &unique_index(u)))?;
        }
        for i in &new.indexes {
            self.exec_mapped(&render_create_index(schema, &new.name, i))?;
        }
        Ok(())
    }

    fn count(&self, sql: &str) -> Result<i64> {
        self.conn.query_row(sql, [], |r| r.get(0)).map_err(|e| self.map_sqlite(e, None))
    }

    fn add_constraint(&mut self, schema: &str, td: &mut TableDef, c: ConstraintDef) -> Result<()> {
        let q = format!("{}.{}", quote_ident(schema), quote_ident(&td.name));
        match c {
            ConstraintDef::PrimaryKey(k) => {
                if td.primary_key.is_some() {
                    return Err(EngineError::new("42P16", format!("multiple primary keys for table \"{}\" are not allowed", td.name))
                        .with_mysql(1068));
                }
                let cols = catalog::join_idents(&k.columns);
                let nulls = self.count(&format!(
                    "SELECT count(*) FROM {q} WHERE {}",
                    k.columns.iter().map(|c| format!("{} IS NULL", quote_ident(c))).collect::<Vec<_>>().join(" OR ")
                ))?;
                if nulls > 0 {
                    return Err(EngineError::new(
                        state::NOT_NULL_VIOLATION,
                        format!("column \"{}\" of relation \"{}\" contains null values", k.columns[0], td.name),
                    ));
                }
                let dups = self.count(&format!("SELECT count(*) FROM (SELECT 1 FROM {q} GROUP BY {cols} HAVING count(*) > 1)"))?;
                if dups > 0 {
                    return Err(EngineError::new(state::UNIQUE_VIOLATION, format!("could not create unique index \"{}\"", k.name))
                        .with_detail(format!("Key ({}) is duplicated.", k.columns.join(", "))));
                }
                let old = td.clone();
                for col in &k.columns {
                    if let Some(c) = td.column_mut(col) {
                        c.nullable = false;
                    }
                }
                td.primary_key = Some(k);
                self.rebuild_table(schema, &old, td)?;
            }
            ConstraintDef::Unique(k) => {
                self.exec(&render_create_index(schema, &td.name, &unique_index(&k)))
                    .map_err(|e| unique_build_error(e, &k.name, self.dialect))?;
                td.uniques.push(k);
            }
            ConstraintDef::Index(i) => {
                self.exec_mapped(&render_create_index(schema, &td.name, &i))?;
                td.indexes.push(i);
            }
            ConstraintDef::ForeignKey(fk) => {
                self.validate_fk(schema, td, &fk)?;
                td.foreign_keys.push(fk);
                self.rewrite_definition(schema, td)?;
            }
            ConstraintDef::Check(ck) => {
                let bad = self.count(&format!("SELECT count(*) FROM {q} WHERE NOT ({})", ck.sql))?;
                if bad > 0 {
                    return Err(EngineError::new(
                        state::CHECK_VIOLATION,
                        format!("check constraint \"{}\" of relation \"{}\" is violated by some row", ck.name, td.name),
                    ));
                }
                td.checks.push(ck);
                self.rewrite_definition(schema, td)?;
            }
        }
        Ok(())
    }

    fn validate_fk(&self, schema: &str, td: &TableDef, fk: &ForeignKeyDef) -> Result<()> {
        if fk.ref_schema != schema || self.catalog().table(&fk.ref_schema, &fk.ref_table).is_none() {
            return Ok(());
        }
        let child = format!("{}.{}", quote_ident(schema), quote_ident(&td.name));
        let parent = format!("{}.{}", quote_ident(&fk.ref_schema), quote_ident(&fk.ref_table));
        let not_null = fk.columns.iter().map(|c| format!("c.{} IS NOT NULL", quote_ident(c))).collect::<Vec<_>>().join(" AND ");
        let join = fk
            .columns
            .iter()
            .zip(&fk.ref_columns)
            .map(|(c, r)| format!("p.{} = c.{}", quote_ident(r), quote_ident(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let bad =
            self.count(&format!("SELECT count(*) FROM {child} c WHERE {not_null} AND NOT EXISTS (SELECT 1 FROM {parent} p WHERE {join})"))?;
        if bad > 0 {
            return Err(if self.dialect == Dialect::MySql {
                EngineError::new(
                    state::FOREIGN_KEY_VIOLATION,
                    format!("Cannot add or update a child row: a foreign key constraint fails (CONSTRAINT `{}`)", fk.name),
                )
                .with_mysql(1452)
            } else {
                EngineError::new(
                    state::FOREIGN_KEY_VIOLATION,
                    format!("insert or update on table \"{}\" violates foreign key constraint \"{}\"", td.name, fk.name),
                )
                .with_detail(format!(
                    "Some values of ({}) are not present in table \"{}\".",
                    fk.columns.join(", "),
                    fk.ref_table
                ))
            });
        }
        Ok(())
    }

    /// Remove foreign keys in other tables that reference `schema.table` (columns: any or the given ones).
    fn drop_referencing_fks(&mut self, schema: &str, table: &str, only_columns: Option<&[String]>) -> Result<()> {
        let cat = self.catalog();
        for (cs, ct, fk) in cat.referencing(schema, table) {
            if let Some(cols) = only_columns
                && !fk.ref_columns.iter().any(|c| cols.contains(c))
            {
                continue;
            }
            let mut child = self.table_def(&cs, &ct)?;
            child.foreign_keys.retain(|f| f.name != fk.name);
            self.rewrite_definition(&cs, &child)?;
            catalog::save_table(&self.conn, &cs, &child)?;
        }
        Ok(())
    }

    fn dependents_error(&self, what: &str, schema: &str, table: &str, cols: Option<&[String]>) -> Option<EngineError> {
        let refs: Vec<_> = self
            .catalog()
            .referencing(schema, table)
            .into_iter()
            .filter(|(_, _, fk)| cols.is_none_or(|c| fk.ref_columns.iter().any(|x| c.contains(x))))
            .collect();
        let (_, ct, fk) = refs.first()?;
        Some(if self.dialect == Dialect::MySql {
            EngineError::new(
                state::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("Cannot drop table '{table}' referenced by a foreign key constraint '{}' on table '{ct}'.", fk.name),
            )
            .with_mysql(3730)
        } else {
            EngineError::new(state::DEPENDENT_OBJECTS_STILL_EXIST, format!("cannot drop {what} because other objects depend on it"))
                .with_detail(format!("constraint {} on table {ct} depends on {what}", fk.name))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
        })
    }

    fn fk_checks_on(&self) -> bool {
        self.conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0)).map(|v| v != 0).unwrap_or(true)
    }

    // ---------------------------------------------------------------------------------------
    // ALTER TABLE
    // ---------------------------------------------------------------------------------------

    fn alter(&mut self, schema: &str, table: &str, op: AlterOp) -> Result<()> {
        let my = self.dialect == Dialect::MySql;
        let mut td = self.table_def(schema, table)?;
        let q = format!("{}.{}", quote_ident(schema), quote_ident(&td.name));
        let missing_col = |c: &str| {
            if my {
                EngineError::new(state::UNDEFINED_COLUMN, format!("Unknown column '{c}' in '{table}'")).with_mysql(1054)
            } else {
                EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{c}\" of relation \"{table}\" does not exist"))
            }
        };
        match op {
            AlterOp::Noop => return Ok(()),
            AlterOp::AddColumn { mut column, constraints, sequences, if_not_exists } => {
                if td.column(&column.name).is_some() {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(if my {
                        EngineError::new(state::DUPLICATE_COLUMN, format!("Duplicate column name '{}'", column.name)).with_mysql(1060)
                    } else {
                        EngineError::new(
                            state::DUPLICATE_COLUMN,
                            format!("column \"{}\" of relation \"{}\" already exists", column.name, td.name),
                        )
                    });
                }
                for s in &sequences {
                    self.create_sequence(s)?;
                }
                self.exec_mapped(&format!(
                    "ALTER TABLE {q} ADD COLUMN {} {}{}",
                    quote_ident(&column.name),
                    column.ty.storage().sqlite_decl(),
                    if my && column.ty.is_textual() { " COLLATE NOCASE" } else { "" }
                ))?;
                let rows = self.count(&format!("SELECT count(*) FROM {q}"))?;
                if let Some(d) = &column.default {
                    self.exec_mapped(&format!("UPDATE {q} SET {} = {}", quote_ident(&column.name), d.sql))?;
                } else if !column.nullable && rows > 0 {
                    if my {
                        let zero = match column.ty.storage() {
                            crate::types::Storage::Integer | crate::types::Storage::Real => "0",
                            _ => "''",
                        };
                        self.exec_mapped(&format!("UPDATE {q} SET {} = {zero}", quote_ident(&column.name)))?;
                    } else {
                        return Err(EngineError::new(
                            state::NOT_NULL_VIOLATION,
                            format!("column \"{}\" of relation \"{}\" contains null values", column.name, td.name),
                        ));
                    }
                }
                let is_pk = constraints.iter().any(|c| matches!(c, ConstraintDef::PrimaryKey(_)));
                if is_pk {
                    column.nullable = false;
                }
                td.columns.push(column);
                self.rewrite_definition(schema, &td)?;
                for c in constraints {
                    self.add_constraint(schema, &mut td, c)?;
                }
            }
            AlterOp::DropColumn { name, if_exists, cascade } => {
                let Some(col) = td.column(&name).cloned() else {
                    if if_exists {
                        return Ok(());
                    }
                    return Err(missing_col(&name));
                };
                if td.primary_key.as_ref().is_some_and(|pk| pk.columns.contains(&col.name)) {
                    let mut new = td.clone();
                    if !self.catalog().referencing(schema, &td.name).is_empty() && !cascade {
                        return Err(self
                            .dependents_error(
                                &format!("column {} of table {}", col.name, td.name),
                                schema,
                                &td.name,
                                Some(std::slice::from_ref(&col.name)),
                            )
                            .unwrap());
                    }
                    self.drop_referencing_fks(schema, &td.name, None)?;
                    new.primary_key = None;
                    self.strip_column(&mut new, &col.name);
                    self.rebuild_table(schema, &td, &new)?;
                    catalog::save_table(&self.conn, schema, &new)?;
                    self.drop_owned_sequence(schema, &col)?;
                    return Ok(());
                }
                if let Some(err) = self.dependents_error(
                    &format!("column {} of table {}", col.name, td.name),
                    schema,
                    &td.name,
                    Some(std::slice::from_ref(&col.name)),
                ) {
                    if !cascade && !my {
                        return Err(err);
                    }
                    self.drop_referencing_fks(schema, &td.name, Some(std::slice::from_ref(&col.name)))?;
                }
                // Remove indexes and constraints that use the column, then drop it natively.
                let doomed: Vec<String> = td
                    .uniques
                    .iter()
                    .filter(|u| u.columns.contains(&col.name))
                    .map(|u| u.name.clone())
                    .chain(td.indexes.iter().filter(|i| i.columns.contains(&col.name)).map(|i| i.name.clone()))
                    .collect();
                for idx in &doomed {
                    self.exec_mapped(&format!("DROP INDEX IF EXISTS {}.{}", quote_ident(schema), quote_ident(idx)))?;
                }
                let mut without_constraints = td.clone();
                self.strip_column_constraints(&mut without_constraints, &col.name);
                self.rewrite_definition(schema, &without_constraints)?;
                self.exec_mapped(&format!("ALTER TABLE {q} DROP COLUMN {}", quote_ident(&col.name)))?;
                td = without_constraints;
                td.columns.retain(|c| c.name != col.name);
                self.rewrite_definition(schema, &td)?;
                self.drop_owned_sequence(schema, &col)?;
            }
            AlterOp::RenameColumn { old, new } => {
                let col = td.column(&old).cloned().ok_or_else(|| missing_col(&old))?;
                if td.column(&new).is_some() {
                    return Err(EngineError::new(
                        state::DUPLICATE_COLUMN,
                        format!("column \"{new}\" of relation \"{}\" already exists", td.name),
                    )
                    .with_mysql(1060));
                }
                self.exec_mapped(&format!("ALTER TABLE {q} RENAME COLUMN {} TO {}", quote_ident(&col.name), quote_ident(&new)))?;
                rename_column_in_def(&mut td, &col.name, &new);
                self.rewrite_definition(schema, &td)?;
                let cat = self.catalog();
                for (cs, ct, fk) in cat.referencing(schema, table) {
                    let mut child = self.table_def(&cs, &ct)?;
                    for f in &mut child.foreign_keys {
                        if f.name == fk.name {
                            for rc in &mut f.ref_columns {
                                if *rc == col.name {
                                    *rc = new.clone();
                                }
                            }
                        }
                    }
                    catalog::save_table(&self.conn, &cs, &child)?;
                }
            }
            AlterOp::RenameTable { new } => {
                if self.catalog().table(schema, &new).is_some() || self.catalog().view(schema, &new).is_some() {
                    return Err(EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{new}\" already exists")).with_mysql(1050));
                }
                self.exec_mapped(&format!("ALTER TABLE {q} RENAME TO {}", quote_ident(&new)))?;
                catalog::delete_meta(&self.conn, schema, "table", &td.name)?;
                let old_name = td.name.clone();
                td.name = new.clone();
                for f in &mut td.foreign_keys {
                    if f.ref_schema == schema && f.ref_table == old_name {
                        f.ref_table = new.clone();
                    }
                }
                let cat = self.catalog();
                for (cs, ct, _) in cat.referencing(schema, &old_name) {
                    let mut child = self.table_def(&cs, &ct)?;
                    for f in &mut child.foreign_keys {
                        if f.ref_schema == schema && f.ref_table == old_name {
                            f.ref_table = new.clone();
                        }
                    }
                    catalog::save_table(&self.conn, &cs, &child)?;
                }
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::SetNotNull(c) => {
                let col = td.column(&c).cloned().ok_or_else(|| missing_col(&c))?;
                let nulls = self.count(&format!("SELECT count(*) FROM {q} WHERE {} IS NULL", quote_ident(&col.name)))?;
                if nulls > 0 {
                    return Err(EngineError::new(
                        state::NOT_NULL_VIOLATION,
                        format!("column \"{}\" of relation \"{}\" contains null values", col.name, td.name),
                    )
                    .with_mysql(1138));
                }
                td.column_mut(&c).unwrap().nullable = false;
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::DropNotNull(c) => {
                let col = td.column(&c).cloned().ok_or_else(|| missing_col(&c))?;
                if td.primary_key.as_ref().is_some_and(|pk| pk.columns.contains(&col.name)) {
                    return Err(EngineError::new("42P16", format!("column \"{}\" is in a primary key", col.name)));
                }
                td.column_mut(&c).unwrap().nullable = true;
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::SetDefault(c, d) => {
                td.column_mut(&c).ok_or_else(|| missing_col(&c))?.default = Some(d);
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::DropDefault(c) => {
                let col = td.column_mut(&c).ok_or_else(|| missing_col(&c))?;
                col.default = None;
                if matches!(col.identity, Some(Identity::Serial { .. })) {
                    col.identity = None;
                }
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::SetType { column, ty, type_text, using } => {
                let col = td.column(&column).cloned().ok_or_else(|| missing_col(&column))?;
                {
                    let c = td.column_mut(&column).unwrap();
                    c.ty = ty.clone();
                    c.type_text = type_text;
                }
                self.rewrite_definition(schema, &td)?;
                let source = using.unwrap_or_else(|| quote_ident(&col.name));
                let spec = match self.dialect {
                    Dialect::MySql => ty.mysql_column_type(),
                    _ => ty.pg_format_type(),
                };
                self.exec_mapped(&format!("UPDATE {q} SET {} = tb_cast({source}, {})", quote_ident(&col.name), quote_literal(&spec)))?;
            }
            AlterOp::AddConstraint(c) => self.add_constraint(schema, &mut td, c)?,
            AlterOp::DropConstraint { name, if_exists, cascade } => {
                let lower = name.to_lowercase();
                if td.primary_key.as_ref().is_some_and(|p| p.name.to_lowercase() == lower) || (my && lower == "primary") {
                    return self.alter(schema, table, AlterOp::DropPrimaryKey);
                }
                if let Some(pos) = td.uniques.iter().position(|u| u.name.to_lowercase() == lower) {
                    let cols = td.uniques[pos].columns.clone();
                    let referenced = self.catalog().referencing(schema, &td.name).into_iter().any(|(_, _, fk)| fk.ref_columns == cols);
                    if referenced {
                        if !cascade {
                            return Err(self
                                .dependents_error(&format!("constraint {name} on table {}", td.name), schema, &td.name, Some(&cols))
                                .unwrap());
                        }
                        self.drop_referencing_fks(schema, &td.name, Some(&cols))?;
                    }
                    self.exec_mapped(&format!("DROP INDEX {}.{}", quote_ident(schema), quote_ident(&td.uniques[pos].name)))?;
                    td.uniques.remove(pos);
                } else if let Some(pos) = td.indexes.iter().position(|i| i.name.to_lowercase() == lower) {
                    self.exec_mapped(&format!("DROP INDEX {}.{}", quote_ident(schema), quote_ident(&td.indexes[pos].name)))?;
                    td.indexes.remove(pos);
                } else if let Some(pos) = td.foreign_keys.iter().position(|f| f.name.to_lowercase() == lower) {
                    td.foreign_keys.remove(pos);
                    self.rewrite_definition(schema, &td)?;
                } else if let Some(pos) = td.checks.iter().position(|c| c.name.to_lowercase() == lower) {
                    td.checks.remove(pos);
                    self.rewrite_definition(schema, &td)?;
                } else if let Some(col) = td
                    .columns
                    .iter()
                    .find(|c| c.default_constraint_name(&td.name).is_some_and(|n| n.to_lowercase() == lower))
                    .map(|c| c.name.clone())
                {
                    // SQL Server default constraints.
                    return self.alter(schema, table, AlterOp::DropDefault(col));
                } else if !if_exists {
                    return Err(if my {
                        EngineError::new(state::UNDEFINED_OBJECT, format!("Can't DROP '{name}'; check that column/key exists"))
                            .with_mysql(1091)
                    } else {
                        EngineError::new(
                            state::UNDEFINED_OBJECT,
                            format!("constraint \"{name}\" of relation \"{}\" does not exist", td.name),
                        )
                    });
                }
            }
            AlterOp::DropPrimaryKey => {
                if td.primary_key.is_none() {
                    return Err(
                        EngineError::new(state::UNDEFINED_OBJECT, "Can't DROP 'PRIMARY'; check that column/key exists").with_mysql(1091)
                    );
                }
                if let Some(err) = self.dependents_error(
                    &format!("constraint on table {}", td.name),
                    schema,
                    &td.name,
                    td.primary_key.as_ref().map(|p| p.columns.as_slice()),
                ) {
                    return Err(err);
                }
                let mut new = td.clone();
                new.primary_key = None;
                self.rebuild_table(schema, &td, &new)?;
                td = new;
            }
            AlterOp::RenameConstraint { old, new } => {
                let mut found = false;
                if let Some(pk) = td.primary_key.as_mut().filter(|p| p.name == old) {
                    pk.name = new.clone();
                    found = true;
                }
                for f in td.foreign_keys.iter_mut().filter(|f| f.name == old) {
                    f.name = new.clone();
                    found = true;
                }
                for c in td.checks.iter_mut().filter(|c| c.name == old) {
                    c.name = new.clone();
                    found = true;
                }
                if let Some(pos) = td.uniques.iter().position(|u| u.name == old) {
                    self.exec_mapped(&format!("DROP INDEX {}.{}", quote_ident(schema), quote_ident(&old)))?;
                    td.uniques[pos].name = new.clone();
                    self.exec_mapped(&render_create_index(schema, &td.name, &unique_index(&td.uniques[pos])))?;
                    found = true;
                }
                if !found {
                    return Err(EngineError::new(
                        state::UNDEFINED_OBJECT,
                        format!("constraint \"{old}\" for table \"{}\" does not exist", td.name),
                    ));
                }
                self.rewrite_definition(schema, &td)?;
            }
            AlterOp::ReplaceColumn { old, column, constraints, sequences } => {
                let existing = td.column(&old).cloned().ok_or_else(|| missing_col(&old))?;
                if existing.name != column.name {
                    self.exec_mapped(&format!(
                        "ALTER TABLE {q} RENAME COLUMN {} TO {}",
                        quote_ident(&existing.name),
                        quote_ident(&column.name)
                    ))?;
                    rename_column_in_def(&mut td, &existing.name, &column.name);
                }
                for s in &sequences {
                    self.create_sequence(s)?;
                }
                if !column.nullable {
                    let nulls = self.count(&format!("SELECT count(*) FROM {q} WHERE {} IS NULL", quote_ident(&column.name)))?;
                    if nulls > 0 {
                        return Err(EngineError::new("22004", "Invalid use of NULL value").with_mysql(1138));
                    }
                }
                let mut column = column;
                if let (Some(Identity::AutoIncrement { .. }), Some(Identity::AutoIncrement { sequence })) =
                    (&existing.identity, &column.identity)
                {
                    let _ = sequence;
                }
                let is_pk_col = td.primary_key.as_ref().is_some_and(|p| p.columns.contains(&column.name));
                if is_pk_col {
                    column.nullable = false;
                }
                let spec = column.ty.mysql_column_type();
                let name = column.name.clone();
                if let Some(slot) = td.column_mut(&name) {
                    *slot = column;
                }
                self.rewrite_definition(schema, &td)?;
                self.exec_mapped(&format!(
                    "UPDATE {q} SET {} = tb_cast({}, {})",
                    quote_ident(&name),
                    quote_ident(&name),
                    quote_literal(&spec)
                ))?;
                if let Some(c) = td.column(&name)
                    && let Some(Identity::AutoIncrement { sequence }) = &c.identity
                {
                    let max: Option<i64> = self
                        .conn
                        .query_row(&format!("SELECT max({}) FROM {q}", quote_ident(&name)), [], |r| r.get(0))
                        .map_err(sqlite_err)?;
                    if let Some(m) = max {
                        self.engine.inner.sequences.bump_to(&format!("{schema}.{sequence}"), m);
                    }
                }
                for c in constraints {
                    if matches!(c, ConstraintDef::PrimaryKey(_)) && td.primary_key.as_ref().is_some_and(|p| p.columns == vec![name.clone()])
                    {
                        continue;
                    }
                    self.add_constraint(schema, &mut td, c)?;
                }
            }
            AlterOp::SetAutoIncrement(n) => {
                let seq = td.columns.iter().find_map(|c| match &c.identity {
                    Some(Identity::AutoIncrement { sequence }) => Some(sequence.clone()),
                    _ => None,
                });
                if let Some(s) = seq {
                    let key = format!("{schema}.{s}");
                    let max: Option<i64> = self
                        .conn
                        .query_row(
                            &format!(
                                "SELECT max({}) FROM {q}",
                                quote_ident(
                                    &td.columns.iter().find(|c| matches!(c.identity, Some(Identity::AutoIncrement { .. }))).unwrap().name
                                )
                            ),
                            [],
                            |r| r.get(0),
                        )
                        .map_err(sqlite_err)?;
                    let start = n.max(max.map(|m| m + 1).unwrap_or(1));
                    self.engine.inner.sequences.setval(&key, start, false)?;
                }
            }
            AlterOp::Comment(text) => td.comment = text,
        }
        catalog::save_table(&self.conn, schema, &td)
    }

    fn drop_owned_sequence(&mut self, schema: &str, col: &catalog::ColumnDef) -> Result<()> {
        if let Some(id) = &col.identity {
            self.drop_sequence(schema, id.sequence())?;
        }
        Ok(())
    }

    fn strip_column_constraints(&self, td: &mut TableDef, col: &str) {
        td.uniques.retain(|u| !u.columns.iter().any(|c| c == col));
        td.indexes.retain(|i| !i.columns.iter().any(|c| c == col));
        td.foreign_keys.retain(|f| !f.columns.iter().any(|c| c == col));
        td.checks.retain(|c| !mentions(&c.sql, col));
    }

    fn strip_column(&self, td: &mut TableDef, col: &str) {
        self.strip_column_constraints(td, col);
        td.columns.retain(|c| c.name != col);
    }

    // ---------------------------------------------------------------------------------------
    // DROP / TRUNCATE
    // ---------------------------------------------------------------------------------------

    fn drop_objects(&mut self, kind: DropKind, names: Vec<(String, String)>, if_exists: bool, cascade: bool) -> Result<()> {
        let my = self.dialect == Dialect::MySql;
        match kind {
            DropKind::Table => {
                let cat = self.catalog();
                let mut targets = Vec::new();
                for (s, t) in &names {
                    match cat.table(s, t) {
                        Some(td) => targets.push((s.clone(), td.name.clone())),
                        None if if_exists => {}
                        None => {
                            return Err(if my {
                                EngineError::new(state::UNDEFINED_TABLE, format!("Unknown table '{s}.{t}'")).with_mysql(1051)
                            } else {
                                EngineError::new(state::UNDEFINED_TABLE, format!("table \"{t}\" does not exist"))
                            });
                        }
                    }
                }
                // Children first, so dropping a parent never runs foreign-key actions.
                targets.sort_by_key(|(s, t)| std::cmp::Reverse(cat.referencing(s, t).len()));
                targets.reverse();
                for (s, t) in &targets {
                    let outside: Vec<_> = cat
                        .referencing(s, t)
                        .into_iter()
                        .filter(|(cs, ct, _)| !targets.iter().any(|(ts, tt)| ts == cs && tt == ct))
                        .collect();
                    if !outside.is_empty() {
                        if my && !self.fk_checks_on() {
                            // MySQL with FOREIGN_KEY_CHECKS=0 keeps the dangling definition.
                        } else if !cascade {
                            return Err(self.dependents_error(&format!("table {t}"), s, t, None).unwrap());
                        } else {
                            self.drop_referencing_fks(s, t, None)?;
                        }
                    }
                }
                for (s, t) in targets {
                    let td = self.table_def(&s, &t)?;
                    // Remove self-references so the implicit delete cannot cascade anywhere.
                    if td.foreign_keys.iter().any(|f| f.ref_table == td.name && f.ref_schema == s) {
                        let mut stripped = td.clone();
                        stripped.foreign_keys.retain(|f| !(f.ref_table == td.name && f.ref_schema == s));
                        self.rewrite_definition(&s, &stripped)?;
                    }
                    self.exec_mapped(&format!("DROP TABLE {}.{}", quote_ident(&s), quote_ident(&t)))?;
                    catalog::delete_meta(&self.conn, &s, "table", &t)?;
                    for c in &td.columns {
                        self.drop_owned_sequence(&s, c)?;
                    }
                }
            }
            DropKind::View => {
                for (s, v) in names {
                    if self.catalog().view(&s, &v).is_none() {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::new(state::UNDEFINED_TABLE, format!("view \"{v}\" does not exist")).with_mysql(1051));
                    }
                    self.exec_mapped(&format!("DROP VIEW {}.{}", quote_ident(&s), quote_ident(&v)))?;
                    catalog::delete_meta(&self.conn, &s, "view", &v)?;
                }
            }
            DropKind::Index => {
                for (s, idx) in names {
                    let cat = self.catalog();
                    let owner = cat.schema(&s).and_then(|sd| {
                        sd.tables.values().find(|t| {
                            t.indexes.iter().any(|i| i.name == idx)
                                || t.uniques.iter().any(|u| u.name == idx)
                                || t.primary_key.as_ref().is_some_and(|p| p.name == idx)
                        })
                    });
                    let Some(owner) = owner.cloned() else {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::new(state::UNDEFINED_OBJECT, format!("index \"{idx}\" does not exist")).with_mysql(1091));
                    };
                    if !my && (owner.uniques.iter().any(|u| u.name == idx) || owner.primary_key.as_ref().is_some_and(|p| p.name == idx)) {
                        return Err(EngineError::new(
                            state::DEPENDENT_OBJECTS_STILL_EXIST,
                            format!("cannot drop index {idx} because constraint {idx} on table {} requires it", owner.name),
                        )
                        .with_hint(format!("You can drop constraint {idx} on table {} instead.", owner.name)));
                    }
                    self.alter(&s, &owner.name.clone(), AlterOp::DropConstraint { name: idx, if_exists, cascade })?;
                }
            }
            DropKind::Sequence => {
                for (s, n) in names {
                    if !self.catalog().schema(&s).is_some_and(|sd| sd.sequences.contains_key(&n)) {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::new(state::UNDEFINED_TABLE, format!("sequence \"{n}\" does not exist")));
                    }
                    self.drop_sequence(&s, &n)?;
                }
            }
            DropKind::Type => {
                for (s, n) in names {
                    let cat = self.catalog();
                    if !cat.schema(&s).is_some_and(|sd| sd.enums.contains_key(&n)) {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::new(state::UNDEFINED_OBJECT, format!("type \"{n}\" does not exist")));
                    }
                    let used = cat
                        .schemas
                        .values()
                        .flat_map(|sd| sd.tables.values())
                        .find(|t| t.columns.iter().any(|c| matches!(&c.ty, SqlType::Enum { name, .. } if *name == n)));
                    if let Some(t) = used {
                        return Err(EngineError::new(
                            state::DEPENDENT_OBJECTS_STILL_EXIST,
                            format!("cannot drop type {n} because other objects depend on it"),
                        )
                        .with_detail(format!("column of table {} depends on type {n}", t.name)));
                    }
                    catalog::delete_meta(&self.conn, &s, "enum", &n)?;
                }
            }
            DropKind::Schema | DropKind::Database => {}
        }
        Ok(())
    }

    fn truncate(&mut self, tables: Vec<(String, String)>, restart: bool, cascade: bool) -> Result<()> {
        let my = self.dialect == Dialect::MySql;
        let cat = self.catalog();
        let mut all = tables.clone();
        let mut i = 0;
        while i < all.len() {
            let (s, t) = all[i].clone();
            for (cs, ct, _) in cat.referencing(&s, &t) {
                if all.iter().any(|(a, b)| *a == cs && *b == ct) {
                    continue;
                }
                if my {
                    if self.fk_checks_on() {
                        return Err(EngineError::new(
                            "42000",
                            format!("Cannot truncate a table referenced in a foreign key constraint (`{cs}`.`{ct}`)"),
                        )
                        .with_mysql(1701));
                    }
                    continue;
                }
                if !cascade {
                    return Err(EngineError::unsupported("cannot truncate a table referenced in a foreign key constraint")
                        .with_detail(format!("Table \"{ct}\" references \"{t}\"."))
                        .with_hint(format!("Truncate table \"{ct}\" at the same time, or use TRUNCATE ... CASCADE.")));
                }
                all.push((cs, ct));
            }
            i += 1;
        }
        self.exec("PRAGMA defer_foreign_keys = ON").map_err(sqlite_err)?;
        for (s, t) in &all {
            self.exec_mapped(&format!("DELETE FROM {}.{}", quote_ident(s), quote_ident(t)))?;
            if (restart || my)
                && let Some(td) = cat.table(s, t)
            {
                for c in &td.columns {
                    if let Some(id) = &c.identity {
                        let key = format!("{s}.{}", id.sequence());
                        if let Some(st) = self.engine.inner.sequences.get(&key) {
                            self.engine.inner.sequences.setval(&key, st.start_value, false)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn mentions(sql: &str, col: &str) -> bool {
    let lower = sql.to_lowercase();
    let col = col.to_lowercase();
    lower.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == col)
}

fn rename_column_in_def(td: &mut TableDef, old: &str, new: &str) {
    if let Some(c) = td.column_mut(old) {
        c.name = new.to_owned();
    }
    let fix = |cols: &mut Vec<String>| {
        for c in cols.iter_mut() {
            if c == old {
                *c = new.to_owned();
            }
        }
    };
    if let Some(pk) = td.primary_key.as_mut() {
        fix(&mut pk.columns);
    }
    for u in &mut td.uniques {
        fix(&mut u.columns);
    }
    for i in &mut td.indexes {
        fix(&mut i.columns);
    }
    for f in &mut td.foreign_keys {
        fix(&mut f.columns);
        if f.ref_table == td.name {
            fix(&mut f.ref_columns);
        }
    }
    for c in &mut td.checks {
        c.sql = replace_word(&c.sql, old, new);
        c.display = replace_word(&c.display, old, new);
    }
}

fn replace_word(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.eq_ignore_ascii_case(old) {
            out.push_str(new);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            out.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn unique_build_error(e: rusqlite::Error, name: &str, dialect: Dialect) -> EngineError {
    let msg = e.to_string();
    if msg.contains("UNIQUE constraint failed") {
        if dialect == Dialect::MySql {
            EngineError::new(state::UNIQUE_VIOLATION, format!("Duplicate entry for key '{name}'")).with_mysql(1062)
        } else {
            EngineError::new(state::UNIQUE_VIOLATION, format!("could not create unique index \"{name}\""))
        }
    } else {
        sqlite_err(e)
    }
}

#[allow(dead_code)]
fn _unused(_: &KeyDef, _: &IndexDef, _: &CheckDef) {}
