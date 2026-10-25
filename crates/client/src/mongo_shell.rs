//! A small mongosh-compatible syntax: values (`{age: {$gt: 30}}`, `ObjectId("…")`, `ISODate("…")`,
//! `/re/i`, Extended JSON) and statements (`db.users.find({…}).sort({…}).limit(10)`, `show dbs`,
//! `use app`, `db.runCommand({…})`).

use bson::{Bson, Document, oid::ObjectId};

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Prop(String),
    Call(String, Vec<Bson>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    ShowDatabases,
    ShowCollections,
    Use(String),
    /// `db.…` chain after `db`.
    Db(Vec<Segment>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at character {})", self.message, self.offset + 1)
    }
}

type PResult<T> = Result<T, ParseError>;

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser { s: src.as_bytes(), src, i: 0 }
    }

    fn err<T>(&self, m: impl Into<String>) -> PResult<T> {
        Err(ParseError { message: m.into(), offset: self.src[..self.i.min(self.src.len())].chars().count() })
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"//") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.i += 1;
                }
            } else if self.s[self.i..].starts_with(b"/*") {
                match self.src[self.i + 2..].find("*/") {
                    Some(end) => self.i += end + 4,
                    None => self.i = self.s.len(),
                }
            } else {
                break;
            }
        }
    }

    /// Whitespace and comments, but not newlines (which end a statement).
    fn inline_ws(&mut self) {
        while self.peek().is_some_and(|c| c == b' ' || c == b'\t' || c == b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> PResult<()> {
        if self.eat(c) { Ok(()) } else { self.err(format!("expected '{}'", c as char)) }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$') {
            self.i += 1;
        }
        (self.i > start).then(|| self.src[start..self.i].to_owned())
    }

    fn string(&mut self) -> PResult<String> {
        let q = self.peek().unwrap();
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.src[self.i..].chars().next() else { return self.err("unterminated string") };
            self.i += c.len_utf8();
            match c {
                c if c as u32 == q as u32 => return Ok(out),
                '\\' => {
                    let Some(e) = self.src[self.i..].chars().next() else { return self.err("unterminated string") };
                    self.i += e.len_utf8();
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex = self.src.get(self.i..self.i + 4).unwrap_or("");
                            let Ok(code) = u32::from_str_radix(hex, 16) else { return self.err("invalid \\u escape") };
                            self.i += 4;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> PResult<Bson> {
        let start = self.i;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| {
            c.is_ascii_digit()
                || matches!(c, b'.' | b'e' | b'E' | b'_')
                || (matches!(c, b'-' | b'+') && matches!(self.s[self.i - 1], b'e' | b'E'))
        }) {
            self.i += 1;
        }
        let text: String = self.src[start..self.i].chars().filter(|c| *c != '_').collect();
        if !text.contains(['.', 'e', 'E'])
            && let Ok(i) = text.parse::<i64>()
        {
            return Ok(i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i)));
        }
        match text.parse::<f64>() {
            Ok(f) => Ok(Bson::Double(f)),
            Err(_) => {
                self.i = start;
                self.err(format!("invalid number '{text}'"))
            }
        }
    }

    fn regex(&mut self) -> PResult<Bson> {
        self.i += 1;
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.src[self.i..].chars().next() else { return self.err("unterminated regular expression") };
            self.i += c.len_utf8();
            match c {
                '\\' => {
                    pattern.push('\\');
                    if let Some(n) = self.src[self.i..].chars().next() {
                        self.i += n.len_utf8();
                        pattern.push(n);
                    }
                }
                '[' => {
                    in_class = true;
                    pattern.push(c);
                }
                ']' => {
                    in_class = false;
                    pattern.push(c);
                }
                '/' if !in_class => break,
                '\n' => return self.err("unterminated regular expression"),
                c => pattern.push(c),
            }
        }
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        let mut flags: Vec<char> = self.src[start..self.i].chars().filter(|c| "imsxu".contains(*c)).collect();
        flags.sort_unstable();
        let flags: String = flags.into_iter().collect();
        make_regex(pattern, flags).map_err(|message| ParseError { message, offset: start })
    }

    fn args(&mut self) -> PResult<Vec<Bson>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        if self.eat(b')') {
            return Ok(out);
        }
        loop {
            out.push(self.value()?);
            if self.eat(b')') {
                return Ok(out);
            }
            self.expect(b',')?;
            if self.eat(b')') {
                return Ok(out);
            }
        }
    }

    fn one_string_arg(&mut self, name: &str) -> PResult<Option<String>> {
        let args = self.args()?;
        match args.as_slice() {
            [] => Ok(None),
            [Bson::String(s)] => Ok(Some(s.clone())),
            [other] if name == "NumberLong" || name == "NumberInt" || name == "NumberDecimal" || name == "Date" => Ok(Some(match other {
                Bson::Int32(i) => i.to_string(),
                Bson::Int64(i) => i.to_string(),
                Bson::Double(d) => d.to_string(),
                _ => return self.err(format!("{name}() takes a string or number")),
            })),
            _ => self.err(format!("{name}() takes one string argument")),
        }
    }

    fn constructor(&mut self, name: &str) -> PResult<Bson> {
        let date = |s: &str| -> Option<Bson> {
            if let Ok(ms) = s.parse::<i64>() {
                return Some(Bson::DateTime(bson::DateTime::from_millis(ms)));
            }
            let parsed = chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .ok()
                .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|d| d.and_utc()))
                .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok().map(|d| d.and_utc()))
                .or_else(|| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc()))?;
            Some(Bson::DateTime(bson::DateTime::from_millis(parsed.timestamp_millis())))
        };
        Ok(match name {
            "ObjectId" => match self.one_string_arg(name)? {
                Some(hex) => Bson::ObjectId(ObjectId::parse_str(&hex).or_else(|_| self.err(format!("invalid ObjectId '{hex}'")))?),
                None => Bson::ObjectId(ObjectId::new()),
            },
            "ISODate" | "Date" => match self.one_string_arg(name)? {
                Some(s) => date(&s).map_or_else(|| self.err(format!("invalid date '{s}'")), Ok)?,
                None => Bson::DateTime(bson::DateTime::now()),
            },
            "NumberLong" | "Long" => {
                let s = self.one_string_arg(name)?.unwrap_or_else(|| "0".into());
                Bson::Int64(s.trim().parse().or_else(|_| self.err(format!("invalid NumberLong '{s}'")))?)
            }
            "NumberInt" | "Int32" => {
                let s = self.one_string_arg(name)?.unwrap_or_else(|| "0".into());
                Bson::Int32(s.trim().parse::<f64>().or_else(|_| self.err(format!("invalid NumberInt '{s}'")))? as i32)
            }
            "NumberDecimal" | "Decimal128" => {
                let s = self.one_string_arg(name)?.unwrap_or_else(|| "0".into());
                Bson::Decimal128(s.trim().parse().or_else(|_| self.err(format!("invalid NumberDecimal '{s}'")))?)
            }
            "UUID" => {
                let s = self.one_string_arg(name)?;
                let u = match s {
                    Some(s) => uuid::Uuid::parse_str(&s).or_else(|_| self.err(format!("invalid UUID '{s}'")))?,
                    None => uuid::Uuid::new_v4(),
                };
                Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Uuid, bytes: u.as_bytes().to_vec() })
            }
            "Timestamp" => {
                let args = self.args()?;
                let n = |i: usize| {
                    args.get(i).and_then(|v| match v {
                        Bson::Int32(x) => Some(*x as u32),
                        Bson::Int64(x) => Some(*x as u32),
                        Bson::Double(x) => Some(*x as u32),
                        _ => None,
                    })
                };
                Bson::Timestamp(bson::Timestamp { time: n(0).unwrap_or(0), increment: n(1).unwrap_or(0) })
            }
            "BinData" => {
                let args = self.args()?;
                let (Some(sub), Some(Bson::String(b64))) = (args.first(), args.get(1)) else {
                    return self.err("BinData() takes a subtype and a base64 string");
                };
                let sub = match sub {
                    Bson::Int32(n) => *n as u8,
                    Bson::Int64(n) => *n as u8,
                    Bson::Double(n) => *n as u8,
                    _ => return self.err("BinData() subtype must be a number"),
                };
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(b64).or_else(|_| self.err("BinData() needs valid base64"))?;
                Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::from(sub), bytes })
            }
            "MinKey" => {
                let _ = self.args();
                Bson::MinKey
            }
            "MaxKey" => {
                let _ = self.args();
                Bson::MaxKey
            }
            "RegExp" => {
                let args = self.args()?;
                let (Some(Bson::String(p)), flags) = (args.first(), args.get(1).and_then(|f| f.as_str()).unwrap_or("")) else {
                    return self.err("RegExp() takes a pattern string");
                };
                make_regex(p.clone(), flags.chars().filter(|c| "imsxu".contains(*c)).collect()).or_else(|m| self.err(m))?
            }
            other => return self.err(format!("unknown function {other}()")),
        })
    }

    fn value(&mut self) -> PResult<Bson> {
        self.ws();
        match self.peek() {
            None => self.err("unexpected end of input"),
            Some(b'{') => {
                self.i += 1;
                let mut d = Document::new();
                loop {
                    if self.eat(b'}') {
                        break;
                    }
                    self.ws();
                    let key = match self.peek() {
                        Some(b'"' | b'\'') => self.string()?,
                        _ => {
                            let start = self.i;
                            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.')) {
                                self.i += 1;
                            }
                            if self.i == start {
                                return self.err("expected a field name");
                            }
                            self.src[start..self.i].to_owned()
                        }
                    };
                    self.expect(b':')?;
                    let v = self.value()?;
                    d.insert(key, v);
                    if self.eat(b'}') {
                        break;
                    }
                    self.expect(b',')?;
                }
                Ok(ext_json(d))
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                loop {
                    if self.eat(b']') {
                        break;
                    }
                    a.push(self.value()?);
                    if self.eat(b']') {
                        break;
                    }
                    self.expect(b',')?;
                }
                Ok(Bson::Array(a))
            }
            Some(b'"' | b'\'') => Ok(Bson::String(self.string()?)),
            Some(b'/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => self.number(),
            _ => {
                let start = self.i;
                let Some(word) = self.ident() else { return self.err("unexpected character") };
                match word.as_str() {
                    "true" => Ok(Bson::Boolean(true)),
                    "false" => Ok(Bson::Boolean(false)),
                    "null" => Ok(Bson::Null),
                    "undefined" => Ok(Bson::Undefined),
                    "Infinity" => Ok(Bson::Double(f64::INFINITY)),
                    "NaN" => Ok(Bson::Double(f64::NAN)),
                    "new" => {
                        let Some(ctor) = self.ident() else { return self.err("expected a constructor after 'new'") };
                        self.constructor(&ctor)
                    }
                    _ => {
                        self.ws();
                        if self.peek() == Some(b'(') {
                            self.constructor(&word)
                        } else {
                            self.i = start;
                            self.err(format!("unknown name '{word}' (quote strings: \"{word}\")"))
                        }
                    }
                }
            }
        }
    }

    fn statement(&mut self) -> PResult<Option<(Statement, String)>> {
        self.ws();
        while self.peek() == Some(b';') {
            self.i += 1;
            self.ws();
        }
        if self.peek().is_none() {
            return Ok(None);
        }
        let start = self.i;
        let Some(word) = self.ident() else { return self.err("expected a statement such as db.collection.find({})") };
        let st = match word.as_str() {
            "show" => {
                self.inline_ws();
                match self.ident().as_deref() {
                    Some("dbs" | "databases") => Statement::ShowDatabases,
                    Some("collections" | "tables") => Statement::ShowCollections,
                    other => return self.err(format!("unknown show target {}", other.unwrap_or("(none)"))),
                }
            }
            "use" => {
                self.inline_ws();
                let s = self.i;
                while self.peek().is_some_and(|c| !c.is_ascii_whitespace() && c != b';') {
                    self.i += 1;
                }
                if self.i == s {
                    return self.err("use needs a database name");
                }
                Statement::Use(self.src[s..self.i].trim_matches(['"', '\'']).to_owned())
            }
            "db" => {
                let mut segs = Vec::new();
                loop {
                    // A chain may continue on the next line, but only with '.' or '['.
                    let before = self.i;
                    self.ws();
                    match self.peek() {
                        Some(b'.') => {
                            self.i += 1;
                            let Some(name) = self.ident() else { return self.err("expected a name after '.'") };
                            self.ws();
                            if self.peek() == Some(b'(') {
                                let args = self.args()?;
                                segs.push(Segment::Call(name, args));
                            } else {
                                segs.push(Segment::Prop(name));
                            }
                        }
                        Some(b'[') => {
                            self.i += 1;
                            self.ws();
                            let name = match self.peek() {
                                Some(b'"' | b'\'') => self.string()?,
                                _ => return self.err("expected a quoted collection name"),
                            };
                            self.expect(b']')?;
                            segs.push(Segment::Prop(name));
                        }
                        _ => {
                            self.i = before;
                            break;
                        }
                    }
                }
                if segs.is_empty() {
                    return self.err("expected db.<collection>.<method>(…)");
                }
                Statement::Db(segs)
            }
            other => {
                self.i = start;
                return self.err(format!(
                    "unsupported statement '{other}': use db.<collection>.<method>(…), show dbs, show collections or use <db>"
                ));
            }
        };
        let text = self.src[start..self.i].trim().to_owned();
        self.inline_ws();
        match self.peek() {
            None | Some(b';' | b'\n') => {}
            Some(b'/') if matches!(self.s.get(self.i + 1), Some(b'/' | b'*')) => {}
            _ => return self.err("unexpected text after statement"),
        }
        Ok(Some((st, text)))
    }
}

