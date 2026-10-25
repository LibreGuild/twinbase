//! Translation of PostgreSQL / MySQL statements into execution plans over SQLite.
//!
//! * Utility statements (`SET`, `SHOW`, `BEGIN`, `USE`, `GRANT`, …) are recognised by a small
//!   keyword pre-parser ([`utility`]), because clients send many variants of them.
//! * Everything else is parsed with `sqlparser`, rewritten ([`expr::Rewriter`]) and rendered as
//!   SQLite SQL, or turned into a structured DDL plan ([`ddl`]) the session executes itself.

pub mod ddl;
pub mod expr;
mod utility;

use std::collections::{BTreeMap, HashMap};

use rusqlite::types::Value;
use sqlparser::ast::{self, Statement};
use sqlparser::dialect::{MsSqlDialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

use crate::Dialect;
use crate::catalog::{Catalog, CheckDef, ColumnDef, DefaultExpr, ForeignKeyDef, IndexDef, KeyDef, TableDef};
use crate::error::{EngineError, Result, state};
use crate::types::SqlType;

pub use utility::split_statements;

/// What the translator needs to know about the session.
pub struct Ctx<'a> {
    pub dialect: Dialect,
    pub catalog: &'a Catalog,
    /// PostgreSQL: existing schemas of the search path. MySQL: the current database (or empty).
    pub search_path: &'a [String],
    pub settings: &'a HashMap<String, String>,
}

