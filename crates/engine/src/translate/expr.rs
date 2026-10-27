//! Expression and DML rewriting: PostgreSQL/MySQL expressions → SQLite, with static type
//! inference for result columns and parameters.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use sqlparser::ast::{
    self, BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArgumentList, FunctionArguments,
    Ident, Insert, ObjectName, ObjectNamePart, OnConflictAction, OnInsert, Query, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableObject, UnaryOperator, Update, Value, VisitMut, VisitorMut,
};

use super::{Ctx, SqlPlan, StmtKind};
use crate::Dialect;
use crate::catalog::{Identity, Relation, TableDef, quote_ident};
use crate::error::{EngineError, Result, state};
use crate::types::SqlType;

const PG_CATALOG_TABLES: &[&str] = &[
    "pg_namespace",
    "pg_class",
    "pg_attribute",
    "pg_type",
    "pg_index",
    "pg_constraint",
    "pg_attrdef",
    "pg_description",
    "pg_enum",
    "pg_tables",
    "pg_views",
    "pg_indexes",
    "pg_sequences",
    "pg_database",
    "pg_roles",
    "pg_user",
    "pg_settings",
    "pg_am",
    "pg_tablespace",
    "pg_extension",
    "pg_language",
    "pg_collation",
    "pg_proc",
    "pg_inherits",
    "pg_depend",
    "pg_trigger",
    "pg_policy",
    "pg_rewrite",
    "pg_matviews",
    "pg_partitioned_table",
    "pg_statistic_ext",
    "pg_publication",
    "pg_publication_rel",
    "pg_publication_namespace",
    "pg_subscription",
    "pg_foreign_table",
    "pg_event_trigger",
    "pg_opclass",
    "pg_operator",
    "pg_range",
    "pg_cast",
    "pg_locks",
    "pg_stat_activity",
];

/// A relation visible in a query scope.
#[derive(Debug, Clone)]
pub struct Rel {
    pub alias: String,
    pub table: String,
    pub columns: Vec<(String, SqlType)>,
    pub known: bool,
}

impl Rel {
    fn from_table(alias: &str, t: &TableDef) -> Rel {
        Rel {
            alias: alias.to_owned(),
            table: t.name.clone(),
            columns: t.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect(),
            known: true,
        }
    }
}

pub struct Rewriter<'a, 'c> {
    pub ctx: &'a Ctx<'c>,
    scopes: Vec<Vec<Rel>>,
    ctes: Vec<Vec<String>>,
    pub params: BTreeMap<usize, SqlType>,
    pub max_param: usize,
    pub needs_pg_catalog: bool,
    pub needs_info_schema: bool,
    pub error: Option<EngineError>,
    query_depth: usize,
    pub projection: Option<Vec<Option<SqlType>>>,
    /// Tables of the SELECT that just closed (for its query's ORDER BY).
    closed_select: Option<Vec<Rel>>,
}

pub(crate) fn ident(s: &str) -> Ident {
    Ident::with_quote('"', s)
}

pub(crate) fn lit(s: &str) -> Expr {
    Expr::Value(Value::SingleQuotedString(s.to_owned()).with_empty_span())
}

fn num(n: i64) -> Expr {
    Expr::Value(Value::Number(n.to_string(), false).with_empty_span())
}

pub(crate) fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args.into_iter().map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e))).collect(),
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn func_args(f: &Function) -> Vec<Expr> {
    match &f.args {
        FunctionArguments::List(l) => l
            .args
            .iter()
            .filter_map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
                FunctionArg::Named { arg: FunctionArgExpr::Expr(e), .. } => Some(e.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn func_name(f: &Function) -> String {
    match f.name.0.last() {
        Some(ObjectNamePart::Identifier(i)) => i.value.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn string_value(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s)
            | Value::DoubleQuotedString(s)
            | Value::EscapedStringLiteral(s)
            | Value::NationalStringLiteral(s) => Some(s.clone()),
            Value::DollarQuotedString(d) => Some(d.value.clone()),
            Value::Number(n, _) => Some(n.clone()),
            _ => None,
        },
        Expr::Nested(inner) => string_value(inner),
        _ => None,
    }
}

pub(crate) fn const_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => n.parse().ok(),
            Value::SingleQuotedString(s) => s.trim().parse().ok(),
            _ => None,
        },
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => const_int(expr).map(|n| -n),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => const_int(expr),
        Expr::Nested(inner) => const_int(inner),
        _ => None,
    }
}

fn is_placeholder(e: &Expr) -> Option<usize> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Placeholder(p) => p.trim_start_matches(['$', '?']).parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn is_literalish(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => !matches!(v.value, Value::Null | Value::Boolean(_)),
        Expr::Nested(inner) => is_literalish(inner),
        _ => false,
    }
}

/// Types whose stored representation differs from literal text and must be normalized
/// before comparisons.
fn needs_literal_coercion(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::Bool
            | SqlType::Date
            | SqlType::Time
            | SqlType::Timestamp
            | SqlType::TimestampTz
            | SqlType::Interval
            | SqlType::Uuid
            | SqlType::Json
            | SqlType::Jsonb
            | SqlType::Array { .. }
            | SqlType::Char { .. }
    )
}

fn tb_cast_type(e: &Expr) -> Option<String> {
    if let Expr::Function(f) = e
        && func_name(f) == "tb_cast"
    {
        return func_args(f).get(1).and_then(string_value);
    }
    None
}

impl<'a, 'c> Rewriter<'a, 'c> {
    pub fn new(ctx: &'a Ctx<'c>) -> Self {
        Self {
            ctx,
            scopes: Vec::new(),
            ctes: Vec::new(),
            params: BTreeMap::new(),
            max_param: 0,
            needs_pg_catalog: false,
            needs_info_schema: false,
            error: None,
            query_depth: 0,
            projection: None,
            closed_select: None,
        }
    }

    pub fn push_scope(&mut self, rels: Vec<Rel>) {
        self.scopes.push(rels);
    }

    fn pg(&self) -> bool {
        self.ctx.dialect == Dialect::Postgres
    }