fn make_regex(pattern: String, flags: String) -> Result<Bson, String> {
    let mut f: Vec<char> = flags.chars().collect();
    f.sort_unstable();
    let pattern = bson::raw::CString::try_from(pattern).map_err(|e| format!("invalid regular expression: {e}"))?;
    let options =
        bson::raw::CString::try_from(f.into_iter().collect::<String>()).map_err(|e| format!("invalid regular expression: {e}"))?;
    Ok(Bson::RegularExpression(bson::Regex { pattern, options }))
}

/// Recognise Extended JSON wrappers (`{"$oid": …}`, `{"$date": …}`, …) inside parsed objects.
fn ext_json(d: Document) -> Bson {
    let is_ext = d.len() <= 2
        && d.keys().next().is_some_and(|k| {
            matches!(
                k.as_str(),
                "$oid"
                    | "$date"
                    | "$numberLong"
                    | "$numberInt"
                    | "$numberDouble"
                    | "$numberDecimal"
                    | "$binary"
                    | "$uuid"
                    | "$regularExpression"
                    | "$timestamp"
                    | "$minKey"
                    | "$maxKey"
                    | "$symbol"
                    | "$code"
                    | "$undefined"
            )
        });
    if is_ext {
        let json = Bson::Document(d.clone()).into_relaxed_extjson();
        if let Ok(b) = Bson::try_from(json)
            && !matches!(b, Bson::Document(_))
        {
            return b;
        }
    }
    Bson::Document(d)
}

