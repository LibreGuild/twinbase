//! DDL translation: CREATE/ALTER/DROP statements become structured plans built on the catalog
//! model ([`TableDef`]); the session then applies them to SQLite.

use std::collections::HashSet;

use sqlparser::ast::{self, ColumnOption, Expr, ObjectName, TableConstraint};

use super::expr::{rel_for_columns, translate_expr, translate_subquery};
use super::{AlterOp, ConstraintDef, Ctx, DropKind, NewSequence, Plan, object_last};
use crate::Dialect;
use crate::catalog::{CheckDef, ColumnDef, DefaultExpr, ForeignKeyDef, Identity, IndexDef, KeyDef, TableDef};
use crate::error::{EngineError, Result, state};
use crate::types::{SerialKind, SqlType};

/// Chooses constraint and index names the way each real database would.
struct Namer {
    dialect: Dialect,
    table: String,
    used: HashSet<String>,
    fk_count: usize,
    check_count: usize,
}

impl Namer {
    fn new(ctx: &Ctx<'_>, schema: &str, table: &str, existing: Option<&TableDef>) -> Self {
        let mut used = HashSet::new();
        if let Some(sd) = ctx.catalog.schema(schema) {
            for t in sd.tables.values() {
                used.insert(t.name.to_lowercase());
                if ctx.dialect == Dialect::Postgres {
                    for n in t.constraint_names() {
                        used.insert(n.to_lowercase());
                    }
                    for i in &t.indexes {
                        used.insert(i.name.to_lowercase());
                    }
                }
            }
            used.extend(sd.views.keys().map(|v| v.to_lowercase()));
        }
        let (mut fk_count, mut check_count) = (0, 0);
        if let Some(t) = existing {
            for n in t.constraint_names() {
                used.insert(n.to_lowercase());
            }
            for i in &t.indexes {
                used.insert(i.name.to_lowercase());
            }
            fk_count = t.foreign_keys.len();
            check_count = t.checks.len();
        }
        Self { dialect: ctx.dialect, table: table.to_owned(), used, fk_count, check_count }
    }

    fn take(&mut self, base: String) -> String {
        // PostgreSQL truncates identifiers to 63 bytes (on a character boundary).
        let base = if self.dialect == Dialect::Postgres && base.len() > 63 {
            let mut cut = 63;
            while !base.is_char_boundary(cut) {
                cut -= 1;
            }
            base[..cut].to_owned()
        } else {
            base
        };
        if self.used.insert(base.to_lowercase()) {
            return base;
        }
        let sep = if self.dialect == Dialect::MySql { "_" } else { "" };
        for n in 1.. {
            let candidate = format!("{base}{sep}{}", if self.dialect == Dialect::MySql { n + 1 } else { n });
            if self.used.insert(candidate.to_lowercase()) {
                return candidate;
            }
        }
        unreachable!()
    }

    fn reserve(&mut self, name: &str) {
        self.used.insert(name.to_lowercase());
    }

    fn primary(&mut self) -> String {
        if self.dialect == Dialect::MySql { "PRIMARY".into() } else { self.take(format!("{}_pkey", self.table)) }
    }

    fn unique(&mut self, cols: &[String]) -> String {
        match self.dialect {
            Dialect::MySql => self.take(cols.first().cloned().unwrap_or_else(|| "key".into())),
            _ => self.take(format!("{}_{}_key", self.table, cols.join("_"))),
        }
    }

    fn foreign(&mut self, cols: &[String]) -> String {
        self.fk_count += 1;
        match self.dialect {
            Dialect::MySql => self.take(format!("{}_ibfk_{}", self.table, self.fk_count)),
            _ => self.take(format!("{}_{}_fkey", self.table, cols.join("_"))),
        }
    }

    fn check(&mut self, col: Option<&str>) -> String {
        self.check_count += 1;
        match (self.dialect, col) {
            (Dialect::MySql, _) => self.take(format!("{}_chk_{}", self.table, self.check_count)),
            (_, Some(c)) => self.take(format!("{}_{c}_check", self.table)),
            (_, None) => self.take(format!("{}_check", self.table)),
        }
    }

    fn index(&mut self, cols: &[String]) -> String {
        match self.dialect {
            Dialect::MySql => self.take(cols.first().cloned().unwrap_or_else(|| "idx".into())),
            _ => self.take(format!("{}_{}_idx", self.table, cols.join("_"))),
        }
    }
}

struct ColumnBuild {
    column: ColumnDef,
    constraints: Vec<ConstraintDef>,
    sequences: Vec<NewSequence>,
}

fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn is_null_literal(e: &Expr) -> bool {
    matches!(e, Expr::Value(v) if matches!(v.value, ast::Value::Null))
}

