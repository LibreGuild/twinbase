//! In-memory Redis data model and command implementations, with JSON snapshot persistence.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::resp::{Reply, format_double};

pub const DATABASES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score(pub f64);

impl Eq for Score {}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ZSet {
    scores: HashMap<Vec<u8>, f64>,
    order: BTreeSet<(Score, Vec<u8>)>,
}

impl ZSet {
    fn insert(&mut self, member: Vec<u8>, score: f64) -> bool {
        let added = match self.scores.insert(member.clone(), score) {
            Some(old) => {
                self.order.remove(&(Score(old), member.clone()));
                false
            }
            None => true,
        };
        self.order.insert((Score(score), member));
        added
    }

    fn remove(&mut self, member: &[u8]) -> bool {
        match self.scores.remove(member) {
            Some(s) => {
                self.order.remove(&(Score(s), member.to_vec()));
                true
            }
            None => false,
        }
    }

    fn len(&self) -> usize {
        self.scores.len()
    }

    fn sorted(&self) -> Vec<(Vec<u8>, f64)> {
        self.order.iter().map(|(s, m)| (m.clone(), s.0)).collect()
    }
}

#[derive(Debug, Clone)]
pub enum RValue {
    Str(Vec<u8>),
    List(VecDeque<Vec<u8>>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
    Set(HashSet<Vec<u8>>),
    ZSet(ZSet),
}

impl RValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            RValue::Str(_) => "string",
            RValue::List(_) => "list",
            RValue::Hash(_) => "hash",
            RValue::Set(_) => "set",
            RValue::ZSet(_) => "zset",
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            RValue::Str(_) => false,
            RValue::List(l) => l.is_empty(),
            RValue::Hash(h) => h.is_empty(),
            RValue::Set(s) => s.is_empty(),
            RValue::ZSet(z) => z.len() == 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub value: RValue,
    /// Expiry as Unix milliseconds.
    pub expires_at: Option<i64>,
}

type Db = HashMap<Vec<u8>, Entry>;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
const NOT_INT: &str = "ERR value is not an integer or out of range";
const NOT_FLOAT: &str = "ERR value is not a valid float";
const SYNTAX: &str = "ERR syntax error";

/// Redis-style glob matching for KEYS/SCAN/PSUBSCRIBE.
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => (0..=t.len()).any(|i| rec(&p[1..], &t[i..])),
            Some(b'?') => !t.is_empty() && rec(&p[1..], &t[1..]),
            Some(b'[') => {
                let Some(&c) = t.first() else { return false };
                let mut i = 1;
                let negate = p.get(1) == Some(&b'^');
                if negate {
                    i += 1;
                }
                let mut matched = false;
                while i < p.len() && p[i] != b']' {
                    if p[i] == b'\\' && i + 1 < p.len() {
                        matched |= p[i + 1] == c;
                        i += 2;
                    } else if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                        matched |= p[i] <= c && c <= p[i + 2];
                        i += 3;
                    } else {
                        matched |= p[i] == c;
                        i += 1;
                    }
                }
                if i >= p.len() {
                    return false;
                }
                matched != negate && rec(&p[i + 1..], &t[1..])
            }
            Some(b'\\') if p.len() > 1 => !t.is_empty() && t[0] == p[1] && rec(&p[2..], &t[1..]),
            Some(c) => !t.is_empty() && t[0] == *c && rec(&p[1..], &t[1..]),
        }
    }
    rec(pattern, text)
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.trim().parse().ok()
}

fn parse_f64(b: &[u8]) -> Option<f64> {
    let t = std::str::from_utf8(b).ok()?.trim().to_ascii_lowercase();
    match t.as_str() {
        "inf" | "+inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        _ => t.parse().ok().filter(|f: &f64| !f.is_nan()),
    }
}

/// Score bound for ZRANGEBYSCORE: `(1.5`, `-inf`, `+inf`.
fn parse_bound(b: &[u8]) -> Option<(f64, bool)> {
    match b.strip_prefix(b"(") {
        Some(rest) => parse_f64(rest).map(|f| (f, true)),
        None => parse_f64(b).map(|f| (f, false)),
    }
}

fn in_bounds(v: f64, min: (f64, bool), max: (f64, bool)) -> bool {
    let lo = if min.1 { v > min.0 } else { v >= min.0 };
    let hi = if max.1 { v < max.0 } else { v <= max.0 };
    lo && hi
}

/// Normalize Redis range indexes (negative from the end) into a Rust range.
fn range(start: i64, stop: i64, len: usize) -> Option<(usize, usize)> {
    let len = len as i64;
    let mut a = if start < 0 { len + start } else { start };
    let mut b = if stop < 0 { len + stop } else { stop };
    a = a.max(0);
    b = b.min(len - 1);
    if a > b || a >= len {
        return None;
    }
    Some((a as usize, b as usize))
}

fn wrong_args(cmd: &str) -> Reply {
    Reply::err(format!("ERR wrong number of arguments for '{}' command", cmd.to_ascii_lowercase()))
}

pub struct RedisStore {
    dbs: Mutex<Vec<Db>>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

#[derive(Serialize, Deserialize)]
struct SnapEntry {
    db: usize,
    key: String,
    #[serde(rename = "type")]
    kind: String,
    value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

fn enc(b: &[u8]) -> serde_json::Value {
    match std::str::from_utf8(b) {
        Ok(t) if !t.starts_with("base64:") => serde_json::Value::String(t.to_owned()),
        _ => {
            use std::fmt::Write;
            let mut hex = String::from("hex:");
            for x in b {
                let _ = write!(hex, "{x:02x}");
            }
            serde_json::Value::String(hex)
        }
    }
}

fn dec(v: &serde_json::Value) -> Vec<u8> {
    let t = v.as_str().unwrap_or_default();
    if let Some(h) = t.strip_prefix("hex:") {
        return (0..h.len()).step_by(2).filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect();
    }
    t.as_bytes().to_vec()
}

impl RedisStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let store = RedisStore { dbs: Mutex::new((0..DATABASES).map(|_| Db::new()).collect()), path, dirty: AtomicBool::new(false) };
        if let Some(p) = &store.path {
            let _ = store.load(p);
        }
        store
    }