    fn fail(&mut self, e: EngineError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn cast_to(&self, e: Expr, ty: &SqlType) -> Expr {
        call("tb_cast", vec![e, lit(&self.ctx.type_spec(ty))])
    }

    fn hint_param(&mut self, e: &Expr, ty: &SqlType) {
        if let Some(n) = is_placeholder(e)
            && n > 0
        {
            self.params.entry(n).or_insert_with(|| ty.clone());
        }
    }

    fn is_cte(&self, name: &str) -> bool {
        self.ctes.iter().any(|level| level.iter().any(|c| c.eq_ignore_ascii_case(name)))
    }

    fn fold(&self, i: &Ident) -> String {
        self.ctx.fold(i)
    }

    /// Resolve a FROM item into scope relations.
    fn add_factor(&self, tf: &TableFactor, out: &mut Vec<Rel>) {
        match tf {
            TableFactor::Table { name, alias, args, .. } => {
                let parts: Vec<String> = name
                    .0
                    .iter()
                    .filter_map(|p| match p {
                        ObjectNamePart::Identifier(i) => Some(self.fold(i)),
                        _ => None,
                    })
                    .collect();
                let alias_name = alias.as_ref().map(|a| self.fold(&a.name)).or_else(|| parts.last().cloned()).unwrap_or_default();
                if args.is_some() {
                    out.push(Rel { alias: alias_name, table: String::new(), columns: vec![], known: false });
                    return;
                }
                let rel = match parts.as_slice() {
                    [t] if !self.is_cte(t) => self.ctx.catalog.resolve_relation(t, self.ctx.search_path).map(|(_, r)| r),
                    [.., s, t] => {
                        let s = self.ctx.schema_name(s);
                        self.ctx.catalog.table(&s, t).map(Relation::Table).or_else(|| self.ctx.catalog.view(&s, t).map(Relation::View))
                    }
                    _ => None,
                };
                match rel {
                    Some(Relation::Table(t)) => out.push(Rel::from_table(&alias_name, t)),
                    Some(Relation::View(v)) => {
                        out.push(Rel { alias: alias_name, table: v.name.clone(), columns: v.columns.clone(), known: !v.columns.is_empty() })
                    }
                    None => {
                        out.push(Rel { alias: alias_name, table: parts.last().cloned().unwrap_or_default(), columns: vec![], known: false })
                    }
                }
            }
            TableFactor::Derived { alias, .. } => out.push(Rel {
                alias: alias.as_ref().map(|a| self.fold(&a.name)).unwrap_or_default(),
                table: String::new(),
                columns: vec![],
                known: false,
            }),
            TableFactor::NestedJoin { table_with_joins, .. } => {
                self.add_factor(&table_with_joins.relation, out);
                for j in &table_with_joins.joins {
                    self.add_factor(&j.relation, out);
                }
            }
            _ => out.push(Rel { alias: String::new(), table: String::new(), columns: vec![], known: false }),
        }
    }

    pub(crate) fn build_scope(&self, from: &[ast::TableWithJoins]) -> (Vec<Rel>, bool) {
        let mut rels = Vec::new();
        let mut merged = false;
        for twj in from {
            self.add_factor(&twj.relation, &mut rels);
            for j in &twj.joins {
                self.add_factor(&j.relation, &mut rels);
                let constraint = match &j.join_operator {
                    ast::JoinOperator::Join(c)
                    | ast::JoinOperator::Inner(c)
                    | ast::JoinOperator::Left(c)
                    | ast::JoinOperator::LeftOuter(c)
                    | ast::JoinOperator::Right(c)
                    | ast::JoinOperator::RightOuter(c)
                    | ast::JoinOperator::FullOuter(c) => Some(c),
                    _ => None,
                };
                if matches!(constraint, Some(ast::JoinConstraint::Using(_)) | Some(ast::JoinConstraint::Natural)) {
                    merged = true;
                }
            }
        }
        (rels, merged)
    }

    /// Type of a column reference, searching scopes from the innermost.
    fn column_type(&self, e: &Expr) -> Option<SqlType> {
        let parts: Vec<String> = match e {
            Expr::Identifier(i) => vec![self.fold(i)],
            Expr::CompoundIdentifier(ids) => ids.iter().map(|i| self.fold(i)).collect(),
            Expr::Nested(inner) => return self.column_type(inner),
            _ => return None,
        };
        let (qual, col) = match parts.as_slice() {
            [c] => (None, c.clone()),
            [.., t, c] => (Some(t.clone()), c.clone()),
            [] => return None,
        };
        // A query's ORDER BY is visited after its SELECT closed; it still sees that SELECT's tables.
        for scope in self.scopes.iter().rev().chain(self.closed_select.iter()) {
            for rel in scope {
                if let Some(q) = &qual
                    && !rel.alias.eq_ignore_ascii_case(q)
                    && !rel.table.eq_ignore_ascii_case(q)
                {
                    continue;
                }
                if let Some((_, ty)) = rel.columns.iter().find(|(n, _)| n.eq_ignore_ascii_case(&col)) {
                    return Some(ty.clone());
                }
            }
        }
        None
    }

    /// Infer the static type of an (already rewritten) expression.
    pub(crate) fn infer(&self, e: &Expr) -> Option<SqlType> {
        let my = self.ctx.dialect == Dialect::MySql;
        match e {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => self.column_type(e),
            Expr::Nested(inner) => self.infer(inner),
            Expr::Value(v) => match &v.value {
                Value::Number(n, _) => Some(if n.contains(['.', 'e', 'E']) {
                    if my {
                        SqlType::Numeric { precision: None, scale: n.split('.').nth(1).map(|f| f.len() as u32) }
                    } else {
                        SqlType::Numeric { precision: None, scale: None }
                    }
                } else if my || n.parse::<i32>().is_err() {
                    SqlType::Int8
                } else {
                    SqlType::Int4
                }),
                Value::Boolean(_) => Some(SqlType::Bool),
                Value::Null => None,
                Value::Placeholder(_) => is_placeholder(e).and_then(|n| self.params.get(&n).cloned()),
                _ => Some(SqlType::Text),
            },
            Expr::BinaryOp { left, op, right } => match op {
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
                | BinaryOperator::Xor => Some(SqlType::Bool),
                // pgvector distances.
                BinaryOperator::Spaceship if self.ctx.dialect == Dialect::Postgres => Some(SqlType::Float8),
                BinaryOperator::LtDashGt
                | BinaryOperator::LtCaret
                | BinaryOperator::GtCaret
                | BinaryOperator::QuestionDashPipe
                | BinaryOperator::QuestionDoublePipe => Some(SqlType::Float8),
                BinaryOperator::Spaceship => Some(SqlType::Bool),
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply | BinaryOperator::StringConcat
                    if self.vector_family(left, right).is_some() =>
                {
                    self.vector_family(left, right).map(|family| SqlType::Vector { family, dim: None })
                }
                BinaryOperator::StringConcat => Some(SqlType::Text),
                BinaryOperator::Arrow => self.infer(left).filter(|t| matches!(t, SqlType::Json | SqlType::Jsonb)).or(Some(SqlType::Jsonb)),
                BinaryOperator::LongArrow => Some(SqlType::Text),
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply | BinaryOperator::Modulo => {
                    numeric_promote(self.infer(left), self.infer(right))
                }
                BinaryOperator::Divide => {
                    if my {
                        Some(SqlType::Numeric { precision: None, scale: Some(4) })
                    } else {
                        numeric_promote(self.infer(left), self.infer(right))
                    }
                }
                _ => None,
            },
            Expr::UnaryOp { op: UnaryOperator::Not, .. } => Some(SqlType::Bool),
            Expr::UnaryOp { expr, .. } => self.infer(expr),
            Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsTrue(_)
            | Expr::IsFalse(_)
            | Expr::IsNotTrue(_)
            | Expr::IsNotFalse(_)
            | Expr::IsDistinctFrom(..)
            | Expr::IsNotDistinctFrom(..)
            | Expr::InList { .. }
            | Expr::InSubquery { .. }
            | Expr::Between { .. }
            | Expr::Like { .. }
            | Expr::ILike { .. }
            | Expr::Exists { .. } => Some(SqlType::Bool),
            Expr::Case { conditions, else_result, .. } => {
                conditions.iter().find_map(|c| self.infer(&c.result)).or_else(|| else_result.as_ref().and_then(|e| self.infer(e)))
            }
            Expr::Function(f) => self.infer_function(f),
            _ => None,
        }
    }

    fn infer_function(&self, f: &Function) -> Option<SqlType> {
        let my = self.ctx.dialect == Dialect::MySql;
        let args = func_args(f);
        let arg0 = || args.first().and_then(|a| self.infer(a));
        let name = func_name(f);
        Some(match name.as_str() {
            "tb_cast" => {
                let spec = args.get(1).and_then(string_value)?;
                let cat = self.ctx.catalog;
                let path = self.ctx.search_path;
                SqlType::parse(&spec, self.ctx.dialect, &|n| cat.enum_values(n, path)).ty
            }
            "tb_vec_l2" | "tb_vec_ip" | "tb_vec_neg_ip" | "tb_vec_cos" | "tb_vec_l1" | "tb_vec_norm" | "tb_bit_hamming"
            | "tb_bit_jaccard" | "l2_distance" | "inner_product" | "cosine_distance" | "l1_distance" | "vector_norm" | "l2_norm"
            | "hamming_distance" | "jaccard_distance" | "vector_distance" => SqlType::Float8,
            "tb_vec_dims" | "vector_dims" | "vector_dim" => SqlType::Int4,
            "tb_vec_binary_quantize" | "binary_quantize" => SqlType::Other { name: "bit".into() },
            "tb_vec_normalize" | "tb_vec_subvector" | "tb_vec_arith" => {
                let family = args.iter().rev().find_map(string_value).and_then(|n| crate::vector::VectorKind::from_name(&n))?;
                SqlType::Vector { family, dim: None }
            }
            "tb_vec_avg" | "tb_vec_sum" | "l2_normalize" | "subvector" => match arg0() {
                Some(SqlType::Vector { family, .. }) => SqlType::Vector { family, dim: None },
                _ => SqlType::Vector { family: crate::vector::VectorKind::Vector, dim: None },
            },
            "sum" | "avg" if matches!(arg0(), Some(SqlType::Vector { .. })) => match arg0() {
                Some(SqlType::Vector { family, .. }) => SqlType::Vector { family, dim: None },
                _ => unreachable!(),
            },
            // SQL Server's COUNT is an int (COUNT_BIG becomes a bigint cast).
            "count" if self.ctx.dialect == Dialect::MsSql => SqlType::Int4,
            "count" => SqlType::Int8,
            "tb_nonzero" => arg0()?,
            "sum" => match arg0() {
                Some(t) if t.is_integer() => {
                    if my {
                        SqlType::Numeric { precision: None, scale: Some(0) }
                    } else {
                        SqlType::Int8
                    }
                }
                Some(SqlType::Float4) | Some(SqlType::Float8) => SqlType::Float8,
                Some(t @ SqlType::Numeric { .. }) => t,
                _ => SqlType::Numeric { precision: None, scale: None },
            },
            "avg" => match arg0() {
                Some(SqlType::Float4) | Some(SqlType::Float8) => SqlType::Float8,
                _ if my => SqlType::Numeric { precision: None, scale: Some(4) },
                _ => SqlType::Numeric { precision: None, scale: None },
            },
            "min" | "max" | "abs" | "coalesce" | "ifnull" | "nullif" | "tb_greatest" | "tb_least" | "lag" | "lead" | "first_value"
            | "last_value" | "nth_value" | "any_value" => args.iter().find_map(|a| self.infer(a))?,
            "iif" => args.get(1).and_then(|a| self.infer(a)).or_else(|| args.get(2).and_then(|a| self.infer(a)))?,
            "tb_now" => {
                if my {
                    SqlType::Timestamp
                } else {
                    SqlType::TimestampTz
                }
            }
            "tb_current_date" => SqlType::Date,
            "tb_current_time" => SqlType::Time,
            "tb_uuid" => {
                if my {
                    SqlType::Varchar { len: Some(36) }
                } else {
                    SqlType::Uuid
                }
            }
            "tb_random" => SqlType::Float8,
            "tb_extract" => {
                if my {
                    SqlType::Int8
                } else {
                    SqlType::Numeric { precision: None, scale: None }
                }
            }
            "tb_date_trunc" => args.get(1).and_then(|a| self.infer(a)).filter(|t| t.is_temporal()).unwrap_or(SqlType::TimestampTz),
            "tb_dt_add" => arg0().unwrap_or(SqlType::Timestamp),
            "tb_dt_diff" | "tb_age" => SqlType::Interval,
            "tb_date_diff_days" | "tb_unix_timestamp" => SqlType::Int8,
            "tb_to_timestamp" => {
                if my {
                    SqlType::Timestamp
                } else {
                    SqlType::TimestampTz
                }
            }
            "length" | "octet_length" | "instr" | "tb_array_length" | "tb_cardinality" | "json_array_length" => {
                if my {
                    SqlType::Int8
                } else {
                    SqlType::Int4
                }
            }
            "row_number" | "rank" | "dense_rank" | "ntile" | "tb_nextval" | "tb_currval" | "tb_setval" | "tb_lastval"
            | "tb_last_insert_id" | "tb_connection_id" | "tb_found_rows" | "tb_row_count" => SqlType::Int8,
            "percent_rank" | "cume_dist" => SqlType::Float8,
            "round" | "trunc" | "floor" | "ceil" | "ceiling" => match arg0() {
                Some(t) if t.is_numeric() => {
                    if matches!(t, SqlType::Float4 | SqlType::Float8) {
                        SqlType::Float8
                    } else {
                        SqlType::Numeric { precision: None, scale: None }
                    }
                }
                _ => SqlType::Numeric { precision: None, scale: None },
            },
            "power" | "pow" | "sqrt" | "exp" | "ln" | "log" | "log10" | "pi" | "sin" | "cos" | "tan" | "degrees" | "radians" => {
                SqlType::Float8
            }
            "json_group_array" => {
                if f.name.to_string().contains("json") && self.pg() {
                    SqlType::Array { element: Box::new(arg0().unwrap_or(SqlType::Text)) }
                } else {
                    SqlType::Json
                }
            }
            "json_object" | "json_array" | "json_group_object" | "json_quote" | "json_set" => {
                if my {
                    SqlType::Json
                } else {
                    SqlType::Jsonb
                }
            }
            "tb_json_contains" | "tb_json_has_key" | "tb_ilike" | "tb_like" | "regexp" | "tb_regexp_ci" | "tb_true"
            | "tb_array_contains" => SqlType::Bool,
            "tb_array" | "tb_array_append" | "tb_array_cat" => SqlType::Array {
                element: Box::new(
                    args.iter()
                        .find_map(|a| self.infer(a))
                        .map(|t| match t {
                            SqlType::Array { element } => *element,
                            other => other,
                        })
                        .unwrap_or(SqlType::Text),
                ),
            },
            "tb_regclass" => SqlType::Other { name: "regclass".into() },
            "tb_regtype" => SqlType::Other { name: "oid".into() },
            "tb_void" => SqlType::Text,
            "lower"
            | "upper"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "replace"
            | "substr"
            | "substring"
            | "concat"
            | "concat_ws"
            | "group_concat"
            | "printf"
            | "format"
            | "tb_left"
            | "tb_right"
            | "tb_lpad"
            | "tb_rpad"
            | "tb_repeat"
            | "tb_reverse"
            | "tb_initcap"
            | "tb_split_part"
            | "tb_md5"
            | "tb_to_char"
            | "tb_date_format"
            | "tb_version"
            | "tb_current_schema"
            | "tb_current_database"
            | "tb_current_user"
            | "tb_setting"
            | "tb_set_config"
            | "tb_format_type"
            | "tb_obj_description"
            | "tb_col_description"
            | "tb_pg_get_expr"
            | "tb_get_constraintdef"
            | "tb_get_indexdef"
            | "tb_regexp_replace"
            | "tb_array_to_string"
            | "hex"
            | "quote"
            | "char"
            | "typeof" => SqlType::Text,
            _ => return None,
        })
    }

    /// Lower-case unquoted identifiers (PostgreSQL folding).
    fn fold_ident_in_place(&self, i: &mut Ident) {
        if self.pg() && i.quote_style.is_none() {
            i.value = i.value.to_lowercase();
        }
    }

    /// Qualify a relation name with its schema and normalize case.
    fn qualify(&mut self, name: &mut ObjectName) {
        let mut parts: Vec<Ident> = Vec::new();
        for p in &name.0 {
            match p {
                ObjectNamePart::Identifier(i) => parts.push(i.clone()),
                _ => return,
            }
        }
        for p in &mut parts {
            self.fold_ident_in_place(p);
        }
        let new_parts: Option<Vec<String>> = match parts.as_slice() {
            [t] => {
                let t = t.value.clone();
                if self.is_cte(&t) || (self.ctx.dialect == Dialect::MySql && t.eq_ignore_ascii_case("dual")) {
                    None
                } else if let Some((schema, rel)) = self.ctx.catalog.resolve_relation(&t, self.ctx.search_path) {
                    let real = match rel {
                        Relation::Table(td) => td.name.clone(),
                        Relation::View(v) => v.name.clone(),
                    };
                    Some(vec![schema, real])
                } else if self.pg() && PG_CATALOG_TABLES.contains(&t.as_str()) {
                    self.needs_pg_catalog = true;
                    Some(vec!["pg_catalog".into(), t])
                } else {
                    None
                }
            }
            [.., s, t] => {
                let s = s.value.clone();
                let t = t.value.clone();
                if s.eq_ignore_ascii_case("pg_catalog") {
                    self.needs_pg_catalog = true;
                    Some(vec!["pg_catalog".into(), t])
                } else if s.eq_ignore_ascii_case("information_schema") {
                    self.needs_info_schema = true;
                    Some(vec!["information_schema".into(), t])
                } else if self.ctx.dialect == Dialect::MsSql && s.eq_ignore_ascii_case("sys") {
                    self.needs_info_schema = true;
                    Some(vec!["sys".into(), t.to_ascii_lowercase()])
                } else {
                    let schema = self.ctx.schema_name(&s);
                    let real = self
                        .ctx
                        .catalog
                        .table(&schema, &t)
                        .map(|x| x.name.clone())
                        .or_else(|| self.ctx.catalog.view(&schema, &t).map(|v| v.name.clone()))
                        .unwrap_or(t);
                    Some(vec![schema, real])
                }
            }
            [] => None,
        };
        match new_parts {
            Some(p) => name.0 = p.iter().map(|s| ObjectNamePart::Identifier(ident(s))).collect(),
            None => name.0 = parts.into_iter().map(ObjectNamePart::Identifier).collect(),
        }
    }

    /// Give unaliased select items the column name the real database would produce.
    fn name_projection(&self, select: &mut Select) {
        for item in &mut select.projection {
            if let SelectItem::ExprWithAlias { alias, .. } = item {
                self.fold_ident_in_place(alias);
                continue;
            }
            let SelectItem::UnnamedExpr(e) = item else { continue };
            let name = if self.pg() {
                match pg_column_name(e) {
                    Some(n) => n,
                    None => continue,
                }
            } else {
                match &*e {
                    Expr::Identifier(_) | Expr::CompoundIdentifier(_) => continue,
                    // SQL Server leaves computed columns unnamed.
                    _ if self.ctx.dialect == Dialect::MsSql => String::new(),
                    other => other.to_string(),
                }
            };
            let expr = std::mem::replace(e, Expr::Value(Value::Null.with_empty_span()));
            *item = SelectItem::ExprWithAlias { expr, alias: ident(&name) };
        }
    }

    /// Replace `*` / `t.*` with explicit columns when every relation is known, so output
    /// types are known statically.
    fn expand_wildcards(&self, select: &mut Select, rels: &[Rel], merged: bool) {
        let has_wild = select.projection.iter().any(|i| matches!(i, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)));
        if !has_wild {
            return;
        }
        let mut out = Vec::new();
        for item in select.projection.drain(..) {
            match &item {
                SelectItem::Wildcard(_) if !merged && !rels.is_empty() && rels.iter().all(|r| r.known) => {
                    for r in rels {
                        for (c, _) in &r.columns {
                            out.push(SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![ident(&r.alias), ident(c)])));
                        }
                    }
                }
                SelectItem::QualifiedWildcard(ast::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
                    let q = match n.0.last() {
                        Some(ObjectNamePart::Identifier(i)) => self.fold(i),
                        _ => String::new(),
                    };
                    match rels.iter().find(|r| r.alias.eq_ignore_ascii_case(&q) && r.known) {
                        Some(r) => {
                            for (c, _) in &r.columns {
                                out.push(SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![ident(&r.alias), ident(c)])));
                            }
                        }
                        None => out.push(item),
                    }
                }
                _ => out.push(item),
            }
        }
        select.projection = out;
    }

    fn projection_types(&self, items: &[SelectItem]) -> Vec<Option<SqlType>> {
        let mut out = Vec::new();
        for item in items {
            match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => out.push(self.infer(e)),
                // Unknown wildcard width: stop static typing here.
                _ => {
                    out.clear();
                    return out;
                }
            }
        }
        out
    }

    /// Rewrite one expression node (children are already rewritten).
    fn rewrite(&mut self, e: &mut Expr) {
        let my = self.ctx.dialect == Dialect::MySql;
        let ms = self.ctx.dialect == Dialect::MsSql;
        let replacement: Option<Expr> = match e {
            // @@ROWCOUNT is an int in SQL Server.
            Expr::Identifier(i) if ms && i.quote_style.is_none() && i.value.eq_ignore_ascii_case("@@ROWCOUNT") => {
                Some(self.cast_to(tsql_global(&i.value), &SqlType::Int4))
            }
            Expr::Identifier(i) if ms && i.quote_style.is_none() && i.value.starts_with("@@") => Some(tsql_global(&i.value)),
            Expr::Identifier(i) if ms && i.quote_style.is_none() && i.value.starts_with('@') => {
                self.fail(EngineError::new("42P02", format!("Must declare the scalar variable \"{}\".", i.value)).with_mssql(137));
                None
            }
            Expr::Identifier(i) => {
                if my && i.quote_style.is_none() && i.value.starts_with('@') {
                    Some(mysql_variable(&i.value))
                } else {
                    self.fold_ident_in_place(i);
                    None
                }
            }
            Expr::CompoundIdentifier(ids) => {
                if my && ids.first().is_some_and(|i| i.quote_style.is_none() && i.value.starts_with('@')) {
                    let joined = ids.iter().map(|i| i.value.clone()).collect::<Vec<_>>().join(".");
                    Some(mysql_variable(&joined))
                } else {
                    for i in ids.iter_mut() {
                        self.fold_ident_in_place(i);
                    }
                    None
                }
            }
            Expr::Value(v) if matches!(v.value, Value::NationalStringLiteral(_)) => {
                if let Value::NationalStringLiteral(s) = &v.value {
                    v.value = Value::SingleQuotedString(s.clone());
                }
                None
            }
            Expr::Value(v) => match &v.value {
                Value::Placeholder(p) if p.starts_with('$') => {
                    if let Ok(n) = p[1..].parse::<usize>() {
                        self.max_param = self.max_param.max(n);
                        v.value = Value::Placeholder(format!("?{n}"));
                    }
                    None
                }
                Value::Placeholder(p) if p == "?" => {
                    self.max_param += 1;
                    None
                }
                Value::EscapedStringLiteral(s)
                | Value::DoubleQuotedString(s)
                | Value::NationalStringLiteral(s)
                | Value::TripleSingleQuotedString(s)
                | Value::TripleDoubleQuotedString(s) => Some(lit(s)),
                Value::DollarQuotedString(d) => Some(lit(&d.value)),
                _ => None,
            },
            Expr::Cast { expr, data_type, .. } => {
                let full = data_type.to_string();
                let type_text = full.strip_prefix("pg_catalog.").or_else(|| full.strip_prefix("PG_CATALOG.")).unwrap_or(&full).to_owned();
                let lower = type_text.to_ascii_lowercase();
                let inner = (**expr).clone();
                match lower.as_str() {
                    "regclass" => Some(call("tb_regclass", vec![inner])),
                    "regtype" => Some(call("tb_regtype", vec![inner])),
                    "regproc" | "regprocedure" | "regnamespace" | "regrole" | "oid" => Some(inner),
                    "signed" | "signed integer" => Some(self.cast_to(inner, &SqlType::Int8)),
                    "unsigned" | "unsigned integer" => Some(self.cast_to(inner, &SqlType::Int8)),
                    _ => {
                        let cat = self.ctx.catalog;
                        let path = self.ctx.search_path;
                        let parsed = SqlType::parse(&type_text, self.ctx.dialect, &|n| cat.enum_values(n, path)).ty;
                        self.hint_param(&inner, &parsed);
                        let parsed = if my && matches!(parsed, SqlType::Char { .. }) && lower == "char" { SqlType::Text } else { parsed };
                        Some(self.cast_to(inner, &parsed))
                    }
                }
            }
            Expr::Convert { expr, data_type: Some(dt), .. } => {
                let parsed = SqlType::parse_simple(&dt.to_string(), self.ctx.dialect);
                Some(self.cast_to((**expr).clone(), &parsed))
            }
            Expr::TypedString(ts) => {
                let s = match &ts.value.value {
                    Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => s.clone(),
                    other => other.to_string(),
                };
                let parsed = SqlType::parse_simple(&ts.data_type.to_string(), self.ctx.dialect);
                Some(self.cast_to(lit(&s), &parsed))
            }
            Expr::Interval(iv) => {
                let field = iv.leading_field.as_ref().map(|f| f.to_string().to_ascii_lowercase());
                let value = (*iv.value).clone();
                let text_expr = match (string_value(&value), field) {
                    (Some(s), Some(f)) => lit(&format!("{s} {f}")),
                    (Some(s), None) => lit(&s),
                    (None, Some(f)) => {
                        Expr::BinaryOp { left: Box::new(value), op: BinaryOperator::StringConcat, right: Box::new(lit(&format!(" {f}"))) }
                    }
                    (None, None) => value,
                };
                Some(self.cast_to(text_expr, &SqlType::Interval))
            }
            Expr::ILike { negated, expr, pattern, .. } => {
                let f = call("tb_ilike", vec![(**expr).clone(), (**pattern).clone()]);
                Some(if *negated { Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(f) } } else { f })
            }
            Expr::SimilarTo { negated, expr, pattern, .. } => {
                let regex = string_value(pattern).map(|p| format!("^(?:{})$", p.replace('%', ".*").replace('_', ".")));
                match regex {
                    Some(r) => {
                        let f = call("regexp", vec![lit(&r), (**expr).clone()]);
                        Some(if *negated { Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(f) } } else { f })
                    }
                    None => {
                        self.fail(EngineError::unsupported("SIMILAR TO requires a literal pattern"));
                        None
                    }
                }
            }
            Expr::RLike { negated, expr, pattern, .. } => {
                let f = call("tb_regexp_ci", vec![(**pattern).clone(), (**expr).clone()]);
                Some(if *negated { Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(f) } } else { f })
            }
            Expr::BinaryOp { left, op, right } => self.rewrite_binary(left, op, right),
            Expr::AnyOp { left, compare_op, right, .. } => {
                let contains = call("tb_array_contains", vec![(**right).clone(), (**left).clone()]);
                match compare_op {
                    BinaryOperator::Eq => Some(contains),
                    BinaryOperator::NotEq => Some(Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(contains) }),
                    _ => {
                        self.fail(EngineError::unsupported("only = ANY(...) and <> ANY(...) are supported"));
                        None
                    }
                }
            }
            Expr::AllOp { left, compare_op: BinaryOperator::NotEq, right } => Some(Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: Box::new(call("tb_array_contains", vec![(**right).clone(), (**left).clone()])),
            }),
            Expr::InList { expr, list, .. } => {
                if let Some(ty) = self.column_type(expr) {
                    for item in list.iter_mut() {
                        self.hint_param(item, &ty);
                        if needs_literal_coercion(&ty) && is_literalish(item) {
                            *item = self.cast_to(item.clone(), &ty);
                        }
                    }
                }
                None
            }
            Expr::Between { expr, low, high, .. } => {
                if let Some(ty) = self.column_type(expr) {
                    for side in [low, high] {
                        self.hint_param(side, &ty);
                        if needs_literal_coercion(&ty) && is_literalish(side) {
                            **side = self.cast_to((**side).clone(), &ty);
                        }
                    }
                }
                None
            }
            Expr::Extract { field, expr, .. } => {
                Some(call("tb_extract", vec![lit(&field.to_string().to_ascii_lowercase()), (**expr).clone()]))
            }
            Expr::Substring { expr, substring_from, substring_for, .. } => {
                let mut args = vec![(**expr).clone(), substring_from.as_deref().cloned().unwrap_or_else(|| num(1))];
                if let Some(f) = substring_for {
                    args.push((**f).clone());
                }
                Some(call("substr", args))
            }
            Expr::Position { expr, r#in } => Some(call("instr", vec![(**r#in).clone(), (**expr).clone()])),
            Expr::Trim { expr, trim_where, trim_what, trim_characters } => {
                let fname = match trim_where {
                    Some(ast::TrimWhereField::Leading) => "ltrim",
                    Some(ast::TrimWhereField::Trailing) => "rtrim",
                    _ => "trim",
                };
                let mut args = vec![(**expr).clone()];
                if let Some(w) = trim_what {
                    args.push((**w).clone());
                } else if let Some(chars) = trim_characters.as_ref().and_then(|c| c.first()) {
                    args.push(chars.clone());
                }
                Some(call(fname, args))
            }
            Expr::Ceil { expr, .. } => Some(call("ceil", vec![(**expr).clone()])),
            Expr::Floor { expr, .. } => Some(call("floor", vec![(**expr).clone()])),
            Expr::AtTimeZone { timestamp, .. } => Some((**timestamp).clone()),
            Expr::Collate { expr, .. } => Some((**expr).clone()),
            Expr::Array(arr) => Some(call("tb_array", arr.elem.clone())),
            // arr[i] (1-based) on JSON-stored arrays.
            Expr::CompoundFieldAccess { root, access_chain } => {
                let mut cur = (**root).clone();
                for access in access_chain.iter() {
                    match access {
                        ast::AccessExpr::Subscript(ast::Subscript::Index { index }) => {
                            let path = Expr::BinaryOp {
                                left: Box::new(Expr::BinaryOp {
                                    left: Box::new(lit("$[")),
                                    op: BinaryOperator::StringConcat,
                                    right: Box::new(Expr::Nested(Box::new(Expr::BinaryOp {
                                        left: Box::new(index.clone()),
                                        op: BinaryOperator::Minus,
                                        right: Box::new(num(1)),
                                    }))),
                                }),
                                op: BinaryOperator::StringConcat,
                                right: Box::new(lit("]")),
                            };
                            cur = call("json_extract", vec![call("tb_cast", vec![cur, lit("text[]")]), path]);
                        }
                        _ => {
                            self.fail(EngineError::unsupported("only array subscripts like arr[1] are supported"));
                            return;
                        }
                    }
                }
                Some(cur)
            }
            Expr::Function(f) => self.rewrite_function(f),
            Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
                let _ = inner;
                None
            }
            _ => None,
        };
        if let Some(r) = replacement {
            *e = r;
        }
    }

    /// The pgvector type family of either operand, if any.
    fn vector_family(&self, left: &Expr, right: &Expr) -> Option<crate::vector::VectorKind> {
        match (self.infer(left), self.infer(right)) {
            (Some(SqlType::Vector { family, .. }), _) | (_, Some(SqlType::Vector { family, .. })) => Some(family),
            _ => None,
        }
    }

    /// pgvector operators: distances (`<->`, `<#>`, `<=>`, `<+>`, `<~>`, `<%>`) and element-wise
    /// arithmetic or concatenation of vectors.
    fn rewrite_vector_op(&mut self, left: &mut Box<Expr>, op: &BinaryOperator, right: &mut Box<Expr>) -> Option<Expr> {
        if self.ctx.dialect != Dialect::Postgres {
            return None;
        }
        let f = match op {
            BinaryOperator::LtDashGt => "tb_vec_l2",
            BinaryOperator::Spaceship => "tb_vec_cos",
            BinaryOperator::LtCaret => "tb_vec_neg_ip",
            BinaryOperator::GtCaret => "tb_vec_l1",
            BinaryOperator::QuestionDashPipe => "tb_bit_hamming",
            BinaryOperator::QuestionDoublePipe => "tb_bit_jaccard",
            BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply | BinaryOperator::StringConcat => {
                let family = self.vector_family(left, right)?;
                let sym = match op {
                    BinaryOperator::Plus => "+",
                    BinaryOperator::Minus => "-",
                    BinaryOperator::Multiply => "*",
                    _ => "||",
                };
                return Some(call("tb_vec_arith", vec![(**left).clone(), (**right).clone(), lit(sym), lit(family.name())]));
            }
            _ => return None,
        };
        // Parameters on the other side of a vector column are vectors too.
        if f.starts_with("tb_vec") {
            if let Some(SqlType::Vector { family, .. }) = self.infer(left) {
                self.hint_param(right, &SqlType::Vector { family, dim: None });
            } else if let Some(SqlType::Vector { family, .. }) = self.infer(right) {
                self.hint_param(left, &SqlType::Vector { family, dim: None });
            }
        }
        Some(call(f, vec![(**left).clone(), (**right).clone()]))
    }

    fn rewrite_binary(&mut self, left: &mut Box<Expr>, op: &BinaryOperator, right: &mut Box<Expr>) -> Option<Expr> {
        let my = self.ctx.dialect == Dialect::MySql;
        if let Some(e) = self.rewrite_vector_op(left, op, right) {
            return Some(e);
        }
        match op {
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => {
                if let Some(ty) = self.column_type(left) {
                    self.hint_param(right, &ty);
                    if needs_literal_coercion(&ty) && is_literalish(right) {
                        **right = self.cast_to((**right).clone(), &ty);
                    }
                } else if let Some(ty) = self.column_type(right) {
                    self.hint_param(left, &ty);
                    if needs_literal_coercion(&ty) && is_literalish(left) {
                        **left = self.cast_to((**left).clone(), &ty);
                    }
                }
                None
            }
            BinaryOperator::Spaceship => Some(Expr::IsNotDistinctFrom(left.clone(), right.clone())),
            // psql writes `x OPERATOR(pg_catalog.~) 'pattern'`.
            BinaryOperator::PGCustomBinaryOperator(parts) => {
                let op = parts.last().cloned().unwrap_or_default();
                let mapped = match op.as_str() {
                    "~" => BinaryOperator::PGRegexMatch,
                    "~*" => BinaryOperator::PGRegexIMatch,
                    "!~" => BinaryOperator::PGRegexNotMatch,
                    "!~*" => BinaryOperator::PGRegexNotIMatch,
                    "~~" => BinaryOperator::PGLikeMatch,
                    "~~*" => BinaryOperator::PGILikeMatch,
                    "=" => BinaryOperator::Eq,
                    "<>" | "!=" => BinaryOperator::NotEq,
                    "<" => BinaryOperator::Lt,
                    "<=" => BinaryOperator::LtEq,
                    ">" => BinaryOperator::Gt,
                    ">=" => BinaryOperator::GtEq,
                    "||" => BinaryOperator::StringConcat,
                    _ => {
                        self.fail(EngineError::unsupported(format!("operator {op} is not supported by the TwinLite engine")));
                        return None;
                    }
                };
                self.rewrite_binary(left, &mapped, right)
                    .or_else(|| Some(Expr::BinaryOp { left: left.clone(), op: mapped, right: right.clone() }))
            }
            BinaryOperator::PGRegexMatch => Some(call("regexp", vec![(**right).clone(), (**left).clone()])),
            BinaryOperator::PGRegexIMatch => Some(call("tb_regexp_ci", vec![(**right).clone(), (**left).clone()])),
            BinaryOperator::PGRegexNotMatch => {
                Some(Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(call("regexp", vec![(**right).clone(), (**left).clone()])) })
            }
            BinaryOperator::PGRegexNotIMatch => Some(Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: Box::new(call("tb_regexp_ci", vec![(**right).clone(), (**left).clone()])),
            }),
            BinaryOperator::Regexp => Some(call("tb_regexp_ci", vec![(**right).clone(), (**left).clone()])),
            BinaryOperator::PGLikeMatch => Some(call("tb_like", vec![(**left).clone(), (**right).clone()])),
            BinaryOperator::PGILikeMatch => Some(call("tb_ilike", vec![(**left).clone(), (**right).clone()])),
            BinaryOperator::PGNotLikeMatch => {
                Some(Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(call("tb_like", vec![(**left).clone(), (**right).clone()])) })
            }
            BinaryOperator::PGNotILikeMatch => {
                Some(Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(call("tb_ilike", vec![(**left).clone(), (**right).clone()])) })
            }
            BinaryOperator::PGStartsWith => Some(call(
                "tb_like",
                vec![(**left).clone(), Expr::BinaryOp { left: right.clone(), op: BinaryOperator::StringConcat, right: Box::new(lit("%")) }],
            )),
            BinaryOperator::HashArrow => Some(call("tb_json_path", vec![(**left).clone(), (**right).clone()])),
            BinaryOperator::HashLongArrow => {
                Some(call("json_extract", vec![call("tb_json_path", vec![(**left).clone(), (**right).clone()]), lit("$")]))
            }
            BinaryOperator::AtArrow => Some(call("tb_json_contains", vec![(**left).clone(), (**right).clone()])),
            BinaryOperator::ArrowAt => Some(call("tb_json_contains", vec![(**right).clone(), (**left).clone()])),
            BinaryOperator::Question => Some(call("tb_json_has_key", vec![(**left).clone(), (**right).clone()])),
            BinaryOperator::MyIntegerDivide => Some(Expr::Cast {
                kind: ast::CastKind::Cast,
                expr: Box::new(Expr::BinaryOp { left: left.clone(), op: BinaryOperator::Divide, right: right.clone() }),
                data_type: ast::DataType::Integer(None),
                format: None,
            }),
            BinaryOperator::Divide if my => Some(Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: left.clone(),
                    op: BinaryOperator::Multiply,
                    right: Box::new(Expr::Value(Value::Number("1.0".into(), false).with_empty_span())),
                }),
                op: BinaryOperator::Divide,
                right: right.clone(),
            }),
            // PostgreSQL and SQL Server fail on a zero divisor (SQLite returns NULL).
            BinaryOperator::Divide | BinaryOperator::Modulo if !my && !is_nonzero_literal(right) => {
                Some(Expr::BinaryOp { left: left.clone(), op: op.clone(), right: Box::new(call("tb_nonzero", vec![(**right).clone()])) })
            }
            // T-SQL `+` concatenates strings.
            BinaryOperator::Plus
                if self.ctx.dialect == Dialect::MsSql
                    && (matches!(self.infer(left), Some(SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. }))
                        || matches!(self.infer(right), Some(SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. }))) =>
            {
                Some(Expr::BinaryOp { left: left.clone(), op: BinaryOperator::StringConcat, right: right.clone() })
            }
            BinaryOperator::Plus | BinaryOperator::Minus => {
                let lt = self.infer(left);
                let rt = self.infer(right);
                let sign = if matches!(op, BinaryOperator::Minus) { -1 } else { 1 };
                match (&lt, &rt) {
                    (Some(l), Some(SqlType::Interval)) if l.is_temporal() || l == &SqlType::Text => {
                        Some(call("tb_dt_add", vec![(**left).clone(), (**right).clone(), num(sign)]))
                    }
                    (Some(SqlType::Interval), Some(r)) if sign > 0 && r.is_temporal() => {
                        Some(call("tb_dt_add", vec![(**right).clone(), (**left).clone(), num(1)]))
                    }
                    (Some(SqlType::Date), Some(r)) if r.is_integer() => {
                        Some(call("tb_dt_add", vec![(**left).clone(), (**right).clone(), num(sign)]))
                    }
                    (Some(SqlType::Date), Some(SqlType::Date)) if sign < 0 => {
                        Some(call("tb_date_diff_days", vec![(**left).clone(), (**right).clone()]))
                    }
                    (Some(l), Some(r)) if sign < 0 && l.is_temporal() && r.is_temporal() => {
                        Some(call("tb_dt_diff", vec![(**left).clone(), (**right).clone()]))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// T-SQL built-in functions (run before the shared mappings).
    fn rewrite_tsql_function(&mut self, name: &str, args: &[Expr]) -> Option<Expr> {
        let a = |i: usize| args.get(i).cloned().unwrap_or_else(|| Expr::Value(Value::Null.with_empty_span()));
        let text_arg = |i: usize| match args.get(i) {
            Some(Expr::Value(v)) => match &v.value {
                Value::SingleQuotedString(s) | Value::NationalStringLiteral(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        };
        let mut sub = |sql: String| -> Option<Expr> {
            self.needs_info_schema = true;
            sqlparser::parser::Parser::new(&sqlparser::dialect::MsSqlDialect {}).try_with_sql(&sql).ok()?.parse_expr().ok()
        };
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        match name {
            "getdate" | "sysdatetime" | "sysutcdatetime" | "getutcdate" | "sysdatetimeoffset" | "current_timestamp" => {
                Some(call("tb_now", vec![]))
            }
            "newid" | "newsequentialid" => Some(call("tb_uuid", vec![])),
            "isnull" => Some(call("coalesce", vec![a(0), a(1)])),
            // LEN ignores trailing spaces.
            "len" => Some(call("length", vec![call("rtrim", vec![a(0)])])),
            "datalength" => Some(call("length", vec![a(0)])),
            "scope_identity" | "ident_current" => Some(call("tb_last_insert_id", vec![])),
            "db_name" => Some(call("tb_current_database", vec![])),
            "user_name" | "suser_sname" | "suser_name" | "original_login" => Some(call("tb_current_user", vec![])),
            "schema_name" if args.is_empty() => Some(lit("dbo")),
            "schema_name" => sub(format!("(SELECT name FROM sys.schemas WHERE schema_id = {})", a(0))),
            "schema_id" if args.is_empty() => Some(Expr::Value(Value::Number("1".into(), false).with_empty_span())),
            "schema_id" => sub(format!("(SELECT schema_id FROM sys.schemas WHERE name = {})", a(0))),
            "object_id" => {
                let full = text_arg(0)?;
                let parts: Vec<String> = full.split('.').map(|p| p.trim().trim_matches(['[', ']', '"']).to_owned()).collect();
                let (schema, object) = match parts.as_slice() {
                    [.., s, o] => (s.clone(), o.clone()),
                    [o] => ("dbo".to_owned(), o.clone()),
                    _ => return Some(Expr::Value(Value::Null.with_empty_span())),
                };
                sub(format!(
                    "(SELECT o.object_id FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE lower(o.name) = lower({}) AND lower(s.name) = lower({}))",
                    quote(&object),
                    quote(&schema)
                ))
            }
            "serverproperty" => Some(match text_arg(0).map(|p| p.to_ascii_lowercase()).as_deref() {
                Some("productversion") => lit("16.0.1000.6"),
                Some("productlevel") => lit("RTM"),
                Some("edition") => lit("TwinBase mock (TwinLite)"),
                Some("engineedition") => Expr::Value(Value::Number("3".into(), false).with_empty_span()),
                Some("machinename") | Some("servername") | Some("instancename") => lit("twinbase"),
                Some("collation") => lit("SQL_Latin1_General_CP1_CI_AS"),
                _ => Expr::Value(Value::Null.with_empty_span()),
            }),
            _ => None,
        }
    }

    /// pgvector (PostgreSQL), MySQL 9 and SQL Server 2025 vector functions.
    fn rewrite_vector_function(&mut self, name: &str, args: &[Expr]) -> Option<Expr> {
        use crate::vector::VectorKind;
        let a = |i: usize| args.get(i).cloned().unwrap_or_else(|| Expr::Value(Value::Null.with_empty_span()));
        let family = || match args.first().and_then(|e| self.infer(e)) {
            Some(SqlType::Vector { family, .. }) => family,
            _ => VectorKind::Vector,
        };
        let metric = |m: &str| match m.to_ascii_lowercase().as_str() {
            "cosine" => Some("tb_vec_cos"),
            "euclidean" | "l2" => Some("tb_vec_l2"),
            "dot" => Some("tb_vec_neg_ip"),
            "manhattan" | "l1" => Some("tb_vec_l1"),
            _ => None,
        };
        Some(match (self.ctx.dialect, name) {
            (_, "l2_distance") => call("tb_vec_l2", vec![a(0), a(1)]),
            (_, "inner_product") => call("tb_vec_ip", vec![a(0), a(1)]),
            (_, "cosine_distance") => call("tb_vec_cos", vec![a(0), a(1)]),
            (_, "l1_distance") => call("tb_vec_l1", vec![a(0), a(1)]),
            (_, "vector_dims") | (Dialect::MySql, "vector_dim") => call("tb_vec_dims", vec![a(0)]),
            (_, "vector_norm") | (_, "l2_norm") => call("tb_vec_norm", vec![a(0)]),
            (_, "l2_normalize") => call("tb_vec_normalize", vec![a(0), lit(family().name())]),
            (_, "subvector") => call("tb_vec_subvector", vec![a(0), a(1), a(2), lit(family().name())]),
            (_, "binary_quantize") => call("tb_vec_binary_quantize", vec![a(0)]),
            (_, "hamming_distance") => call("tb_bit_hamming", vec![a(0), a(1)]),
            (_, "jaccard_distance") => call("tb_bit_jaccard", vec![a(0), a(1)]),
            (_, "avg") if matches!(args.first().and_then(|e| self.infer(e)), Some(SqlType::Vector { .. })) => {
                call("tb_vec_avg", vec![a(0)])
            }
            (_, "sum") if matches!(args.first().and_then(|e| self.infer(e)), Some(SqlType::Vector { .. })) => {
                call("tb_vec_sum", vec![a(0)])
            }
            // MySQL 9: STRING_TO_VECTOR, VECTOR_TO_STRING, DISTANCE(a, b, 'COSINE').
            (Dialect::MySql, "string_to_vector" | "to_vector") => {
                self.cast_to(a(0), &SqlType::Vector { family: VectorKind::Vector, dim: None })
            }
            (Dialect::MySql, "vector_to_string" | "from_vector") => a(0),
            (Dialect::MySql, "distance") => call(metric(&string_value(&a(2)).unwrap_or_default())?, vec![a(0), a(1)]),
            // SQL Server 2025: VECTOR_DISTANCE('cosine', a, b).
            (Dialect::MsSql, "vector_distance") => call(metric(&string_value(&a(0)).unwrap_or_default())?, vec![a(1), a(2)]),
            _ => return None,
        })
    }

    fn rewrite_function(&mut self, f: &mut Function) -> Option<Expr> {
        let name = func_name(f);
        let args = func_args(f);
        let my = self.ctx.dialect == Dialect::MySql;
        let a = |i: usize| args.get(i).cloned().unwrap_or_else(|| Expr::Value(Value::Null.with_empty_span()));
        let rename = |f: &mut Function, to: &str| {
            f.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(to))]);
        };
        // Strip schema qualification (pg_catalog.foo).
        if f.name.0.len() > 1 {
            let last = f.name.0.last().cloned();
            if let Some(l) = last {
                f.name = ObjectName(vec![l]);
            }
        }
        // ARRAY(subquery) -> JSON array of the subquery's single column.
        if name == "array"
            && let FunctionArguments::Subquery(q) = &f.args
        {
            let sql = format!("SELECT (WITH \"_tb_arr\"(v) AS ({q}) SELECT json_group_array(v) FROM \"_tb_arr\")");
            if let Ok(mut stmts) = sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &sql)
                && let Some(Statement::Query(q)) = stmts.pop()
                && let SetExpr::Select(sel) = *q.body
                && let Some(SelectItem::UnnamedExpr(e)) = sel.projection.into_iter().next()
            {
                return Some(e);
            }
        }
        if self.ctx.dialect == Dialect::MsSql && name == "count_big" {
            let mut count = f.clone();
            rename(&mut count, "count");
            return Some(self.cast_to(Expr::Function(count), &SqlType::Int8));
        }
        if self.ctx.dialect == Dialect::MsSql
            && let Some(e) = self.rewrite_tsql_function(&name, &args)
        {
            return Some(e);
        }
        if let Some(e) = self.rewrite_vector_function(&name, &args) {
            return Some(e);
        }
        match name.as_str() {
            "now"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
            | "current_timestamp"
            | "localtimestamp"
            | "sysdate"
            | "localtime"
            | "utc_timestamp" => Some(call("tb_now", vec![])),
            "current_date" | "curdate" | "utc_date" => Some(call("tb_current_date", vec![])),
            "current_time" | "curtime" | "utc_time" => Some(call("tb_current_time", vec![])),
            "gen_random_uuid" | "uuid_generate_v4" | "uuid" => Some(call("tb_uuid", vec![])),
            "random" | "rand" => Some(call("tb_random", vec![])),
            "version" => Some(call("tb_version", vec![])),
            "current_schema" => Some(call("tb_current_schema", vec![])),
            "current_database" | "database" | "schema" if !(name == "schema" && !my) => Some(call("tb_current_database", vec![])),
            "current_user" | "session_user" | "user" | "system_user" | "current_role" => Some(call("tb_current_user", vec![])),
            "current_schemas" => Some(lit("{public}")),
            "pg_backend_pid" | "connection_id" => Some(call("tb_connection_id", vec![])),
            "current_setting" => Some(call("tb_setting", args.clone())),
            "set_config" => Some(call("tb_set_config", args.clone())),
            "nextval" | "currval" | "setval" => {
                let mut v = args.clone();
                if let Some(Expr::Function(inner)) = v.first()
                    && func_name(inner) == "tb_regclass"
                {
                    v[0] = func_args(inner).first().cloned().unwrap_or_else(|| lit(""));
                }
                Some(call(&format!("tb_{name}"), v))
            }
            "lastval" => Some(call("tb_lastval", vec![])),
            "last_insert_id" => Some(call("tb_last_insert_id", args.clone())),
            "found_rows" => Some(call("tb_found_rows", vec![])),
            "row_count" => Some(call("tb_row_count", vec![])),
            "string_agg" => {
                rename(f, "group_concat");
                None
            }
            "group_concat" => {
                if let FunctionArguments::List(list) = &mut f.args {
                    let mut sep = None;
                    list.clauses.retain(|c| match c {
                        FunctionArgumentClause::Separator(v) => {
                            sep = Some(v.value.clone());
                            false
                        }
                        _ => true,
                    });
                    let sep_text = match sep {
                        Some(Value::SingleQuotedString(s)) | Some(Value::DoubleQuotedString(s)) => s,
                        _ => ",".into(),
                    };
                    if list.args.len() == 1 {
                        list.args.push(FunctionArg::Unnamed(FunctionArgExpr::Expr(lit(&sep_text))));
                    }
                }
                None
            }
            "array_agg" | "json_agg" | "jsonb_agg" => {
                rename(f, "json_group_array");
                None
            }
            "json_object_agg" | "jsonb_object_agg" => {
                rename(f, "json_group_object");
                None
            }
            "json_build_object" | "jsonb_build_object" => Some(call("json_object", args.clone())),
            "json_build_array" | "jsonb_build_array" => Some(call("json_array", args.clone())),
            "to_json" | "to_jsonb" => Some(call("json_quote", args.clone())),
            "jsonb_array_length" => Some(call("json_array_length", args.clone())),
            "json_unquote" => Some(call("json_extract", vec![a(0), lit("$")])),
            "json_length" => Some(call("json_array_length", args.clone())),
            "json_contains" if my => Some(call("tb_json_contains", args.clone())),
            "greatest" if my => Some(call("max", args.clone())),
            "least" if my => Some(call("min", args.clone())),
            "greatest" => Some(call("tb_greatest", args.clone())),
            "least" => Some(call("tb_least", args.clone())),
            "char_length" | "character_length" => Some(call("length", args.clone())),
            "lcase" => Some(call("lower", args.clone())),
            "ucase" => Some(call("upper", args.clone())),
            "strpos" => Some(call("instr", vec![a(0), a(1)])),
            "locate" => Some(call("instr", vec![a(1), a(0)])),
            "btrim" => Some(call("trim", args.clone())),
            "left" | "right" | "split_part" | "lpad" | "rpad" | "repeat" | "reverse" | "initcap" | "md5" | "regexp_replace" | "to_char"
            | "date_trunc" | "age" | "array_append" | "array_cat" | "array_to_string" | "cardinality" => {
                Some(call(&format!("tb_{name}"), args.clone()))
            }
            "array_length" | "array_upper" => Some(call("tb_array_length", args.clone())),
            "array_lower" => Some(call(
                "iif",
                vec![Expr::IsNull(Box::new(call("tb_array_length", args.clone()))), Expr::Value(Value::Null.with_empty_span()), num(1)],
            )),
            "date_part" => Some(call("tb_extract", args.clone())),
            "to_timestamp" if args.len() == 1 => Some(call("tb_to_timestamp", args.clone())),
            "from_unixtime" => Some(call("tb_to_timestamp", vec![a(0)])),
            "unix_timestamp" => Some(call("tb_unix_timestamp", args.clone())),
            "date_format" => Some(call("tb_date_format", vec![a(0), a(1)])),
            "date_add" | "adddate" => Some(call("tb_dt_add", vec![a(0), a(1), num(1)])),
            "date_sub" | "subdate" => Some(call("tb_dt_add", vec![a(0), a(1), num(-1)])),
            "datediff" => Some(call("tb_date_diff_days", vec![a(0), a(1)])),
            "date" if my => Some(self.cast_to(a(0), &SqlType::Date)),
            "year" | "month" | "day" | "dayofmonth" | "hour" | "minute" | "second" | "quarter" if my => {
                let field = if name == "dayofmonth" { "day".to_owned() } else { name.clone() };
                Some(self.cast_to(call("tb_extract", vec![lit(&field), a(0)]), &SqlType::Int8))
            }
            "dayofweek" if my => Some(Expr::BinaryOp {
                left: Box::new(self.cast_to(call("tb_extract", vec![lit("dow"), a(0)]), &SqlType::Int8)),
                op: BinaryOperator::Plus,
                right: Box::new(num(1)),
            }),
            "if" => Some(call("iif", args.clone())),
            "isnull" if my => Some(Expr::IsNull(Box::new(a(0)))),
            "sleep" | "pg_sleep" => Some(call("tb_pg_sleep", vec![a(0)])),
            "pg_table_is_visible"
            | "has_table_privilege"
            | "has_schema_privilege"
            | "has_database_privilege"
            | "has_column_privilege"
            | "has_any_column_privilege"
            | "has_function_privilege"
            | "has_sequence_privilege"
            | "pg_has_role"
            | "pg_try_advisory_lock"
            | "pg_try_advisory_xact_lock"
            | "pg_try_advisory_lock_shared"
            | "pg_advisory_unlock"
            | "pg_advisory_unlock_shared"
            | "pg_type_is_visible"
            | "pg_function_is_visible" => Some(call("tb_true", args.clone())),
            // No partitioning: a relation is its only ancestor.
            "pg_partition_ancestors" | "pg_partition_root" => Some(call("tb_regclass", vec![a(0)])),
            "pg_is_in_recovery" | "pg_relation_is_publishable" => Some(Expr::Value(Value::Boolean(false).with_empty_span())),
            "pg_advisory_lock" | "pg_advisory_xact_lock" | "pg_advisory_lock_shared" | "pg_advisory_unlock_all" | "pg_notify" => {
                Some(call("tb_void", args.clone()))
            }
            "format_type" => Some(call("tb_format_type", vec![a(0), a(1)])),
            "obj_description" | "shobj_description" => Some(call("tb_obj_description", args.clone())),
            "col_description" => Some(call("tb_col_description", vec![a(0), a(1)])),
            "pg_get_expr" => Some(call("tb_pg_get_expr", args.clone())),
            "pg_get_constraintdef" => Some(call("tb_get_constraintdef", args.clone())),
            "pg_get_indexdef" => Some(call("tb_get_indexdef", args.clone())),
            "pg_get_userbyid" => Some(lit("twinbase")),
            "pg_encoding_to_char" => Some(lit("UTF8")),
            n if n.starts_with("pg_get_")
                && !matches!(
                    n,
                    "pg_get_expr" | "pg_get_constraintdef" | "pg_get_indexdef" | "pg_get_userbyid" | "pg_get_serial_sequence"
                ) =>
            {
                Some(call("tb_null", args.clone()))
            }
            "pg_get_serial_sequence" => Some(Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::BinaryOp {
                        left: Box::new(lit("public.")),
                        op: BinaryOperator::StringConcat,
                        right: Box::new(a(0)),
                    }),
                    op: BinaryOperator::StringConcat,
                    right: Box::new(Expr::BinaryOp { left: Box::new(lit("_")), op: BinaryOperator::StringConcat, right: Box::new(a(1)) }),
                }),
                op: BinaryOperator::StringConcat,
                right: Box::new(lit("_seq")),
            }),
            "txid_current" | "pg_current_xact_id" => Some(num(1)),
            "pg_postmaster_start_time" => Some(call("tb_now", vec![])),
            "inet_server_addr" | "inet_client_addr" => Some(lit("127.0.0.1")),
            "inet_server_port" => Some(num(5432)),
            "pg_total_relation_size" | "pg_relation_size" | "pg_table_size" | "pg_indexes_size" | "pg_database_size" => Some(num(8192)),
            "pg_size_pretty" => Some(lit("8192 bytes")),
            "bool_and" | "every" => {
                rename(f, "min");
                None
            }
            "bool_or" => {
                rename(f, "max");
                None
            }
            "ceiling" => {
                rename(f, "ceil");
                None
            }
            "pow" => {
                rename(f, "power");
                None
            }
            _ => None,
        }
    }
}

