//! T-SQL batches: `GO` separators, statements without semicolons, local variables (`DECLARE`,
//! `SET @v`, `SELECT @v = …`), `IF … ELSE`, `WHILE`, `BEGIN … END` blocks, dynamic SQL
//! (`EXEC (…)`, `sp_executesql`), `RETURN`, `THROW` and `RAISERROR`.
//!
//! Control flow runs here, statement by statement; every other statement goes through the normal
//! translation path with variables replaced by literals.

use std::collections::HashMap;

use rusqlite::types::Value;
use sqlparser::ast::{self, Expr, SelectItem, SetExpr, Statement};
use sqlparser::dialect::MsSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Token};

use crate::error::{EngineError, Result};
use crate::session::{BatchResult, Outcome, Session, TxState};
use crate::translate;

/// Loops stop after this many iterations, so a script with a wrong condition cannot hang the server.
const MAX_LOOP: usize = 100_000;
/// Nesting of dynamic SQL.
const MAX_DEPTH: usize = 32;

/// Split a script at lines that hold only `GO` (the client-side batch separator).
pub fn split_go(sql: &str) -> Vec<String> {
    let mut batches = vec![String::new()];
    for line in sql.split_inclusive('\n') {
        let t = line.trim();
        let is_go = t.len() >= 2
            && t[..2].eq_ignore_ascii_case("go")
            && t[2..].trim().trim_end_matches(';').trim().chars().all(|c| c.is_ascii_digit());
        if is_go {
            batches.push(String::new());
        } else {
            batches.last_mut().unwrap().push_str(line);
        }
    }
    batches.into_iter().filter(|b| !b.trim().is_empty()).collect()
}