fn fk_action(a: &Option<ast::ReferentialAction>) -> Option<String> {
    a.as_ref().map(|x| x.to_string().to_ascii_uppercase())
}

fn index_columns(cols: &[ast::IndexColumn], ctx: &Ctx<'_>) -> Result<(Vec<String>, Vec<bool>)> {
    let mut names = Vec::new();
    let mut desc = Vec::new();
    for ic in cols {
        match &ic.column.expr {
            Expr::Identifier(i) => names.push(ctx.fold(i)),
            Expr::CompoundIdentifier(ids) => names.push(ids.last().map(|i| ctx.fold(i)).unwrap_or_default()),
            // MySQL prefix indexes `col(10)` parse as a function call.
            Expr::Function(f) if ctx.dialect == Dialect::MySql => names.push(object_last(&f.name, ctx)),
            other => {
                return Err(EngineError::unsupported(format!("index expressions are not supported by the TwinLite engine: {other}")));
            }
        }
        desc.push(matches!(ic.column.options.sort, Some(ast::OrderBySort::Desc)));
    }
    Ok((names, desc))
}

struct TableCtx<'a> {
    schema: String,
    table: String,
    columns: &'a [ColumnDef],
    pk: Option<&'a KeyDef>,
}

#[allow(clippy::too_many_arguments)]
fn build_foreign_key(
    ctx: &Ctx<'_>,
    tc: &TableCtx<'_>,
    namer: &mut Namer,
    name: Option<&ast::Ident>,
    columns: Vec<String>,
    foreign_table: &ObjectName,
    referred: &[ast::Ident],
    on_delete: &Option<ast::ReferentialAction>,
    on_update: &Option<ast::ReferentialAction>,
) -> Result<ForeignKeyDef> {
    let ref_name = object_last(foreign_table, ctx);
    let self_ref = foreign_table.0.len() == 1 && ref_name == tc.table;
    let (ref_schema, ref_table) =
        if self_ref { (tc.schema.clone(), tc.table.clone()) } else { ctx.resolve_name(foreign_table, ctx.dialect == Dialect::Postgres)? };
    let ref_def = if self_ref { None } else { ctx.catalog.table(&ref_schema, &ref_table) };
    let mut ref_columns: Vec<String> = referred.iter().map(|i| ctx.fold(i)).collect();
    if ref_columns.is_empty() {
        ref_columns = if self_ref {
            tc.pk.map(|k| k.columns.clone()).unwrap_or_default()
        } else {
            ref_def.and_then(|t| t.primary_key.as_ref().map(|k| k.columns.clone())).unwrap_or_default()
        };
        if ref_columns.is_empty() {
            return Err(EngineError::new("42830", format!("there is no primary key for referenced table \"{ref_table}\"")));
        }
    }
    if ref_columns.len() != columns.len() {
        return Err(EngineError::new("42830", "number of referencing and referenced columns for foreign key disagree"));
    }
    if let Some(rt) = ref_def {
        let unique_match = rt.primary_key.iter().chain(rt.uniques.iter()).any(|k| k.columns == ref_columns)
            || rt.indexes.iter().any(|i| i.unique && i.columns == ref_columns);
        if !unique_match {
            return Err(if ctx.dialect == Dialect::MySql {
                EngineError::new(
                    "HY000",
                    format!("Failed to add the foreign key constraint. Missing index for constraint in the referenced table '{ref_table}'"),
                )
                .with_mysql(1822)
            } else {
                EngineError::new("42830", format!("there is no unique constraint matching given keys for referenced table \"{ref_table}\""))
            });
        }
        for c in &ref_columns {
            if rt.column(c).is_none() {
                return Err(EngineError::new(
                    state::UNDEFINED_COLUMN,
                    format!("column \"{c}\" referenced in foreign key constraint does not exist"),
                ));
            }
        }
    }
    let name = match name {
        Some(n) => {
            let n = ctx.fold(n);
            namer.reserve(&n);
            n
        }
        None => namer.foreign(&columns),
    };
    Ok(ForeignKeyDef {
        name,
        columns,
        ref_schema,
        ref_table,
        ref_columns,
        on_delete: fk_action(on_delete),
        on_update: fk_action(on_update),
    })
}

fn build_check(ctx: &Ctx<'_>, tc: &TableCtx<'_>, namer: &mut Namer, name: Option<&ast::Ident>, expr: &Expr) -> Result<CheckDef> {
    let cols: Vec<(String, SqlType)> = tc.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
    let (sql, _) = translate_expr(expr, ctx, vec![rel_for_columns(&tc.table, &cols)])?;
    let referenced: Vec<&String> = cols.iter().map(|(n, _)| n).filter(|n| expr_mentions(expr, n)).collect();
    let name = match name {
        Some(n) => {
            let n = ctx.fold(n);
            namer.reserve(&n);
            n
        }
        None => namer.check(if referenced.len() == 1 { Some(referenced[0].as_str()) } else { None }),
    };
    Ok(CheckDef { name, display: expr.to_string(), sql })
}