    fn load(&self, path: &Path) -> std::io::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let entries: Vec<SnapEntry> = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        let mut dbs = self.dbs.lock();
        let now = now_ms();
        for e in entries {
            if e.db >= DATABASES || e.expires_at.is_some_and(|t| t <= now) {
                continue;
            }
            let arr = |v: &serde_json::Value| v.as_array().cloned().unwrap_or_default();
            let value = match e.kind.as_str() {
                "string" => RValue::Str(dec(&e.value)),
                "list" => RValue::List(arr(&e.value).iter().map(dec).collect()),
                "set" => RValue::Set(arr(&e.value).iter().map(dec).collect()),
                "hash" => RValue::Hash(
                    e.value
                        .as_object()
                        .map(|o| o.iter().map(|(k, v)| (dec(&serde_json::Value::String(k.clone())), dec(v))).collect())
                        .unwrap_or_default(),
                ),
                "zset" => {
                    let mut z = ZSet::default();
                    for pair in arr(&e.value) {
                        if let (Some(m), Some(sc)) = (pair.get(0), pair.get(1).and_then(|x| x.as_f64())) {
                            z.insert(dec(m), sc);
                        }
                    }
                    RValue::ZSet(z)
                }
                _ => continue,
            };
            dbs[e.db].insert(dec(&serde_json::Value::String(e.key)), Entry { value, expires_at: e.expires_at });
        }
        Ok(())
    }

    /// Write a snapshot if data changed since the last one.
    pub fn save_if_dirty(&self) -> std::io::Result<()> {
        if self.dirty.swap(false, Ordering::SeqCst)
            && let Err(e) = self.save()
        {
            self.dirty.store(true, Ordering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut out = Vec::new();
        {
            let dbs = self.dbs.lock();
            for (n, db) in dbs.iter().enumerate() {
                for (k, e) in db {
                    let value = match &e.value {
                        RValue::Str(v) => enc(v),
                        RValue::List(l) => serde_json::Value::Array(l.iter().map(|x| enc(x)).collect()),
                        RValue::Set(st) => serde_json::Value::Array(st.iter().map(|x| enc(x)).collect()),
                        RValue::Hash(h) => serde_json::Value::Object(
                            h.iter().map(|(k, v)| (enc(k).as_str().unwrap_or_default().to_owned(), enc(v))).collect(),
                        ),
                        RValue::ZSet(z) => {
                            serde_json::Value::Array(z.sorted().iter().map(|(m, sc)| serde_json::json!([enc(m), sc])).collect())
                        }
                    };
                    let key = enc(k).as_str().unwrap_or_default().to_owned();
                    out.push(SnapEntry { db: n, key, kind: e.value.type_name().into(), value, expires_at: e.expires_at });
                }
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&out).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Remove expired keys (called periodically).
    pub fn sweep(&self) {
        let now = now_ms();
        let mut dbs = self.dbs.lock();
        let mut removed = false;
        for db in dbs.iter_mut() {
            let before = db.len();
            db.retain(|_, e| e.expires_at.is_none_or(|t| t > now));
            removed |= db.len() != before;
        }
        if removed {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    pub fn key_count(&self) -> usize {
        self.dbs.lock().iter().map(|d| d.len()).sum()
    }

    /// Keys per non-empty database, for `INFO keyspace`.
    pub fn keyspace(&self) -> Vec<(usize, usize)> {
        self.dbs.lock().iter().enumerate().filter(|(_, d)| !d.is_empty()).map(|(i, d)| (i, d.len())).collect()
    }

    pub fn flush_all(&self) {
        for db in self.dbs.lock().iter_mut() {
            db.clear();
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Run several commands under one lock (MULTI/EXEC).
    pub fn execute_many(&self, db: &mut usize, commands: &[Vec<Vec<u8>>]) -> Vec<Reply> {
        let mut dbs = self.dbs.lock();
        commands.iter().map(|c| self.exec_locked(&mut dbs, db, c)).collect()
    }

    pub fn execute(&self, db: &mut usize, args: &[Vec<u8>]) -> Reply {
        let mut dbs = self.dbs.lock();
        self.exec_locked(&mut dbs, db, args)
    }

    fn exec_locked(&self, dbs: &mut [Db], dbi: &mut usize, args: &[Vec<u8>]) -> Reply {
        let name = s(&args[0]).to_ascii_uppercase();
        let reply = command(dbs, dbi, &name, args);
        if is_write(&name) && !matches!(reply, Reply::Error(_)) {
            self.dirty.store(true, Ordering::SeqCst);
        }
        reply
    }
}

fn is_write(cmd: &str) -> bool {
    !matches!(
        cmd,
        "GET"
            | "MGET"
            | "EXISTS"
            | "TYPE"
            | "TTL"
            | "PTTL"
            | "EXPIRETIME"
            | "PEXPIRETIME"
            | "KEYS"
            | "SCAN"
            | "STRLEN"
            | "GETRANGE"
            | "SUBSTR"
            | "HGET"
            | "HMGET"
            | "HGETALL"
            | "HEXISTS"
            | "HKEYS"
            | "HVALS"
            | "HLEN"
            | "HSTRLEN"
            | "HSCAN"
            | "LRANGE"
            | "LLEN"
            | "LINDEX"
            | "LPOS"
            | "SMEMBERS"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SCARD"
            | "SINTER"
            | "SUNION"
            | "SDIFF"
            | "SSCAN"
            | "ZSCORE"
            | "ZMSCORE"
            | "ZCARD"
            | "ZCOUNT"
            | "ZRANK"
            | "ZREVRANK"
            | "ZRANGE"
            | "ZREVRANGE"
            | "ZRANGEBYSCORE"
            | "ZREVRANGEBYSCORE"
            | "ZSCAN"
            | "DBSIZE"
            | "RANDOMKEY"
            | "SRANDMEMBER"
            | "HRANDFIELD"
            | "PING"
            | "ECHO"
            | "SINTERCARD"
    )
}

/// Fetch a live (non-expired) entry.
fn live<'a>(db: &'a mut Db, key: &[u8]) -> Option<&'a mut Entry> {
    let expired = db.get(key).is_some_and(|e| e.expires_at.is_some_and(|t| t <= now_ms()));
    if expired {
        db.remove(key);
    }
    db.get_mut(key)
}

macro_rules! get_typed {
    ($db:expr, $key:expr, $variant:ident) => {
        match live($db, $key) {
            None => None,
            Some(Entry { value: RValue::$variant(v), .. }) => Some(v),
            Some(_) => return Reply::err(WRONGTYPE),
        }
    };
}

macro_rules! get_or_create {
    ($db:expr, $key:expr, $variant:ident, $default:expr) => {{
        if live($db, $key).is_none() {
            $db.insert($key.to_vec(), Entry { value: RValue::$variant($default), expires_at: None });
        }
        match &mut $db.get_mut($key).unwrap().value {
            RValue::$variant(v) => v,
            _ => return Reply::err(WRONGTYPE),
        }
    }};
}

fn cleanup(db: &mut Db, key: &[u8]) {
    if db.get(key).is_some_and(|e| e.value.is_empty()) {
        db.remove(key);
    }
}

/// SCAN options: MATCH pattern, COUNT, TYPE.
type ScanArgs = (Option<Vec<u8>>, usize, Option<String>);

fn scan_args(args: &[Vec<u8>], start: usize) -> Result<ScanArgs, Reply> {
    let mut pattern = None;
    let mut count = 10usize;
    let mut ty = None;
    let mut i = start;
    while i < args.len() {
        let opt = s(&args[i]).to_ascii_uppercase();
        let val = args.get(i + 1).ok_or_else(|| Reply::err(SYNTAX))?;
        match opt.as_str() {
            "MATCH" => pattern = Some(val.clone()),
            "COUNT" => count = parse_i64(val).filter(|c| *c > 0).ok_or_else(|| Reply::err(NOT_INT))? as usize,
            "TYPE" => ty = Some(s(val).to_ascii_lowercase()),
            _ => return Err(Reply::err(SYNTAX)),
        }
        i += 2;
    }
    Ok((pattern, count, ty))
}

/// Cursor-based iteration over a sorted list of items.
fn cursor_page<T: Clone>(items: &[T], cursor: usize, count: usize) -> (usize, Vec<T>) {
    let end = (cursor + count).min(items.len());
    let page = items.get(cursor..end).map(|p| p.to_vec()).unwrap_or_default();
    (if end >= items.len() { 0 } else { end }, page)
}

fn zrange_reply(items: Vec<(Vec<u8>, f64)>, withscores: bool) -> Reply {
    let mut out = Vec::new();
    for (m, sc) in items {
        if withscores {
            out.push(Reply::Array(vec![Reply::Bulk(m), Reply::Double(sc)]));
        } else {
            out.push(Reply::Bulk(m));
        }
    }
    if withscores {
        // RESP2 clients expect a flat [member, score, ...] list.
        Reply::Array(out.into_iter().flat_map(|r| if let Reply::Array(v) = r { v } else { vec![r] }).collect())
    } else {
        Reply::Array(out)
    }
}

fn set_op(db: &mut Db, keys: &[Vec<u8>], op: &str) -> Result<HashSet<Vec<u8>>, Reply> {
    let mut sets = Vec::new();
    for k in keys {
        match live(db, k) {
            None => sets.push(HashSet::new()),
            Some(Entry { value: RValue::Set(v), .. }) => sets.push(v.clone()),
            Some(_) => return Err(Reply::err(WRONGTYPE)),
        }
    }
    let mut iter = sets.into_iter();
    let first = iter.next().unwrap_or_default();
    Ok(iter.fold(first, |acc, s2| match op {
        "inter" => acc.intersection(&s2).cloned().collect(),
        "union" => acc.union(&s2).cloned().collect(),
        _ => acc.difference(&s2).cloned().collect(),
    }))
}

fn sorted_members(set: &HashSet<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = set.iter().cloned().collect();
    v.sort();
    v
}

#[allow(clippy::cognitive_complexity)]
fn command(dbs: &mut [Db], dbi: &mut usize, name: &str, args: &[Vec<u8>]) -> Reply {
    let argc = args.len();
    let need = |n: usize| argc >= n;
    let db = &mut dbs[*dbi];
    macro_rules! arity {
        ($min:expr) => {
            if !need($min) {
                return wrong_args(name);
            }
        };
    }
    macro_rules! int_arg {
        ($i:expr) => {
            match parse_i64(&args[$i]) {
                Some(v) => v,
                None => return Reply::err(NOT_INT),
            }
        };
    }
    macro_rules! float_arg {
        ($i:expr) => {
            match parse_f64(&args[$i]) {
                Some(v) => v,
                None => return Reply::err(NOT_FLOAT),
            }
        };
    }
    match name {
        // ------------------------------------------------------------------ keyspace
        "DEL" | "UNLINK" => {
            arity!(2);
            Reply::Int(args[1..].iter().filter(|k| live(db, k).is_some() && db.remove(*k).is_some()).count() as i64)
        }
        "EXISTS" => {
            arity!(2);
            Reply::Int(args[1..].iter().filter(|k| live(db, k).is_some()).count() as i64)
        }
        "TOUCH" => {
            arity!(2);
            Reply::Int(args[1..].iter().filter(|k| live(db, k).is_some()).count() as i64)
        }
        "TYPE" => {
            arity!(2);
            Reply::Simple(live(db, &args[1]).map(|e| e.value.type_name()).unwrap_or("none").into())
        }
        "KEYS" => {
            arity!(2);
            let now = now_ms();
            let mut keys: Vec<&Vec<u8>> =
                db.iter().filter(|(k, e)| e.expires_at.is_none_or(|t| t > now) && glob_match(&args[1], k)).map(|(k, _)| k).collect();
            keys.sort();
            Reply::bulks(keys)
        }
        "SCAN" => {
            arity!(2);
            let cursor = int_arg!(1).max(0) as usize;
            let (pattern, count, ty) = match scan_args(args, 2) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let now = now_ms();
            let mut keys: Vec<Vec<u8>> = db.iter().filter(|(_, e)| e.expires_at.is_none_or(|t| t > now)).map(|(k, _)| k.clone()).collect();
            keys.sort();
            let (next, page) = cursor_page(&keys, cursor, count);
            let page: Vec<Vec<u8>> = page
                .into_iter()
                .filter(|k| pattern.as_ref().is_none_or(|p| glob_match(p, k)))
                .filter(|k| ty.as_ref().is_none_or(|t| db.get(k).is_some_and(|e| e.value.type_name() == t)))
                .collect();
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::bulks(page)])
        }
        "RANDOMKEY" => {
            let now = now_ms();
            Reply::opt_bulk(db.iter().filter(|(_, e)| e.expires_at.is_none_or(|t| t > now)).map(|(k, _)| k.clone()).next())
        }
        "RENAME" | "RENAMENX" => {
            arity!(3);
            if live(db, &args[1]).is_none() {
                return Reply::err("ERR no such key");
            }
            if name == "RENAMENX" && live(db, &args[2]).is_some() {
                return Reply::Int(0);
            }
            let e = db.remove(&args[1]).unwrap();
            db.insert(args[2].clone(), e);
            if name == "RENAMENX" { Reply::Int(1) } else { Reply::ok() }
        }
        "COPY" => {
            arity!(3);
            let replace = args[3..].iter().any(|a| s(a).eq_ignore_ascii_case("REPLACE"));
            let Some(e) = live(db, &args[1]).cloned() else { return Reply::Int(0) };
            if !replace && live(db, &args[2]).is_some() {
                return Reply::Int(0);
            }
            db.insert(args[2].clone(), e);
            Reply::Int(1)
        }
        "MOVE" => {
            arity!(3);
            let target = int_arg!(2);
            if target < 0 || target as usize >= DATABASES {
                return Reply::err("ERR DB index is out of range");
            }
            let Some(e) = live(db, &args[1]).cloned() else { return Reply::Int(0) };
            if dbs[target as usize].contains_key(&args[1]) {
                return Reply::Int(0);
            }
            dbs[*dbi].remove(&args[1]);
            dbs[target as usize].insert(args[1].clone(), e);
            Reply::Int(1)
        }
        "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" => {
            arity!(3);
            let n = int_arg!(2);
            let at = match name {
                "EXPIRE" => now_ms() + n * 1000,
                "PEXPIRE" => now_ms() + n,
                "EXPIREAT" => n * 1000,
                _ => n,
            };
            let flag = args.get(3).map(|f| s(f).to_ascii_uppercase());
            let Some(e) = live(db, &args[1]) else { return Reply::Int(0) };
            let ok = match flag.as_deref() {
                Some("NX") => e.expires_at.is_none(),
                Some("XX") => e.expires_at.is_some(),
                Some("GT") => e.expires_at.is_some_and(|t| at > t),
                Some("LT") => e.expires_at.is_none_or(|t| at < t),
                _ => true,
            };
            if !ok {
                return Reply::Int(0);
            }
            if at <= now_ms() {
                db.remove(&args[1]);
            } else {
                e.expires_at = Some(at);
            }
            Reply::Int(1)
        }
        "TTL" | "PTTL" | "EXPIRETIME" | "PEXPIRETIME" => {
            arity!(2);
            match live(db, &args[1]) {
                None => Reply::Int(-2),
                Some(Entry { expires_at: None, .. }) => Reply::Int(-1),
                Some(Entry { expires_at: Some(t), .. }) => Reply::Int(match name {
                    "TTL" => ((*t - now_ms()) as f64 / 1000.0).round() as i64,
                    "PTTL" => *t - now_ms(),
                    "EXPIRETIME" => *t / 1000,
                    _ => *t,
                }),
            }
        }
        "PERSIST" => {
            arity!(2);
            match live(db, &args[1]) {
                Some(e) if e.expires_at.is_some() => {
                    e.expires_at = None;
                    Reply::Int(1)
                }
                _ => Reply::Int(0),
            }
        }
        "DBSIZE" => {
            let now = now_ms();
            Reply::Int(db.values().filter(|e| e.expires_at.is_none_or(|t| t > now)).count() as i64)
        }
        "FLUSHDB" => {
            db.clear();
            Reply::ok()
        }
        "FLUSHALL" => {
            for d in dbs.iter_mut() {
                d.clear();
            }
            Reply::ok()
        }
        "SELECT" => {
            arity!(2);
            let n = int_arg!(1);
            if n < 0 || n as usize >= DATABASES {
                return Reply::err("ERR DB index is out of range");
            }
            *dbi = n as usize;
            Reply::ok()
        }
        "SWAPDB" => {
            arity!(3);
            let (a, b) = (int_arg!(1), int_arg!(2));
            if a < 0 || b < 0 || a as usize >= DATABASES || b as usize >= DATABASES {
                return Reply::err("ERR invalid DB index");
            }
            dbs.swap(a as usize, b as usize);
            Reply::ok()
        }
        // ------------------------------------------------------------------ strings
        "GET" => {
            arity!(2);
            Reply::opt_bulk(get_typed!(db, &args[1], Str).cloned())
        }
        "SET" => {
            arity!(3);
            let mut expires = None;
            let (mut nx, mut xx, mut get, mut keepttl) = (false, false, false, false);
            let mut i = 3;
            while i < argc {
                let opt = s(&args[i]).to_ascii_uppercase();
                let val = args.get(i + 1).and_then(|v| parse_i64(v));
                match opt.as_str() {
                    "NX" => nx = true,
                    "XX" => xx = true,
                    "GET" => get = true,
                    "KEEPTTL" => keepttl = true,
                    "EX" | "PX" | "EXAT" | "PXAT" => {
                        let Some(v) = val.filter(|v| *v > 0) else { return Reply::err("ERR invalid expire time in 'set' command") };
                        expires = Some(match opt.as_str() {
                            "EX" => now_ms() + v * 1000,
                            "PX" => now_ms() + v,
                            "EXAT" => v * 1000,
                            _ => v,
                        });
                        i += 1;
                    }
                    _ => return Reply::err(SYNTAX),
                }
                i += 1;
            }
            let old = match live(db, &args[1]) {
                None => None,
                Some(Entry { value: RValue::Str(v), expires_at }) => Some((v.clone(), *expires_at)),
                Some(_) if get => return Reply::err(WRONGTYPE),
                Some(e) => Some((Vec::new(), e.expires_at)),
            };
            let exists = live(db, &args[1]).is_some();
            if (nx && exists) || (xx && !exists) {
                return if get { Reply::opt_bulk(old.map(|o| o.0)) } else { Reply::Null };
            }
            let ttl = if keepttl { old.as_ref().and_then(|o| o.1) } else { expires };
            db.insert(args[1].clone(), Entry { value: RValue::Str(args[2].clone()), expires_at: ttl });
            if get { Reply::opt_bulk(old.map(|o| o.0)) } else { Reply::ok() }
        }
        "SETNX" => {
            arity!(3);
            if live(db, &args[1]).is_some() {
                return Reply::Int(0);
            }
            db.insert(args[1].clone(), Entry { value: RValue::Str(args[2].clone()), expires_at: None });
            Reply::Int(1)
        }
        "SETEX" | "PSETEX" => {
            arity!(4);
            let n = int_arg!(2);
            if n <= 0 {
                return Reply::err(format!("ERR invalid expire time in '{}' command", name.to_ascii_lowercase()));
            }
            let at = now_ms() + if name == "SETEX" { n * 1000 } else { n };
            db.insert(args[1].clone(), Entry { value: RValue::Str(args[3].clone()), expires_at: Some(at) });
            Reply::ok()
        }
        "MGET" => {
            arity!(2);
            Reply::Array(
                args[1..]
                    .iter()
                    .map(|k| match live(db, k) {
                        Some(Entry { value: RValue::Str(v), .. }) => Reply::Bulk(v.clone()),
                        _ => Reply::Null,
                    })
                    .collect(),
            )
        }
        "MSET" | "MSETNX" => {
            if argc < 3 || argc.is_multiple_of(2) {
                return wrong_args(name);
            }
            if name == "MSETNX" && args[1..].chunks(2).any(|p| live(db, &p[0]).is_some()) {
                return Reply::Int(0);
            }
            for p in args[1..].chunks(2) {
                db.insert(p[0].clone(), Entry { value: RValue::Str(p[1].clone()), expires_at: None });
            }
            if name == "MSETNX" { Reply::Int(1) } else { Reply::ok() }
        }
        "GETSET" => {
            arity!(3);
            let old = get_typed!(db, &args[1], Str).cloned();
            db.insert(args[1].clone(), Entry { value: RValue::Str(args[2].clone()), expires_at: None });
            Reply::opt_bulk(old)
        }
        "GETDEL" => {
            arity!(2);
            let old = get_typed!(db, &args[1], Str).cloned();
            db.remove(&args[1]);
            Reply::opt_bulk(old)
        }
        "GETEX" => {
            arity!(2);
            let v = get_typed!(db, &args[1], Str).cloned();
            if v.is_some() && argc >= 3 {
                let opt = s(&args[2]).to_ascii_uppercase();
                let n = args.get(3).and_then(|x| parse_i64(x)).unwrap_or(0);
                let e = live(db, &args[1]).unwrap();
                e.expires_at = match opt.as_str() {
                    "EX" => Some(now_ms() + n * 1000),
                    "PX" => Some(now_ms() + n),
                    "EXAT" => Some(n * 1000),
                    "PXAT" => Some(n),
                    "PERSIST" => None,
                    _ => e.expires_at,
                };
            }
            Reply::opt_bulk(v)
        }
        "APPEND" => {
            arity!(3);
            let v = get_or_create!(db, &args[1], Str, Vec::new());
            v.extend_from_slice(&args[2]);
            Reply::Int(v.len() as i64)
        }
        "STRLEN" => {
            arity!(2);
            Reply::Int(get_typed!(db, &args[1], Str).map(|v| v.len()).unwrap_or(0) as i64)
        }
        "GETRANGE" | "SUBSTR" => {
            arity!(4);
            let (a, b) = (int_arg!(2), int_arg!(3));
            let v = get_typed!(db, &args[1], Str).cloned().unwrap_or_default();
            match range(a, b, v.len()) {
                Some((x, y)) => Reply::Bulk(v[x..=y].to_vec()),
                None => Reply::bulk(""),
            }
        }
        "SETRANGE" => {
            arity!(4);
            let off = int_arg!(2);
            if off < 0 {
                return Reply::err("ERR offset is out of range");
            }
            let v = get_or_create!(db, &args[1], Str, Vec::new());
            let off = off as usize;
            if v.len() < off + args[3].len() {
                v.resize(off + args[3].len(), 0);
            }
            v[off..off + args[3].len()].copy_from_slice(&args[3]);
            Reply::Int(v.len() as i64)
        }
        "INCR" | "DECR" | "INCRBY" | "DECRBY" => {
            arity!(if name.ends_with("BY") { 3 } else { 2 });
            let delta = match name {
                "INCR" => 1,
                "DECR" => -1,
                "INCRBY" => int_arg!(2),
                _ => match parse_i64(&args[2]) {
                    Some(v) => -v,
                    None => return Reply::err(NOT_INT),
                },
            };
            let v = get_or_create!(db, &args[1], Str, b"0".to_vec());
            let Some(cur) = parse_i64(v) else { return Reply::err(NOT_INT) };
            let Some(n) = cur.checked_add(delta) else { return Reply::err("ERR increment or decrement would overflow") };
            *v = n.to_string().into_bytes();
            Reply::Int(n)
        }
        "INCRBYFLOAT" => {
            arity!(3);
            let d = float_arg!(2);
            let v = get_or_create!(db, &args[1], Str, b"0".to_vec());
            let Some(cur) = parse_f64(v) else { return Reply::err(NOT_FLOAT) };
            let n = cur + d;
            *v = format_double(n).into_bytes();
            Reply::Bulk(v.clone())
        }
        // ------------------------------------------------------------------ hashes
        "HSET" | "HMSET" => {
            if argc < 4 || !argc.is_multiple_of(2) {
                return wrong_args(name);
            }
            let h = get_or_create!(db, &args[1], Hash, HashMap::new());
            let mut added = 0;
            for p in args[2..].chunks(2) {
                if h.insert(p[0].clone(), p[1].clone()).is_none() {
                    added += 1;
                }
            }
            if name == "HMSET" { Reply::ok() } else { Reply::Int(added) }
        }
        "HSETNX" => {
            arity!(4);
            let h = get_or_create!(db, &args[1], Hash, HashMap::new());
            if h.contains_key(&args[2]) {
                return Reply::Int(0);
            }
            h.insert(args[2].clone(), args[3].clone());
            Reply::Int(1)
        }
        "HGET" => {
            arity!(3);
            Reply::opt_bulk(get_typed!(db, &args[1], Hash).and_then(|h| h.get(&args[2]).cloned()))
        }
        "HMGET" => {
            arity!(3);
            let h = get_typed!(db, &args[1], Hash).cloned().unwrap_or_default();
            Reply::Array(args[2..].iter().map(|f| Reply::opt_bulk(h.get(f))).collect())
        }
        "HGETALL" => {
            arity!(2);
            let h = get_typed!(db, &args[1], Hash).cloned().unwrap_or_default();
            let mut items: Vec<(Vec<u8>, Vec<u8>)> = h.into_iter().collect();
            items.sort();
            Reply::Map(items.into_iter().map(|(k, v)| (Reply::Bulk(k), Reply::Bulk(v))).collect())
        }
        "HDEL" => {
            arity!(3);
            let Some(h) = get_typed!(db, &args[1], Hash) else { return Reply::Int(0) };
            let n = args[2..].iter().filter(|f| h.remove(*f).is_some()).count();
            cleanup(db, &args[1]);
            Reply::Int(n as i64)
        }
        "HEXISTS" => {
            arity!(3);
            Reply::Int(get_typed!(db, &args[1], Hash).is_some_and(|h| h.contains_key(&args[2])) as i64)
        }
        "HKEYS" | "HVALS" => {
            arity!(2);
            let h = get_typed!(db, &args[1], Hash).cloned().unwrap_or_default();
            let mut items: Vec<(Vec<u8>, Vec<u8>)> = h.into_iter().collect();
            items.sort();
            Reply::bulks(items.into_iter().map(|(k, v)| if name == "HKEYS" { k } else { v }))
        }
        "HLEN" => {
            arity!(2);
            Reply::Int(get_typed!(db, &args[1], Hash).map(|h| h.len()).unwrap_or(0) as i64)
        }
        "HSTRLEN" => {
            arity!(3);
            Reply::Int(get_typed!(db, &args[1], Hash).and_then(|h| h.get(&args[2]).map(|v| v.len())).unwrap_or(0) as i64)
        }
        "HINCRBY" => {
            arity!(4);
            let d = int_arg!(3);
            let h = get_or_create!(db, &args[1], Hash, HashMap::new());
            let cur = match h.get(&args[2]) {
                Some(v) => match parse_i64(v) {
                    Some(n) => n,
                    None => return Reply::err("ERR hash value is not an integer"),
                },
                None => 0,
            };
            let n = cur + d;
            h.insert(args[2].clone(), n.to_string().into_bytes());
            Reply::Int(n)
        }
        "HINCRBYFLOAT" => {
            arity!(4);
            let d = float_arg!(3);
            let h = get_or_create!(db, &args[1], Hash, HashMap::new());
            let cur = h.get(&args[2]).and_then(|v| parse_f64(v)).unwrap_or(0.0);
            let n = format_double(cur + d);
            h.insert(args[2].clone(), n.clone().into_bytes());
            Reply::bulk(n)
        }
        "HSCAN" => {
            arity!(3);
            let cursor = int_arg!(2).max(0) as usize;
            let (pattern, count, _) = match scan_args(args, 3) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let h = get_typed!(db, &args[1], Hash).cloned().unwrap_or_default();
            let mut items: Vec<(Vec<u8>, Vec<u8>)> = h.into_iter().collect();
            items.sort();
            let (next, page) = cursor_page(&items, cursor, count);
            let flat: Vec<Vec<u8>> =
                page.into_iter().filter(|(k, _)| pattern.as_ref().is_none_or(|p| glob_match(p, k))).flat_map(|(k, v)| [k, v]).collect();
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::bulks(flat)])
        }
        "HRANDFIELD" => {
            arity!(2);
            let h = get_typed!(db, &args[1], Hash).cloned().unwrap_or_default();
            Reply::opt_bulk(h.keys().next().cloned())
        }
        // ------------------------------------------------------------------ lists
        "LPUSH" | "RPUSH" | "LPUSHX" | "RPUSHX" => {
            arity!(3);
            if name.ends_with('X') && live(db, &args[1]).is_none() {
                return Reply::Int(0);
            }
            let l = get_or_create!(db, &args[1], List, VecDeque::new());
            for v in &args[2..] {
                if name.starts_with('L') {
                    l.push_front(v.clone());
                } else {
                    l.push_back(v.clone());
                }
            }
            Reply::Int(l.len() as i64)
        }
        "LPOP" | "RPOP" => {
            arity!(2);
            let count = if argc > 2 { Some(int_arg!(2).max(0) as usize) } else { None };
            let Some(l) = get_typed!(db, &args[1], List) else { return if count.is_some() { Reply::NullArray } else { Reply::Null } };
            let mut out = Vec::new();
            for _ in 0..count.unwrap_or(1) {
                match if name == "LPOP" { l.pop_front() } else { l.pop_back() } {
                    Some(v) => out.push(v),
                    None => break,
                }
            }
            cleanup(db, &args[1]);
            match count {
                Some(_) => Reply::bulks(out),
                None => Reply::opt_bulk(out.into_iter().next()),
            }
        }
        "LLEN" => {
            arity!(2);
            Reply::Int(get_typed!(db, &args[1], List).map(|l| l.len()).unwrap_or(0) as i64)
        }
        "LRANGE" => {
            arity!(4);
            let (a, b) = (int_arg!(2), int_arg!(3));
            let l = get_typed!(db, &args[1], List).cloned().unwrap_or_default();
            match range(a, b, l.len()) {
                Some((x, y)) => Reply::bulks(l.range(x..=y)),
                None => Reply::Array(vec![]),
            }
        }
        "LINDEX" => {
            arity!(3);
            let i = int_arg!(2);
            let l = get_typed!(db, &args[1], List).cloned().unwrap_or_default();
            let idx = if i < 0 { l.len() as i64 + i } else { i };
            Reply::opt_bulk(if idx >= 0 { l.get(idx as usize).cloned() } else { None })
        }
        "LSET" => {
            arity!(4);
            let i = int_arg!(2);
            let Some(l) = get_typed!(db, &args[1], List) else { return Reply::err("ERR no such key") };
            let idx = if i < 0 { l.len() as i64 + i } else { i };
            match (idx >= 0).then(|| l.get_mut(idx as usize)).flatten() {
                Some(slot) => {
                    *slot = args[3].clone();
                    Reply::ok()
                }
                None => Reply::err("ERR index out of range"),
            }
        }
        "LREM" => {
            arity!(4);
            let count = int_arg!(2);
            let Some(l) = get_typed!(db, &args[1], List) else { return Reply::Int(0) };
            let target = &args[3];
            let mut removed = 0;
            let limit = if count == 0 { usize::MAX } else { count.unsigned_abs() as usize };
            if count >= 0 {
                let mut i = 0;
                while i < l.len() && removed < limit {
                    if &l[i] == target {
                        l.remove(i);
                        removed += 1;
                    } else {
                        i += 1;
                    }
                }
            } else {
                let mut i = l.len();
                while i > 0 && removed < limit {
                    i -= 1;
                    if &l[i] == target {
                        l.remove(i);
                        removed += 1;
                    }
                }
            }
            cleanup(db, &args[1]);
            Reply::Int(removed as i64)
        }
        "LTRIM" => {
            arity!(4);
            let (a, b) = (int_arg!(2), int_arg!(3));
            let Some(l) = get_typed!(db, &args[1], List) else { return Reply::ok() };
            *l = match range(a, b, l.len()) {
                Some((x, y)) => l.range(x..=y).cloned().collect(),
                None => VecDeque::new(),
            };
            cleanup(db, &args[1]);
            Reply::ok()
        }
        "LINSERT" => {
            arity!(5);
            let before = s(&args[2]).eq_ignore_ascii_case("BEFORE");
            let Some(l) = get_typed!(db, &args[1], List) else { return Reply::Int(0) };
            match l.iter().position(|v| v == &args[3]) {
                Some(p) => {
                    l.insert(if before { p } else { p + 1 }, args[4].clone());
                    Reply::Int(l.len() as i64)
                }
                None => Reply::Int(-1),
            }
        }
        "LPOS" => {
            arity!(3);
            let l = get_typed!(db, &args[1], List).cloned().unwrap_or_default();
            Reply::opt_bulk(l.iter().position(|v| v == &args[2]).map(|p| p.to_string())).map_int()
        }
        "RPOPLPUSH" | "LMOVE" | "BLMOVE" | "BRPOPLPUSH" => {
            arity!(3);
            let (from_left, to_left) = if name.contains("LMOVE") {
                arity!(5);
                (s(&args[3]).eq_ignore_ascii_case("LEFT"), s(&args[4]).eq_ignore_ascii_case("LEFT"))
            } else {
                (false, true)
            };
            let v = {
                let Some(l) = get_typed!(db, &args[1], List) else { return Reply::Null };

                if from_left { l.pop_front() } else { l.pop_back() }
            };
            cleanup(db, &args[1]);
            let Some(v) = v else { return Reply::Null };
            let dst = get_or_create!(db, &args[2], List, VecDeque::new());
            if to_left {
                dst.push_front(v.clone());
            } else {
                dst.push_back(v.clone());
            }
            Reply::Bulk(v)
        }
        "BLPOP" | "BRPOP" => {
            arity!(3);
            for k in &args[1..argc - 1] {
                let popped = match live(db, k) {
                    Some(Entry { value: RValue::List(l), .. }) => {
                        if name == "BLPOP" {
                            l.pop_front()
                        } else {
                            l.pop_back()
                        }
                    }
                    Some(_) => return Reply::err(WRONGTYPE),
                    None => None,
                };
                if let Some(v) = popped {
                    cleanup(db, k);
                    return Reply::Array(vec![Reply::Bulk(k.clone()), Reply::Bulk(v)]);
                }
            }
            Reply::NullArray
        }
        // ------------------------------------------------------------------ sets
        "SADD" => {
            arity!(3);
            let set = get_or_create!(db, &args[1], Set, HashSet::new());
            Reply::Int(args[2..].iter().filter(|m| set.insert((*m).clone())).count() as i64)
        }
        "SREM" => {
            arity!(3);
            let Some(set) = get_typed!(db, &args[1], Set) else { return Reply::Int(0) };
            let n = args[2..].iter().filter(|m| set.remove(*m)).count();
            cleanup(db, &args[1]);
            Reply::Int(n as i64)
        }
        "SMEMBERS" => {
            arity!(2);
            let set = get_typed!(db, &args[1], Set).cloned().unwrap_or_default();
            Reply::Set(sorted_members(&set).into_iter().map(Reply::Bulk).collect())
        }
        "SISMEMBER" => {
            arity!(3);
            Reply::Int(get_typed!(db, &args[1], Set).is_some_and(|st| st.contains(&args[2])) as i64)
        }
        "SMISMEMBER" => {
            arity!(3);
            let set = get_typed!(db, &args[1], Set).cloned().unwrap_or_default();
            Reply::Array(args[2..].iter().map(|m| Reply::Int(set.contains(m) as i64)).collect())
        }
        "SCARD" => {
            arity!(2);
            Reply::Int(get_typed!(db, &args[1], Set).map(|st| st.len()).unwrap_or(0) as i64)
        }
        "SPOP" | "SRANDMEMBER" => {
            arity!(2);
            let count = if argc > 2 { Some(int_arg!(2)) } else { None };
            let Some(set) = get_typed!(db, &args[1], Set) else { return if count.is_some() { Reply::Array(vec![]) } else { Reply::Null } };
            let mut members = sorted_members(set);
            use rand::seq::SliceRandom;
            members.shuffle(&mut rand::rng());
            let n = count.map(|c| c.unsigned_abs() as usize).unwrap_or(1).min(members.len());
            let picked: Vec<Vec<u8>> = members.into_iter().take(n).collect();
            if name == "SPOP" {
                for m in &picked {
                    set.remove(m);
                }
                cleanup(db, &args[1]);
            }
            match count {
                Some(_) => Reply::bulks(picked),
                None => Reply::opt_bulk(picked.into_iter().next()),
            }
        }
        "SINTER" | "SUNION" | "SDIFF" => {
            arity!(2);
            let op = match name {
                "SINTER" => "inter",
                "SUNION" => "union",
                _ => "diff",
            };
            match set_op(db, &args[1..], op) {
                Ok(r) => Reply::Set(sorted_members(&r).into_iter().map(Reply::Bulk).collect()),
                Err(e) => e,
            }
        }
        "SINTERCARD" => {
            arity!(3);
            let n = int_arg!(1).max(0) as usize;
            match set_op(db, &args[2..2 + n.min(argc - 2)], "inter") {
                Ok(r) => Reply::Int(r.len() as i64),
                Err(e) => e,
            }
        }
        "SINTERSTORE" | "SUNIONSTORE" | "SDIFFSTORE" => {
            arity!(3);
            let op = match name {
                "SINTERSTORE" => "inter",
                "SUNIONSTORE" => "union",
                _ => "diff",
            };
            match set_op(db, &args[2..], op) {
                Ok(r) => {
                    let n = r.len();
                    if r.is_empty() {
                        db.remove(&args[1]);
                    } else {
                        db.insert(args[1].clone(), Entry { value: RValue::Set(r), expires_at: None });
                    }
                    Reply::Int(n as i64)
                }
                Err(e) => e,
            }
        }
        "SMOVE" => {
            arity!(4);
            let removed = match get_typed!(db, &args[1], Set) {
                Some(set) => set.remove(&args[3]),
                None => false,
            };
            if !removed {
                return Reply::Int(0);
            }
            cleanup(db, &args[1]);
            let dst = get_or_create!(db, &args[2], Set, HashSet::new());
            dst.insert(args[3].clone());
            Reply::Int(1)
        }
        "SSCAN" => {
            arity!(3);
            let cursor = int_arg!(2).max(0) as usize;
            let (pattern, count, _) = match scan_args(args, 3) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let set = get_typed!(db, &args[1], Set).cloned().unwrap_or_default();
            let members = sorted_members(&set);
            let (next, page) = cursor_page(&members, cursor, count);
            let page: Vec<Vec<u8>> = page.into_iter().filter(|m| pattern.as_ref().is_none_or(|p| glob_match(p, m))).collect();
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::bulks(page)])
        }
        // ------------------------------------------------------------------ sorted sets
        "ZADD" => {
            arity!(4);
            let (mut nx, mut xx, mut gt, mut lt, mut ch, mut incr) = (false, false, false, false, false, false);
            let mut i = 2;
            while i < argc {
                match s(&args[i]).to_ascii_uppercase().as_str() {
                    "NX" => nx = true,
                    "XX" => xx = true,
                    "GT" => gt = true,
                    "LT" => lt = true,
                    "CH" => ch = true,
                    "INCR" => incr = true,
                    _ => break,
                }
                i += 1;
            }
            let rest = &args[i..];
            if rest.is_empty() || !rest.len().is_multiple_of(2) {
                return Reply::err(SYNTAX);
            }
            let mut pairs = Vec::new();
            for p in rest.chunks(2) {
                let Some(sc) = parse_f64(&p[0]) else { return Reply::err(NOT_FLOAT) };
                pairs.push((sc, p[1].clone()));
            }
            let z = get_or_create!(db, &args[1], ZSet, ZSet::default());
            let mut added = 0;
            let mut changed = 0;
            let mut last = None;
            for (sc, m) in pairs {
                let old = z.scores.get(&m).copied();
                if (nx && old.is_some()) || (xx && old.is_none()) {
                    continue;
                }
                let new = if incr { old.unwrap_or(0.0) + sc } else { sc };
                if let Some(o) = old
                    && ((gt && new <= o) || (lt && new >= o))
                {
                    continue;
                }
                if z.insert(m, new) {
                    added += 1;
                } else if old != Some(new) {
                    changed += 1;
                }
                last = Some(new);
            }
            cleanup(db, &args[1]);
            if incr {
                return match last {
                    Some(v) => Reply::Double(v),
                    None => Reply::Null,
                };
            }
            Reply::Int(if ch { added + changed } else { added })
        }
        "ZREM" => {
            arity!(3);
            let Some(z) = get_typed!(db, &args[1], ZSet) else { return Reply::Int(0) };
            let n = args[2..].iter().filter(|m| z.remove(m)).count();
            cleanup(db, &args[1]);
            Reply::Int(n as i64)
        }
        "ZSCORE" => {
            arity!(3);
            match get_typed!(db, &args[1], ZSet).and_then(|z| z.scores.get(&args[2]).copied()) {
                Some(v) => Reply::Double(v),
                None => Reply::Null,
            }
        }
        "ZMSCORE" => {
            arity!(3);
            let z = get_typed!(db, &args[1], ZSet).cloned().unwrap_or_default();
            Reply::Array(args[2..].iter().map(|m| z.scores.get(m).map(|v| Reply::Double(*v)).unwrap_or(Reply::Null)).collect())
        }
        "ZINCRBY" => {
            arity!(4);
            let d = float_arg!(2);
            let z = get_or_create!(db, &args[1], ZSet, ZSet::default());
            let n = z.scores.get(&args[3]).copied().unwrap_or(0.0) + d;
            z.insert(args[3].clone(), n);
            Reply::Double(n)
        }
        "ZCARD" => {
            arity!(2);
            Reply::Int(get_typed!(db, &args[1], ZSet).map(|z| z.len()).unwrap_or(0) as i64)
        }
        "ZCOUNT" => {
            arity!(4);
            let (Some(min), Some(max)) = (parse_bound(&args[2]), parse_bound(&args[3])) else {
                return Reply::err("ERR min or max is not a float");
            };
            let z = get_typed!(db, &args[1], ZSet).cloned().unwrap_or_default();
            Reply::Int(z.sorted().iter().filter(|(_, sc)| in_bounds(*sc, min, max)).count() as i64)
        }
        "ZRANK" | "ZREVRANK" => {
            arity!(3);
            let z = get_typed!(db, &args[1], ZSet).cloned().unwrap_or_default();
            let mut sorted = z.sorted();
            if name == "ZREVRANK" {
                sorted.reverse();
            }
            match sorted.iter().position(|(m, _)| m == &args[2]) {
                Some(p) => Reply::Int(p as i64),
                None => Reply::Null,
            }
        }
        "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" => {
            arity!(4);
            let opts: Vec<String> = args[4..].iter().map(|a| s(a).to_ascii_uppercase()).collect();
            let withscores = opts.iter().any(|o| o == "WITHSCORES");
            let mut rev = name.starts_with("ZREV") || opts.iter().any(|o| o == "REV");
            let byscore = name.contains("BYSCORE") || opts.iter().any(|o| o == "BYSCORE");
            let limit = opts
                .iter()
                .position(|o| o == "LIMIT")
                .and_then(|p| Some((parse_i64(args.get(4 + p + 1)?)?, parse_i64(args.get(4 + p + 2)?)?)));
            let z = get_typed!(db, &args[1], ZSet).cloned().unwrap_or_default();
            let mut sorted = z.sorted();
            let items: Vec<(Vec<u8>, f64)> = if byscore {
                let (lo, hi) = if name.starts_with("ZREV") || rev { (&args[3], &args[2]) } else { (&args[2], &args[3]) };
                let (Some(min), Some(max)) = (parse_bound(lo), parse_bound(hi)) else { return Reply::err("ERR min or max is not a float") };
                if rev {
                    sorted.reverse();
                }
                let mut v: Vec<(Vec<u8>, f64)> = sorted.into_iter().filter(|(_, sc)| in_bounds(*sc, min, max)).collect();
                if let Some((off, cnt)) = limit {
                    v = v.into_iter().skip(off.max(0) as usize).take(if cnt < 0 { usize::MAX } else { cnt as usize }).collect();
                }
                v
            } else {
                let (a, b) = (int_arg!(2), int_arg!(3));
                if rev {
                    sorted.reverse();
                    rev = false;
                }
                let _ = rev;
                match range(a, b, sorted.len()) {
                    Some((x, y)) => sorted[x..=y].to_vec(),
                    None => vec![],
                }
            };
            zrange_reply(items, withscores)
        }
        "ZPOPMIN" | "ZPOPMAX" => {
            arity!(2);
            let count = if argc > 2 { int_arg!(2).max(0) as usize } else { 1 };
            let Some(z) = get_typed!(db, &args[1], ZSet) else { return Reply::Array(vec![]) };
            let mut sorted = z.sorted();
            if name == "ZPOPMAX" {
                sorted.reverse();
            }
            let taken: Vec<(Vec<u8>, f64)> = sorted.into_iter().take(count).collect();
            for (m, _) in &taken {
                z.remove(m);
            }
            cleanup(db, &args[1]);
            zrange_reply(taken, true)
        }
        "ZREMRANGEBYRANK" | "ZREMRANGEBYSCORE" => {
            arity!(4);
            let Some(z) = get_typed!(db, &args[1], ZSet) else { return Reply::Int(0) };
            let sorted = z.sorted();
            let doomed: Vec<Vec<u8>> = if name == "ZREMRANGEBYRANK" {
                let (Some(a), Some(b)) = (parse_i64(&args[2]), parse_i64(&args[3])) else { return Reply::err(NOT_INT) };
                range(a, b, sorted.len()).map(|(x, y)| sorted[x..=y].iter().map(|(m, _)| m.clone()).collect()).unwrap_or_default()
            } else {
                let (Some(min), Some(max)) = (parse_bound(&args[2]), parse_bound(&args[3])) else {
                    return Reply::err("ERR min or max is not a float");
                };
                sorted.into_iter().filter(|(_, sc)| in_bounds(*sc, min, max)).map(|(m, _)| m).collect()
            };
            for m in &doomed {
                z.remove(m);
            }
            cleanup(db, &args[1]);
            Reply::Int(doomed.len() as i64)
        }
        "ZSCAN" => {
            arity!(3);
            let cursor = int_arg!(2).max(0) as usize;
            let (pattern, count, _) = match scan_args(args, 3) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let z = get_typed!(db, &args[1], ZSet).cloned().unwrap_or_default();
            let sorted = z.sorted();
            let (next, page) = cursor_page(&sorted, cursor, count);
            let flat: Vec<Vec<u8>> = page
                .into_iter()
                .filter(|(m, _)| pattern.as_ref().is_none_or(|p| glob_match(p, m)))
                .flat_map(|(m, sc)| [m, format_double(sc).into_bytes()])
                .collect();
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::bulks(flat)])
        }
        _ => Reply::err(format!(
            "ERR unknown command '{}', with args beginning with: {}",
            s(&args[0]),
            args[1..].iter().take(3).map(|a| format!("'{}' ", s(a))).collect::<String>()
        )),
    }
}