fn mysql_variable(raw: &str) -> Expr {
    if let Some(user_var) = raw.strip_prefix('@').filter(|r| !r.starts_with('@')) {
        return call("tb_setting", vec![lit(&format!("@{}", user_var.trim_matches('`'))), num(1)]);
    }
    let name = raw
        .trim_start_matches("@@")
        .trim_start_matches("session.")
        .trim_start_matches("SESSION.")
        .trim_start_matches("global.")
        .trim_start_matches("GLOBAL.")
        .trim_start_matches("local.")
        .to_ascii_lowercase();
    call("tb_setting", vec![lit(&name)])
}

fn numeric_promote(a: Option<SqlType>, b: Option<SqlType>) -> Option<SqlType> {
    let rank = |t: &SqlType| match t {
        SqlType::Int1 | SqlType::Int2 => 1,
        SqlType::Int4 => 2,
        SqlType::Int8 => 3,
        SqlType::Numeric { .. } => 4,
        SqlType::Float4 => 5,
        SqlType::Float8 => 6,
        _ => 0,
    };
    match (a, b) {
        (Some(x), Some(y)) => {
            let (rx, ry) = (rank(&x), rank(&y));
            if rx == 0 || ry == 0 {
                return if x.is_temporal() { Some(x) } else { None };
            }
            let top = if rx >= ry { x } else { y };
            Some(match top {
                SqlType::Int1 | SqlType::Int2 => SqlType::Int4,
                SqlType::Numeric { .. } => SqlType::Numeric { precision: None, scale: None },
                SqlType::Float4 => SqlType::Float8,
                other => other,
            })
        }
        (Some(x), None) | (None, Some(x)) if x.is_numeric() => Some(x),
        _ => None,
    }
}