fn expr_mentions(e: &Expr, col: &str) -> bool {
    let text = e.to_string().to_lowercase();
    let col = col.to_lowercase();
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == col)
}

fn build_column(ctx: &Ctx<'_>, tc: &TableCtx<'_>, namer: &mut Namer, cd: &ast::ColumnDef) -> Result<ColumnBuild> {
    let my = ctx.dialect == Dialect::MySql;
    let name = ctx.fold(&cd.name);
    let type_text = cd.data_type.to_string();
    let parsed = SqlType::parse(&type_text, ctx.dialect, &|n| ctx.catalog.enum_values(n, ctx.search_path));
    if ctx.dialect == Dialect::Postgres
        && let SqlType::Other { name: tn } = &parsed.ty
    {
        let known = [
            "oid",
            "name",
            "inet",
            "cidr",
            "macaddr",
            "xml",
            "tsvector",
            "tsquery",
            "point",
            "bit varying",
            "varbit",
            "ltree",
            "hstore",
            "money",
        ];
        if !known.iter().any(|k| tn.starts_with(k)) {
            return Err(EngineError::new(state::UNDEFINED_OBJECT, format!("type \"{tn}\" does not exist")));
        }
    }
    let mut column = ColumnDef {
        name: name.clone(),
        ty: parsed.ty.clone(),
        type_text: type_text.clone(),
        nullable: true,
        default: None,
        identity: None,
        comment: None,
        on_update_now: false,
        unsigned: parsed.unsigned,
    };
    let mut constraints = Vec::new();
    let mut sequences = Vec::new();
    let seq_name = format!("{}_{}_seq", tc.table, name);
    let make_identity = |column: &mut ColumnDef, sequences: &mut Vec<NewSequence>, identity: Identity| {
        sequences.push(NewSequence {
            schema: tc.schema.clone(),
            name: seq_name.clone(),
            start: 1,
            increment: 1,
            owner: Some(format!("{}.{}", tc.table, name)),
        });
        column.default = Some(DefaultExpr {
            display: if matches!(identity, Identity::Serial { .. }) { format!("nextval('{seq_name}'::regclass)") } else { String::new() },
            sql: format!("tb_nextval('{}.{}')", sql_quote(&tc.schema), sql_quote(&seq_name)),
            name: None,
        });
        column.identity = Some(identity);
        column.nullable = false;
    };
    if parsed.serial == Some(SerialKind::Serial) {
        if my {
            make_identity(&mut column, &mut sequences, Identity::AutoIncrement { sequence: seq_name.clone() });
            column.default.as_mut().unwrap().display.clear();
            let n = namer.unique(std::slice::from_ref(&name));
            constraints.push(ConstraintDef::Unique(KeyDef { name: n, columns: vec![name.clone()] }));
        } else {
            make_identity(&mut column, &mut sequences, Identity::Serial { sequence: seq_name.clone() });
        }
    }
    let mut checks_pending: Vec<(Option<ast::Ident>, Expr)> = Vec::new();
    for opt in &cd.options {
        match &opt.option {
            ColumnOption::Null => column.nullable = true,
            ColumnOption::NotNull => column.nullable = false,
            ColumnOption::Default(e) => {
                if is_null_literal(e) {
                    column.default = None;
                } else {
                    let (sql, _) = translate_expr(e, ctx, vec![])?;
                    column.default = Some(DefaultExpr {
                        display: e.to_string(),
                        sql: format!("tb_cast({sql}, '{}')", sql_quote(&ctx.type_spec(&column.ty))),
                        name: opt.name.as_ref().map(|n| n.value.clone()),
                    });
                }
            }
            ColumnOption::PrimaryKey(pk) => {
                let n = match &pk.name {
                    Some(i) => ctx.fold(i),
                    None => namer.primary(),
                };
                column.nullable = false;
                constraints.push(ConstraintDef::PrimaryKey(KeyDef { name: n, columns: vec![name.clone()] }));
            }
            ColumnOption::Unique(u) => {
                let n = match &u.name {
                    Some(i) => ctx.fold(i),
                    None => namer.unique(std::slice::from_ref(&name)),
                };
                constraints.push(ConstraintDef::Unique(KeyDef { name: n, columns: vec![name.clone()] }));
            }
            ColumnOption::ForeignKey(fk) => {
                let def = build_foreign_key(
                    ctx,
                    tc,
                    namer,
                    fk.name.as_ref().or(opt.name.as_ref()),
                    vec![name.clone()],
                    &fk.foreign_table,
                    &fk.referred_columns,
                    &fk.on_delete,
                    &fk.on_update,
                )?;
                constraints.push(ConstraintDef::ForeignKey(def));
            }
            ColumnOption::Check(ck) => checks_pending.push((ck.name.clone().or_else(|| opt.name.clone()), (*ck.expr).clone())),
            ColumnOption::DialectSpecific(tokens) => {
                let text: String = tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" ").to_ascii_uppercase();
                if text.contains("AUTO_INCREMENT") || text.contains("AUTOINCREMENT") {
                    make_identity(&mut column, &mut sequences, Identity::AutoIncrement { sequence: seq_name.clone() });
                    column.default.as_mut().unwrap().display.clear();
                }
            }
            // T-SQL IDENTITY(seed, increment): a sequence-backed default, like PostgreSQL's
            // BY DEFAULT identity (explicit values are accepted; SQL Server would need IDENTITY_INSERT).
            ColumnOption::Identity(_) => make_identity(&mut column, &mut sequences, Identity::ByDefault { sequence: seq_name.clone() }),
            ColumnOption::Comment(c) => column.comment = Some(c.clone()),
            ColumnOption::OnUpdate(e) => {
                let t = e.to_string().to_ascii_lowercase();
                if t.contains("current_timestamp") || t.contains("now") {
                    column.on_update_now = true;
                }
            }
            ColumnOption::Generated { generated_as, generation_expr, .. } => {
                if generation_expr.is_some() {
                    return Err(EngineError::unsupported("generated (computed) columns are not supported by the TwinLite engine yet"));
                }
                let identity = match generated_as {
                    ast::GeneratedAs::Always => Identity::Always { sequence: seq_name.clone() },
                    _ => Identity::ByDefault { sequence: seq_name.clone() },
                };
                make_identity(&mut column, &mut sequences, identity);
            }
            _ => {}
        }
    }
    if !checks_pending.is_empty() {
        let mut cols: Vec<ColumnDef> = tc.columns.to_vec();
        cols.push(column.clone());
        let local = TableCtx { schema: tc.schema.clone(), table: tc.table.clone(), columns: &cols, pk: tc.pk };
        for (n, e) in checks_pending {
            constraints.push(ConstraintDef::Check(build_check(ctx, &local, namer, n.as_ref(), &e)?));
        }
    }
    Ok(ColumnBuild { column, constraints, sequences })
}