impl Ctx<'_> {
    /// Schema where new objects are created.
    pub fn creation_schema(&self) -> Result<String> {
        match self.search_path.first() {
            Some(s) => Ok(s.clone()),
            None if self.dialect == Dialect::MySql => {
                Err(EngineError::new(state::INVALID_CATALOG_NAME, "No database selected").with_mysql(1046))
            }
            None => Err(EngineError::new(state::UNDEFINED_SCHEMA, "no schema has been selected to create in")),
        }
    }

    pub fn fold(&self, ident: &ast::Ident) -> String {
        if self.dialect == Dialect::Postgres && ident.quote_style.is_none() { ident.value.to_lowercase() } else { ident.value.clone() }
    }

    /// Canonical schema name (MySQL database names are matched case-insensitively).
    pub fn schema_name(&self, raw: &str) -> String {
        if let Some(sd) = self.catalog.schema(raw) {
            return sd.name.clone();
        }
        raw.to_owned()
    }

    /// Resolve an object name to (schema, name). Unqualified names use the search path.
    pub fn resolve_name(&self, name: &ast::ObjectName, must_exist: bool) -> Result<(String, String)> {
        let parts: Vec<String> = name
            .0
            .iter()
            .filter_map(|p| match p {
                ast::ObjectNamePart::Identifier(i) => Some(self.fold(i)),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [table] => {
                if let Some((schema, _)) = self.catalog.resolve_relation(table, self.search_path) {
                    let real = self
                        .catalog
                        .table(&schema, table)
                        .map(|t| t.name.clone())
                        .or_else(|| self.catalog.view(&schema, table).map(|v| v.name.clone()))
                        .unwrap_or_else(|| table.clone());
                    return Ok((schema, real));
                }
                if must_exist {
                    return Err(self.undefined_table(None, table));
                }
                Ok((self.creation_schema()?, table.clone()))
            }
            [.., schema, table] => {
                let schema = self.schema_name(schema);
                if self.catalog.schema(&schema).is_none() && schema != "pg_catalog" && schema != "information_schema" {
                    return Err(if self.dialect == Dialect::MySql {
                        EngineError::new(state::INVALID_CATALOG_NAME, format!("Unknown database '{schema}'")).with_mysql(1049)
                    } else if must_exist {
                        self.undefined_table(Some(&schema), table)
                    } else {
                        EngineError::new(state::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
                    });
                }
                let real = self
                    .catalog
                    .table(&schema, table)
                    .map(|t| t.name.clone())
                    .or_else(|| self.catalog.view(&schema, table).map(|v| v.name.clone()));
                match real {
                    Some(r) => Ok((schema, r)),
                    None if must_exist => Err(self.undefined_table(Some(&schema), table)),
                    None => Ok((schema, table.clone())),
                }
            }
            [] => Err(EngineError::syntax("empty object name")),
        }
    }

    pub fn undefined_table(&self, schema: Option<&str>, table: &str) -> EngineError {
        if self.dialect == Dialect::MySql {
            let db = schema.map(str::to_owned).or_else(|| self.search_path.first().cloned()).unwrap_or_default();
            EngineError::new(state::UNDEFINED_TABLE, format!("Table '{db}.{table}' doesn't exist")).with_mysql(1146)
        } else {
            let shown = match schema {
                Some(s) => format!("{s}.{table}"),
                None => table.to_owned(),
            };
            EngineError::new(state::UNDEFINED_TABLE, format!("relation \"{shown}\" does not exist"))
        }
    }

    /// Type text understood by `tb_cast` for a column type.
    pub fn type_spec(&self, ty: &SqlType) -> String {
        match self.dialect {
            Dialect::MySql => ty.mysql_column_type(),
            _ => ty.pg_format_type(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    Select,
    Insert,
    Update,
    Delete,
    Other,
}

/// A statement that is executed as SQLite SQL.
#[derive(Debug, Clone)]
pub struct SqlPlan {
    pub sql: String,
    pub kind: StmtKind,
    /// Command tag base (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, …).
    pub tag: String,
    /// Output column types inferred statically (None = decide at run time).
    pub projection: Vec<Option<SqlType>>,
    /// Inferred parameter types by 1-based position.
    pub params: BTreeMap<usize, SqlType>,
    pub param_count: usize,
    pub needs_pg_catalog: bool,
    pub needs_info_schema: bool,
    /// Target table of INSERT/UPDATE/DELETE.
    pub target: Option<(String, String)>,
    /// MySQL AUTO_INCREMENT to bump after an INSERT: (sequence key, schema, table, column).
    pub autoinc: Option<(String, String, String, String)>,
}

impl SqlPlan {
    pub fn new(sql: String, kind: StmtKind, tag: &str) -> Self {
        Self {
            sql,
            kind,
            tag: tag.to_owned(),
            projection: Vec::new(),
            params: BTreeMap::new(),
            param_count: 0,
            needs_pg_catalog: false,
            needs_info_schema: false,
            target: None,
            autoinc: None,
        }
    }
}

/// A new sequence created alongside a table (SERIAL / IDENTITY / AUTO_INCREMENT).
#[derive(Debug, Clone)]
pub struct NewSequence {
    pub schema: String,
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub owner: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ConstraintDef {
    PrimaryKey(KeyDef),
    Unique(KeyDef),
    ForeignKey(ForeignKeyDef),
    Check(CheckDef),
    Index(IndexDef),
}

#[derive(Debug, Clone)]
pub enum AlterOp {
    AddColumn {
        column: ColumnDef,
        constraints: Vec<ConstraintDef>,
        sequences: Vec<NewSequence>,
        if_not_exists: bool,
    },
    DropColumn {
        name: String,
        if_exists: bool,
        cascade: bool,
    },
    RenameColumn {
        old: String,
        new: String,
    },
    RenameTable {
        new: String,
    },
    SetNotNull(String),
    DropNotNull(String),
    SetDefault(String, DefaultExpr),
    DropDefault(String),
    SetType {
        column: String,
        ty: SqlType,
        type_text: String,
        using: Option<String>,
    },
    AddConstraint(ConstraintDef),
    DropConstraint {
        name: String,
        if_exists: bool,
        cascade: bool,
    },
    DropPrimaryKey,
    RenameConstraint {
        old: String,
        new: String,
    },
    /// MySQL `CHANGE`/`MODIFY`: replace a column definition.
    ReplaceColumn {
        old: String,
        column: ColumnDef,
        constraints: Vec<ConstraintDef>,
        sequences: Vec<NewSequence>,
    },
    SetAutoIncrement(i64),
    Comment(Option<String>),
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropKind {
    Table,
    View,
    Index,
    Sequence,
    Type,
    Schema,
    Database,
}

#[derive(Debug, Clone)]
pub enum CommentTarget {
    Table(String, String),
    Column(String, String, String),
    Other,
}

/// A synthesized result set (SHOW, DESCRIBE, …).
#[derive(Debug, Clone)]
pub struct Rows {
    pub columns: Vec<(String, SqlType)>,
    pub rows: Vec<Vec<Value>>,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub enum Plan {
    Sql(SqlPlan),
    /// Raw statement passed through unchanged after a parse failure (tried as a last resort).
    Raw {
        sql: String,
        parse_error: EngineError,
    },
    CreateTable {
        schema: String,
        def: TableDef,
        if_not_exists: bool,
        sequences: Vec<NewSequence>,
        as_query: Option<SqlPlan>,
    },
    AlterTable {
        schema: String,
        table: String,
        ops: Vec<AlterOp>,
        if_exists: bool,
    },
    Drop {
        kind: DropKind,
        names: Vec<(String, String)>,
        if_exists: bool,
        cascade: bool,
    },
    CreateIndex {
        schema: String,
        table: String,
        def: IndexDef,
        if_not_exists: bool,
    },
    CreateView {
        schema: String,
        name: String,
        definition: String,
        plan: SqlPlan,
        columns: Vec<String>,
        or_replace: bool,
        if_not_exists: bool,
    },
    CreateSchema {
        name: String,
        if_not_exists: bool,
    },
    CreateDatabase {
        name: String,
        if_not_exists: bool,
    },
    CreateEnum {
        schema: String,
        name: String,
        values: Vec<String>,
    },
    AlterEnumAddValue {
        schema: String,
        name: String,
        value: String,
        if_not_exists: bool,
    },
    CreateSequence {
        seq: NewSequence,
        if_not_exists: bool,
    },
    Truncate {
        tables: Vec<(String, String)>,
        restart_identity: bool,
        cascade: bool,
    },
    Comment {
        target: CommentTarget,
        text: Option<String>,
    },
    /// `read_only`: `BEGIN READ ONLY` / `START TRANSACTION READ ONLY`.
    Begin {
        read_only: bool,
    },
    Commit,
    Rollback,
    Savepoint(String),
    Release(String),
    RollbackTo(String),
    /// `SET name = value, ...` (a None value resets to the default).
    Set {
        vars: Vec<(String, Option<String>)>,
    },
    Show {
        name: String,
    },
    Use {
        database: String,
    },
    Explain(SqlPlan),
    Rows(Rows),
    Noop {
        tag: String,
    },
}

impl Plan {
    pub fn is_transaction_control(&self) -> bool {
        matches!(self, Plan::Begin { .. } | Plan::Commit | Plan::Rollback | Plan::Savepoint(_) | Plan::Release(_) | Plan::RollbackTo(_))
    }
}

/// Translate one statement (already split from a batch).
pub fn translate(sql: &str, ctx: &Ctx<'_>) -> Result<Plan> {
    if let Some(plan) = utility::preparse(sql, ctx)? {
        return Ok(plan);
    }
    let tsql = (ctx.dialect == Dialect::MsSql).then(|| utility::tsql_alter_column(sql)).flatten();
    let (sql, default_name) = match &tsql {
        Some((s, n)) => (s.as_str(), n.clone()),
        None => (sql, None),
    };
    let statements = match ctx.dialect {
        Dialect::MySql => Parser::parse_sql(&MySqlDialect {}, sql),
        Dialect::MsSql => Parser::parse_sql(&MsSqlDialect {}, sql),
        Dialect::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, sql),
    };
    let mut statements = match statements {
        Ok(s) => s,
        Err(e) => {
            let message = e.to_string();
            let message = message.strip_prefix("sql parser error: ").unwrap_or(&message).to_owned();
            let err = if ctx.dialect == Dialect::MsSql {
                EngineError::syntax(format!("Incorrect syntax: {message}"))
            } else if ctx.dialect == Dialect::MySql {
                EngineError::syntax(format!(
                    "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use: {message}"
                ))
            } else {
                EngineError::syntax(format!("syntax error: {message}"))
            };
            return Ok(Plan::Raw { sql: sql.to_owned(), parse_error: err });
        }
    };
    match statements.len() {
        0 => Ok(Plan::Noop { tag: String::new() }),
        1 => {
            let mut plan = translate_statement(statements.remove(0), ctx)?;
            // T-SQL `ADD CONSTRAINT name DEFAULT … FOR col` keeps its name.
            if let (Some(n), Plan::AlterTable { ops, .. }) = (default_name, &mut plan) {
                for op in ops {
                    if let AlterOp::SetDefault(_, d) = op {
                        d.name = Some(n.clone());
                    }
                }
            }
            Ok(plan)
        }
        _ => Err(EngineError::syntax("cannot translate multiple statements at once")),
    }
}

fn translate_statement(stmt: Statement, ctx: &Ctx<'_>) -> Result<Plan> {
    let stmt = if ctx.dialect == Dialect::MsSql { expr::tsql_output_to_returning(stmt)? } else { stmt };
    match stmt {
        Statement::Query(_) => expr::translate_query(stmt, ctx).map(Plan::Sql),
        Statement::Insert(ins) => expr::translate_insert(ins, ctx).map(Plan::Sql),
        Statement::Update(upd) => expr::translate_update(upd, ctx).map(Plan::Sql),
        Statement::Delete(del) => expr::translate_delete(del, ctx).map(Plan::Sql),
        Statement::Explain { statement, .. } => {
            let inner = translate_statement(*statement, ctx)?;
            match inner {
                Plan::Sql(p) => Ok(Plan::Explain(p)),
                _ => Ok(Plan::Rows(Rows {
                    columns: vec![("QUERY PLAN".into(), SqlType::Text)],
                    rows: vec![vec![Value::Text("Utility statement".into())]],
                    tag: "EXPLAIN".into(),
                })),
            }
        }
        Statement::CreateTable(ct) => ddl::translate_create_table(ct, ctx),
        Statement::AlterTable(at) => ddl::translate_alter_table(at, ctx),
        Statement::CreateIndex(ci) => ddl::translate_create_index(ci, ctx),
        Statement::CreateView(cv) => ddl::translate_create_view(cv, ctx),
        Statement::Drop { object_type, if_exists, names, cascade, .. } => ddl::translate_drop(object_type, if_exists, names, cascade, ctx),
        Statement::CreateSchema { schema_name, if_not_exists, .. } => {
            let name = match schema_name {
                ast::SchemaName::Simple(n) | ast::SchemaName::NamedAuthorization(n, _) => object_last(&n, ctx),
                ast::SchemaName::UnnamedAuthorization(i) => ctx.fold(&i),
            };
            if ctx.dialect == Dialect::MySql {
                Ok(Plan::CreateDatabase { name, if_not_exists })
            } else {
                Ok(Plan::CreateSchema { name, if_not_exists })
            }
        }
        Statement::CreateDatabase { db_name, if_not_exists, .. } => {
            let name = object_last(&db_name, ctx);
            Ok(Plan::CreateDatabase { name, if_not_exists })
        }
        Statement::CreateType { name, representation } => match representation {
            Some(ast::UserDefinedTypeRepresentation::Enum { labels }) => {
                let (schema, n) = ctx.resolve_name(&name, false)?;
                Ok(Plan::CreateEnum { schema, name: n, values: labels.into_iter().map(|l| l.value).collect() })
            }
            _ => Err(EngineError::unsupported("only CREATE TYPE ... AS ENUM is supported by the TwinLite engine")),
        },
        Statement::AlterType(at) => ddl::translate_alter_type(at, ctx),
        Statement::CreateSequence { if_not_exists, name, sequence_options, owned_by, .. } => {
            let (schema, n) = ctx.resolve_name(&name, false)?;
            let mut seq = NewSequence { schema, name: n, start: 1, increment: 1, owner: owned_by.map(|o| o.to_string()) };
            let mut start_set = false;
            for opt in sequence_options {
                match opt {
                    ast::SequenceOptions::IncrementBy(e, _) => seq.increment = expr::const_int(&e).unwrap_or(1),
                    ast::SequenceOptions::StartWith(e, _) => {
                        seq.start = expr::const_int(&e).unwrap_or(1);
                        start_set = true;
                    }
                    _ => {}
                }
            }
            if !start_set && seq.increment < 0 {
                seq.start = -1;
            }
            Ok(Plan::CreateSequence { seq, if_not_exists })
        }
        Statement::Truncate(tr) => {
            let mut tables = Vec::new();
            for t in &tr.table_names {
                tables.push(ctx.resolve_name(&t.name, true)?);
            }
            Ok(Plan::Truncate {
                tables,
                restart_identity: matches!(tr.identity, Some(ast::TruncateIdentityOption::Restart)),
                cascade: matches!(tr.cascade, Some(ast::CascadeOption::Cascade)),
            })
        }
        Statement::Comment { object_type, object_name, comment, .. } => {
            let target = match object_type {
                ast::CommentObject::Table => {
                    let (s, t) = ctx.resolve_name(&object_name, true)?;
                    CommentTarget::Table(s, t)
                }
                ast::CommentObject::Column => {
                    let mut parts = object_name.0.clone();
                    let col = match parts.pop() {
                        Some(ast::ObjectNamePart::Identifier(i)) => ctx.fold(&i),
                        _ => return Err(EngineError::syntax("invalid column name")),
                    };
                    let (s, t) = ctx.resolve_name(&ast::ObjectName(parts), true)?;
                    CommentTarget::Column(s, t, col)
                }
                _ => CommentTarget::Other,
            };
            Ok(Plan::Comment { target, text: comment })
        }
        Statement::StartTransaction { modes, .. } => Ok(Plan::Begin {
            read_only: modes.iter().any(|m| matches!(m, ast::TransactionMode::AccessMode(ast::TransactionAccessMode::ReadOnly))),
        }),
        Statement::Commit { .. } => Ok(Plan::Commit),
        Statement::Rollback { savepoint: Some(name), .. } => Ok(Plan::RollbackTo(ctx.fold(&name))),
        Statement::Rollback { .. } => Ok(Plan::Rollback),
        Statement::Savepoint { name } => Ok(Plan::Savepoint(ctx.fold(&name))),
        Statement::ReleaseSavepoint { name } => Ok(Plan::Release(ctx.fold(&name))),
        Statement::CreateExtension(_) => Ok(Plan::Noop { tag: "CREATE EXTENSION".into() }),
        Statement::DropExtension(_) => Ok(Plan::Noop { tag: "DROP EXTENSION".into() }),
        Statement::Grant(_) => Ok(Plan::Noop { tag: "GRANT".into() }),
        Statement::Revoke(_) => Ok(Plan::Noop { tag: "REVOKE".into() }),
        Statement::CreateRole(_) => Ok(Plan::Noop { tag: "CREATE ROLE".into() }),
        Statement::CreateFunction(_) | Statement::CreateTrigger(_) | Statement::CreateProcedure { .. } => {
            Err(EngineError::unsupported("functions, procedures and triggers are not supported by the TwinLite engine yet"))
        }
        other => {
            let text = other.to_string();
            Err(EngineError::unsupported(format!(
                "statement is not supported by the TwinLite engine: {}",
                text.chars().take(80).collect::<String>()
            )))
        }
    }
}

pub(crate) fn object_last(name: &ast::ObjectName, ctx: &Ctx<'_>) -> String {
    match name.0.last() {
        Some(ast::ObjectNamePart::Identifier(i)) => ctx.fold(i),
        _ => name.to_string(),
    }
}