/// PostgreSQL's FigureColname for an unaliased select item. None = keep as is.
fn pg_column_name(e: &Expr) -> Option<String> {
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => None,
        _ => Some(pg_name_of(e).unwrap_or_else(|| "?column?".into())),
    }
}

fn pg_name_of(e: &Expr) -> Option<String> {
    match e {
        Expr::Identifier(i) => Some(if i.quote_style.is_none() { i.value.to_lowercase() } else { i.value.clone() }),
        Expr::CompoundIdentifier(ids) => ids.last().map(|i| if i.quote_style.is_none() { i.value.to_lowercase() } else { i.value.clone() }),
        Expr::Function(f) => Some(func_name(f)),
        Expr::Cast { expr, data_type, .. } => pg_name_of(expr).or_else(|| {
            let t = SqlType::parse_simple(&data_type.to_string(), Dialect::Postgres);
            Some(match t {
                SqlType::Varchar { .. } => "varchar".into(),
                SqlType::Char { .. } => "bpchar".into(),
                other => other.pg_udt_name(),
            })
        }),
        Expr::Nested(inner) => pg_name_of(inner),
        Expr::Case { .. } => Some("case".into()),
        Expr::Exists { .. } => Some("exists".into()),
        Expr::Array(_) => Some("array".into()),
        Expr::Extract { .. } => Some("extract".into()),
        Expr::Substring { .. } => Some("substring".into()),
        Expr::Position { .. } => Some("position".into()),
        Expr::Trim { .. } => Some("btrim".into()),
        Expr::Ceil { .. } => Some("ceil".into()),
        Expr::Floor { .. } => Some("floor".into()),
        Expr::TypedString(ts) => Some(SqlType::parse_simple(&ts.data_type.to_string(), Dialect::Postgres).pg_udt_name()),
        Expr::Interval(_) => Some("interval".into()),
        Expr::Collate { expr, .. } => pg_name_of(expr),
        _ => None,
    }
}