/// Parse one value in mongosh syntax (a superset of JSON / Extended JSON).
pub fn parse_value(src: &str) -> Result<Bson, ParseError> {
    let mut p = Parser::new(src);
    let v = p.value()?;
    p.ws();
    if p.peek().is_some() {
        return p.err("unexpected text after value");
    }
    Ok(v)
}

/// Parse a document; empty input is `{}`.
pub fn parse_document(src: &str) -> Result<Document, ParseError> {
    if src.trim().is_empty() {
        return Ok(Document::new());
    }
    match parse_value(src)? {
        Bson::Document(d) => Ok(d),
        other => Err(ParseError { message: format!("expected an object, found {}", type_name(&other)), offset: 0 }),
    }
}

/// Parse an array of documents (a pipeline or several documents to insert). A single object is
/// accepted as a one-element list.
pub fn parse_documents(src: &str) -> Result<Vec<Document>, ParseError> {
    if src.trim().is_empty() {
        return Ok(vec![]);
    }
    match parse_value(src)? {
        Bson::Array(items) => items
            .into_iter()
            .map(|i| match i {
                Bson::Document(d) => Ok(d),
                other => Err(ParseError { message: format!("expected objects in the list, found {}", type_name(&other)), offset: 0 }),
            })
            .collect(),
        Bson::Document(d) => Ok(vec![d]),
        other => Err(ParseError { message: format!("expected an array of objects, found {}", type_name(&other)), offset: 0 }),
    }
}

