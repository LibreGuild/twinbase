//! DynamoDB expressions: condition / filter / key-condition expressions, projections and update
//! expressions, with `#name` and `:value` placeholders and the reserved-word rules.

use std::cell::RefCell;
use std::collections::HashSet;

use bigdecimal::BigDecimal;
use serde_json::{Map, Value};

use super::DynError;
use super::value::{Av, Item};

#[derive(Debug, Clone, PartialEq)]
pub enum Elem {
    Name(String),
    Index(usize),
}

pub type Path = Vec<Elem>;

pub fn path_text(p: &Path) -> String {
    let mut s = String::new();
    for (i, e) in p.iter().enumerate() {
        match e {
            Elem::Name(n) if i == 0 => s.push_str(n),
            Elem::Name(n) => {
                s.push('.');
                s.push_str(n);
            }
            Elem::Index(n) => s.push_str(&format!("[{n}]")),
        }
    }
    s
}

#[derive(Debug, Clone)]
pub enum Operand {
    Path(Path),
    Value(Av),
    Size(Path),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub enum Cond {
    Cmp(Operand, CmpOp, Operand),
    Between(Operand, Operand, Operand),
    In(Operand, Vec<Operand>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Exists(Path),
    NotExists(Path),
    Type(Path, Operand),
    BeginsWith(Operand, Operand),
    Contains(Operand, Operand),
}

#[derive(Debug, Clone)]
pub enum SetValue {
    Operand(Operand),
    IfNotExists(Path, Box<SetValue>),
    ListAppend(Box<SetValue>, Box<SetValue>),
    Plus(Box<SetValue>, Box<SetValue>),
    Minus(Box<SetValue>, Box<SetValue>),
}

#[derive(Debug, Clone, Default)]
pub struct Update {
    pub set: Vec<(Path, SetValue)>,
    pub remove: Vec<Path>,
    pub add: Vec<(Path, Av)>,
    pub delete: Vec<(Path, Av)>,
}

// ------------------------------------------------------------------ reserved words

const RESERVED: &str = "ABORT ABSOLUTE ACTION ADD AFTER AGENT AGGREGATE ALL ALLOCATE ALTER ANALYZE AND ANY ARCHIVE ARE ARRAY AS ASC ASCII ASENSITIVE ASSERTION ASYMMETRIC AT ATOMIC ATTACH ATTRIBUTE AUTH AUTHORIZATION AUTHORIZE AUTO AVG BACK BACKUP BASE BATCH BEFORE BEGIN BETWEEN BIGINT BINARY BIT BLOB BLOCK BOOLEAN BOTH BREADTH BUCKET BULK BY BYTE CALL CALLED CALLING CAPACITY CASCADE CASCADED CASE CAST CATALOG CHAR CHARACTER CHECK CLASS CLOB CLOSE CLUSTER CLUSTERED CLUSTERING CLUSTERS COALESCE COLLATE COLLATION COLLECTION COLUMN COLUMNS COMBINE COMMENT COMMIT COMPACT COMPILE COMPRESS CONDITION CONFLICT CONNECT CONNECTION CONSISTENCY CONSISTENT CONSTRAINT CONSTRAINTS CONSTRUCTOR CONSUMED CONTINUE CONVERT COPY CORRESPONDING COUNT COUNTER CREATE CROSS CUBE CURRENT CURSOR CYCLE DATA DATABASE DATE DATETIME DAY DEALLOCATE DEC DECIMAL DECLARE DEFAULT DEFERRABLE DEFERRED DEFINE DEFINED DEFINITION DELETE DELIMITED DEPTH DEREF DESC DESCRIBE DESCRIPTOR DETACH DETERMINISTIC DIAGNOSTICS DIRECTORIES DISABLE DISCONNECT DISTINCT DISTRIBUTE DO DOMAIN DOUBLE DROP DUMP DURATION DYNAMIC EACH ELEMENT ELSE ELSEIF EMPTY ENABLE END EQUAL EQUALS ERROR ESCAPE ESCAPED EVAL EVALUATE EXCEEDED EXCEPT EXCEPTION EXCEPTIONS EXCLUSIVE EXEC EXECUTE EXISTS EXIT EXPLAIN EXPLODE EXPORT EXPRESSION EXTENDED EXTERNAL EXTRACT FAIL FALSE FAMILY FETCH FIELDS FILE FILTER FILTERING FINAL FINISH FIRST FIXED FLATTERN FLOAT FOR FORCE FOREIGN FORMAT FORWARD FOUND FREE FROM FULL FUNCTION FUNCTIONS GENERAL GENERATE GET GLOB GLOBAL GO GOTO GRANT GREATER GROUP GROUPING HANDLER HASH HAVE HAVING HEAP HIDDEN HOLD HOUR IDENTIFIED IDENTITY IF IGNORE IMMEDIATE IMPORT IN INCLUDING INCLUSIVE INCREMENT INCREMENTAL INDEX INDEXED INDEXES INDICATOR INFINITE INITIALLY INLINE INNER INNTER INOUT INPUT INSENSITIVE INSERT INSTEAD INT INTEGER INTERSECT INTERVAL INTO INVALIDATE IS ISOLATION ITEM ITEMS ITERATE JOIN KEY KEYS LAG LANGUAGE LARGE LAST LATERAL LEAD LEADING LEAVE LEFT LENGTH LESS LEVEL LIKE LIMIT LIMITED LINES LIST LOAD LOCAL LOCALTIME LOCALTIMESTAMP LOCATION LOCATOR LOCK LOCKS LOG LOGED LONG LOOP LOWER MAP MATCH MATERIALIZED MAX MAXLEN MEMBER MERGE METHOD METRICS MIN MINUS MINUTE MISSING MOD MODE MODIFIES MODIFY MODULE MONTH MULTI MULTISET NAME NAMES NATIONAL NATURAL NCHAR NCLOB NEW NEXT NO NONE NOT NULL NULLIF NUMBER NUMERIC OBJECT OF OFFLINE OFFSET OLD ON ONLINE ONLY OPAQUE OPEN OPERATOR OPTION OR ORDER ORDINALITY OTHER OTHERS OUT OUTER OUTPUT OVER OVERLAPS OVERRIDE OWNER PAD PARALLEL PARAMETER PARAMETERS PARTIAL PARTITION PARTITIONED PARTITIONS PATH PERCENT PERCENTILE PERMISSION PERMISSIONS PIPE PIPELINED PLAN POOL POSITION PRECISION PREPARE PRESERVE PRIMARY PRIOR PRIVATE PRIVILEGES PROCEDURE PROCESSED PROJECT PROJECTION PROPERTY PROVISIONING PUBLIC PUT QUERY QUIT QUORUM RAISE RANDOM RANGE RANK RAW READ READS REAL REBUILD RECORD RECURSIVE REDUCE REF REFERENCE REFERENCES REFERENCING REGEXP REGION REINDEX RELATIVE RELEASE REMAINDER RENAME REPEAT REPLACE REQUEST RESET RESIGNAL RESOURCE RESPONSE RESTORE RESTRICT RESULT RETURN RETURNING RETURNS REVERSE REVOKE RIGHT ROLE ROLES ROLLBACK ROLLUP ROUTINE ROW ROWS RULE RULES SAMPLE SATISFIES SAVE SAVEPOINT SCAN SCHEMA SCOPE SCROLL SEARCH SECOND SECTION SEGMENT SEGMENTS SELECT SELF SEMI SENSITIVE SEPARATE SEQUENCE SERIALIZABLE SESSION SET SETS SHARD SHARE SHARED SHORT SHOW SIGNAL SIMILAR SIZE SKEWED SMALLINT SNAPSHOT SOME SOURCE SPACE SPACES SPARSE SPECIFIC SPECIFICTYPE SPLIT SQL SQLCODE SQLERROR SQLEXCEPTION SQLSTATE SQLWARNING START STATE STATIC STATUS STORAGE STORE STORED STREAM STRING STRUCT STYLE SUB SUBMULTISET SUBPARTITION SUBSTRING SUBTYPE SUM SUPER SYMMETRIC SYNONYM SYSTEM TABLE TABLESAMPLE TEMP TEMPORARY TERMINATED TEXT THAN THEN THROUGHPUT TIME TIMESTAMP TIMEZONE TINYINT TO TOKEN TOTAL TOUCH TRAILING TRANSACTION TRANSFORM TRANSLATE TRANSLATION TREAT TRIGGER TRIM TRUE TRUNCATE TTL TUPLE TYPE UNDER UNDO UNION UNIQUE UNIT UNKNOWN UNLOGGED UNNEST UNPROCESSED UNSIGNED UNTIL UPDATE UPPER URL USAGE USE USER USERS USING UUID VACUUM VALUE VALUED VALUES VARCHAR VARIABLE VARIANCE VARINT VARYING VIEW VIEWS VIRTUAL VOID WAIT WHEN WHENEVER WHERE WHILE WINDOW WITH WITHIN WITHOUT WORK WRAPPED WRITE YEAR ZONE";

pub fn is_reserved(word: &str) -> bool {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| RESERVED.split(' ').collect()).contains(word.to_ascii_uppercase().as_str())
}

// ------------------------------------------------------------------ tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Name(String),
    Value(String),
    Num(usize),
    Sym(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<(Tok, usize)>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let word = |i: &mut usize| {
            let s = *i;
            while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
                *i += 1;
            }
            src[s..*i].to_owned()
        };
        let tok = match c {
            b'#' => {
                i += 1;
                let w = word(&mut i);
                if w.is_empty() {
                    return Err("Syntax error; token: \"#\"".into());
                }
                Tok::Name(format!("#{w}"))
            }
            b':' => {
                i += 1;
                let w = word(&mut i);
                if w.is_empty() {
                    return Err("Syntax error; token: \":\"".into());
                }
                Tok::Value(format!(":{w}"))
            }
            b'0'..=b'9' => {
                let w = word(&mut i);
                Tok::Num(w.parse().map_err(|_| format!("Syntax error; token: \"{w}\""))?)
            }
            c if c.is_ascii_alphabetic() || c == b'_' => Tok::Ident(word(&mut i)),
            _ => {
                let two = src.get(i..i + 2).unwrap_or("");
                let sym: &'static str = match two {
                    "<>" => "<>",
                    "<=" => "<=",
                    ">=" => ">=",
                    _ => match c {
                        b'(' => "(",
                        b')' => ")",
                        b'[' => "[",
                        b']' => "]",
                        b',' => ",",
                        b'.' => ".",
                        b'=' => "=",
                        b'<' => "<",
                        b'>' => ">",
                        b'+' => "+",
                        b'-' => "-",
                        _ => {
                            return Err(format!(
                                "Invalid character encountered; character: \"{}\"",
                                src[i..].chars().next().unwrap_or('?')
                            ));
                        }
                    },
                };
                i += sym.len();
                Tok::Sym(sym)
            }
        };
        out.push((tok, start));
    }
    Ok(out)
}