fn table_constraint(ctx: &Ctx<'_>, tc: &TableCtx<'_>, namer: &mut Namer, c: &TableConstraint) -> Result<Option<ConstraintDef>> {
    Ok(Some(match c {
        TableConstraint::PrimaryKey(pk) => {
            let (cols, _) = index_columns(&pk.columns, ctx)?;
            let n = match &pk.name {
                Some(i) => ctx.fold(i),
                None => namer.primary(),
            };
            ConstraintDef::PrimaryKey(KeyDef { name: n, columns: cols })
        }
        TableConstraint::Unique(u) => {
            let (cols, _) = index_columns(&u.columns, ctx)?;
            let n = match u.name.as_ref().or(u.index_name.as_ref()) {
                Some(i) => {
                    let n = ctx.fold(i);
                    namer.reserve(&n);
                    n
                }
                None => namer.unique(&cols),
            };
            ConstraintDef::Unique(KeyDef { name: n, columns: cols })
        }
        TableConstraint::ForeignKey(fk) => {
            let cols: Vec<String> = fk.columns.iter().map(|i| ctx.fold(i)).collect();
            ConstraintDef::ForeignKey(build_foreign_key(
                ctx,
                tc,
                namer,
                fk.name.as_ref(),
                cols,
                &fk.foreign_table,
                &fk.referred_columns,
                &fk.on_delete,
                &fk.on_update,
            )?)
        }
        TableConstraint::Check(ck) => ConstraintDef::Check(build_check(ctx, tc, namer, ck.name.as_ref(), &ck.expr)?),
        TableConstraint::Index(ix) => {
            let (cols, desc) = index_columns(&ix.columns, ctx)?;
            let n = match &ix.name {
                Some(i) => {
                    let n = ctx.fold(i);
                    namer.reserve(&n);
                    n
                }
                None => namer.index(&cols),
            };
            ConstraintDef::Index(IndexDef { name: n, columns: cols, descending: desc, unique: false, where_sql: None })
        }
        _ => return Ok(None),
    }))
}