impl VisitorMut for Rewriter<'_, '_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        self.query_depth += 1;
        let mut names = Vec::new();
        if let Some(w) = &mut q.with {
            for c in &mut w.cte_tables {
                self.fold_ident_in_place(&mut c.alias.name);
                names.push(c.alias.name.value.clone());
            }
        }
        self.ctes.push(names);
        q.locks.clear();
        q.for_clause = None;
        if self.ctx.dialect == Dialect::MsSql {
            tsql_limits(q);
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        // LIMIT/OFFSET placeholders are integers.
        if let Some(lc) = &q.limit_clause {
            let exprs: Vec<Expr> = match lc {
                ast::LimitClause::LimitOffset { limit, offset, .. } => {
                    limit.iter().cloned().chain(offset.iter().map(|o| o.value.clone())).collect()
                }
                ast::LimitClause::OffsetCommaLimit { offset, limit } => vec![offset.clone(), limit.clone()],
            };
            for e in exprs {
                self.hint_param(&e, &SqlType::Int8);
            }
        }
        self.ctes.pop();
        self.query_depth -= 1;
        self.closed_select = None;
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, s: &mut Select) -> ControlFlow<()> {
        if let Some(ast::Distinct::On(_)) = s.distinct {
            self.fail(EngineError::unsupported("SELECT DISTINCT ON is not supported by the TwinLite engine"));
        }
        let (rels, merged) = self.build_scope(&s.from);
        self.name_projection(s);
        self.expand_wildcards(s, &rels, merged);
        self.scopes.push(rels);
        ControlFlow::Continue(())
    }

    fn post_visit_select(&mut self, s: &mut Select) -> ControlFlow<()> {
        if self.query_depth == 1 && self.projection.is_none() {
            self.projection = Some(self.projection_types(&s.projection));
        }
        self.closed_select = self.scopes.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, name: &mut ObjectName) -> ControlFlow<()> {
        self.qualify(name);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, tf: &mut TableFactor) -> ControlFlow<()> {
        // Set-returning functions in FROM: unnest(array) and generate_series(...) become derived
        // tables over SQLite's json_each / generate_series, with PostgreSQL's column naming.
        if let TableFactor::Table { name, args: Some(args), alias, with_ordinality, .. } = tf {
            let fname = match name.0.last() {
                Some(ObjectNamePart::Identifier(i)) => i.value.to_ascii_lowercase(),
                _ => String::new(),
            };
            let already = alias.as_ref().is_some_and(|a| a.name.value == "_tb_srf");
            if matches!(fname.as_str(), "unnest" | "generate_series" | "pg_partition_ancestors") && !already {
                let alias_name = alias.as_ref().map(|a| self.fold(&a.name)).unwrap_or_else(|| fname.clone());
                let col = alias
                    .as_ref()
                    .and_then(|a| a.columns.first().map(|c| self.fold(&c.name)))
                    .unwrap_or_else(|| if alias.is_some() { alias_name.clone() } else { fname.clone() });
                let arg_list = args.args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
                let source = match fname.as_str() {
                    "unnest" => format!("json_each(tb_cast({arg_list}, 'text[]'))"),
                    // No partitioning: a relation is its only ancestor.
                    "pg_partition_ancestors" => format!("(SELECT {arg_list} AS value, 1 AS key)"),
                    _ => format!("generate_series({arg_list})"),
                };
                let ordinality = if *with_ordinality {
                    let name =
                        alias.as_ref().and_then(|a| a.columns.get(1).map(|c| self.fold(&c.name))).unwrap_or_else(|| "ordinality".into());
                    let expr = if fname == "generate_series" { "rowid" } else { "key + 1" };
                    format!(", {expr} AS {}", quote_ident(&name))
                } else {
                    String::new()
                };
                let sql = format!("SELECT value AS {}{ordinality} FROM {source} AS _tb_srf", quote_ident(&col));
                let parsed = sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &sql);
                if let Ok(mut stmts) = parsed
                    && let Some(Statement::Query(q)) = stmts.pop()
                {
                    *tf = TableFactor::Derived {
                        lateral: false,
                        subquery: q,
                        alias: Some(ast::TableAlias { explicit: true, name: ident(&alias_name), columns: vec![], at: None }),
                        sample: None,
                    };
                }
            }
        }
        // SQLite has no column alias lists on derived tables: `(subquery) AS t(a, b)` becomes
        // `(WITH _tb_d(a, b) AS (subquery) SELECT * FROM _tb_d) AS t`.
        if let TableFactor::Derived { subquery, alias: Some(alias), .. } = tf
            && !alias.columns.is_empty()
        {
            let cols: Vec<String> = alias.columns.iter().map(|c| quote_ident(&self.fold(&c.name))).collect();
            let wrapped = format!("WITH \"_tb_d\"({}) AS ({subquery}) SELECT * FROM \"_tb_d\"", cols.join(", "));
            let parsed = match self.ctx.dialect {
                Dialect::MySql => sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::MySqlDialect {}, &wrapped),
                _ => sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &wrapped),
            };
            if let Ok(mut stmts) = parsed
                && let Some(Statement::Query(q)) = stmts.pop()
            {
                **subquery = *q;
                alias.columns.clear();
            }
        }
        let alias = match tf {
            TableFactor::Table { alias, .. } | TableFactor::Derived { alias, .. } | TableFactor::Function { alias, .. } => alias.as_mut(),
            _ => None,
        };
        if let Some(a) = alias
            && self.ctx.dialect == Dialect::Postgres
            && a.name.quote_style.is_none()
        {
            a.name.value = a.name.value.to_lowercase();
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        self.rewrite(e);
        ControlFlow::Continue(())
    }
}