/// Parse a script of statements, with the source text of each.
pub fn parse_script(src: &str) -> Result<Vec<(Statement, String)>, ParseError> {
    let mut p = Parser::new(src);
    let mut out = Vec::new();
    while let Some(st) = p.statement()? {
        out.push(st);
    }
    Ok(out)
}

pub fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Document(_) => "an object",
        Bson::Array(_) => "an array",
        Bson::String(_) => "a string",
        Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => "a number",
        Bson::Boolean(_) => "a boolean",
        Bson::Null => "null",
        _ => "a value",
    }
}

/// Read-only commands for `db.runCommand` (anything else counts as a write for access checks).
pub fn is_read_command(name: &str) -> bool {
    matches!(
        name,
        "find"
            | "count"
            | "distinct"
            | "listCollections"
            | "listDatabases"
            | "listIndexes"
            | "dbStats"
            | "collStats"
            | "dataSize"
            | "serverStatus"
            | "buildInfo"
            | "buildinfo"
            | "ping"
            | "hello"
            | "isMaster"
            | "ismaster"
            | "hostInfo"
            | "connectionStatus"
            | "explain"
            | "getParameter"
            | "getCmdLineOpts"
            | "whatsmyuri"
            | "validate"
            | "currentOp"
            | "top"
            | "getLog"
            | "listCommands"
            | "usersInfo"
            | "rolesInfo"
    )
}