/// Apply constraint definitions to a table definition (used by CREATE TABLE).
fn apply_constraints(def: &mut TableDef, constraints: Vec<ConstraintDef>, dialect: Dialect) -> Result<()> {
    for c in constraints {
        match c {
            ConstraintDef::PrimaryKey(k) => {
                if def.primary_key.is_some() {
                    return Err(if dialect == Dialect::MySql {
                        EngineError::new("42000", "Multiple primary key defined").with_mysql(1068)
                    } else {
                        EngineError::new("42P16", format!("multiple primary keys for table \"{}\" are not allowed", def.name))
                    });
                }
                for col in &k.columns {
                    match def.column_mut(col) {
                        Some(c) => c.nullable = false,
                        None => return Err(missing_column(col, dialect)),
                    }
                }
                def.primary_key = Some(k);
            }
            ConstraintDef::Unique(k) => {
                for col in &k.columns {
                    if def.column(col).is_none() {
                        return Err(missing_column(col, dialect));
                    }
                }
                def.uniques.push(k);
            }
            ConstraintDef::ForeignKey(f) => {
                for col in &f.columns {
                    if def.column(col).is_none() {
                        return Err(missing_column(col, dialect));
                    }
                }
                def.foreign_keys.push(f);
            }
            ConstraintDef::Check(c) => def.checks.push(c),
            ConstraintDef::Index(i) => {
                for col in &i.columns {
                    if def.column(col).is_none() {
                        return Err(missing_column(col, dialect));
                    }
                }
                def.indexes.push(i);
            }
        }
    }
    Ok(())
}

fn missing_column(col: &str, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::UNDEFINED_COLUMN, format!("Key column '{col}' doesn't exist in table")).with_mysql(1072)
    } else {
        EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{col}\" named in key does not exist"))
    }
}

fn already_exists(name: &str, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::DUPLICATE_TABLE, format!("Table '{name}' already exists")).with_mysql(1050)
    } else {
        EngineError::new(state::DUPLICATE_TABLE, format!("relation \"{name}\" already exists"))
    }
}