fn finish_plan(rw: &mut Rewriter<'_, '_>, sql: String, kind: StmtKind, tag: &str) -> Result<SqlPlan> {
    rw.finish()?;
    let mut plan = SqlPlan::new(sql, kind, tag);
    plan.projection = rw.projection.take().unwrap_or_default();
    plan.params = std::mem::take(&mut rw.params);
    plan.param_count = rw.max_param;
    plan.needs_pg_catalog = rw.needs_pg_catalog;
    plan.needs_info_schema = rw.needs_info_schema;
    Ok(plan)
}

pub fn translate_query(mut stmt: Statement, ctx: &Ctx<'_>) -> Result<SqlPlan> {
    let mut rw = Rewriter::new(ctx);
    let _ = stmt.visit(&mut rw);
    let sql = stmt.to_string();
    finish_plan(&mut rw, sql, StmtKind::Select, "SELECT")
}

/// Translate a query used as a sub-part (INSERT ... SELECT, CREATE VIEW, CTAS).
pub fn translate_subquery(query: &mut Query, ctx: &Ctx<'_>) -> Result<SqlPlan> {
    let mut rw = Rewriter::new(ctx);
    let _ = query.visit(&mut rw);
    let sql = query.to_string();
    finish_plan(&mut rw, sql, StmtKind::Select, "SELECT")
}

fn rewrite_expr(rw: &mut Rewriter<'_, '_>, e: &mut Expr) {
    let _ = e.visit(rw);
}

fn is_default_keyword(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(i) if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("default"))
}

fn returning_sql(
    rw: &mut Rewriter<'_, '_>,
    items: Option<Vec<SelectItem>>,
    target: &TableDef,
    alias: &str,
) -> (String, Vec<Option<SqlType>>) {
    let Some(items) = items else { return (String::new(), Vec::new()) };
    let mut out_items = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                for c in &target.columns {
                    out_items.push(SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![ident(alias), ident(&c.name)])));
                }
            }
            other => out_items.push(other),
        }
    }
    let mut fake = Select {
        select_token: ast::helpers::attached_token::AttachedToken::empty(),
        optimizer_hints: vec![],
        distinct: None,
        select_modifiers: None,
        top: None,
        top_before_distinct: false,
        projection: out_items,
        exclude: None,
        into: None,
        from: vec![],
        lateral_views: vec![],
        prewhere: None,
        selection: None,
        connect_by: vec![],
        group_by: ast::GroupByExpr::Expressions(vec![], vec![]),
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
        named_window: vec![],
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        flavor: ast::SelectFlavor::Standard,
    };
    rw.name_projection(&mut fake);
    for item in &mut fake.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => rewrite_expr(rw, e),
            _ => {}
        }
    }
    let hints = rw.projection_types(&fake.projection);
    let text = fake.projection.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    (format!(" RETURNING {text}"), hints)
}