trait MapInt {
    fn map_int(self) -> Reply;
}

impl MapInt for Reply {
    /// LPOS returns an integer, not a bulk string.
    fn map_int(self) -> Reply {
        match self {
            Reply::Bulk(b) => parse_i64(&b).map(Reply::Int).unwrap_or(Reply::Null),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(st: &RedisStore, db: &mut usize, cmd: &str) -> Reply {
        let args: Vec<Vec<u8>> = cmd.split_whitespace().map(|a| a.as_bytes().to_vec()).collect();
        st.execute(db, &args)
    }

    #[test]
    fn strings_lists_hashes_sets_zsets() {
        let st = RedisStore::new(None);
        let mut db = 0;
        assert_eq!(run(&st, &mut db, "SET a 1"), Reply::ok());
        assert_eq!(run(&st, &mut db, "INCRBY a 41"), Reply::Int(42));
        assert_eq!(run(&st, &mut db, "LPUSH l x"), Reply::Int(1));
        assert_eq!(run(&st, &mut db, "GET l"), Reply::err(WRONGTYPE));
        run(&st, &mut db, "RPUSH l y z");
        assert_eq!(run(&st, &mut db, "LRANGE l 0 -1"), Reply::bulks(["x", "y", "z"]));
        run(&st, &mut db, "HSET h f1 v1 f2 v2");
        assert_eq!(run(&st, &mut db, "HGET h f2"), Reply::bulk("v2"));
        run(&st, &mut db, "SADD s b a c a");
        assert_eq!(run(&st, &mut db, "SCARD s"), Reply::Int(3));
        run(&st, &mut db, "ZADD z 2 two 1 one 3 three");
        assert_eq!(run(&st, &mut db, "ZRANGE z 0 -1"), Reply::bulks(["one", "two", "three"]));
        assert_eq!(run(&st, &mut db, "ZRANGEBYSCORE z (1 +inf"), Reply::bulks(["two", "three"]));
        assert_eq!(run(&st, &mut db, "KEYS *"), Reply::bulks(["a", "h", "l", "s", "z"]));
        assert_eq!(run(&st, &mut db, "SET e v PX 1"), Reply::ok());
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(run(&st, &mut db, "GET e"), Reply::Null);
        assert_eq!(run(&st, &mut db, "EXPIRE a 100"), Reply::Int(1));
        assert_eq!(run(&st, &mut db, "TTL a"), Reply::Int(100));
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match(b"user:*", b"user:42"));
        assert!(glob_match(b"h?llo", b"hello"));
        assert!(glob_match(b"h[ae]llo", b"hallo"));
        assert!(!glob_match(b"h[^e]llo", b"hello"));
        assert!(glob_match(b"h[a-c]llo", b"hbllo"));
    }

    #[test]
    fn snapshot_round_trip() {
        let path = std::env::temp_dir().join(format!("tb-redis-{}.json", uuid::Uuid::new_v4()));
        {
            let st = RedisStore::new(Some(path.clone()));
            let mut db = 0;
            run(&st, &mut db, "HSET user:1 name ann");
            run(&st, &mut db, "ZADD board 10 ann");
            run(&st, &mut db, "SELECT 3");
            run(&st, &mut db, "SET x y");
            st.save_if_dirty().unwrap();
        }
        let st = RedisStore::new(Some(path.clone()));
        let mut db = 0;
        assert_eq!(run(&st, &mut db, "HGET user:1 name"), Reply::bulk("ann"));
        assert_eq!(run(&st, &mut db, "ZSCORE board ann"), Reply::Double(10.0));
        db = 3;
        assert_eq!(run(&st, &mut db, "GET x"), Reply::bulk("y"));
        let _ = std::fs::remove_file(path);
    }
}
