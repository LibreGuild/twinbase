//! Statement splitting and the keyword pre-parser for utility statements (SET, SHOW, BEGIN, USE,
//! DESCRIBE, GRANT, …). Clients send many dialect-specific variants of these, so they are
//! recognised by keywords instead of a full parser.

use rusqlite::types::Value;

use super::{Ctx, Plan, Rows};
use crate::Dialect;
use crate::catalog::{Identity, render_dialect_ddl};
use crate::error::{EngineError, Result, state};
use crate::types::SqlType;

/// Split a batch into statements, respecting quotes, dollar quotes and comments.
/// Statements that contain only comments/whitespace are dropped.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut has_code = false;
    let my = dialect == Dialect::MySql;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' | b'`' => {
                let escape_backslash = my || (c == b'\'' && i > 0 && (bytes[i - 1] == b'E' || bytes[i - 1] == b'e'));
                i += 1;
                while i < bytes.len() {
                    if escape_backslash && bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == c {
                        if i + 1 < bytes.len() && bytes[i + 1] == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                has_code = true;
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if my => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // MySQL executable comments `/*! ... */` contain code.
                if my && i + 2 < bytes.len() && bytes[i + 2] == b'!' {
                    has_code = true;
                }
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if !my && bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' if !my => {
                // Dollar quote: $tag$ ... $tag$ (tag may be empty, never starts with a digit).
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let tag_ok = j < bytes.len() && bytes[j] == b'$' && !(j > i + 1 && bytes[i + 1].is_ascii_digit());
                if tag_ok {
                    let tag = &sql[i..=j];
                    if let Some(end) = sql[j + 1..].find(tag) {
                        i = j + 1 + end + tag.len();
                    } else {
                        i = bytes.len();
                    }
                    has_code = true;
                } else {
                    i += 1;
                    has_code = true;
                }
            }
            b';' => {
                if has_code {
                    out.push(sql[start..i].trim().to_owned());
                }
                i += 1;
                start = i;
                has_code = false;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                has_code = true;
                i += 1;
            }
        }
    }
    if has_code && start < sql.len() {
        let s = sql[start..].trim();
        if !s.is_empty() {
            out.push(s.to_owned());
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Ident(String),
    Num(String),
    Sym(char),
}

impl Tok {
    fn upper(&self) -> String {
        match self {
            Tok::Word(w) => w.to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    fn text(&self) -> String {
        match self {
            Tok::Word(w) | Tok::Str(w) | Tok::Ident(w) | Tok::Num(w) => w.clone(),
            Tok::Sym(c) => c.to_string(),
        }
    }
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') || c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' || c == '`' {
            let q = c;
            let mut s = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && q == '\'' && i + 1 < chars.len() {
                    s.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == q {
                    if chars.get(i + 1) == Some(&q) {
                        s.push(q);
                        i += 2;
                        continue;
                    }
                    break;
                }
                s.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(if q == '\'' { Tok::Str(s) } else { Tok::Ident(s) });
        } else if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut s = String::from(c);
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Num(s));
        } else if c.is_alphanumeric() || c == '_' || c == '@' || c == '$' {
            let mut s = String::new();
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '@' || chars[i] == '$' || chars[i] == '.')
            {
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Word(s));
        } else {
            out.push(Tok::Sym(c));
            i += 1;
        }
    }
    out
}

fn noop(tag: &str) -> Result<Option<Plan>> {
    Ok(Some(Plan::Noop { tag: tag.to_owned() }))
}