fn pipeline_writes(p: &Bson) -> bool {
    p.as_array().is_some_and(|a| a.iter().any(|s| s.as_document().is_some_and(|d| d.contains_key("$out") || d.contains_key("$merge"))))
}

/// Can this statement change data? Unparseable text counts as a write.
pub fn statement_writes(st: &Statement) -> bool {
    match st {
        Statement::ShowDatabases | Statement::ShowCollections | Statement::Use(_) => false,
        Statement::Db(segs) => segs.iter().any(|s| match s {
            Segment::Call(name, args) => match name.as_str() {
                "find"
                | "findOne"
                | "countDocuments"
                | "estimatedDocumentCount"
                | "count"
                | "distinct"
                | "getIndexes"
                | "getCollectionNames"
                | "getCollectionInfos"
                | "stats"
                | "getCollection"
                | "sort"
                | "limit"
                | "skip"
                | "projection"
                | "project"
                | "toArray"
                | "pretty"
                | "explain"
                | "hint"
                | "batchSize"
                | "maxTimeMS"
                | "getName"
                | "itcount"
                | "size"
                | "collation"
                | "comment"
                | "allowDiskUse"
                | "getSiblingDB"
                | "serverStatus"
                | "version"
                | "hostInfo"
                | "listCollections"
                | "indexes" => false,
                "aggregate" => args.first().is_some_and(pipeline_writes),
                "runCommand" | "adminCommand" => !args
                    .first()
                    .and_then(|a| a.as_document())
                    .and_then(|d| d.keys().next())
                    .is_some_and(|k| is_read_command(k) && (k != "aggregate" || !d_pipeline_writes(args.first()))),
                _ => true,
            },
            Segment::Prop(_) => false,
        }),
    }
}

fn d_pipeline_writes(cmd: Option<&Bson>) -> bool {
    cmd.and_then(|c| c.as_document()).and_then(|d| d.get("pipeline")).is_some_and(pipeline_writes)
}