fn target_rel(alias: &str, td: &TableDef) -> Rel {
    Rel::from_table(alias, td)
}

pub fn translate_insert(ins: Insert, ctx: &Ctx<'_>) -> Result<SqlPlan> {
    let my = ctx.dialect == Dialect::MySql;
    let table_name = match &ins.table {
        TableObject::TableName(n) => n.clone(),
        _ => return Err(EngineError::unsupported("INSERT INTO a table function is not supported")),
    };
    let (schema, table) = ctx.resolve_name(&table_name, true)?;
    let td =
        ctx.catalog.table(&schema, &table).ok_or_else(|| EngineError::unsupported(format!("cannot insert into view \"{table}\"")))?.clone();
    let alias = ins.table_alias.as_ref().map(|a| ctx.fold(&a.alias)).unwrap_or_else(|| td.name.clone());
    let mut rw = Rewriter::new(ctx);

    // Target columns.
    let mut columns: Vec<String> = Vec::new();
    let mut assignment_values: Option<Vec<Expr>> = None;
    if !ins.assignments.is_empty() {
        let mut vals = Vec::new();
        for a in &ins.assignments {
            let name = match &a.target {
                ast::AssignmentTarget::ColumnName(n) => super::object_last(n, ctx),
                _ => return Err(EngineError::unsupported("tuple assignment in INSERT ... SET")),
            };
            columns.push(name);
            vals.push(a.value.clone());
        }
        assignment_values = Some(vals);
    } else if ins.columns.is_empty() {
        columns = td.columns.iter().map(|c| c.name.clone()).collect();
    } else {
        for c in &ins.columns {
            columns.push(super::object_last(c, ctx));
        }
    }
    let mut col_defs = Vec::new();
    for c in &columns {
        let def = td.column(c).ok_or_else(|| {
            if my {
                EngineError::new(state::UNDEFINED_COLUMN, format!("Unknown column '{c}' in 'field list'")).with_mysql(1054)
            } else {
                EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{c}\" of relation \"{table}\" does not exist"))
            }
        })?;
        col_defs.push(def.clone());
    }

    let wrap_value = |rw: &mut Rewriter<'_, '_>, mut e: Expr, col: &crate::catalog::ColumnDef| -> Result<String> {
        if is_default_keyword(&e) {
            return Ok(col.default.as_ref().map(|d| d.sql.clone()).unwrap_or_else(|| "NULL".into()));
        }
        if !my && matches!(col.identity, Some(Identity::Always { .. })) {
            return Err(EngineError::new("428C9", format!("cannot insert a non-DEFAULT value into column \"{}\"", col.name))
                .with_detail(format!("Column \"{}\" is an identity column defined as GENERATED ALWAYS.", col.name))
                .with_hint("Use OVERRIDING SYSTEM VALUE to override."));
        }
        rw.hint_param(&e, &col.ty);
        // Literals are validated now, like PostgreSQL's parse-time coercion: a bad literal must
        // fail before column defaults (nextval) run.
        if let (Expr::Value(v), true) = (&e, col.ty.needs_coercion())
            && let Value::SingleQuotedString(text) = &v.value
        {
            crate::value::coerce(rusqlite::types::ValueRef::Text(text.as_bytes()), &col.ty, ctx.dialect)?;
        }
        rewrite_expr(rw, &mut e);
        let mut s = e.to_string();
        if col.ty.needs_coercion() && tb_cast_type(&e).is_none() {
            s = format!("tb_cast({s}, '{}')", ctx.type_spec(&col.ty).replace('\'', "''"));
        }
        if let (true, Some(Identity::AutoIncrement { sequence })) = (my, &col.identity) {
            s = format!("COALESCE(NULLIF({s}, 0), tb_nextval('{}.{}'))", schema.replace('\'', "''"), sequence.replace('\'', "''"));
        }
        Ok(s)
    };

    let source_sql = if let Some(vals) = assignment_values {
        let mut parts = Vec::new();
        for (v, col) in vals.into_iter().zip(&col_defs) {
            parts.push(wrap_value(&mut rw, v, col)?);
        }
        format!("VALUES ({})", parts.join(", "))
    } else {
        match ins.source {
            None => "DEFAULT VALUES".to_owned(),
            Some(q) => {
                let q = *q;
                match (*q.body, q.with.is_none() && q.order_by.is_none() && q.limit_clause.is_none()) {
                    (SetExpr::Values(values), true) => {
                        let mut rows = Vec::new();
                        for (n, row) in values.rows.into_iter().enumerate() {
                            let row = row.content;
                            if row.len() != col_defs.len() {
                                return Err(if my {
                                    EngineError::new(
                                        state::SYNTAX_ERROR,
                                        format!("Column count doesn't match value count at row {}", n + 1),
                                    )
                                    .with_mysql(1136)
                                } else if row.len() > col_defs.len() {
                                    EngineError::syntax("INSERT has more expressions than target columns")
                                } else {
                                    EngineError::syntax("INSERT has more target columns than expressions")
                                });
                            }
                            let mut parts = Vec::new();
                            for (e, col) in row.into_iter().zip(&col_defs) {
                                parts.push(wrap_value(&mut rw, e, col)?);
                            }
                            rows.push(format!("({})", parts.join(", ")));
                        }
                        format!("VALUES {}", rows.join(", "))
                    }
                    (body, _) => {
                        let mut query = Query { body: Box::new(body), ..q };
                        let sub = translate_subquery(&mut query, ctx)?;
                        let names: Vec<String> = (0..col_defs.len()).map(|i| format!("_c{i}")).collect();
                        let exprs: Vec<String> = col_defs
                            .iter()
                            .zip(&names)
                            .map(|(c, n)| {
                                let mut s = n.clone();
                                if c.ty.needs_coercion() {
                                    s = format!("tb_cast({s}, '{}')", ctx.type_spec(&c.ty).replace('\'', "''"));
                                }
                                if let (true, Some(Identity::AutoIncrement { sequence })) = (my, &c.identity) {
                                    s = format!("COALESCE(NULLIF({s}, 0), tb_nextval('{schema}.{sequence}'))");
                                }
                                s
                            })
                            .collect();
                        rw.max_param = rw.max_param.max(sub.param_count);
                        rw.params.extend(sub.params);
                        format!("WITH _tb_src({}) AS ({}) SELECT {} FROM _tb_src WHERE true", names.join(", "), sub.sql, exprs.join(", "))
                    }
                }
            }
        }
    };

    // Conflict handling.
    rw.push_scope(vec![target_rel(&alias, &td), Rel { alias: "excluded".into(), ..target_rel("excluded", &td) }]);
    let mut conflict = String::new();
    let assignments_sql = |rw: &mut Rewriter<'_, '_>, assignments: Vec<ast::Assignment>, row_alias: Option<String>| -> Result<String> {
        let mut parts = Vec::new();
        for a in assignments {
            let col = match &a.target {
                ast::AssignmentTarget::ColumnName(n) => super::object_last(n, ctx),
                _ => return Err(EngineError::unsupported("tuple assignment in ON CONFLICT")),
            };
            let def =
                td.column(&col).ok_or_else(|| EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{col}\" does not exist")))?;
            let mut value = a.value;
            if my {
                value = replace_values_fn(value, row_alias.as_deref());
            }
            rewrite_expr(rw, &mut value);
            let mut s = value.to_string();
            if def.ty.needs_coercion() && tb_cast_type(&value).is_none() {
                s = format!("tb_cast({s}, '{}')", ctx.type_spec(&def.ty).replace('\'', "''"));
            }
            parts.push(format!("{} = {s}", quote_ident(&def.name)));
        }
        Ok(parts.join(", "))
    };
    match ins.on {
        Some(OnInsert::OnConflict(oc)) => {
            let target = match oc.conflict_target {
                Some(ast::ConflictTarget::Columns(cols)) => {
                    format!(" ({})", cols.iter().map(|c| quote_ident(&ctx.fold(c))).collect::<Vec<_>>().join(", "))
                }
                Some(ast::ConflictTarget::OnConstraint(name)) => {
                    let n = super::object_last(&name, ctx);
                    let cols = td
                        .primary_key
                        .iter()
                        .chain(td.uniques.iter())
                        .find(|k| k.name == n)
                        .map(|k| k.columns.clone())
                        .or_else(|| td.indexes.iter().find(|i| i.name == n && i.unique).map(|i| i.columns.clone()))
                        .ok_or_else(|| {
                            EngineError::new(state::UNDEFINED_OBJECT, format!("constraint \"{n}\" for table \"{table}\" does not exist"))
                        })?;
                    format!(" ({})", cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", "))
                }
                None => String::new(),
            };
            match oc.action {
                OnConflictAction::DoNothing => conflict = format!(" ON CONFLICT{target} DO NOTHING"),
                OnConflictAction::DoUpdate(du) => {
                    let set = assignments_sql(&mut rw, du.assignments, None)?;
                    conflict = format!(" ON CONFLICT{target} DO UPDATE SET {set}");
                    if let Some(mut w) = du.selection {
                        rewrite_expr(&mut rw, &mut w);
                        conflict.push_str(&format!(" WHERE {w}"));
                    }
                }
            }
        }
        Some(OnInsert::DuplicateKeyUpdate(assignments)) => {
            let row_alias = ins.insert_alias.as_ref().map(|a| a.row_alias.to_string());
            let set = assignments_sql(&mut rw, assignments, row_alias)?;
            conflict = format!(" ON CONFLICT DO UPDATE SET {set}");
        }
        Some(_) => return Err(EngineError::unsupported("unsupported ON conflict clause")),
        None => {}
    }
    let (returning, hints) = returning_sql(&mut rw, ins.returning, &td, &alias);
    let verb = if ins.replace_into {
        "REPLACE"
    } else if ins.ignore {
        "INSERT OR IGNORE"
    } else {
        "INSERT"
    };
    let alias_sql = if ins.table_alias.is_some() { format!(" AS {}", quote_ident(&alias)) } else { String::new() };
    let cols_sql = if source_sql == "DEFAULT VALUES" {
        String::new()
    } else {
        format!(
            " ({})",
            columns
                .iter()
                .map(|c| quote_ident(&td.column(c).map(|d| d.name.clone()).unwrap_or_else(|| c.clone())))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let sql =
        format!("{verb} INTO {}.{}{alias_sql}{cols_sql} {source_sql}{conflict}{returning}", quote_ident(&schema), quote_ident(&td.name));
    let mut plan = finish_plan(&mut rw, sql, StmtKind::Insert, "INSERT")?;
    plan.projection = hints;
    plan.target = Some((schema.clone(), td.name.clone()));
    // MySQL AUTO_INCREMENT and SQL Server IDENTITY move past explicitly inserted ids.
    let ms = ctx.dialect == Dialect::MsSql;
    if my || ms {
        plan.autoinc = td.columns.iter().find_map(|c| match &c.identity {
            Some(Identity::AutoIncrement { sequence }) => {
                Some((format!("{schema}.{sequence}"), schema.clone(), td.name.clone(), c.name.clone()))
            }
            Some(Identity::ByDefault { sequence }) if ms => {
                Some((format!("{schema}.{sequence}"), schema.clone(), td.name.clone(), c.name.clone()))
            }
            _ => None,
        });
    }
    Ok(plan)
}

/// MySQL `VALUES(col)` and `alias.col` in ON DUPLICATE KEY UPDATE → `excluded.col`.
fn replace_values_fn(e: Expr, row_alias: Option<&str>) -> Expr {
    struct V<'a>(Option<&'a str>);
    impl VisitorMut for V<'_> {
        type Break = ();
        fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
            match e {
                Expr::Function(f) if func_name(f) == "values" => {
                    if let Some(Expr::Identifier(col)) = func_args(f).first() {
                        *e = Expr::CompoundIdentifier(vec![Ident::new("excluded"), col.clone()]);
                    }
                }
                Expr::CompoundIdentifier(ids) if ids.len() == 2 => {
                    if let Some(a) = self.0
                        && ids[0].value.eq_ignore_ascii_case(a)
                    {
                        ids[0] = Ident::new("excluded");
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e;
    let _ = e.visit(&mut V(row_alias));
    e
}

pub fn translate_update(upd: Update, ctx: &Ctx<'_>) -> Result<SqlPlan> {
    let my = ctx.dialect == Dialect::MySql;
    if !upd.table.joins.is_empty() {
        return Err(EngineError::unsupported("multi-table UPDATE is not supported by the TwinLite engine yet"));
    }
    let (name, alias) = match &upd.table.relation {
        TableFactor::Table { name, alias, .. } => (name.clone(), alias.as_ref().map(|a| ctx.fold(&a.name))),
        _ => return Err(EngineError::unsupported("UPDATE target must be a table")),
    };
    let (schema, table) = ctx.resolve_name(&name, true)?;
    let td = ctx.catalog.table(&schema, &table).ok_or_else(|| EngineError::unsupported(format!("cannot update view \"{table}\"")))?.clone();
    let alias_name = alias.clone().unwrap_or_else(|| td.name.clone());
    let mut rw = Rewriter::new(ctx);
    let mut rels = vec![target_rel(&alias_name, &td)];
    let mut from_sql = String::new();
    if let Some(from) = upd.from {
        let mut tables = match from {
            ast::UpdateTableFromKind::BeforeSet(t) | ast::UpdateTableFromKind::AfterSet(t) => t,
        };
        let (extra, _) = rw.build_scope(&tables);
        rels.extend(extra);
        for t in &mut tables {
            let _ = t.visit(&mut rw);
        }
        from_sql = format!(" FROM {}", tables.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", "));
    }
    rw.push_scope(rels);
    let mut sets = Vec::new();
    let mut assigned = Vec::new();
    for a in upd.assignments {
        let col = match &a.target {
            ast::AssignmentTarget::ColumnName(n) => super::object_last(n, ctx),
            _ => return Err(EngineError::unsupported("tuple assignment in UPDATE")),
        };
        let def = td.column(&col).ok_or_else(|| {
            if my {
                EngineError::new(state::UNDEFINED_COLUMN, format!("Unknown column '{col}' in 'field list'")).with_mysql(1054)
            } else {
                EngineError::new(state::UNDEFINED_COLUMN, format!("column \"{col}\" of relation \"{table}\" does not exist"))
            }
        })?;
        let mut value = a.value;
        let sql = if is_default_keyword(&value) {
            def.default.as_ref().map(|d| d.sql.clone()).unwrap_or_else(|| "NULL".into())
        } else {
            rw.hint_param(&value, &def.ty);
            rewrite_expr(&mut rw, &mut value);
            let s = value.to_string();
            if def.ty.needs_coercion() && tb_cast_type(&value).is_none() {
                format!("tb_cast({s}, '{}')", ctx.type_spec(&def.ty).replace('\'', "''"))
            } else {
                s
            }
        };
        assigned.push(def.name.clone());
        sets.push(format!("{} = {sql}", quote_ident(&def.name)));
    }
    if my {
        for c in td.columns.iter().filter(|c| c.on_update_now && !assigned.contains(&c.name)) {
            sets.push(format!("{} = tb_cast(tb_now(), '{}')", quote_ident(&c.name), ctx.type_spec(&c.ty)));
        }
    }
    let mut where_sql = String::new();
    if let Some(mut sel) = upd.selection {
        rewrite_expr(&mut rw, &mut sel);
        where_sql = format!(" WHERE {sel}");
    }
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&td.name));
    if !upd.order_by.is_empty() || upd.limit.is_some() {
        let mut order = String::new();
        if !upd.order_by.is_empty() {
            let mut items = upd.order_by.clone();
            for o in &mut items {
                rewrite_expr(&mut rw, &mut o.expr);
            }
            order = format!(" ORDER BY {}", items.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", "));
        }
        let limit = upd.limit.map(|l| format!(" LIMIT {l}")).unwrap_or_default();
        where_sql = format!(" WHERE rowid IN (SELECT rowid FROM {qualified}{where_sql}{order}{limit})");
    }
    let (returning, hints) = returning_sql(&mut rw, upd.returning, &td, &alias_name);
    let alias_sql = alias.map(|a| format!(" AS {}", quote_ident(&a))).unwrap_or_default();
    let sql = format!("UPDATE {qualified}{alias_sql} SET {}{from_sql}{where_sql}{returning}", sets.join(", "));
    let mut plan = finish_plan(&mut rw, sql, StmtKind::Update, "UPDATE")?;
    plan.projection = hints;
    plan.target = Some((schema, td.name.clone()));
    Ok(plan)
}