pub fn translate_create_table(ct: ast::CreateTable, ctx: &Ctx<'_>) -> Result<Plan> {
    let (schema, name) = ctx.resolve_name(&ct.name, false)?;
    let exists = ctx.catalog.table(&schema, &name).is_some() || ctx.catalog.view(&schema, &name).is_some();
    if exists {
        if ct.if_not_exists {
            return Ok(Plan::Noop { tag: "CREATE TABLE".into() });
        }
        return Err(already_exists(&name, ctx.dialect));
    }
    if ct.like.is_some() || ct.clone.is_some() {
        return Err(EngineError::unsupported("CREATE TABLE ... LIKE is not supported by the TwinLite engine yet"));
    }
    if let Some(mut q) = ct.query {
        let plan = translate_subquery(&mut q, ctx)?;
        let def = TableDef {
            name: name.clone(),
            columns: vec![],
            primary_key: None,
            uniques: vec![],
            foreign_keys: vec![],
            checks: vec![],
            indexes: vec![],
            comment: None,
        };
        return Ok(Plan::CreateTable { schema, def, if_not_exists: ct.if_not_exists, sequences: vec![], as_query: Some(plan) });
    }
    let mut namer = Namer::new(ctx, &schema, &name, None);
    let mut columns: Vec<ColumnDef> = Vec::new();
    let mut constraints = Vec::new();
    let mut sequences = Vec::new();
    // Primary key declared as a table constraint is needed for self-referencing FKs.
    let mut pk_hint: Option<KeyDef> = None;
    for c in &ct.constraints {
        if let TableConstraint::PrimaryKey(pk) = c {
            let (cols, _) = index_columns(&pk.columns, ctx)?;
            pk_hint = Some(KeyDef { name: String::new(), columns: cols });
        }
    }
    for cd in &ct.columns {
        if !pk_hint.as_ref().is_some_and(|k| !k.columns.is_empty())
            && cd.options.iter().any(|o| matches!(o.option, ColumnOption::PrimaryKey(_)))
        {
            pk_hint = Some(KeyDef { name: String::new(), columns: vec![ctx.fold(&cd.name)] });
        }
    }
    for cd in &ct.columns {
        let tc = TableCtx { schema: schema.clone(), table: name.clone(), columns: &columns, pk: pk_hint.as_ref() };
        let built = build_column(ctx, &tc, &mut namer, cd)?;
        if columns.iter().any(|c| c.name.eq_ignore_ascii_case(&built.column.name)) {
            return Err(if ctx.dialect == Dialect::MySql {
                EngineError::new(state::DUPLICATE_COLUMN, format!("Duplicate column name '{}'", built.column.name)).with_mysql(1060)
            } else {
                EngineError::new(state::DUPLICATE_COLUMN, format!("column \"{}\" specified more than once", built.column.name))
            });
        }
        columns.push(built.column);
        constraints.extend(built.constraints);
        sequences.extend(built.sequences);
    }
    let tc = TableCtx { schema: schema.clone(), table: name.clone(), columns: &columns, pk: pk_hint.as_ref() };
    for c in &ct.constraints {
        if let Some(def) = table_constraint(ctx, &tc, &mut namer, c)? {
            constraints.push(def);
        }
    }
    let comment = ct.comment.map(|c| match c {
        ast::CommentDef::WithEq(s) | ast::CommentDef::WithoutEq(s) => s,
    });
    let mut def = TableDef {
        name: name.clone(),
        columns,
        primary_key: None,
        uniques: vec![],
        foreign_keys: vec![],
        checks: vec![],
        indexes: vec![],
        comment,
    };
    apply_constraints(&mut def, constraints, ctx.dialect)?;
    // MySQL AUTO_INCREMENT=N table option.
    if ctx.dialect == Dialect::MySql {
        let opts = ct.table_options.to_string().to_ascii_uppercase();
        if let Some(pos) = opts.find("AUTO_INCREMENT") {
            let digits: String = opts[pos..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<i64>() {
                for s in &mut sequences {
                    s.start = n;
                }
            }
        }
    }
    Ok(Plan::CreateTable { schema, def, if_not_exists: ct.if_not_exists, sequences, as_query: None })
}

pub fn translate_alter_table(at: ast::AlterTable, ctx: &Ctx<'_>) -> Result<Plan> {
    let resolved = ctx.resolve_name(&at.name, true);
    let (schema, table) = match resolved {
        Ok(v) => v,
        Err(_) if at.if_exists => return Ok(Plan::Noop { tag: "ALTER TABLE".into() }),
        Err(e) => return Err(e),
    };
    let td = ctx.catalog.table(&schema, &table).ok_or_else(|| EngineError::unsupported("ALTER TABLE on a view"))?.clone();
    let mut namer = Namer::new(ctx, &schema, &table, Some(&td));
    let mut ops = Vec::new();
    let tc_cols = td.columns.clone();
    for op in at.operations {
        let tc = TableCtx { schema: schema.clone(), table: table.clone(), columns: &tc_cols, pk: td.primary_key.as_ref() };
        let mapped = match op {
            ast::AlterTableOperation::AddColumn { if_not_exists, column_def, .. } => {
                let built = build_column(ctx, &tc, &mut namer, &column_def)?;
                AlterOp::AddColumn { column: built.column, constraints: built.constraints, sequences: built.sequences, if_not_exists }
            }
            ast::AlterTableOperation::DropColumn { column_names, if_exists, drop_behavior, .. } => {
                for c in &column_names[..column_names.len().saturating_sub(1)] {
                    ops.push(AlterOp::DropColumn {
                        name: ctx.fold(c),
                        if_exists,
                        cascade: matches!(drop_behavior, Some(ast::DropBehavior::Cascade)),
                    });
                }
                match column_names.last() {
                    Some(c) => AlterOp::DropColumn {
                        name: ctx.fold(c),
                        if_exists,
                        cascade: matches!(drop_behavior, Some(ast::DropBehavior::Cascade)),
                    },
                    None => AlterOp::Noop,
                }
            }
            ast::AlterTableOperation::RenameColumn { old_column_name, new_column_name } => {
                AlterOp::RenameColumn { old: ctx.fold(&old_column_name), new: ctx.fold(&new_column_name) }
            }
            ast::AlterTableOperation::RenameTable { table_name } => {
                let n = match table_name {
                    ast::RenameTableNameKind::As(n) | ast::RenameTableNameKind::To(n) => n,
                };
                AlterOp::RenameTable { new: object_last(&n, ctx) }
            }
            ast::AlterTableOperation::AlterColumn { column_name, op } => {
                let col = ctx.fold(&column_name);
                let def = td.column(&col).ok_or_else(|| missing_alter_column(&col, &table, ctx.dialect))?.clone();
                match op {
                    ast::AlterColumnOperation::SetNotNull => AlterOp::SetNotNull(def.name),
                    ast::AlterColumnOperation::DropNotNull => AlterOp::DropNotNull(def.name),
                    ast::AlterColumnOperation::DropDefault => AlterOp::DropDefault(def.name),
                    ast::AlterColumnOperation::SetDefault { value } => {
                        let (sql, _) = translate_expr(&value, ctx, vec![])?;
                        AlterOp::SetDefault(
                            def.name,
                            DefaultExpr {
                                display: value.to_string(),
                                sql: format!("tb_cast({sql}, '{}')", sql_quote(&ctx.type_spec(&def.ty))),
                                name: None,
                            },
                        )
                    }
                    ast::AlterColumnOperation::SetDataType { data_type, using, .. } => {
                        let type_text = data_type.to_string();
                        let parsed = SqlType::parse(&type_text, ctx.dialect, &|n| ctx.catalog.enum_values(n, ctx.search_path));
                        let using = match using {
                            Some(e) => {
                                let cols: Vec<(String, SqlType)> = td.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                                Some(translate_expr(&e, ctx, vec![rel_for_columns(&table, &cols)])?.0)
                            }
                            None => None,
                        };
                        AlterOp::SetType { column: def.name, ty: parsed.ty, type_text, using }
                    }
                    _ => return Err(EngineError::unsupported("this ALTER COLUMN form is not supported by the TwinLite engine")),
                }
            }
            ast::AlterTableOperation::AddConstraint { constraint, .. } => match table_constraint(ctx, &tc, &mut namer, &constraint)? {
                Some(c) => AlterOp::AddConstraint(c),
                None => AlterOp::Noop,
            },
            ast::AlterTableOperation::DropConstraint { if_exists, name, drop_behavior } => AlterOp::DropConstraint {
                name: ctx.fold(&name),
                if_exists,
                cascade: matches!(drop_behavior, Some(ast::DropBehavior::Cascade)),
            },
            ast::AlterTableOperation::DropPrimaryKey { .. } => AlterOp::DropPrimaryKey,
            ast::AlterTableOperation::DropForeignKey { name, .. } => {
                AlterOp::DropConstraint { name: ctx.fold(&name), if_exists: false, cascade: false }
            }
            ast::AlterTableOperation::DropIndex { name } => {
                AlterOp::DropConstraint { name: ctx.fold(&name), if_exists: false, cascade: false }
            }
            ast::AlterTableOperation::RenameConstraint { old_name, new_name } => {
                AlterOp::RenameConstraint { old: ctx.fold(&old_name), new: ctx.fold(&new_name) }
            }
            ast::AlterTableOperation::ChangeColumn { old_name, new_name, data_type, options, .. } => {
                let cd = ast::ColumnDef {
                    name: new_name,
                    data_type,
                    options: options.into_iter().map(|o| ast::ColumnOptionDef { name: None, option: o }).collect(),
                };
                replace_column(ctx, &tc, &mut namer, ctx.fold(&old_name), &cd)?
            }
            ast::AlterTableOperation::ModifyColumn { col_name, data_type, options, .. } => {
                let cd = ast::ColumnDef {
                    name: col_name.clone(),
                    data_type,
                    options: options.into_iter().map(|o| ast::ColumnOptionDef { name: None, option: o }).collect(),
                };
                replace_column(ctx, &tc, &mut namer, ctx.fold(&col_name), &cd)?
            }
            ast::AlterTableOperation::AutoIncrement { value, .. } => {
                AlterOp::SetAutoIncrement(value.value.to_string().parse().unwrap_or(1))
            }
            ast::AlterTableOperation::OwnerTo { .. }
            | ast::AlterTableOperation::EnableRowLevelSecurity
            | ast::AlterTableOperation::DisableRowLevelSecurity
            | ast::AlterTableOperation::ForceRowLevelSecurity
            | ast::AlterTableOperation::NoForceRowLevelSecurity
            | ast::AlterTableOperation::SetLogged
            | ast::AlterTableOperation::SetUnlogged
            | ast::AlterTableOperation::Algorithm { .. }
            | ast::AlterTableOperation::Lock { .. }
            | ast::AlterTableOperation::ValidateConstraint { .. }
            | ast::AlterTableOperation::EnableTrigger { .. }
            | ast::AlterTableOperation::DisableTrigger { .. } => AlterOp::Noop,
            other => {
                return Err(EngineError::unsupported(format!("ALTER TABLE operation is not supported by the TwinLite engine: {other}")));
            }
        };
        ops.push(mapped);
    }
    Ok(Plan::AlterTable { schema, table, ops, if_exists: at.if_exists })
}

fn missing_alter_column(col: &str, table: &str, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::UNDEFINED_COLUMN, format!("Unknown column '{col}' in '{table}'")).with_mysql(1054)
    } else {
        EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{col}\" of relation \"{table}\" does not exist"))
    }
}