/// Does any statement in the script change data?
pub fn script_writes(src: &str) -> bool {
    match parse_script(src) {
        Ok(sts) => sts.iter().any(|(s, _)| statement_writes(s)),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn values_in_shell_syntax() {
        let v = parse_document(r#"{ name: 'Ann', "age": {$gt: 30, $lte: 4e1}, tags: ["a", /^b/i,], id: ObjectId("65a1b2c3d4e5f60718293a4b"),
            at: ISODate("2024-01-02T03:04:05Z"), n: NumberLong("9007199254740993"), d: new Date("2024-01-02"), x: {"$oid": "65a1b2c3d4e5f60718293a4b"},
            // comment
            'address.city': null, neg: -3, f: 1.5 }"#)
        .unwrap();
        assert_eq!(v.get_str("name").unwrap(), "Ann");
        assert_eq!(v.get_document("age").unwrap(), &doc! { "$gt": 30, "$lte": 40.0 });
        assert!(
            matches!(v.get_array("tags").unwrap()[1], Bson::RegularExpression(ref r) if r.pattern.as_str() == "^b" && r.options.as_str() == "i")
        );
        assert_eq!(v.get_object_id("id").unwrap().to_hex(), "65a1b2c3d4e5f60718293a4b");
        assert_eq!(v.get_object_id("x").unwrap().to_hex(), "65a1b2c3d4e5f60718293a4b");
        assert_eq!(v.get_datetime("at").unwrap().timestamp_millis(), 1_704_164_645_000);
        assert_eq!(v.get_i64("n").unwrap(), 9_007_199_254_740_993);
        assert_eq!(v.get_datetime("d").unwrap().timestamp_millis(), 1_704_153_600_000);
        assert_eq!(v.get("address.city"), Some(&Bson::Null));
        assert_eq!(v.get_i32("neg").unwrap(), -3);
        let b = parse_value("BinData(0, \"aGk=\")").unwrap();
        assert!(matches!(b, Bson::Binary(ref x) if x.bytes == b"hi"));
        assert!(parse_document("{a: }").is_err());
        assert!(parse_document("{a: foo}").unwrap_err().message.contains("quote strings"));
        assert!(parse_document("[1]").is_err());
        assert_eq!(parse_documents("[{$match: {}}, {$limit: 5}]").unwrap().len(), 2);
    }

    #[test]
    fn statements() {
        let sts = parse_script("use shop\nshow collections; db.users.find({age: {$gt: 1}}).sort({age: -1}).limit(5)\ndb['order-items'].countDocuments()\ndb.getCollection('x').insertOne({a: 1})").unwrap();
        assert_eq!(sts.len(), 5);
        assert_eq!(sts[0].0, Statement::Use("shop".into()));
        assert_eq!(sts[1].0, Statement::ShowCollections);
        let Statement::Db(segs) = &sts[2].0 else { panic!() };
        assert_eq!(segs[0], Segment::Prop("users".into()));
        assert_eq!(segs[1], Segment::Call("find".into(), vec![Bson::Document(doc! { "age": { "$gt": 1 } })]));
        assert_eq!(segs[3], Segment::Call("limit".into(), vec![Bson::Int32(5)]));
        assert_eq!(sts[2].1, "db.users.find({age: {$gt: 1}}).sort({age: -1}).limit(5)");
        let Statement::Db(segs) = &sts[3].0 else { panic!() };
        assert_eq!(segs[0], Segment::Prop("order-items".into()));
        assert!(!statement_writes(&sts[2].0));
        assert!(statement_writes(&sts[4].0));
        assert!(script_writes("db.a.aggregate([{$match: {}}, {$out: 'b'}])"));
        assert!(!script_writes("db.a.aggregate([{$match: {}}])"));
        assert!(!script_writes("db.runCommand({dbStats: 1})"));
        assert!(script_writes("db.runCommand({drop: 'x'})"));
        assert!(script_writes("db.a.find(")); // unparseable counts as a write
        assert!(parse_script("db.users.find() junk").is_err());
        // Multi-line statements continue inside brackets.
        assert_eq!(parse_script("db.users.find({\n  a: 1\n})\n.limit(2)").unwrap().len(), 1);
    }
}