/// Replace `@name` references (outside strings, comments and quoted names) with `lookup`'s text;
/// `lookup` gets the lower-cased name with its `@`. `@@` system functions are left alone.
pub fn bind(sql: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let b: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '\'' || c == '[' || c == '"' {
            let end = if c == '[' { ']' } else { c };
            out.push(c);
            i += 1;
            while i < b.len() {
                out.push(b[i]);
                if b[i] == end {
                    if b.get(i + 1) == Some(&end) {
                        out.push(end);
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' {
                out.push(b[i]);
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                out.push(b[i]);
                i += 1;
            }
            continue;
        }
        if c == '@' && b.get(i + 1) == Some(&'@') {
            while i < b.len() && (b[i] == '@' || b[i].is_alphanumeric() || b[i] == '_') {
                out.push(b[i]);
                i += 1;
            }
            continue;
        }
        if c == '@' && b.get(i + 1).is_some_and(|n| n.is_alphanumeric() || *n == '_') {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$' || b[i] == '#') {
                i += 1;
            }
            let name: String = b[start..i].iter().collect();
            match lookup(&name.to_lowercase()) {
                Some(v) => out.push_str(&v),
                None => out.push_str(&name),
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// A value as a T-SQL literal.
pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) if f.is_finite() => format!("{f:?}"),
        Value::Real(_) => "NULL".into(),
        Value::Text(s) => format!("N'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("N'{}'", String::from_utf8_lossy(b).replace('\'', "''")),
    }
}

/// A parsed batch: control flow here, every other statement with its source text.
enum Node {
    Stmt(String, Box<Statement>),
    If { cond: Expr, then: Vec<Node>, otherwise: Option<Vec<Node>> },
    While { cond: Expr, body: Vec<Node> },
    Block(Vec<Node>),
    Try { body: Vec<Node>, catch: Vec<Node> },
}

/// T-SQL needs no semicolons between statements, also inside blocks; the parser's own IF, WHILE
/// and BEGIN … END handling does, so those are read here.
struct BatchParser<'a> {
    sql: &'a str,
    lines: Vec<usize>,
    p: Parser<'a>,
}

impl<'a> BatchParser<'a> {
    fn offset(&self, l: Location) -> usize {
        let Some(&start) = self.lines.get(l.line.saturating_sub(1) as usize) else { return self.sql.len() };
        self.sql[start..].char_indices().nth(l.column.saturating_sub(1) as usize).map(|(i, _)| start + i).unwrap_or(self.sql.len())
    }

    fn skip_semicolons(&mut self) {
        while self.p.consume_token(&Token::SemiColon) {}
    }

    fn at_end(&self) -> bool {
        self.p.peek_token_ref().token == Token::EOF
    }

    /// `BEGIN` that opens a block (not `BEGIN TRAN`, `BEGIN TRY`, …).
    fn at_block(&self) -> bool {
        use sqlparser::keywords::Keyword;
        self.p.peek_keyword(Keyword::BEGIN)
            && !matches!(&self.p.peek_nth_token_ref(1).token,
                Token::Word(w) if matches!(w.keyword, Keyword::TRAN | Keyword::TRANSACTION | Keyword::WORK | Keyword::TRY | Keyword::CATCH)
                    || w.value.eq_ignore_ascii_case("DISTRIBUTED"))
            && !matches!(self.p.peek_nth_token_ref(1).token, Token::SemiColon | Token::EOF)
    }

    fn node(&mut self) -> std::result::Result<Node, String> {
        use sqlparser::keywords::Keyword;
        let p = &mut self.p;
        if p.parse_keyword(Keyword::IF) {
            let cond = p.parse_expr().map_err(|e| e.to_string())?;
            let then = self.branch()?;
            self.skip_semicolons();
            let otherwise = if self.p.parse_keyword(Keyword::ELSE) { Some(self.branch()?) } else { None };
            return Ok(Node::If { cond, then, otherwise });
        }
        if p.parse_keyword(Keyword::WHILE) {
            let cond = p.parse_expr().map_err(|e| e.to_string())?;
            let body = self.branch()?;
            return Ok(Node::While { cond, body });
        }
        if p.parse_keywords(&[Keyword::BEGIN, Keyword::TRY]) {
            let body = self.list(&[Keyword::END, Keyword::TRY])?;
            if !self.p.parse_keywords(&[Keyword::BEGIN, Keyword::CATCH]) {
                return Err("BEGIN TRY must be followed by BEGIN CATCH".into());
            }
            self.skip_semicolons();
            let catch = self.list(&[Keyword::END, Keyword::CATCH])?;
            return Ok(Node::Try { body, catch });
        }
        if self.at_block() {
            let _ = self.p.parse_keyword(Keyword::BEGIN);
            return Ok(Node::Block(self.list(&[Keyword::END])?));
        }
        let start = self.offset(self.p.peek_token_ref().span.start);
        let stmt = self.p.parse_statement().map_err(|e| e.to_string())?;
        let end = self.offset(self.p.get_current_token().span.end).max(start);
        Ok(Node::Stmt(self.sql[start..end].to_owned(), Box::new(stmt)))
    }

    /// One statement or a BEGIN … END block.
    fn branch(&mut self) -> std::result::Result<Vec<Node>, String> {
        match self.node()? {
            Node::Block(nodes) => Ok(nodes),
            n => Ok(vec![n]),
        }
    }

    /// Statements up to (and including) the closing keywords.
    fn list(&mut self, close: &[sqlparser::keywords::Keyword]) -> std::result::Result<Vec<Node>, String> {
        let mut nodes = Vec::new();
        loop {
            self.skip_semicolons();
            if self.p.parse_keywords(close) {
                return Ok(nodes);
            }
            if self.at_end() {
                return Err(format!("missing {}", close.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>().join(" ")));
            }
            nodes.push(self.node()?);
        }
    }
}

/// The batch's statements, or `None` when the parser cannot read all of it (the batch then runs
/// statement by statement as before, which also covers session commands the parser does not know).
fn parse_batch(sql: &str) -> Option<Vec<Node>> {
    let dialect = MsSqlDialect {};
    let p = Parser::new(&dialect).try_with_sql(sql).ok()?;
    let lines = std::iter::once(0).chain(sql.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let mut b = BatchParser { sql, lines, p };
    let mut nodes = Vec::new();
    loop {
        b.skip_semicolons();
        if b.at_end() {
            return Some(nodes);
        }
        nodes.push(b.node().ok()?);
    }
}

enum Flow {
    Next,
    Return,
}

struct Scope {
    vars: HashMap<String, Value>,
    depth: usize,
    /// The error a CATCH block handles (for ERROR_MESSAGE() and a bare THROW).
    caught: Option<EngineError>,
}

impl Session {
    /// Run a T-SQL script (one or more `GO`-separated batches).
    pub(crate) fn execute_tsql(&mut self, sql: &str) -> BatchResult {
        let mut outcomes = Vec::new();
        for batch in split_go(sql) {
            if let Err(e) = self.tsql_batch(&batch, 0, &mut outcomes) {
                return BatchResult { outcomes, error: Some(e) };
            }
        }
        BatchResult { outcomes, error: None }
    }

    fn tsql_batch(&mut self, batch: &str, depth: usize, outcomes: &mut Vec<Outcome>) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(EngineError::unsupported("dynamic SQL is nested too deeply"));
        }
        let Some(stmts) = parse_batch(batch) else {
            // Statements the parser cannot read (some session commands): the usual path.
            for stmt in translate::split_statements(batch, self.dialect) {
                outcomes.push(self.execute_one(&stmt, &[], true)?);
            }
            return Ok(());
        };
        let mut scope = Scope { vars: HashMap::new(), depth, caught: None };
        self.tsql_block(&stmts, &mut scope, outcomes)?;
        Ok(())
    }

    fn tsql_block(&mut self, nodes: &[Node], scope: &mut Scope, outcomes: &mut Vec<Outcome>) -> Result<Flow> {
        for n in nodes {
            if let Flow::Return = self.tsql_node(n, scope, outcomes)? {
                return Ok(Flow::Return);
            }
        }
        Ok(Flow::Next)
    }

    fn tsql_node(&mut self, node: &Node, scope: &mut Scope, outcomes: &mut Vec<Outcome>) -> Result<Flow> {
        match node {
            Node::Stmt(text, stmt) => self.tsql_stmt(stmt, text, scope, outcomes),
            Node::Block(nodes) => self.tsql_block(nodes, scope, outcomes),
            Node::If { cond, then, otherwise } => {
                if self.tsql_condition(cond, scope)? {
                    self.tsql_block(then, scope, outcomes)
                } else if let Some(o) = otherwise {
                    self.tsql_block(o, scope, outcomes)
                } else {
                    Ok(Flow::Next)
                }
            }
            Node::While { cond, body } => {
                let mut n = 0;
                while self.tsql_condition(cond, scope)? {
                    n += 1;
                    if n > MAX_LOOP {
                        return Err(EngineError::unsupported(format!("WHILE loop stopped after {MAX_LOOP} iterations")));
                    }
                    if let Flow::Return = self.tsql_block(body, scope, outcomes)? {
                        return Ok(Flow::Return);
                    }
                }
                Ok(Flow::Next)
            }
            Node::Try { body, catch } => match self.tsql_block(body, scope, outcomes) {
                Ok(flow) => Ok(flow),
                Err(e) => {
                    let outer = scope.caught.replace(e);
                    let r = self.tsql_block(catch, scope, outcomes);
                    scope.caught = outer;
                    r
                }
            },
        }
    }

    fn tsql_stmt(&mut self, stmt: &Statement, text: &str, scope: &mut Scope, outcomes: &mut Vec<Outcome>) -> Result<Flow> {
        match stmt {
            Statement::Declare { stmts } => {
                for d in stmts {
                    if matches!(d.data_type, Some(ast::DataType::Table(_))) || d.for_query.is_some() {
                        return Err(EngineError::unsupported("table variables and cursors are not supported by the TwinLite engine"));
                    }
                    let value = match &d.assignment {
                        Some(a) => {
                            let e = match a {
                                ast::DeclareAssignment::Expr(e)
                                | ast::DeclareAssignment::Default(e)
                                | ast::DeclareAssignment::DuckAssignment(e)
                                | ast::DeclareAssignment::For(e)
                                | ast::DeclareAssignment::MsSqlAssignment(e) => e,
                            };
                            self.tsql_eval(e, d.data_type.as_ref(), scope)?
                        }
                        None => Value::Null,
                    };
                    for name in &d.names {
                        scope.vars.insert(name.value.to_lowercase(), value.clone());
                    }
                }
                Ok(Flow::Next)
            }
            Statement::Set(ast::Set::SingleAssignment { variable, values, .. })
                if variable.to_string().starts_with('@') && values.len() == 1 =>
            {
                let v = self.tsql_eval(&values[0], None, scope)?;
                scope.vars.insert(variable.to_string().to_lowercase(), v);
                Ok(Flow::Next)
            }
            Statement::Execute { name, parameters, .. } => {
                let proc = name.as_ref().map(|n| n.to_string().to_ascii_lowercase());
                match proc.as_deref().map(|p| p.trim_start_matches("sys.").trim_start_matches("dbo.")) {
                    // EXEC (<string expression>): its own batch, without the caller's variables.
                    None => {
                        let mut sql = String::new();
                        for p in parameters {
                            if let Value::Text(t) = self.tsql_eval(p, None, scope)? {
                                sql.push_str(&t);
                            }
                        }
                        self.tsql_batch(&sql, scope.depth + 1, outcomes)?;
                        Ok(Flow::Next)
                    }
                    Some("sp_executesql") => {
                        let sql = match parameters.first() {
                            Some(p) => match self.tsql_eval(p, None, scope)? {
                                Value::Text(t) => t,
                                _ => String::new(),
                            },
                            None => String::new(),
                        };
                        let decl = match parameters.get(1) {
                            Some(p) => match self.tsql_eval(p, None, scope)? {
                                Value::Text(t) => t,
                                _ => String::new(),
                            },
                            None => String::new(),
                        };
                        let names = declared_names(&decl);
                        let mut values = HashMap::new();
                        for (i, p) in parameters.iter().skip(2).enumerate() {
                            let (name, e) = match p {
                                Expr::BinaryOp { left, op: ast::BinaryOperator::Eq, right } if left.to_string().starts_with('@') => {
                                    (left.to_string().to_lowercase(), right.as_ref())
                                }
                                e => (names.get(i).cloned().unwrap_or_default(), e),
                            };
                            values.insert(name, literal(&self.tsql_eval(e, None, scope)?));
                        }
                        let bound = bind(&sql, |n| values.get(n).cloned());
                        self.tsql_batch(&bound, scope.depth + 1, outcomes)?;
                        Ok(Flow::Next)
                    }
                    Some(_) => self.tsql_plain(text, scope, outcomes),
                }
            }
            Statement::Return(_) => Ok(Flow::Return),
            Statement::Throw(t) => {
                let message = match &t.message {
                    Some(m) => match self.tsql_eval(m, None, scope)? {
                        Value::Null => String::new(),
                        v => value_text(&v),
                    },
                    None => {
                        return Err(match &scope.caught {
                            Some(e) => e.clone(),
                            None => EngineError::syntax("THROW without arguments is only allowed inside a CATCH block"),
                        });
                    }
                };
                let number = match &t.error_number {
                    Some(n) => match self.tsql_eval(n, None, scope)? {
                        Value::Integer(i) => i as u32,
                        _ => 50000,
                    },
                    None => 50000,
                };
                Err(EngineError::new("P0001", message).with_mssql(number))
            }
            Statement::RaisError { message, severity, arguments, .. } => {
                let severity = match self.tsql_eval(severity, None, scope)? {
                    Value::Integer(i) => i,
                    _ => 16,
                };
                let mut text = value_text(&self.tsql_eval(message, None, scope)?);
                // printf-style placeholders, in order.
                for a in arguments {
                    let v = value_text(&self.tsql_eval(a, None, scope)?);
                    if let Some(p) = ["%s", "%d", "%i", "%u"].iter().filter_map(|f| text.find(f)).min() {
                        text.replace_range(p..p + 2, &v);
                    }
                }
                if severity > 10 { Err(EngineError::new("P0001", text).with_mssql(50000)) } else { Ok(Flow::Next) }
            }
            Statement::Print(p) => {
                let text = value_text(&self.tsql_eval(&p.message, None, scope)?);
                outcomes.push(Outcome { message: Some(text), ..Outcome::command("PRINT") });
                Ok(Flow::Next)
            }
            Statement::Query(q) => {
                // SELECT @a = expr, @b = expr …: assign from the last row.
                if let SetExpr::Select(sel) = q.body.as_ref() {
                    let targets: Vec<Option<String>> = sel
                        .projection
                        .iter()
                        .map(|i| match i {
                            SelectItem::ExprWithAlias { alias, .. } if alias.value.starts_with('@') => Some(alias.value.to_lowercase()),
                            _ => None,
                        })
                        .collect();
                    if targets.iter().any(Option::is_some) {
                        if targets.iter().any(Option::is_none) {
                            return Err(EngineError::syntax(
                                "A SELECT statement that assigns a value to a variable must not be combined with data-retrieval operations.",
                            ));
                        }
                        let mut q = q.clone();
                        if let SetExpr::Select(sel) = q.body.as_mut() {
                            for item in sel.projection.iter_mut() {
                                if let SelectItem::ExprWithAlias { expr, .. } = item {
                                    *item = SelectItem::UnnamedExpr(expr.clone());
                                }
                            }
                        }
                        let o = self.execute_one(&self.tsql_bind(&q.to_string(), scope), &[], true)?;
                        if let Some(row) = o.result.as_ref().and_then(|r| r.rows.last()) {
                            for (t, v) in targets.into_iter().zip(row) {
                                scope.vars.insert(t.unwrap(), v.clone());
                            }
                        }
                        return Ok(Flow::Next);
                    }
                }
                self.tsql_plain(text, scope, outcomes)
            }
            _ => self.tsql_plain(text, scope, outcomes),
        }
    }

    /// A statement without control flow: variables become literals, then it runs as usual.
    fn tsql_plain(&mut self, text: &str, scope: &Scope, outcomes: &mut Vec<Outcome>) -> Result<Flow> {
        let bound = self.tsql_bind(text, scope);
        outcomes.push(self.execute_one(&bound, &[], true)?);
        Ok(Flow::Next)
    }

    fn tsql_bind(&self, sql: &str, scope: &Scope) -> String {
        let trancount = if self.tx_state() == TxState::Idle { 0 } else { 1 };
        let mut sql = replace_ci(sql, "@@TRANCOUNT", &trancount.to_string());
        if let Some(e) = &scope.caught {
            let number = error_number(e);
            for (f, v) in [
                ("ERROR_MESSAGE()", literal(&Value::Text(e.message.clone()))),
                ("ERROR_NUMBER()", number.to_string()),
                ("ERROR_SEVERITY()", "16".into()),
                ("ERROR_STATE()", "1".into()),
                ("ERROR_LINE()", "1".into()),
                ("ERROR_PROCEDURE()", "NULL".into()),
            ] {
                sql = replace_ci(&sql, f, &v);
            }
        }
        bind(&sql, |name| scope.vars.get(name).map(literal))
    }

    fn tsql_eval(&mut self, e: &Expr, ty: Option<&ast::DataType>, scope: &Scope) -> Result<Value> {
        let expr = match ty {
            // Typed declarations convert the value (e.g. DECLARE @n int = '5').
            Some(t) if !matches!(t, ast::DataType::Custom(..)) => format!("CAST(({e}) AS {t})"),
            _ => e.to_string(),
        };
        let o = self.execute_one(&self.tsql_bind(&format!("SELECT {expr}"), scope), &[], true)?;
        Ok(o.result.and_then(|r| r.rows.into_iter().next()).and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
    }

    fn tsql_condition(&mut self, cond: &Expr, scope: &Scope) -> Result<bool> {
        let o = self.execute_one(&self.tsql_bind(&format!("SELECT CASE WHEN {cond} THEN 1 ELSE 0 END"), scope), &[], true)?;
        Ok(matches!(o.result.and_then(|r| r.rows.into_iter().next()).and_then(|r| r.into_iter().next()), Some(Value::Integer(1))))
    }
}

/// Case-insensitive replacement outside string literals is not needed for these markers (they
/// never appear quoted in practice), so a plain scan does.
fn replace_ci(s: &str, from: &str, to: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let needle = from.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(&needle) {
        out.push_str(&s[last..i]);
        out.push_str(to);
        last = i + from.len();
    }
    out.push_str(&s[last..]);
    out
}

/// The SQL Server error number for an engine error.
pub fn error_number(e: &EngineError) -> u32 {
    if let Some(n) = e.mssql_code {
        return n;
    }
    match e.sqlstate.as_str() {
        "23505" => 2627,
        "23503" | "23514" => 547,
        "23502" => 515,
        "42P01" => 208,
        "42703" => 207,
        "42601" => 102,
        "42P07" | "42710" => 2714,
        "42883" => 195,
        "22P02" | "22007" | "22008" => 245,
        "22003" => 8115,
        "22001" => 2628,
        "22012" => 8134,
        "40001" | "40P01" => 1205,
        "25P02" | "25001" => 3902,
        "3D000" => 911,
        _ => 50000,
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Parameter names from a declaration such as `@P1 int, @P2 nvarchar(4000) OUTPUT`.
pub fn declared_names(decl: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for ch in decl.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter().filter_map(|d| d.split_whitespace().next().map(|n| n.to_lowercase())).filter(|n| n.starts_with('@')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_go_and_binds_variables() {
        assert_eq!(split_go("CREATE TABLE a (x int)\ngo\nSELECT 1\n  GO 2\n"), vec!["CREATE TABLE a (x int)\n", "SELECT 1\n"]);
        let v = |n: &str| (n == "@p1").then(|| "N'x''y'".to_owned());
        assert_eq!(
            bind("SELECT @P1, '@p1', [@p1], @@ROWCOUNT /* @p1 */ -- @p1", v),
            "SELECT N'x''y', '@p1', [@p1], @@ROWCOUNT /* @p1 */ -- @p1"
        );
        assert_eq!(declared_names("@P1 int,@P2 nvarchar(4000), @P3 decimal(10,2) OUTPUT"), vec!["@p1", "@p2", "@p3"]);
        let parsed = parse_batch("CREATE TABLE a (x int)\nINSERT INTO a VALUES (1) SELECT 'it''s' AS s;").unwrap();
        let texts: Vec<&str> = parsed
            .iter()
            .map(|n| match n {
                Node::Stmt(t, _) => t.trim(),
                _ => "",
            })
            .collect();
        assert_eq!(texts, vec!["CREATE TABLE a (x int)", "INSERT INTO a VALUES (1)", "SELECT 'it''s' AS s"]);
    }
}