// ------------------------------------------------------------------ parser

/// Placeholders and which of them were used (DynamoDB rejects unused ones).
pub struct Placeholders<'a> {
    names: Option<&'a Map<String, Value>>,
    values: Option<&'a Map<String, Value>>,
    used_names: RefCell<HashSet<String>>,
    used_values: RefCell<HashSet<String>>,
}

impl<'a> Placeholders<'a> {
    pub fn new(request: &'a Value) -> Self {
        Placeholders {
            names: request.get("ExpressionAttributeNames").and_then(Value::as_object),
            values: request.get("ExpressionAttributeValues").and_then(Value::as_object),
            used_names: RefCell::new(HashSet::new()),
            used_values: RefCell::new(HashSet::new()),
        }
    }

    /// After every expression of a request was parsed: unused placeholders are an error.
    pub fn check_unused(&self) -> Result<(), DynError> {
        if let Some(n) = self.names {
            let unused: Vec<&String> = n.keys().filter(|k| !self.used_names.borrow().contains(*k)).collect();
            if !unused.is_empty() {
                return Err(DynError::validation(format!(
                    "Value provided in ExpressionAttributeNames unused in expressions: keys: {{{}}}",
                    unused.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
        }
        if let Some(v) = self.values {
            let unused: Vec<&String> = v.keys().filter(|k| !self.used_values.borrow().contains(*k)).collect();
            if !unused.is_empty() {
                return Err(DynError::validation(format!(
                    "Value provided in ExpressionAttributeValues unused in expressions: keys: {{{}}}",
                    unused.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
        }
        Ok(())
    }
}

struct Parser<'a, 'p> {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    kind: &'static str,
    src: &'a str,
    ph: &'a Placeholders<'p>,
}

impl<'a, 'p> Parser<'a, 'p> {
    fn new(kind: &'static str, src: &'a str, ph: &'a Placeholders<'p>) -> Result<Self, DynError> {
        if src.trim().is_empty() {
            return Err(DynError::validation(format!("Invalid {kind}: The expression can not be empty;")));
        }
        let toks = tokenize(src).map_err(|e| DynError::validation(format!("Invalid {kind}: {e}")))?;
        Ok(Parser { toks, pos: 0, kind, src, ph })
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, DynError> {
        Err(DynError::validation(format!("Invalid {}: {}", self.kind, msg.into())))
    }

    fn syntax<T>(&self) -> Result<T, DynError> {
        match self.toks.get(self.pos) {
            Some((t, at)) => {
                let text = match t {
                    Tok::Ident(s) | Tok::Name(s) | Tok::Value(s) => s.clone(),
                    Tok::Num(n) => n.to_string(),
                    Tok::Sym(s) => (*s).to_owned(),
                };
                let near: String = self.src[at.saturating_sub(8)..(*at + text.len() + 8).min(self.src.len())].to_owned();
                self.err(format!("Syntax error; token: \"{text}\", near: \"{near}\""))
            }
            None => self.err("Syntax error; token: <EOF>, near: end of expression"),
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat(&mut self, sym: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(s)) if *s == sym) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, sym: &str) -> Result<(), DynError> {
        if self.eat(sym) { Ok(()) } else { self.syntax() }
    }

    fn done(&self) -> Result<(), DynError> {
        if self.pos < self.toks.len() { self.syntax() } else { Ok(()) }
    }

    fn name_of(&self, tok: &Tok) -> Result<String, DynError> {
        match tok {
            Tok::Ident(w) => {
                if is_reserved(w) {
                    return self.err(format!("Attribute name is a reserved keyword; reserved keyword: {w}"));
                }
                Ok(w.clone())
            }
            Tok::Name(n) => {
                let v = self.ph.names.and_then(|m| m.get(n)).and_then(Value::as_str);
                match v {
                    Some(v) => {
                        self.ph.used_names.borrow_mut().insert(n.clone());
                        Ok(v.to_owned())
                    }
                    None => self.err(format!("An expression attribute name used in the document path is not defined; attribute name: {n}")),
                }
            }
            _ => self.syntax(),
        }
    }

    fn value_of(&self, v: &str) -> Result<Av, DynError> {
        match self.ph.values.and_then(|m| m.get(v)) {
            Some(x) => {
                self.ph.used_values.borrow_mut().insert(v.to_owned());
                Av::from_json(x)
            }
            None => self.err(format!("An expression attribute value used in expression is not defined; attribute value: {v}")),
        }
    }

    fn path(&mut self) -> Result<Path, DynError> {
        let Some(t) = self.peek().cloned() else { return self.syntax() };
        if !matches!(t, Tok::Ident(_) | Tok::Name(_)) {
            return self.syntax();
        }
        self.pos += 1;
        let mut p = vec![Elem::Name(self.name_of(&t)?)];
        loop {
            if self.eat(".") {
                let Some(t) = self.peek().cloned() else { return self.syntax() };
                self.pos += 1;
                p.push(Elem::Name(self.name_of(&t)?));
            } else if self.eat("[") {
                let Some(Tok::Num(n)) = self.peek().cloned() else { return self.syntax() };
                self.pos += 1;
                self.expect("]")?;
                p.push(Elem::Index(n));
            } else {
                break;
            }
        }
        Ok(p)
    }

    fn operand(&mut self) -> Result<Operand, DynError> {
        match self.peek().cloned() {
            Some(Tok::Value(v)) => {
                self.pos += 1;
                Ok(Operand::Value(self.value_of(&v)?))
            }
            Some(Tok::Ident(w)) if w == "size" && matches!(self.toks.get(self.pos + 1), Some((Tok::Sym("("), _))) => {
                self.pos += 2;
                let p = self.path()?;
                self.expect(")")?;
                Ok(Operand::Size(p))
            }
            _ => Ok(Operand::Path(self.path()?)),
        }
    }

    fn condition(&mut self) -> Result<Cond, DynError> {
        let mut left = self.and()?;
        while self.eat_kw("OR") {
            let right = self.and()?;
            left = Cond::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Cond, DynError> {
        let mut left = self.not()?;
        while self.eat_kw("AND") {
            let right = self.not()?;
            left = Cond::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Cond, DynError> {
        if self.eat_kw("NOT") {
            return Ok(Cond::Not(Box::new(self.not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Cond, DynError> {
        if self.eat("(") {
            let c = self.condition()?;
            self.expect(")")?;
            return Ok(c);
        }
        if let Some(Tok::Ident(f)) = self.peek().cloned()
            && matches!(self.toks.get(self.pos + 1), Some((Tok::Sym("("), _)))
            && f != "size"
        {
            self.pos += 2;
            let c = match f.as_str() {
                "attribute_exists" => Cond::Exists(self.path()?),
                "attribute_not_exists" => Cond::NotExists(self.path()?),
                "attribute_type" => {
                    let p = self.path()?;
                    self.expect(",")?;
                    Cond::Type(p, self.operand()?)
                }
                "begins_with" => {
                    let a = self.operand()?;
                    self.expect(",")?;
                    Cond::BeginsWith(a, self.operand()?)
                }
                "contains" => {
                    let a = self.operand()?;
                    self.expect(",")?;
                    Cond::Contains(a, self.operand()?)
                }
                other => return self.err(format!("Invalid function name; function: {other}")),
            };
            self.expect(")")?;
            return Ok(c);
        }
        let left = self.operand()?;
        if self.eat_kw("BETWEEN") {
            let lo = self.operand()?;
            if !self.eat_kw("AND") {
                return self.syntax();
            }
            let hi = self.operand()?;
            return Ok(Cond::Between(left, lo, hi));
        }
        if self.eat_kw("IN") {
            self.expect("(")?;
            let mut list = vec![self.operand()?];
            while self.eat(",") {
                list.push(self.operand()?);
            }
            self.expect(")")?;
            return Ok(Cond::In(left, list));
        }
        let op = match self.peek() {
            Some(Tok::Sym("=")) => CmpOp::Eq,
            Some(Tok::Sym("<>")) => CmpOp::Ne,
            Some(Tok::Sym("<")) => CmpOp::Lt,
            Some(Tok::Sym("<=")) => CmpOp::Le,
            Some(Tok::Sym(">")) => CmpOp::Gt,
            Some(Tok::Sym(">=")) => CmpOp::Ge,
            _ => return self.syntax(),
        };
        self.pos += 1;
        let right = self.operand()?;
        Ok(Cond::Cmp(left, op, right))
    }

    fn set_value(&mut self) -> Result<SetValue, DynError> {
        let left = self.set_term()?;
        if self.eat("+") {
            return Ok(SetValue::Plus(Box::new(left), Box::new(self.set_term()?)));
        }
        if self.eat("-") {
            return Ok(SetValue::Minus(Box::new(left), Box::new(self.set_term()?)));
        }
        Ok(left)
    }

    fn set_term(&mut self) -> Result<SetValue, DynError> {
        if let Some(Tok::Ident(f)) = self.peek().cloned()
            && matches!(self.toks.get(self.pos + 1), Some((Tok::Sym("("), _)))
        {
            self.pos += 2;
            let v = match f.as_str() {
                "if_not_exists" => {
                    let p = self.path()?;
                    self.expect(",")?;
                    SetValue::IfNotExists(p, Box::new(self.set_term()?))
                }
                "list_append" => {
                    let a = self.set_term()?;
                    self.expect(",")?;
                    SetValue::ListAppend(Box::new(a), Box::new(self.set_term()?))
                }
                other => return self.err(format!("Invalid function name; function: {other}")),
            };
            self.expect(")")?;
            return Ok(v);
        }
        Ok(SetValue::Operand(self.operand()?))
    }

    fn update(&mut self) -> Result<Update, DynError> {
        let mut u = Update::default();
        let mut seen = HashSet::new();
        while self.pos < self.toks.len() {
            let Some(Tok::Ident(kw)) = self.peek().cloned() else { return self.syntax() };
            let kw = kw.to_ascii_uppercase();
            if !matches!(kw.as_str(), "SET" | "REMOVE" | "ADD" | "DELETE") {
                return self.syntax();
            }
            if !seen.insert(kw.clone()) {
                return self.err(format!("The \"{kw}\" section can only be used once in an update expression;"));
            }
            self.pos += 1;
            loop {
                let p = self.path()?;
                match kw.as_str() {
                    "SET" => {
                        self.expect("=")?;
                        u.set.push((p, self.set_value()?));
                    }
                    "REMOVE" => u.remove.push(p),
                    _ => {
                        let Some(Tok::Value(v)) = self.peek().cloned() else { return self.syntax() };
                        self.pos += 1;
                        let val = self.value_of(&v)?;
                        if kw == "ADD" {
                            u.add.push((p, val));
                        } else {
                            u.delete.push((p, val));
                        }
                    }
                }
                if !self.eat(",") {
                    break;
                }
            }
        }
        // Overlapping paths are rejected, like the real service.
        let all: Vec<&Path> = u
            .set
            .iter()
            .map(|(p, _)| p)
            .chain(u.remove.iter())
            .chain(u.add.iter().map(|(p, _)| p))
            .chain(u.delete.iter().map(|(p, _)| p))
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                let n = a.len().min(b.len());
                if a[..n] == b[..n] {
                    return self.err(format!("Two document paths overlap with each other; must remove or rewrite one of these paths; path one: [{}], path two: [{}]", path_text(a), path_text(b)));
                }
            }
        }
        Ok(u)
    }
}

pub fn parse_condition(kind: &'static str, src: &str, ph: &Placeholders) -> Result<Cond, DynError> {
    let mut p = Parser::new(kind, src, ph)?;
    let c = p.condition()?;
    p.done()?;
    Ok(c)
}

pub fn parse_projection(src: &str, ph: &Placeholders) -> Result<Vec<Path>, DynError> {
    let mut p = Parser::new("ProjectionExpression", src, ph)?;
    let mut out = vec![p.path()?];
    while p.eat(",") {
        out.push(p.path()?);
    }
    p.done()?;
    Ok(out)
}

pub fn parse_update(src: &str, ph: &Placeholders) -> Result<Update, DynError> {
    let mut p = Parser::new("UpdateExpression", src, ph)?;
    let u = p.update()?;
    p.done()?;
    Ok(u)
}

// ------------------------------------------------------------------ evaluation

pub fn resolve<'a>(item: &'a Item, path: &Path) -> Option<&'a Av> {
    let Elem::Name(first) = path.first()? else { return None };
    let mut cur = item.get(first)?;
    for e in &path[1..] {
        cur = match (e, cur) {
            (Elem::Name(n), Av::M(m)) => m.get(n)?,
            (Elem::Index(i), Av::L(l)) => l.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn operand_value(item: &Item, o: &Operand) -> Option<Av> {
    match o {
        Operand::Value(v) => Some(v.clone()),
        Operand::Path(p) => resolve(item, p).cloned(),
        Operand::Size(p) => {
            let n = match resolve(item, p)? {
                Av::S(s) => s.len(),
                Av::B(b) => b.len(),
                Av::Ss(v) => v.len(),
                Av::Ns(v) => v.len(),
                Av::Bs(v) => v.len(),
                Av::L(v) => v.len(),
                Av::M(v) => v.len(),
                _ => return None,
            };
            Some(Av::N(BigDecimal::from(n as u64)))
        }
    }
}

fn compare(a: &Av, op: CmpOp, b: &Av) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        _ => match a.key_cmp(b) {
            Some(o) => match op {
                CmpOp::Lt => o.is_lt(),
                CmpOp::Le => o.is_le(),
                CmpOp::Gt => o.is_gt(),
                _ => o.is_ge(),
            },
            None => false,
        },
    }
}

pub fn eval(c: &Cond, item: &Item) -> bool {
    match c {
        Cond::Cmp(a, op, b) => match (operand_value(item, a), operand_value(item, b)) {
            (Some(x), Some(y)) => compare(&x, *op, &y),
            _ => false,
        },
        Cond::Between(a, lo, hi) => match (operand_value(item, a), operand_value(item, lo), operand_value(item, hi)) {
            (Some(x), Some(l), Some(h)) => compare(&x, CmpOp::Ge, &l) && compare(&x, CmpOp::Le, &h),
            _ => false,
        },
        Cond::In(a, list) => match operand_value(item, a) {
            Some(x) => list.iter().any(|o| operand_value(item, o).is_some_and(|y| x == y)),
            None => false,
        },
        Cond::And(a, b) => eval(a, item) && eval(b, item),
        Cond::Or(a, b) => eval(a, item) || eval(b, item),
        Cond::Not(a) => !eval(a, item),
        Cond::Exists(p) => resolve(item, p).is_some(),
        Cond::NotExists(p) => resolve(item, p).is_none(),
        Cond::Type(p, t) => match (resolve(item, p), operand_value(item, t)) {
            (Some(v), Some(Av::S(t))) => v.type_name() == t,
            _ => false,
        },
        Cond::BeginsWith(a, b) => match (operand_value(item, a), operand_value(item, b)) {
            (Some(Av::S(x)), Some(Av::S(y))) => x.starts_with(&y),
            (Some(Av::B(x)), Some(Av::B(y))) => x.starts_with(&y),
            _ => false,
        },
        Cond::Contains(a, b) => match (operand_value(item, a), operand_value(item, b)) {
            (Some(Av::S(x)), Some(Av::S(y))) => x.contains(&y),
            (Some(Av::B(x)), Some(Av::B(y))) => y.is_empty() || x.windows(y.len()).any(|w| w == y.as_slice()),
            (Some(Av::Ss(x)), Some(Av::S(y))) => x.contains(&y),
            (Some(Av::Ns(x)), Some(Av::N(y))) => x.contains(&y),
            (Some(Av::Bs(x)), Some(Av::B(y))) => x.contains(&y),
            (Some(Av::L(x)), Some(y)) => x.contains(&y),
            _ => false,
        },
    }
}

/// Keep only the projected paths (nested paths keep their structure).
pub fn project(item: &Item, paths: &[Path]) -> Item {
    let mut out = Item::new();
    for p in paths {
        if let Some(v) = resolve(item, p) {
            insert_projected(&mut out, p, v.clone());
        }
    }
    out
}

fn insert_projected(out: &mut Item, path: &Path, v: Av) {
    let Some(Elem::Name(first)) = path.first() else { return };
    if path.len() == 1 {
        out.insert(first.clone(), v);
        return;
    }
    let slot = out.entry(first.clone()).or_insert_with(|| match path[1] {
        Elem::Name(_) => Av::M(Item::new()),
        Elem::Index(_) => Av::L(vec![]),
    });
    insert_nested(slot, &path[1..], v);
}

fn insert_nested(slot: &mut Av, path: &[Elem], v: Av) {
    let fresh = |next: Option<&Elem>| match next {
        Some(Elem::Index(_)) => Av::L(vec![]),
        _ => Av::M(Item::new()),
    };
    match (slot, &path[0]) {
        (Av::M(m), Elem::Name(n)) => {
            if path.len() == 1 {
                m.insert(n.clone(), v);
            } else {
                let s = m.entry(n.clone()).or_insert_with(|| fresh(path.get(1)));
                insert_nested(s, &path[1..], v);
            }
        }
        (Av::L(l), Elem::Index(_)) => {
            // Projected list elements are compacted, as DynamoDB does.
            if path.len() == 1 {
                l.push(v);
            } else {
                l.push(fresh(path.get(1)));
                let last = l.last_mut().unwrap();
                insert_nested(last, &path[1..], v);
            }
        }
        _ => {}
    }
}

fn bad_operand() -> DynError {
    DynError::validation("An operand in the update expression has an incorrect data type")
}

fn eval_set_value(item: &Item, v: &SetValue) -> Result<Av, DynError> {
    Ok(match v {
        SetValue::Operand(o) => operand_value(item, o).ok_or_else(|| {
            DynError::validation(format!(
                "The provided expression refers to an attribute that does not exist in the item{}",
                match o {
                    Operand::Path(p) => format!("; attribute: {}", path_text(p)),
                    _ => String::new(),
                }
            ))
        })?,
        SetValue::IfNotExists(p, dflt) => match resolve(item, p) {
            Some(x) => x.clone(),
            None => eval_set_value(item, dflt)?,
        },
        SetValue::ListAppend(a, b) => match (eval_set_value(item, a)?, eval_set_value(item, b)?) {
            (Av::L(mut x), Av::L(y)) => {
                x.extend(y);
                Av::L(x)
            }
            _ => return Err(bad_operand()),
        },
        SetValue::Plus(a, b) | SetValue::Minus(a, b) => match (eval_set_value(item, a)?, eval_set_value(item, b)?) {
            (Av::N(x), Av::N(y)) => {
                let r = if matches!(v, SetValue::Plus(..)) { x + y } else { x - y };
                Av::N(super::value::parse_number(&r.to_plain_string())?)
            }
            _ => return Err(bad_operand()),
        },
    })
}

fn invalid_path() -> DynError {
    DynError::validation("The document path provided in the update expression is invalid for update")
}

fn set_path(item: &mut Item, path: &Path, v: Av) -> Result<(), DynError> {
    let Some(Elem::Name(first)) = path.first() else { return Err(invalid_path()) };
    if path.len() == 1 {
        item.insert(first.clone(), v);
        return Ok(());
    }
    let mut cur = item.get_mut(first).ok_or_else(invalid_path)?;
    for (i, e) in path[1..].iter().enumerate() {
        let last = i + 2 == path.len();
        match (e, cur) {
            (Elem::Name(n), Av::M(m)) => {
                if last {
                    m.insert(n.clone(), v);
                    return Ok(());
                }
                cur = m.get_mut(n).ok_or_else(invalid_path)?;
            }
            (Elem::Index(idx), Av::L(l)) => {
                if last {
                    if *idx < l.len() {
                        l[*idx] = v;
                    } else {
                        l.push(v);
                    }
                    return Ok(());
                }
                cur = l.get_mut(*idx).ok_or_else(invalid_path)?;
            }
            _ => return Err(invalid_path()),
        }
    }
    Ok(())
}

fn remove_path(item: &mut Item, path: &Path) {
    let Some(Elem::Name(first)) = path.first() else { return };
    if path.len() == 1 {
        item.remove(first);
        return;
    }
    let Some(mut cur) = item.get_mut(first) else { return };
    for (i, e) in path[1..].iter().enumerate() {
        let last = i + 2 == path.len();
        match (e, cur) {
            (Elem::Name(n), Av::M(m)) => {
                if last {
                    m.remove(n);
                    return;
                }
                match m.get_mut(n) {
                    Some(x) => cur = x,
                    None => return,
                }
            }
            (Elem::Index(idx), Av::L(l)) => {
                if last {
                    if *idx < l.len() {
                        l.remove(*idx);
                    }
                    return;
                }
                match l.get_mut(*idx) {
                    Some(x) => cur = x,
                    None => return,
                }
            }
            _ => return,
        }
    }
}

/// Apply an update expression; all values are computed from the item as it was before.
/// Returns the top-level attribute names that changed.
pub fn apply_update(item: &mut Item, u: &Update, key_attrs: &[String]) -> Result<Vec<String>, DynError> {
    let original = item.clone();
    let mut touched = Vec::new();
    let mut touch = |p: &Path| {
        if let Some(Elem::Name(n)) = p.first() {
            if key_attrs.contains(n) {
                return Err(DynError::validation(format!(
                    "One or more parameter values were invalid: Cannot update attribute {n}. This attribute is part of the key"
                )));
            }
            if !touched.contains(n) {
                touched.push(n.clone());
            }
        }
        Ok(())
    };
    for (p, v) in &u.set {
        touch(p)?;
        let value = eval_set_value(&original, v)?;
        set_path(item, p, value)?;
    }
    // Remove list elements from the highest index down so earlier removals do not shift later ones.
    let mut removes = u.remove.clone();
    removes.sort_by(|a, b| match (a.last(), b.last()) {
        (Some(Elem::Index(x)), Some(Elem::Index(y))) if a[..a.len() - 1] == b[..b.len() - 1] => y.cmp(x),
        _ => std::cmp::Ordering::Equal,
    });
    for p in &removes {
        touch(p)?;
        remove_path(item, p);
    }
    for (p, v) in &u.add {
        touch(p)?;
        let new = match (resolve(&original, p), v) {
            (None, Av::N(_) | Av::Ss(_) | Av::Ns(_) | Av::Bs(_)) => v.clone(),
            (Some(Av::N(a)), Av::N(b)) => Av::N(super::value::parse_number(&(a + b).to_plain_string())?),
            (Some(Av::Ss(a)), Av::Ss(b)) => Av::Ss(union(a, b)),
            (Some(Av::Ns(a)), Av::Ns(b)) => Av::Ns(union(a, b)),
            (Some(Av::Bs(a)), Av::Bs(b)) => Av::Bs(union(a, b)),
            _ => return Err(bad_operand()),
        };
        set_path(item, p, new)?;
    }
    for (p, v) in &u.delete {
        touch(p)?;
        let new = match (resolve(&original, p), v) {
            (None, _) => continue,
            (Some(Av::Ss(a)), Av::Ss(b)) => difference(a, b).map(Av::Ss),
            (Some(Av::Ns(a)), Av::Ns(b)) => difference(a, b).map(Av::Ns),
            (Some(Av::Bs(a)), Av::Bs(b)) => difference(a, b).map(Av::Bs),
            _ => return Err(bad_operand()),
        };
        match new {
            Some(v) => set_path(item, p, v)?,
            None => remove_path(item, p),
        }
    }
    Ok(touched)
}

fn union<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = a.to_vec();
    for x in b {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

fn difference<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Option<Vec<T>> {
    let out: Vec<T> = a.iter().filter(|x| !b.contains(x)).cloned().collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamo::value::item_from_json;
    use serde_json::json;

    fn item() -> Item {
        item_from_json(&json!({
            "pk": { "S": "user#1" }, "sk": { "N": "5" }, "name": { "S": "Ann" }, "tags": { "SS": ["a", "b"] },
            "profile": { "M": { "city": { "S": "Lisbon" }, "visits": { "L": [{ "N": "1" }, { "N": "2" }] } } }, "count": { "N": "3" }
        }))
        .unwrap()
    }

    fn cond(src: &str, req: Value) -> Result<bool, DynError> {
        let ph = Placeholders::new(&req);
        let c = parse_condition("ConditionExpression", src, &ph)?;
        ph.check_unused()?;
        Ok(eval(&c, &item()))
    }

    #[test]
    fn conditions_functions_and_reserved_words() {
        let vals = |v: Value| json!({ "ExpressionAttributeValues": v });
        assert!(cond("sk BETWEEN :a AND :b", vals(json!({ ":a": { "N": "1" }, ":b": { "N": "10" } }))).unwrap());
        assert!(cond("begins_with(pk, :p) AND contains(tags, :t)", vals(json!({ ":p": { "S": "user#" }, ":t": { "S": "b" } }))).unwrap());
        assert!(cond("profile.visits[1] = :two AND size(tags) = :two", vals(json!({ ":two": { "N": "2" } }))).unwrap());
        assert!(cond("attribute_exists(profile.city) AND attribute_not_exists(absent)", json!({})).unwrap());
        assert!(!cond("NOT (sk < :n) OR sk IN (:n)", vals(json!({ ":n": { "N": "9" } }))).unwrap());
        assert!(!cond("absent <> :n", vals(json!({ ":n": { "N": "9" } }))).unwrap(), "comparisons with missing attributes are false");
        assert!(cond("attribute_type(tags, :t)", vals(json!({ ":t": { "S": "SS" } }))).unwrap());
        let e = cond("#c > :n", json!({ "ExpressionAttributeNames": { "#c": "count" }, "ExpressionAttributeValues": { ":n": { "N": "1" }, ":unused": { "N": "1" } } })).unwrap_err();
        assert!(e.message.contains("unused in expressions: keys: {:unused}"), "{}", e.message);
        let e = cond("name = :n", vals(json!({ ":n": { "S": "Ann" } }))).unwrap_err();
        assert!(e.message.contains("reserved keyword: name"), "{}", e.message);
        let e = cond("pk = ", json!({})).unwrap_err();
        assert!(e.message.starts_with("Invalid ConditionExpression: Syntax error"), "{}", e.message);
        assert!(cond("pk = :missing", json!({})).unwrap_err().message.contains("not defined"));
    }

    #[test]
    fn updates_and_projection() {
        let req = json!({
            "ExpressionAttributeNames": { "#n": "name", "#c": "count" },
            "ExpressionAttributeValues": { ":one": { "N": "1" }, ":new": { "L": [{ "N": "3" }] }, ":t": { "SS": ["c", "a"] }, ":d": { "SS": ["b"] }, ":x": { "S": "x" } }
        });
        let ph = Placeholders::new(&req);
        let u = parse_update(
            "SET #c = #c + :one, profile.visits = list_append(profile.visits, :new), nick = if_not_exists(nick, :x) REMOVE #n ADD tags :t DELETE oldtags :d",
            &ph,
        );
        let u = u.unwrap();
        let mut it = item();
        it.insert("oldtags".into(), Av::Ss(vec!["b".into()]));
        let touched = apply_update(&mut it, &u, &["pk".into(), "sk".into()]).unwrap();
        assert_eq!(touched, vec!["count", "profile", "nick", "name", "tags", "oldtags"]);
        assert_eq!(it["count"], Av::N(4.into()));
        assert!(matches!(&it["profile"], Av::M(m) if matches!(&m["visits"], Av::L(l) if l.len() == 3)));
        assert_eq!(it["nick"], Av::S("x".into()));
        assert!(!it.contains_key("name") && !it.contains_key("oldtags"));
        assert_eq!(it["tags"], Av::Ss(vec!["a".into(), "b".into(), "c".into()]));
        let ph = Placeholders::new(&req);
        let key_update = parse_update("SET pk = :x", &ph).unwrap();
        assert!(apply_update(&mut it, &key_update, &["pk".into()]).unwrap_err().message.contains("part of the key"));
        let ph = Placeholders::new(&req);
        assert!(parse_update("SET a = :x, a.b = :x", &ph).unwrap_err().message.contains("overlap"));
        let names = json!({ "ExpressionAttributeNames": { "#n": "name" } });
        let ph = Placeholders::new(&names);
        let p = parse_projection("#n, profile.visits[1], profile.city", &ph).unwrap();
        let out = project(&item(), &p);
        assert_eq!(
            super::super::value::item_json(&out),
            json!({ "name": { "S": "Ann" }, "profile": { "M": { "city": { "S": "Lisbon" }, "visits": { "L": [{ "N": "2" }] } } } })
        );
    }
}