fn replace_column(ctx: &Ctx<'_>, tc: &TableCtx<'_>, namer: &mut Namer, old: String, cd: &ast::ColumnDef) -> Result<AlterOp> {
    if !tc.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&old)) {
        return Err(missing_alter_column(&old, &tc.table, ctx.dialect));
    }
    let built = build_column(ctx, tc, namer, cd)?;
    Ok(AlterOp::ReplaceColumn { old, column: built.column, constraints: built.constraints, sequences: built.sequences })
}

pub fn translate_create_index(ci: ast::CreateIndex, ctx: &Ctx<'_>) -> Result<Plan> {
    let (schema, table) = ctx.resolve_name(&ci.table_name, true)?;
    let td = ctx.catalog.table(&schema, &table).ok_or_else(|| EngineError::unsupported("indexes on views are not supported"))?;
    let (cols, desc) = index_columns(&ci.columns, ctx)?;
    for c in &cols {
        if td.column(c).is_none() {
            return Err(if ctx.dialect == Dialect::MySql {
                EngineError::new(state::UNDEFINED_COLUMN, format!("Key column '{c}' doesn't exist in table")).with_mysql(1072)
            } else {
                EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{c}\" does not exist"))
            });
        }
    }
    let mut namer = Namer::new(ctx, &schema, &table, Some(td));
    let name = match &ci.name {
        Some(n) => object_last(n, ctx),
        None => namer.index(&cols),
    };
    let where_sql = match &ci.predicate {
        Some(p) => {
            let colsdef: Vec<(String, SqlType)> = td.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
            Some(translate_expr(p, ctx, vec![rel_for_columns(&table, &colsdef)])?.0)
        }
        None => None,
    };
    Ok(Plan::CreateIndex {
        schema,
        table,
        def: IndexDef { name, columns: cols, descending: desc, unique: ci.unique, where_sql },
        if_not_exists: ci.if_not_exists,
    })
}