/// Recognise utility statements. Returns `None` for statements that need the full parser.
pub(super) fn preparse(sql: &str, ctx: &Ctx<'_>) -> Result<Option<Plan>> {
    let toks = tokenize(sql);
    let Some(first) = toks.first() else { return Ok(Some(Plan::Noop { tag: String::new() })) };
    let w = |i: usize| toks.get(i).map(Tok::upper).unwrap_or_default();
    if ctx.dialect == Dialect::MsSql
        && let Some(plan) = preparse_tsql(&toks, ctx)?
    {
        return Ok(Some(plan));
    }
    let my = ctx.dialect == Dialect::MySql;
    match first.upper().as_str() {
        "BEGIN" if toks.len() == 1 || matches!(w(1).as_str(), "WORK" | "TRANSACTION" | "ISOLATION" | "READ" | "NOT" | "DEFERRABLE") => {
            Ok(Some(Plan::Begin { read_only: read_only_mode(&toks) }))
        }
        "START" if w(1) == "TRANSACTION" => Ok(Some(Plan::Begin { read_only: read_only_mode(&toks) })),
        "COMMIT" | "END" => {
            if w(1) == "PREPARED" {
                return Err(EngineError::unsupported("two-phase commit is not supported"));
            }
            Ok(Some(Plan::Commit))
        }
        "ROLLBACK" | "ABORT" => {
            if w(1) == "PREPARED" {
                return Err(EngineError::unsupported("two-phase commit is not supported"));
            }
            let pos = toks.iter().position(|t| t.upper() == "TO");
            match pos {
                Some(p) => {
                    let name_idx = if w(p + 1) == "SAVEPOINT" { p + 2 } else { p + 1 };
                    let name = toks.get(name_idx).map(|t| fold_tok(t, ctx)).unwrap_or_default();
                    Ok(Some(Plan::RollbackTo(name)))
                }
                None => Ok(Some(Plan::Rollback)),
            }
        }
        "SAVEPOINT" => Ok(Some(Plan::Savepoint(toks.get(1).map(|t| fold_tok(t, ctx)).unwrap_or_default()))),
        "RELEASE" => {
            let idx = if w(1) == "SAVEPOINT" { 2 } else { 1 };
            Ok(Some(Plan::Release(toks.get(idx).map(|t| fold_tok(t, ctx)).unwrap_or_default())))
        }
        "SET" => parse_set(&toks, ctx).map(Some),
        "RESET" => {
            let name = toks.get(1).map(|t| t.text().to_ascii_lowercase()).unwrap_or_default();
            Ok(Some(Plan::Set { vars: vec![(name, None)] }))
        }
        "SHOW" if !my => {
            let name: Vec<String> = toks[1..].iter().map(|t| t.text()).collect();
            let joined = name.join(" ").to_ascii_lowercase();
            let name = match joined.as_str() {
                "transaction isolation level" => "transaction_isolation".to_owned(),
                "time zone" => "timezone".to_owned(),
                other => other.to_owned(),
            };
            Ok(Some(Plan::Show { name }))
        }
        "SHOW" => mysql_show(&toks, ctx).map(Some),
        "DESCRIBE" | "DESC" if my => describe(&toks[1..], ctx).map(Some),
        "EXPLAIN" if my && toks.len() >= 2 && matches!(toks[1], Tok::Word(_) | Tok::Ident(_)) && !is_statement_keyword(&w(1)) => {
            describe(&toks[1..], ctx).map(Some)
        }
        "USE" => {
            let db = toks.get(1).map(Tok::text).unwrap_or_default();
            Ok(Some(Plan::Use { database: db }))
        }
        "DISCARD" => noop(&format!("DISCARD {}", w(1))),
        "LISTEN" => noop("LISTEN"),
        "UNLISTEN" => noop("UNLISTEN"),
        "NOTIFY" => noop("NOTIFY"),
        "DEALLOCATE" => noop(if w(1) == "ALL" || w(2) == "ALL" { "DEALLOCATE ALL" } else { "DEALLOCATE" }),
        "GRANT" => noop("GRANT"),
        "REVOKE" => noop("REVOKE"),
        "VACUUM" => noop("VACUUM"),
        "ANALYZE" | "ANALYSE" if !my => noop("ANALYZE"),
        "CHECKPOINT" => noop("CHECKPOINT"),
        "CLUSTER" => noop("CLUSTER"),
        "REINDEX" => noop("REINDEX"),
        "LOCK" => noop(if my { "LOCK TABLES" } else { "LOCK TABLE" }),
        "UNLOCK" => noop("UNLOCK TABLES"),
        "FLUSH" | "KILL" | "OPTIMIZE" | "REPAIR" | "CHECK" | "ANALYZE" | "INSTALL" | "UNINSTALL" | "PURGE" => noop(&first.upper()),
        "DO" => Err(EngineError::unsupported("anonymous code blocks (DO) are not supported by the TwinLite engine")),
        "COPY" => Err(EngineError::unsupported("COPY is not supported by the TwinLite engine yet; use INSERT statements")),
        "PREPARE" | "EXECUTE" if !my => {
            Err(EngineError::unsupported("SQL-level PREPARE/EXECUTE is not supported; use protocol-level prepared statements"))
        }
        "CREATE" | "ALTER" | "DROP" if matches!(w(1).as_str(), "ROLE" | "USER" | "GROUP") => noop(&format!("{} ROLE", first.upper())),
        "CREATE" if w(1) == "EXTENSION" => noop("CREATE EXTENSION"),
        "DROP" if w(1) == "EXTENSION" => noop("DROP EXTENSION"),
        "COMMENT" if w(2) == "EXTENSION" || w(2) == "SCHEMA" || w(2) == "DATABASE" => noop("COMMENT"),
        "ALTER" if w(1) == "DEFAULT" && w(2) == "PRIVILEGES" => noop("ALTER DEFAULT PRIVILEGES"),
        "ALTER" if matches!(w(1).as_str(), "SCHEMA" | "DATABASE" | "SEQUENCE" | "FUNCTION" | "VIEW") => noop(&format!("ALTER {}", w(1))),
        "ALTER" if w(1) == "TABLE" && toks.iter().any(|t| t.upper() == "OWNER") => noop("ALTER TABLE"),
        "CREATE" if w(1) == "SCHEMA" && my => Ok(None),
        "XA" => Err(EngineError::unsupported("XA transactions are not supported")),
        _ => Ok(None),
    }
}