pub fn translate_delete(del: ast::Delete, ctx: &Ctx<'_>) -> Result<SqlPlan> {
    let from = match del.from {
        ast::FromTable::WithFromKeyword(t) | ast::FromTable::WithoutKeyword(t) => t,
    };
    if from.len() != 1 || !from[0].joins.is_empty() || !del.tables.is_empty() {
        return Err(EngineError::unsupported("multi-table DELETE is not supported by the TwinLite engine yet"));
    }
    let (name, alias) = match &from[0].relation {
        TableFactor::Table { name, alias, .. } => (name.clone(), alias.as_ref().map(|a| ctx.fold(&a.name))),
        _ => return Err(EngineError::unsupported("DELETE target must be a table")),
    };
    let (schema, table) = ctx.resolve_name(&name, true)?;
    let td =
        ctx.catalog.table(&schema, &table).ok_or_else(|| EngineError::unsupported(format!("cannot delete from view \"{table}\"")))?.clone();
    let alias_name = alias.clone().unwrap_or_else(|| td.name.clone());
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&td.name));
    let alias_sql = alias.as_ref().map(|a| format!(" AS {}", quote_ident(a))).unwrap_or_default();
    let mut rw = Rewriter::new(ctx);
    let mut rels = vec![target_rel(&alias_name, &td)];
    let mut using_sql = String::new();
    if let Some(mut using) = del.using {
        let (extra, _) = rw.build_scope(&using);
        rels.extend(extra);
        for t in &mut using {
            let _ = t.visit(&mut rw);
        }
        using_sql = using.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
    }
    rw.push_scope(rels);
    let mut where_sql = String::new();
    if let Some(mut sel) = del.selection {
        rewrite_expr(&mut rw, &mut sel);
        where_sql = format!(" WHERE {sel}");
    }
    if !using_sql.is_empty() {
        let inner_alias = quote_ident(&alias_name);
        where_sql = format!(" WHERE rowid IN (SELECT {inner_alias}.rowid FROM {qualified} AS {inner_alias}, {using_sql}{where_sql})");
    }
    if !del.order_by.is_empty() || del.limit.is_some() {
        let mut order = String::new();
        if !del.order_by.is_empty() {
            let mut items = del.order_by.clone();
            for o in &mut items {
                rewrite_expr(&mut rw, &mut o.expr);
            }
            order = format!(" ORDER BY {}", items.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", "));
        }
        let limit = del.limit.map(|l| format!(" LIMIT {l}")).unwrap_or_default();
        where_sql = format!(" WHERE rowid IN (SELECT rowid FROM {qualified}{alias_sql}{where_sql}{order}{limit})");
    }
    let (returning, hints) = returning_sql(&mut rw, del.returning, &td, &alias_name);
    let alias_part = if using_sql.is_empty() { alias_sql } else { String::new() };
    let sql = format!("DELETE FROM {qualified}{alias_part}{where_sql}{returning}");
    let mut plan = finish_plan(&mut rw, sql, StmtKind::Delete, "DELETE")?;
    plan.projection = hints;
    plan.target = Some((schema, td.name.clone()));
    Ok(plan)
}

/// Rewrite a standalone expression (defaults, CHECK constraints) with the given columns in scope.
pub fn translate_expr(e: &Expr, ctx: &Ctx<'_>, scope: Vec<Rel>) -> Result<(String, Option<SqlType>)> {
    let mut rw = Rewriter::new(ctx);
    rw.push_scope(scope);
    let mut e = e.clone();
    rewrite_expr(&mut rw, &mut e);
    rw.finish()?;
    let ty = rw.infer(&e);
    Ok((e.to_string(), ty))
}

pub(crate) fn rel_for_columns(alias: &str, cols: &[(String, SqlType)]) -> Rel {
    Rel { alias: alias.to_owned(), table: alias.to_owned(), columns: cols.to_vec(), known: true }
}

/// T-SQL `@@` system functions.
fn tsql_global(name: &str) -> Expr {
    match name.to_ascii_uppercase().as_str() {
        "@@ROWCOUNT" => call("tb_row_count", vec![]),
        "@@IDENTITY" => call("tb_last_insert_id", vec![]),
        "@@VERSION" => call("tb_version", vec![]),
        "@@SPID" => call("tb_connection_id", vec![]),
        "@@TRANCOUNT" | "@@ERROR" => Expr::Value(Value::Number("0".into(), false).with_empty_span()),
        "@@SERVERNAME" | "@@SERVICENAME" => lit("twinbase"),
        "@@LANGUAGE" => lit("us_english"),
        _ => Expr::Value(Value::Null.with_empty_span()),
    }
}

/// `TOP n` and `OFFSET … FETCH NEXT n ROWS ONLY` become SQLite's LIMIT/OFFSET.
fn tsql_limits(q: &mut Query) {
    let mut limit: Option<Expr> = None;
    if let SetExpr::Select(sel) = q.body.as_mut()
        && let Some(top) = sel.top.take()
    {
        limit = top.quantity.map(|qty| match qty {
            ast::TopQuantity::Expr(e) => e,
            ast::TopQuantity::Constant(n) => Expr::Value(Value::Number(n.to_string(), false).with_empty_span()),
        });
    }
    if let Some(f) = q.fetch.take() {
        limit = f.quantity.or(limit);
    }
    // `OFFSET n ROWS`: SQLite has no ROWS keyword.
    let offset = match &q.limit_clause {
        Some(ast::LimitClause::LimitOffset { offset, .. }) => offset.clone().map(|mut o| {
            o.rows = ast::OffsetRows::None;
            o
        }),
        _ => None,
    };
    if limit.is_some() || offset.is_some() {
        let existing = match &q.limit_clause {
            Some(ast::LimitClause::LimitOffset { limit: l, .. }) => l.clone(),
            _ => None,
        };
        // SQLite needs a LIMIT before OFFSET.
        let limit =
            limit.or(existing).or_else(|| offset.as_ref().map(|_| Expr::Value(Value::Number("-1".into(), false).with_empty_span())));
        q.limit_clause = Some(ast::LimitClause::LimitOffset { limit, offset, limit_by: vec![] });
    }
}

/// T-SQL `OUTPUT INSERTED.x` / `DELETED.x` is SQLite's RETURNING (new values for inserts and
/// updates, old values for deletes).
pub fn tsql_output_to_returning(stmt: Statement) -> Result<Statement> {
    fn convert(output: Option<ast::OutputClause>, allowed: &str) -> Result<Option<Vec<SelectItem>>> {
        let Some(ast::OutputClause::Output { select_items, into_table: None, .. }) = output else {
            return match output {
                None => Ok(None),
                Some(_) => Err(EngineError::unsupported("OUTPUT … INTO is not supported by the TwinLite engine")),
            };
        };
        let mut items = Vec::new();
        for it in select_items {
            items.push(match it {
                SelectItem::QualifiedWildcard(_, _) => SelectItem::Wildcard(Default::default()),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) if ids.len() == 2 => {
                    check_pseudo(&ids[0].value, allowed)?;
                    SelectItem::UnnamedExpr(Expr::Identifier(ids[1].clone()))
                }
                SelectItem::ExprWithAlias { expr: Expr::CompoundIdentifier(ids), alias } if ids.len() == 2 => {
                    check_pseudo(&ids[0].value, allowed)?;
                    SelectItem::ExprWithAlias { expr: Expr::Identifier(ids[1].clone()), alias }
                }
                other => other,
            });
        }
        Ok(Some(items))
    }
    fn check_pseudo(name: &str, allowed: &str) -> Result<()> {
        if name.eq_ignore_ascii_case(allowed) {
            Ok(())
        } else {
            Err(EngineError::unsupported(format!("OUTPUT {name}.… is not supported here by the TwinLite engine (use {allowed}.…)")))
        }
    }
    Ok(match stmt {
        Statement::Insert(mut ins) => {
            if let Some(r) = convert(ins.output.take(), "INSERTED")? {
                ins.returning = Some(r);
            }
            Statement::Insert(ins)
        }
        Statement::Update(mut upd) => {
            if let Some(r) = convert(upd.output.take(), "INSERTED")? {
                upd.returning = Some(r);
            }
            Statement::Update(upd)
        }
        Statement::Delete(mut del) => {
            if let Some(r) = convert(del.output.take(), "DELETED")? {
                del.returning = Some(r);
            }
            Statement::Delete(del)
        }
        other => other,
    })
}

/// A numeric literal other than zero (no zero check needed).
fn is_nonzero_literal(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => matches!(&v.value, Value::Number(n, _) if n.parse::<f64>().is_ok_and(|f| f != 0.0)),
        Expr::Nested(inner) => is_nonzero_literal(inner),
        _ => false,
    }
}