pub fn translate_create_view(cv: ast::CreateView, ctx: &Ctx<'_>) -> Result<Plan> {
    let (schema, name) = ctx.resolve_name(&cv.name, false)?;
    let definition = cv.query.to_string();
    let mut query = *cv.query;
    let plan = translate_subquery(&mut query, ctx)?;
    let columns: Vec<String> = cv.columns.iter().map(|c| ctx.fold(&c.name)).collect();
    Ok(Plan::CreateView { schema, name, definition, plan, columns, or_replace: cv.or_replace, if_not_exists: cv.if_not_exists })
}

pub fn translate_drop(object_type: ast::ObjectType, if_exists: bool, names: Vec<ObjectName>, cascade: bool, ctx: &Ctx<'_>) -> Result<Plan> {
    let kind = match object_type {
        ast::ObjectType::Table => DropKind::Table,
        ast::ObjectType::View | ast::ObjectType::MaterializedView => DropKind::View,
        ast::ObjectType::Index => DropKind::Index,
        ast::ObjectType::Sequence => DropKind::Sequence,
        ast::ObjectType::Type => DropKind::Type,
        ast::ObjectType::Schema if ctx.dialect == Dialect::MySql => DropKind::Database,
        ast::ObjectType::Schema => DropKind::Schema,
        ast::ObjectType::Database => DropKind::Database,
        ast::ObjectType::Role | ast::ObjectType::User => return Ok(Plan::Noop { tag: "DROP ROLE".into() }),
        other => return Err(EngineError::unsupported(format!("DROP {other} is not supported by the TwinLite engine"))),
    };
    let mut resolved = Vec::new();
    for n in &names {
        match kind {
            DropKind::Schema | DropKind::Database => resolved.push((object_last(n, ctx), String::new())),
            DropKind::Index => {
                let idx = object_last(n, ctx);
                let schema = if n.0.len() > 1 {
                    match &n.0[n.0.len() - 2] {
                        ast::ObjectNamePart::Identifier(i) => ctx.schema_name(&ctx.fold(i)),
                        _ => String::new(),
                    }
                } else {
                    ctx.search_path
                        .iter()
                        .find(|s| {
                            ctx.catalog.schema(s).is_some_and(|sd| {
                                sd.tables.values().any(|t| {
                                    t.indexes.iter().any(|i| i.name == idx)
                                        || t.uniques.iter().any(|u| u.name == idx)
                                        || t.primary_key.as_ref().is_some_and(|p| p.name == idx)
                                })
                            })
                        })
                        .cloned()
                        .or_else(|| ctx.search_path.first().cloned())
                        .unwrap_or_default()
                };
                resolved.push((schema, idx));
            }
            _ => match ctx.resolve_name(n, false) {
                Ok(v) => resolved.push(v),
                Err(e) if !if_exists => return Err(e),
                Err(_) => {}
            },
        }
    }
    Ok(Plan::Drop { kind, names: resolved, if_exists, cascade })
}

pub fn translate_alter_type(at: ast::AlterType, ctx: &Ctx<'_>) -> Result<Plan> {
    let (schema, name) = ctx.resolve_name(&at.name, false)?;
    match at.operation {
        ast::AlterTypeOperation::AddValue(v) => {
            Ok(Plan::AlterEnumAddValue { schema, name, value: v.value.value, if_not_exists: v.if_not_exists })
        }
        _ => Err(EngineError::unsupported("only ALTER TYPE ... ADD VALUE is supported by the TwinLite engine")),
    }
}