fn is_statement_keyword(w: &str) -> bool {
    matches!(w, "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "ANALYZE" | "FORMAT" | "EXTENDED" | "PARTITIONS" | "REPLACE" | "TABLE")
}

fn fold_tok(t: &Tok, ctx: &Ctx<'_>) -> String {
    match t {
        Tok::Word(w) if ctx.dialect == Dialect::Postgres => w.to_lowercase(),
        other => other.text(),
    }
}

/// `SET ...` in either dialect. Returns a plan setting one or more variables.
fn parse_set(toks: &[Tok], ctx: &Ctx<'_>) -> Result<Plan> {
    let my = ctx.dialect == Dialect::MySql;
    let w = |i: usize| toks.get(i).map(Tok::upper).unwrap_or_default();
    let mut i = 1;
    // PG `SET SESSION|LOCAL`, MySQL `SET GLOBAL|SESSION|PERSIST`.
    if matches!(w(i).as_str(), "SESSION" | "LOCAL" | "GLOBAL" | "PERSIST" | "PERSIST_ONLY")
        && w(i + 1) != "CHARACTERISTICS"
        && w(i + 1) != "AUTHORIZATION"
    {
        i += 1;
    }
    match w(i).as_str() {
        "TRANSACTION" | "CHARACTERISTICS" | "SESSION" => return Ok(Plan::Noop { tag: "SET".into() }),
        "CONSTRAINTS" => return Ok(Plan::Noop { tag: "SET CONSTRAINTS".into() }),
        "ROLE" | "AUTHORIZATION" => return Ok(Plan::Noop { tag: "SET".into() }),
        "TIME" if w(i + 1) == "ZONE" => {
            let v = toks.get(i + 2).map(Tok::text);
            return Ok(Plan::Set {
                vars: vec![("timezone".into(), v.filter(|v| !v.eq_ignore_ascii_case("default") && !v.eq_ignore_ascii_case("local")))],
            });
        }
        "NAMES" => {
            let v = toks.get(i + 1).map(Tok::text).unwrap_or_else(|| "utf8mb4".into());
            return Ok(Plan::Set {
                vars: if my {
                    vec![
                        ("character_set_client".into(), Some(v.clone())),
                        ("character_set_connection".into(), Some(v.clone())),
                        ("character_set_results".into(), Some(v)),
                    ]
                } else {
                    vec![("client_encoding".into(), Some(v))]
                },
            });
        }
        "CHARACTER" | "CHARSET" => return Ok(Plan::Set { vars: vec![("character_set_client".into(), toks.last().map(Tok::text))] }),
        _ => {}
    }
    let mut vars = Vec::new();
    let rest = &toks[i..];
    // Split assignments on top-level commas (MySQL allows several; PG has value lists).
    let mut groups: Vec<Vec<Tok>> = vec![Vec::new()];
    let mut depth = 0;
    for t in rest {
        match t {
            Tok::Sym('(') => depth += 1,
            Tok::Sym(')') => depth -= 1,
            Tok::Sym(',') if depth == 0 && my => {
                groups.push(Vec::new());
                continue;
            }
            _ => {}
        }
        groups.last_mut().unwrap().push(t.clone());
    }
    for g in groups {
        if g.is_empty() {
            continue;
        }
        let eq = g.iter().position(|t| matches!(t, Tok::Sym('=')) || t.upper() == "TO");
        let Some(eq) = eq else {
            return Err(EngineError::syntax(format!("syntax error at or near \"{}\"", g[0].text())));
        };
        let mut name: String = g[..eq].iter().map(Tok::text).collect::<Vec<_>>().join("");
        if name.ends_with(':') {
            name.pop();
        }
        let name = name
            .trim_start_matches("@@session.")
            .trim_start_matches("@@SESSION.")
            .trim_start_matches("@@global.")
            .trim_start_matches("@@GLOBAL.")
            .trim_start_matches("@@local.")
            .trim_start_matches("@@")
            .to_owned();
        let name = if name.starts_with('@') { name } else { name.to_ascii_lowercase() };
        let value_toks: Vec<&Tok> = g[eq + 1..].iter().filter(|t| !matches!(t, Tok::Sym(':'))).collect();
        let value = if value_toks.len() == 1 && value_toks[0].upper() == "DEFAULT" {
            None
        } else {
            let parts: Vec<String> = value_toks.iter().filter(|t| !matches!(t, Tok::Sym(','))).map(|t| t.text()).collect();
            let v = parts.join(", ");
            Some(match v.to_ascii_uppercase().as_str() {
                "ON" | "TRUE" if my => "1".into(),
                "OFF" | "FALSE" if my => "0".into(),
                _ => v,
            })
        };
        vars.push((name, value));
    }
    Ok(Plan::Set { vars })
}

fn text(v: impl Into<String>) -> Value {
    Value::Text(v.into())
}

fn rows(columns: &[&str], rows: Vec<Vec<Value>>) -> Plan {
    Plan::Rows(Rows { columns: columns.iter().map(|c| (c.to_string(), SqlType::Text)).collect(), rows, tag: "SHOW".into() })
}

fn like_filter(toks: &[Tok]) -> Option<String> {
    toks.iter().position(|t| t.upper() == "LIKE").and_then(|p| toks.get(p + 1)).map(Tok::text)
}

fn ci_like(s: &str, pattern: &Option<String>) -> bool {
    match pattern {
        None => true,
        Some(p) => crate::functions::like_match_ci(s, p),
    }
}

fn table_and_db(toks: &[Tok], start: usize, ctx: &Ctx<'_>) -> (String, String) {
    // `<table> [FROM|IN db]` or `db.table`
    let raw = toks.get(start).map(Tok::text).unwrap_or_default();
    let mut db = ctx.search_path.first().cloned().unwrap_or_default();
    let mut table = raw.clone();
    if let Some((d, t)) = raw.split_once('.') {
        db = d.trim_matches('`').to_owned();
        table = t.trim_matches('`').to_owned();
    }
    if let Some(p) = toks.iter().skip(start + 1).position(|t| t.upper() == "FROM" || t.upper() == "IN")
        && let Some(d) = toks.get(start + 1 + p + 1)
    {
        db = d.text();
    }
    (ctx.schema_name(&db), table)
}

fn describe(toks: &[Tok], ctx: &Ctx<'_>) -> Result<Plan> {
    let (db, table) = table_and_db(toks, 0, ctx);
    show_columns(&db, &table, false, &None, ctx)
}

fn show_columns(db: &str, table: &str, full: bool, like: &Option<String>, ctx: &Ctx<'_>) -> Result<Plan> {
    let td = ctx
        .catalog
        .table(db, table)
        .ok_or_else(|| EngineError::new(state::UNDEFINED_TABLE, format!("Table '{db}.{table}' doesn't exist")).with_mysql(1146))?;
    let mut out = Vec::new();
    for c in &td.columns {
        if !ci_like(&c.name, like) {
            continue;
        }
        let key = if td.primary_key.as_ref().is_some_and(|pk| pk.columns.contains(&c.name)) {
            "PRI"
        } else if td.uniques.iter().any(|u| u.columns.first() == Some(&c.name)) {
            "UNI"
        } else if td.indexes.iter().any(|i| i.columns.first() == Some(&c.name))
            || td.foreign_keys.iter().any(|f| f.columns.first() == Some(&c.name))
        {
            "MUL"
        } else {
            ""
        };
        let mut extra = String::new();
        if matches!(c.identity, Some(Identity::AutoIncrement { .. })) {
            extra.push_str("auto_increment");
        } else if c.default.as_ref().is_some_and(|d| d.display.to_ascii_uppercase().contains("CURRENT_TIMESTAMP")) {
            extra.push_str("DEFAULT_GENERATED");
        }
        if c.on_update_now {
            extra.push_str(if extra.is_empty() { "on update CURRENT_TIMESTAMP" } else { " on update CURRENT_TIMESTAMP" });
        }
        let default = c
            .default
            .as_ref()
            .filter(|_| !matches!(c.identity, Some(Identity::AutoIncrement { .. })))
            .map(|d| {
                let disp = d.display.trim();
                text(
                    disp.strip_prefix('\'')
                        .and_then(|x| x.strip_suffix('\''))
                        .map(|x| x.replace("''", "'"))
                        .unwrap_or_else(|| disp.to_owned()),
                )
            })
            .unwrap_or(Value::Null);
        let ty = c.ty.mysql_column_type() + if c.unsigned { " unsigned" } else { "" };
        let mut row = vec![text(&c.name), text(ty)];
        if full {
            row.push(if c.ty.is_textual() { text("utf8mb4_0900_ai_ci") } else { Value::Null });
        }
        row.extend([text(if c.nullable { "YES" } else { "NO" }), text(key), default, text(extra)]);
        if full {
            row.push(text("select,insert,update,references"));
            row.push(text(c.comment.clone().unwrap_or_default()));
        }
        out.push(row);
    }
    let cols: &[&str] = if full {
        &["Field", "Type", "Collation", "Null", "Key", "Default", "Extra", "Privileges", "Comment"]
    } else {
        &["Field", "Type", "Null", "Key", "Default", "Extra"]
    };
    Ok(rows(cols, out))
}

fn mysql_show(toks: &[Tok], ctx: &Ctx<'_>) -> Result<Plan> {
    let mut i = 1;
    let w = |i: usize| toks.get(i).map(Tok::upper).unwrap_or_default();
    let mut full = false;
    let mut global = false;
    loop {
        match w(i).as_str() {
            "FULL" => full = true,
            "GLOBAL" => global = true,
            "SESSION" | "LOCAL" | "EXTENDED" => {}
            _ => break,
        }
        i += 1;
    }
    let _ = global;
    let like = like_filter(toks);
    match w(i).as_str() {
        "DATABASES" | "SCHEMAS" => {
            let mut out = vec![vec![text("information_schema")]];
            for s in ctx.catalog.schemas.keys() {
                if ci_like(s, &like) {
                    out.push(vec![text(s)]);
                }
            }
            Ok(rows(&["Database"], out))
        }
        "TABLES" => {
            let db = toks
                .iter()
                .position(|t| t.upper() == "FROM" || t.upper() == "IN")
                .and_then(|p| toks.get(p + 1))
                .map(|t| ctx.schema_name(&t.text()))
                .or_else(|| ctx.search_path.first().cloned())
                .ok_or_else(|| EngineError::new(state::INVALID_CATALOG_NAME, "No database selected").with_mysql(1046))?;
            let sd = ctx
                .catalog
                .schema(&db)
                .ok_or_else(|| EngineError::new(state::INVALID_CATALOG_NAME, format!("Unknown database '{db}'")).with_mysql(1049))?;
            let mut names: Vec<(String, &str)> = sd.tables.keys().map(|t| (t.clone(), "BASE TABLE")).collect();
            names.extend(sd.views.keys().map(|v| (v.clone(), "VIEW")));
            names.sort();
            let col = format!("Tables_in_{db}");
            let out: Vec<Vec<Value>> = names
                .into_iter()
                .filter(|(n, _)| ci_like(n, &like))
                .map(|(n, kind)| if full { vec![text(n), text(kind)] } else { vec![text(n)] })
                .collect();
            Ok(Plan::Rows(Rows {
                columns: if full { vec![(col, SqlType::Text), ("Table_type".into(), SqlType::Text)] } else { vec![(col, SqlType::Text)] },
                rows: out,
                tag: "SHOW".into(),
            }))
        }
        "COLUMNS" | "FIELDS" => {
            let from = toks.iter().position(|t| t.upper() == "FROM" || t.upper() == "IN").unwrap_or(i);
            let (db, table) = table_and_db(toks, from + 1, ctx);
            show_columns(&db, &table, full, &like, ctx)
        }
        "CREATE" => match w(i + 1).as_str() {
            "TABLE" | "VIEW" => {
                let (db, table) = table_and_db(toks, i + 2, ctx);
                if let Some(td) = ctx.catalog.table(&db, &table) {
                    Ok(rows(&["Table", "Create Table"], vec![vec![text(&td.name), text(render_dialect_ddl(&db, td, Dialect::MySql))]]))
                } else if let Some(v) = ctx.catalog.view(&db, &table) {
                    Ok(rows(
                        &["View", "Create View", "character_set_client", "collation_connection"],
                        vec![vec![
                            text(&v.name),
                            text(format!("CREATE VIEW `{}` AS {}", v.name, v.definition)),
                            text("utf8mb4"),
                            text("utf8mb4_0900_ai_ci"),
                        ]],
                    ))
                } else {
                    Err(EngineError::new(state::UNDEFINED_TABLE, format!("Table '{db}.{table}' doesn't exist")).with_mysql(1146))
                }
            }
            "DATABASE" | "SCHEMA" => {
                let db = toks.last().map(Tok::text).unwrap_or_default();
                Ok(rows(
                    &["Database", "Create Database"],
                    vec![vec![
                        text(&db),
                        text(format!("CREATE DATABASE `{db}` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci */")),
                    ]],
                ))
            }
            _ => Ok(rows(&["Value"], vec![])),
        },
        "INDEX" | "INDEXES" | "KEYS" => {
            let from = toks.iter().position(|t| t.upper() == "FROM" || t.upper() == "IN").unwrap_or(i);
            let (db, table) = table_and_db(toks, from + 1, ctx);
            let td = ctx
                .catalog
                .table(&db, &table)
                .ok_or_else(|| EngineError::new(state::UNDEFINED_TABLE, format!("Table '{db}.{table}' doesn't exist")).with_mysql(1146))?;
            let mut out = Vec::new();
            let mut push = |name: &str, cols: &[String], unique: bool| {
                for (n, c) in cols.iter().enumerate() {
                    out.push(vec![
                        text(&td.name),
                        Value::Integer(!unique as i64),
                        text(name),
                        Value::Integer((n + 1) as i64),
                        text(c),
                        text("A"),
                        Value::Integer(0),
                        Value::Null,
                        Value::Null,
                        text(if td.column(c).is_some_and(|x| x.nullable) { "YES" } else { "" }),
                        text("BTREE"),
                        text(""),
                        text(""),
                        text("YES"),
                        Value::Null,
                    ]);
                }
            };
            if let Some(pk) = &td.primary_key {
                push("PRIMARY", &pk.columns, true);
            }
            for u in &td.uniques {
                push(&u.name, &u.columns, true);
            }
            for ix in &td.indexes {
                push(&ix.name, &ix.columns, ix.unique);
            }
            for fk in &td.foreign_keys {
                if !td.indexes.iter().any(|ix| ix.name == fk.name) {
                    push(&fk.name, &fk.columns, false);
                }
            }
            let mut plan = rows(
                &[
                    "Table",
                    "Non_unique",
                    "Key_name",
                    "Seq_in_index",
                    "Column_name",
                    "Collation",
                    "Cardinality",
                    "Sub_part",
                    "Packed",
                    "Null",
                    "Index_type",
                    "Comment",
                    "Index_comment",
                    "Visible",
                    "Expression",
                ],
                out,
            );
            if let Plan::Rows(r) = &mut plan {
                r.columns[1].1 = SqlType::Int8;
                r.columns[3].1 = SqlType::Int8;
                r.columns[6].1 = SqlType::Int8;
            }
            Ok(plan)
        }
        "VARIABLES" => {
            let mut vars: Vec<(&String, &String)> = ctx.settings.iter().filter(|(k, _)| !k.starts_with('@')).collect();
            vars.sort();
            let out = vars.into_iter().filter(|(k, _)| ci_like(k, &like)).map(|(k, v)| vec![text(k), text(v)]).collect();
            Ok(rows(&["Variable_name", "Value"], out))
        }
        "STATUS" => {
            let status = [("Uptime", "1"), ("Threads_connected", "1"), ("Questions", "0"), ("Ssl_cipher", "")];
            let out = status.iter().filter(|(k, _)| ci_like(k, &like)).map(|(k, v)| vec![text(*k), text(*v)]).collect();
            Ok(rows(&["Variable_name", "Value"], out))
        }
        "WARNINGS" | "ERRORS" => Ok(rows(&["Level", "Code", "Message"], vec![])),
        "ENGINES" => Ok(rows(
            &["Engine", "Support", "Comment", "Transactions", "XA", "Savepoints"],
            vec![vec![text("InnoDB"), text("DEFAULT"), text("TwinBase mock storage"), text("YES"), text("NO"), text("YES")]],
        )),
        "CHARSET" | "CHARACTER" => Ok(rows(
            &["Charset", "Description", "Default collation", "Maxlen"],
            vec![vec![text("utf8mb4"), text("UTF-8 Unicode"), text("utf8mb4_0900_ai_ci"), Value::Integer(4)]],
        )),
        "COLLATION" => Ok(rows(
            &["Collation", "Charset", "Id", "Default", "Compiled", "Sortlen", "Pad_attribute"],
            vec![vec![
                text("utf8mb4_0900_ai_ci"),
                text("utf8mb4"),
                Value::Integer(255),
                text("Yes"),
                text("Yes"),
                Value::Integer(0),
                text("NO PAD"),
            ]],
        )),
        "PROCESSLIST" => Ok(rows(&["Id", "User", "Host", "db", "Command", "Time", "State", "Info"], vec![])),
        "GRANTS" => Ok(rows(&["Grants for root@%"], vec![vec![text("GRANT ALL PRIVILEGES ON *.* TO `root`@`%` WITH GRANT OPTION")]])),
        "TABLE" if w(i + 1) == "STATUS" => {
            let db = ctx.search_path.first().cloned().unwrap_or_default();
            let out = ctx
                .catalog
                .schema(&db)
                .map(|sd| {
                    sd.tables
                        .values()
                        .filter(|t| ci_like(&t.name, &like))
                        .map(|t| {
                            vec![
                                text(&t.name),
                                text("InnoDB"),
                                Value::Integer(10),
                                text("Dynamic"),
                                Value::Integer(0),
                                Value::Null,
                                text("utf8mb4_0900_ai_ci"),
                                text(t.comment.clone().unwrap_or_default()),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(rows(&["Name", "Engine", "Version", "Row_format", "Rows", "Auto_increment", "Collation", "Comment"], out))
        }
        "TRIGGERS" | "EVENTS" | "PLUGINS" | "PROCEDURE" | "FUNCTION" | "PRIVILEGES" | "OPEN" | "PROFILES" => {
            Ok(rows(&["Name", "Value"], vec![]))
        }
        "MASTER" | "BINARY" | "REPLICA" | "SLAVE" => Ok(rows(&["File", "Position"], vec![])),
        other => {
            Err(EngineError::syntax(format!("You have an error in your SQL syntax; SHOW {other} is not supported by the TwinBase mock")))
        }
    }
}

/// T-SQL utility statements: transactions, session options, `USE`, `PRINT`; procedural T-SQL is
/// reported as unsupported with a clear message.
fn preparse_tsql(toks: &[Tok], _ctx: &Ctx<'_>) -> Result<Option<Plan>> {
    let w = |i: usize| toks.get(i).map(Tok::upper).unwrap_or_default();
    let tran = |i: usize| matches!(w(i).as_str(), "TRAN" | "TRANSACTION");
    match w(0).as_str() {
        "BEGIN" if tran(1) || toks.len() == 1 => Ok(Some(Plan::Begin { read_only: false })),
        "BEGIN" if w(1) == "DISTRIBUTED" => {
            Err(EngineError::unsupported("distributed transactions are not supported by the TwinLite engine"))
        }
        "GOTO" => Err(EngineError::unsupported("T-SQL GOTO is not supported by the TwinLite engine")),
        "COMMIT" => Ok(Some(Plan::Commit)),
        "ROLLBACK" => Ok(Some(Plan::Rollback)),
        "SAVE" if tran(1) => Ok(Some(Plan::Savepoint(toks.get(2).map(|t| t.text().to_owned()).unwrap_or_default()))),
        "PRINT" => Ok(Some(Plan::Noop { tag: "PRINT".into() })),
        // A mock serves one database: USE is accepted and changes nothing.
        "USE" => Ok(Some(Plan::Noop { tag: "USE".into() })),
        "SET" if w(1).starts_with("SHOWPLAN") || (w(1) == "STATISTICS" && matches!(w(2).as_str(), "PROFILE" | "XML")) => {
            Err(EngineError::unsupported("query plans (SHOWPLAN, STATISTICS PROFILE) are not available on the TwinLite engine"))
        }
        // These would make later statements not run; ignoring them would run them.
        "SET" if matches!(w(1).as_str(), "FMTONLY" | "NOEXEC" | "PARSEONLY") && w(2) == "ON" => {
            Err(EngineError::unsupported(format!("SET {} ON is not supported by the TwinLite engine", w(1))))
        }
        // Session options drivers send on connect.
        "SET"
            if matches!(
                w(1).as_str(),
                "NOCOUNT"
                    | "ANSI_NULLS"
                    | "ANSI_PADDING"
                    | "ANSI_WARNINGS"
                    | "ANSI_NULL_DFLT_ON"
                    | "ANSI_NULL_DFLT_OFF"
                    | "ANSI_DEFAULTS"
                    | "QUOTED_IDENTIFIER"
                    | "ARITHABORT"
                    | "ARITHIGNORE"
                    | "CONCAT_NULL_YIELDS_NULL"
                    | "NUMERIC_ROUNDABORT"
                    | "TEXTSIZE"
                    | "DATEFORMAT"
                    | "DATEFIRST"
                    | "LANGUAGE"
                    | "LOCK_TIMEOUT"
                    | "XACT_ABORT"
                    | "IMPLICIT_TRANSACTIONS"
                    | "CURSOR_CLOSE_ON_COMMIT"
                    | "DEADLOCK_PRIORITY"
                    | "TRANSACTION"
                    | "STATISTICS"
                    | "ROWCOUNT"
                    | "FMTONLY"
                    | "NOEXEC"
                    | "PARSEONLY"
                    | "IDENTITY_INSERT"
                    | "CONTEXT_INFO"
                    | "QUERY_GOVERNOR_COST_LIMIT"
                    | "FORCEPLAN"
            ) =>
        {
            Ok(Some(Plan::Noop { tag: "SET".into() }))
        }
        _ => Ok(None),
    }
}

/// T-SQL `ALTER TABLE t ALTER COLUMN c <type> [NULL | NOT NULL]` and `ADD DEFAULT x FOR c` in the
/// standard forms the parser understands. As in SQL Server, leaving out NOT NULL makes the column nullable.
pub fn tsql_alter_column(sql: &str) -> Option<(String, Option<String>)> {
    use sqlparser::keywords::Keyword;
    use sqlparser::tokenizer::{Token, Tokenizer};
    let tokens: Vec<Token> = Tokenizer::new(&sqlparser::dialect::MsSqlDialect {}, sql)
        .tokenize()
        .ok()?
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::SemiColon | Token::EOF))
        .collect();
    let kw = |i: usize, k: Keyword| matches!(tokens.get(i), Some(Token::Word(w)) if w.keyword == k && w.quote_style.is_none());
    if !(kw(0, Keyword::ALTER) && kw(1, Keyword::TABLE)) {
        return None;
    }
    let text = |ts: &[&Token]| ts.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" ");
    // ADD [CONSTRAINT name] DEFAULT expr FOR column.
    if let Some(add) = (2..tokens.len()).find(|&i| kw(i, Keyword::ADD)) {
        let mut d = add + 1;
        let mut name = None;
        if kw(d, Keyword::CONSTRAINT) {
            name = tokens.get(d + 1).cloned();
            d += 2;
        }
        let f = (d + 1..tokens.len()).rev().find(|&i| kw(i, Keyword::FOR));
        if kw(d, Keyword::DEFAULT)
            && let Some(f) = f
            && f + 2 == tokens.len()
        {
            let table: Vec<&Token> = tokens[2..add].iter().collect();
            let expr: Vec<&Token> = tokens[d + 1..f].iter().collect();
            let name = name.and_then(|n| match n {
                Token::Word(w) => Some(w.value),
                _ => None,
            });
            return Some((format!("ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {}", text(&table), tokens[f + 1], text(&expr)), name));
        }
    }
    let at = (2..tokens.len().saturating_sub(2)).find(|&i| kw(i, Keyword::ALTER) && kw(i + 1, Keyword::COLUMN))?;
    let column = tokens.get(at + 2)?;
    if [Keyword::SET, Keyword::DROP, Keyword::ADD, Keyword::TYPE].iter().any(|k| kw(at + 3, *k)) {
        return None;
    }
    let mut ty: Vec<&Token> = tokens[at + 3..].iter().collect();
    let mut not_null = false;
    // Trailing NULL / NOT NULL; a COLLATE clause is dropped (TwinLite has no collations).
    if let Some(p) = ty.iter().position(|t| matches!(t, Token::Word(w) if w.keyword == Keyword::COLLATE)) {
        let rest: Vec<&Token> = ty[p..].iter().skip(2).copied().collect();
        ty.truncate(p);
        ty.extend(rest);
    }
    if matches!(ty.last(), Some(Token::Word(w)) if w.keyword == Keyword::NULL) {
        ty.pop();
        if matches!(ty.last(), Some(Token::Word(w)) if w.keyword == Keyword::NOT) {
            ty.pop();
            not_null = true;
        }
    }
    if ty.is_empty() {
        return None;
    }
    let table: Vec<&Token> = tokens[2..at].iter().collect();
    Some((
        format!(
            "ALTER TABLE {} ALTER COLUMN {column} SET DATA TYPE {}, ALTER COLUMN {column} {} NOT NULL",
            text(&table),
            text(&ty),
            if not_null { "SET" } else { "DROP" }
        ),
        None,
    ))
}

/// `READ ONLY` among a BEGIN / START TRANSACTION statement's modes.
fn read_only_mode(toks: &[Tok]) -> bool {
    toks.windows(2).any(|w| w[0].upper() == "READ" && w[1].upper() == "ONLY")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_respecting_quotes_and_comments() {
        let s = split_statements("SELECT 'a;b'; -- x;\n SELECT $$ c;d $$; /* ; */ SELECT \"e;f\";;", Dialect::Postgres);
        assert_eq!(s, vec!["SELECT 'a;b'", "-- x;\n SELECT $$ c;d $$", "/* ; */ SELECT \"e;f\""]);
        let s = split_statements("SELECT 'it\\'s; fine'; SELECT 1 # c;\n", Dialect::MySql);
        assert_eq!(s.len(), 2);
        assert!(split_statements("  -- only a comment\n", Dialect::Postgres).is_empty());
        assert_eq!(split_statements("SELECT $1, $2", Dialect::Postgres), vec!["SELECT $1, $2"]);
    }
}
