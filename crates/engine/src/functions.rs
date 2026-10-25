//! SQLite user functions implementing PostgreSQL/MySQL built-ins and the engine's own helpers
//! (`tb_cast`, sequences, catalog lookups). Registered on every session connection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{Datelike, NaiveDateTime, Timelike};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::{Value, ValueRef};

use crate::Dialect;
use crate::catalog::{Catalog, enum_oid, table_oid};
use crate::error::{EngineError, state};
use crate::types::SqlType;
use crate::value::{self, Interval, TS_FORMAT};

/// Engine-wide sequence registry (sequences are non-transactional, like PostgreSQL).
#[derive(Default)]
pub struct Sequences {
    inner: Mutex<HashMap<String, SeqState>>,
    dirty: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone)]
pub struct SeqState {
    pub last_value: i64,
    pub is_called: bool,
    pub increment: i64,
    pub start_value: i64,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
}

impl Sequences {
    /// Register a sequence loaded from disk unless it is already tracked in memory.
    pub fn ensure(&self, key: &str, state: SeqState) {
        self.inner.lock().entry(key.to_owned()).or_insert(state);
    }

    pub fn replace(&self, key: &str, state: SeqState) {
        self.inner.lock().insert(key.to_owned(), state);
        self.dirty.lock().insert(key.to_owned());
    }

    pub fn remove(&self, key: &str) {
        self.inner.lock().remove(key);
        self.dirty.lock().remove(key);
    }

    pub fn get(&self, key: &str) -> Option<SeqState> {
        self.inner.lock().get(key).cloned()
    }

    pub fn nextval(&self, key: &str) -> Result<i64, EngineError> {
        let mut map = self.inner.lock();
        let seq = map.get_mut(key).ok_or_else(|| {
            EngineError::new(state::UNDEFINED_TABLE, format!("relation \"{}\" does not exist", key.split('.').next_back().unwrap_or(key)))
        })?;
        let next = if seq.is_called { seq.last_value + seq.increment } else { seq.start_value };
        if let Some(max) = seq.max_value
            && next > max
        {
            return Err(EngineError::new(
                "2200H",
                format!("nextval: reached maximum value of sequence \"{}\" ({max})", key.split('.').next_back().unwrap_or(key)),
            ));
        }
        seq.last_value = next;
        seq.is_called = true;
        drop(map);
        self.dirty.lock().insert(key.to_owned());
        Ok(next)
    }

    /// Move a sequence past `value` (MySQL AUTO_INCREMENT semantics after explicit inserts).
    pub fn bump_to(&self, key: &str, value: i64) {
        let mut map = self.inner.lock();
        if let Some(seq) = map.get_mut(key) {
            let current = if seq.is_called { seq.last_value } else { seq.start_value - seq.increment };
            if value > current {
                seq.last_value = value;
                seq.is_called = true;
                drop(map);
                self.dirty.lock().insert(key.to_owned());
            }
        }
    }

    pub fn setval(&self, key: &str, value: i64, is_called: bool) -> Result<i64, EngineError> {
        let mut map = self.inner.lock();
        let seq = map.get_mut(key).ok_or_else(|| EngineError::new(state::UNDEFINED_TABLE, format!("relation \"{key}\" does not exist")))?;
        seq.last_value = value;
        seq.is_called = is_called;
        drop(map);
        self.dirty.lock().insert(key.to_owned());
        Ok(value)
    }

    /// Take the set of sequences changed since the last flush.
    pub fn take_dirty(&self) -> Vec<(String, SeqState)> {
        let keys: Vec<String> = self.dirty.lock().drain().collect();
        let map = self.inner.lock();
        keys.into_iter().filter_map(|k| map.get(&k).map(|s| (k, s.clone()))).collect()
    }
}

/// Per-session state visible to functions.
pub struct SessionShared {
    pub settings: HashMap<String, String>,
    pub last_insert_id: i64,
    pub lastval: Option<i64>,
    pub currvals: HashMap<String, i64>,
    pub search_path: Vec<String>,
    pub database: String,
    pub user: String,
    pub connection_id: u32,
    pub found_rows: i64,
    pub row_count: i64,
}

pub struct FnEnv {
    pub dialect: Dialect,
    pub catalog: Arc<RwLock<Arc<Catalog>>>,
    pub sequences: Arc<Sequences>,
    pub shared: Arc<Mutex<SessionShared>>,
    pub version: String,
}

fn err(e: EngineError) -> rusqlite::Error {
    e.to_function_error()
}

fn text_arg(ctx: &Context<'_>, idx: usize) -> Option<String> {
    match ctx.get_raw(idx) {
        ValueRef::Null => None,
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(value::format_float(f, false, Dialect::Postgres)),
        ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
    }
}

fn int_arg(ctx: &Context<'_>, idx: usize) -> Option<i64> {
    match ctx.get_raw(idx) {
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn ts_arg(ctx: &Context<'_>, idx: usize) -> Option<NaiveDateTime> {
    text_arg(ctx, idx).and_then(|s| value::parse_any_timestamp(&s))
}

pub fn register(conn: &Connection, env: Arc<FnEnv>) -> rusqlite::Result<()> {
    let det = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let vol = FunctionFlags::SQLITE_UTF8;

    // --- tb_cast(value, 'type') -----------------------------------------------------------
    {
        let env = env.clone();
        let cache: Mutex<HashMap<String, SqlType>> = Mutex::new(HashMap::new());
        conn.create_scalar_function("tb_cast", 2, det, move |ctx| {
            let type_text = text_arg(ctx, 1).unwrap_or_default();
            let ty = {
                let mut c = cache.lock();
                if let Some(t) = c.get(&type_text) {
                    t.clone()
                } else {
                    let cat = env.catalog.read().clone();
                    let path = env.shared.lock().search_path.clone();
                    let parsed = SqlType::parse(&type_text, env.dialect, &|n| cat.enum_values(n, &path)).ty;
                    c.insert(type_text.clone(), parsed.clone());
                    parsed
                }
            };
            value::coerce(ctx.get_raw(0), &ty, env.dialect).map_err(err)
        })?;
    }

    // --- sequences ----------------------------------------------------------------------------
    {
        let env = env.clone();
        conn.create_scalar_function("tb_nextval", 1, vol, move |ctx| {
            let key = resolve_seq_key(&env, &text_arg(ctx, 0).unwrap_or_default());
            let v = env.sequences.nextval(&key).map_err(err)?;
            let mut sh = env.shared.lock();
            sh.lastval = Some(v);
            sh.last_insert_id = v;
            sh.currvals.insert(key, v);
            Ok(v)
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_currval", 1, vol, move |ctx| {
            let key = resolve_seq_key(&env, &text_arg(ctx, 0).unwrap_or_default());
            env.shared.lock().currvals.get(&key).copied().ok_or_else(|| {
                err(EngineError::new(
                    "55000",
                    format!("currval of sequence \"{}\" is not yet defined in this session", key.split('.').next_back().unwrap_or(&key)),
                ))
            })
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_setval", -1, vol, move |ctx| {
            let key = resolve_seq_key(&env, &text_arg(ctx, 0).unwrap_or_default());
            let v = int_arg(ctx, 1).unwrap_or(1);
            let called = if ctx.len() > 2 { int_arg(ctx, 2).unwrap_or(1) != 0 } else { true };
            env.sequences.setval(&key, v, called).map_err(err)
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_lastval", 0, vol, move |_| {
            env.shared.lock().lastval.ok_or_else(|| err(EngineError::new("55000", "lastval is not yet defined in this session")))
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_last_insert_id", -1, vol, move |ctx| {
            let mut sh = env.shared.lock();
            if ctx.len() == 1
                && let Some(v) = int_arg(ctx, 0)
            {
                sh.last_insert_id = v;
                return Ok(v);
            }
            Ok(sh.last_insert_id)
        })?;
    }

    // --- arithmetic -------------------------------------------------------------------------------
    {
        let dialect = env.dialect;
        conn.create_scalar_function("tb_nonzero", 1, det, move |ctx| {
            let zero = match ctx.get_raw(0) {
                ValueRef::Integer(0) => true,
                ValueRef::Real(f) => f == 0.0,
                ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|t| t.trim().parse::<f64>().ok()) == Some(0.0),
                _ => false,
            };
            if zero {
                let message = if dialect == Dialect::MsSql { "Divide by zero error encountered." } else { "division by zero" };
                return Err(err(EngineError::new(state::DIVISION_BY_ZERO, message)));
            }
            Ok(match ctx.get_raw(0) {
                ValueRef::Null => rusqlite::types::Value::Null,
                ValueRef::Integer(i) => rusqlite::types::Value::Integer(i),
                ValueRef::Real(f) => rusqlite::types::Value::Real(f),
                ValueRef::Text(t) => rusqlite::types::Value::Text(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => rusqlite::types::Value::Blob(b.to_vec()),
            })
        })?;
    }

    // --- time -----------------------------------------------------------------------------------
    conn.create_scalar_function("tb_now", 0, vol, |_| Ok(value::now_text()))?;
    conn.create_scalar_function("tb_current_date", 0, vol, |_| Ok(chrono::Utc::now().format("%Y-%m-%d").to_string()))?;
    conn.create_scalar_function("tb_current_time", 0, vol, |_| Ok(chrono::Utc::now().format("%H:%M:%S%.6f").to_string()))?;
    conn.create_scalar_function("tb_extract", 2, det, |ctx| {
        let field = text_arg(ctx, 0).unwrap_or_default();
        let raw = text_arg(ctx, 1);
        let Some(raw) = raw else { return Ok(None) };
        if let Some(iv) = Interval::parse(&raw).filter(|_| !raw.contains('-') || raw.contains(' '))
            && value::parse_any_timestamp(&raw).is_none()
        {
            let total_secs = iv.micros as f64 / 1e6;
            return Ok(Some(match field.to_ascii_lowercase().as_str() {
                "epoch" => (iv.months as f64 * 30.0 + iv.days as f64) * 86400.0 + total_secs,
                "day" | "days" => iv.days as f64,
                "hour" | "hours" => (iv.micros / 3_600_000_000) as f64,
                "minute" | "minutes" => ((iv.micros / 60_000_000) % 60) as f64,
                "second" | "seconds" => total_secs % 60.0,
                "month" | "months" => (iv.months % 12) as f64,
                "year" | "years" => (iv.months / 12) as f64,
                _ => 0.0,
            }));
        }
        let ts = value::parse_any_timestamp(&raw)
            .or_else(|| value::parse_time_only(&raw).map(|t| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_time(t)))
            .ok_or_else(|| {
                err(EngineError::new(state::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type timestamp: \"{raw}\"")))
            })?;
        value::extract_field(&field, ts)
            .map(Some)
            .ok_or_else(|| err(EngineError::new(state::INVALID_PARAMETER_VALUE, format!("unit \"{field}\" not recognized"))))
    })?;
    conn.create_scalar_function("tb_date_trunc", 2, det, |ctx| {
        let field = text_arg(ctx, 0).unwrap_or_default();
        let Some(ts) = ts_arg(ctx, 1) else { return Ok(None) };
        value::date_trunc(&field, ts)
            .map(|t| Some(t.format(TS_FORMAT).to_string()))
            .ok_or_else(|| err(EngineError::new(state::INVALID_PARAMETER_VALUE, format!("unit \"{field}\" not recognized"))))
    })?;
    // tb_dt_add(timestamp, interval, sign): timestamp ± interval.
    conn.create_scalar_function("tb_dt_add", 3, det, |ctx| {
        let (Some(ts_text), Some(iv_text)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        let sign = int_arg(ctx, 2).unwrap_or(1);
        // A bare integer means days (date + integer).
        let iv = iv_text
            .trim()
            .parse::<i64>()
            .ok()
            .map(|d| Interval { months: 0, days: d as i32, micros: 0 })
            .or_else(|| Interval::parse(&iv_text))
            .ok_or_else(|| {
                err(EngineError::new(state::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type interval: \"{iv_text}\"")))
            })?;
        let iv = if sign < 0 { iv.negate() } else { iv };
        let date_only = ts_text.trim().len() == 10;
        let ts = value::parse_any_timestamp(&ts_text).ok_or_else(|| {
            err(EngineError::new(state::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type timestamp: \"{ts_text}\"")))
        })?;
        let out = iv.add_to(ts).ok_or_else(|| err(EngineError::new(state::DATETIME_FIELD_OVERFLOW, "timestamp out of range")))?;
        Ok(Some(if date_only && iv.micros == 0 { out.format("%Y-%m-%d").to_string() } else { out.format(TS_FORMAT).to_string() }))
    })?;
    conn.create_scalar_function("tb_dt_diff", 2, det, |ctx| {
        let (Some(a), Some(b)) = (ts_arg(ctx, 0), ts_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some(Interval::between(a, b).to_pg_string()))
    })?;
    conn.create_scalar_function("tb_date_diff_days", 2, det, |ctx| {
        let (Some(a), Some(b)) = (ts_arg(ctx, 0), ts_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some((a.date() - b.date()).num_days()))
    })?;
    conn.create_scalar_function("tb_age", -1, vol, |ctx| {
        let a = ts_arg(ctx, 0);
        let b = if ctx.len() > 1 { ts_arg(ctx, 1) } else { a };
        let (a, b) = if ctx.len() > 1 { (a, b) } else { (Some(chrono::Utc::now().naive_utc().date().and_hms_opt(0, 0, 0).unwrap()), a) };
        let (Some(a), Some(b)) = (a, b) else { return Ok(None) };
        let months = (a.year() - b.year()) * 12 + a.month() as i32 - b.month() as i32;
        let mut iv = Interval { months, days: a.day() as i32 - b.day() as i32, micros: 0 };
        let ta = a.num_seconds_from_midnight() as i64 * 1_000_000;
        let tb = b.num_seconds_from_midnight() as i64 * 1_000_000;
        iv.micros = ta - tb;
        if iv.micros < 0 {
            iv.micros += 86_400_000_000;
            iv.days -= 1;
        }
        if iv.days < 0 {
            iv.months -= 1;
            iv.days += 30;
        }
        Ok(Some(iv.to_pg_string()))
    })?;
    conn.create_scalar_function("tb_to_timestamp", 1, det, |ctx| match ctx.get_raw(0) {
        ValueRef::Null => Ok(None),
        _ => {
            let secs = match ctx.get_raw(0) {
                ValueRef::Integer(i) => i as f64,
                ValueRef::Real(f) => f,
                _ => text_arg(ctx, 0).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            };
            let dt = chrono::DateTime::from_timestamp_micros((secs * 1e6) as i64).unwrap_or_default();
            Ok(Some(dt.naive_utc().format(TS_FORMAT).to_string()))
        }
    })?;
    conn.create_scalar_function("tb_unix_timestamp", -1, vol, |ctx| {
        if ctx.is_empty() {
            return Ok(Some(chrono::Utc::now().timestamp()));
        }
        Ok(ts_arg(ctx, 0).map(|t| t.and_utc().timestamp()))
    })?;
    conn.create_scalar_function("tb_to_char", 2, det, |ctx| {
        let (Some(raw), Some(fmt)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        match value::parse_any_timestamp(&raw) {
            Some(ts) => Ok(Some(pg_to_char(ts, &fmt))),
            None => Ok(Some(raw)),
        }
    })?;
    conn.create_scalar_function("tb_date_format", 2, det, |ctx| {
        let (Some(ts), Some(fmt)) = (ts_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some(mysql_date_format(ts, &fmt)))
    })?;

    // --- identity / environment -----------------------------------------------------------------
    {
        let v = env.version.clone();
        conn.create_scalar_function("tb_version", 0, det, move |_| Ok(v.clone()))?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_current_schema", 0, vol, move |_| {
            let sh = env.shared.lock();
            Ok(if env.dialect == Dialect::MySql {
                Some(sh.database.clone()).filter(|d| !d.is_empty())
            } else {
                sh.search_path.first().cloned()
            })
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_current_database", 0, vol, move |_| {
            let sh = env.shared.lock();
            Ok(Some(sh.database.clone()).filter(|d| !d.is_empty()))
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_current_user", 0, vol, move |_| {
            let sh = env.shared.lock();
            Ok(if env.dialect == Dialect::MySql { format!("{}@localhost", sh.user) } else { sh.user.clone() })
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_connection_id", 0, vol, move |_| Ok(env.shared.lock().connection_id as i64))?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_found_rows", 0, vol, move |_| Ok(env.shared.lock().found_rows))?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_row_count", 0, vol, move |_| Ok(env.shared.lock().row_count))?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_setting", -1, vol, move |ctx| {
            let name = text_arg(ctx, 0).unwrap_or_default().to_ascii_lowercase();
            let missing_ok = ctx.len() > 1 && int_arg(ctx, 1).unwrap_or(0) != 0;
            match env.shared.lock().settings.get(&name) {
                Some(v) => Ok(Some(v.clone())),
                None if missing_ok => Ok(None),
                None => Err(err(EngineError::new(state::UNDEFINED_PARAMETER, format!("unrecognized configuration parameter \"{name}\"")))),
            }
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_set_config", 3, vol, move |ctx| {
            let name = text_arg(ctx, 0).unwrap_or_default().to_ascii_lowercase();
            let value = text_arg(ctx, 1).unwrap_or_default();
            env.shared.lock().settings.insert(name, value.clone());
            Ok(value)
        })?;
    }

    // --- catalog helpers ------------------------------------------------------------------------
    {
        let env = env.clone();
        conn.create_scalar_function("tb_regclass", 1, vol, move |ctx| {
            let raw = match ctx.get_raw(0) {
                ValueRef::Integer(i) => return Ok(Some(i)),
                ValueRef::Null => return Ok(None),
                _ => text_arg(ctx, 0).unwrap_or_default(),
            };
            // '16384'::regclass is an OID literal.
            if let Ok(oid) = raw.trim().parse::<i64>() {
                return Ok(Some(oid));
            }
            let cat = env.catalog.read().clone();
            let path = env.shared.lock().search_path.clone();
            let (schema, name) = split_qualified(&raw, env.dialect == Dialect::Postgres);
            let found = match schema {
                Some(s) => (cat.table(&s, &name).is_some() || cat.view(&s, &name).is_some()).then_some(s),
                None => cat.resolve_relation(&name, &path).map(|(s, _)| s),
            };
            match found {
                Some(s) => Ok(Some(table_oid(&s, &name) as i64)),
                None => Err(err(EngineError::new(state::UNDEFINED_TABLE, format!("relation \"{raw}\" does not exist")))),
            }
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_regtype", 1, vol, move |ctx| {
            let raw = text_arg(ctx, 0).unwrap_or_default();
            let cat = env.catalog.read().clone();
            let path = env.shared.lock().search_path.clone();
            if let Some(schema) = path.iter().find(|s| cat.schema(s).is_some_and(|sd| sd.enums.contains_key(&raw))) {
                return Ok(enum_oid(schema, &raw) as i64);
            }
            Ok(SqlType::parse_simple(&raw, Dialect::Postgres).pg_oid() as i64)
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_format_type", 2, vol, move |ctx| {
            let Some(oid) = int_arg(ctx, 0) else { return Ok(None) };
            let typmod = int_arg(ctx, 1).unwrap_or(-1);
            Ok(Some(format_type_oid(&env.catalog.read(), oid as u32, typmod)))
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_obj_description", -1, vol, move |ctx| {
            let Some(oid) = int_arg(ctx, 0) else { return Ok(None) };
            let cat = env.catalog.read().clone();
            for sd in cat.schemas.values() {
                for (tn, td) in &sd.tables {
                    if table_oid(&sd.name, tn) as i64 == oid {
                        return Ok(td.comment.clone());
                    }
                }
            }
            Ok(None)
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_col_description", 2, vol, move |ctx| {
            let (Some(oid), Some(attnum)) = (int_arg(ctx, 0), int_arg(ctx, 1)) else { return Ok(None) };
            let cat = env.catalog.read().clone();
            for sd in cat.schemas.values() {
                for (tn, td) in &sd.tables {
                    if table_oid(&sd.name, tn) as i64 == oid {
                        return Ok(td.columns.get((attnum - 1).max(0) as usize).and_then(|c| c.comment.clone()));
                    }
                }
            }
            Ok(None)
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_get_constraintdef", -1, vol, move |ctx| {
            let Some(oid) = int_arg(ctx, 0) else { return Ok(None) };
            let cat = env.catalog.read().clone();
            Ok(constraint_def(&cat, oid as u32))
        })?;
    }
    {
        let env = env.clone();
        conn.create_scalar_function("tb_get_indexdef", -1, vol, move |ctx| {
            let Some(oid) = int_arg(ctx, 0) else { return Ok(None) };
            let cat = env.catalog.read().clone();
            for sd in cat.schemas.values() {
                for (tn, td) in &sd.tables {
                    let keys = td
                        .primary_key
                        .iter()
                        .map(|k| (&k.name, &k.columns, true))
                        .chain(td.uniques.iter().map(|k| (&k.name, &k.columns, true)))
                        .chain(td.indexes.iter().map(|k| (&k.name, &k.columns, k.unique)));
                    for (name, cols, unique) in keys {
                        if crate::catalog::oid_for(&["idx", &sd.name, tn, name]) as i64 == oid {
                            return Ok(Some(format!(
                                "CREATE {}INDEX {} ON {}.{} USING btree ({})",
                                if unique { "UNIQUE " } else { "" },
                                name,
                                sd.name,
                                tn,
                                cols.join(", ")
                            )));
                        }
                    }
                }
            }
            Ok(None)
        })?;
    }
    conn.create_scalar_function("tb_pg_get_expr", -1, det, |ctx| Ok(text_arg(ctx, 0)))?;
    conn.create_scalar_function("tb_true", -1, det, |_| Ok(1i64))?;
    conn.create_scalar_function("tb_void", -1, vol, |_| Ok(""))?;
    conn.create_scalar_function("tb_null", -1, det, |_| Ok(None::<i64>))?;
    conn.create_scalar_function("tb_pg_sleep", 1, vol, |ctx| {
        let secs = match ctx.get_raw(0) {
            ValueRef::Integer(i) => i as f64,
            ValueRef::Real(f) => f,
            _ => 0.0,
        };
        std::thread::sleep(std::time::Duration::from_secs_f64(secs.clamp(0.0, 30.0)));
        Ok("")
    })?;

    // --- strings ---------------------------------------------------------------------------------
    conn.create_scalar_function("tb_uuid", 0, vol, |_| Ok(uuid::Uuid::new_v4().hyphenated().to_string()))?;
    conn.create_scalar_function("tb_random", 0, vol, |_| Ok(rand::random::<f64>()))?;
    conn.create_scalar_function("tb_md5", 1, det, |ctx| Ok(text_arg(ctx, 0).map(|s| format!("{:x}", md5::compute(s.as_bytes())))))?;
    conn.create_scalar_function("tb_left", 2, det, |ctx| {
        let (Some(s), Some(n)) = (text_arg(ctx, 0), int_arg(ctx, 1)) else { return Ok(None) };
        let len = s.chars().count() as i64;
        let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
        Ok(Some(s.chars().take(take as usize).collect::<String>()))
    })?;
    conn.create_scalar_function("tb_right", 2, det, |ctx| {
        let (Some(s), Some(n)) = (text_arg(ctx, 0), int_arg(ctx, 1)) else { return Ok(None) };
        let len = s.chars().count() as i64;
        let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
        Ok(Some(s.chars().skip((len - take) as usize).collect::<String>()))
    })?;
    conn.create_scalar_function("tb_split_part", 3, det, |ctx| {
        let (Some(s), Some(d), Some(n)) = (text_arg(ctx, 0), text_arg(ctx, 1), int_arg(ctx, 2)) else { return Ok(None) };
        let parts: Vec<&str> = if d.is_empty() { vec![s.as_str()] } else { s.split(d.as_str()).collect() };
        let idx = if n > 0 { n - 1 } else { parts.len() as i64 + n };
        Ok(Some(parts.get(idx.max(0) as usize).filter(|_| idx >= 0).map(|p| p.to_string()).unwrap_or_default()))
    })?;
    conn.create_scalar_function("tb_lpad", -1, det, |ctx| pad(ctx, true))?;
    conn.create_scalar_function("tb_rpad", -1, det, |ctx| pad(ctx, false))?;
    conn.create_scalar_function("tb_repeat", 2, det, |ctx| {
        let (Some(s), Some(n)) = (text_arg(ctx, 0), int_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some(s.repeat(n.clamp(0, 1_000_000) as usize)))
    })?;
    conn.create_scalar_function("tb_reverse", 1, det, |ctx| Ok(text_arg(ctx, 0).map(|s| s.chars().rev().collect::<String>())))?;
    conn.create_scalar_function("tb_initcap", 1, det, |ctx| {
        Ok(text_arg(ctx, 0).map(|s| {
            let mut out = String::with_capacity(s.len());
            let mut start = true;
            for c in s.chars() {
                if c.is_alphanumeric() {
                    if start {
                        out.extend(c.to_uppercase())
                    } else {
                        out.extend(c.to_lowercase())
                    }
                    start = false;
                } else {
                    out.push(c);
                    start = true;
                }
            }
            out
        }))
    })?;
    conn.create_scalar_function("tb_greatest", -1, det, |ctx| extreme(ctx, true))?;
    conn.create_scalar_function("tb_least", -1, det, |ctx| extreme(ctx, false))?;
    conn.create_scalar_function("tb_ilike", 2, det, |ctx| {
        let (Some(s), Some(p)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some(like_match(&s.to_lowercase(), &p.to_lowercase()) as i64))
    })?;
    conn.create_scalar_function("tb_like", 2, det, |ctx| {
        let (Some(s), Some(p)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        Ok(Some(like_match(&s, &p) as i64))
    })?;

    // --- regular expressions ---------------------------------------------------------------------
    {
        let cache: Mutex<HashMap<String, regex::Regex>> = Mutex::new(HashMap::new());
        // SQLite's `X REGEXP Y` calls regexp(Y, X).
        conn.create_scalar_function("regexp", 2, det, move |ctx| {
            let (Some(pattern), Some(text)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
            let re = compile(&cache, &pattern)?;
            Ok(Some(re.is_match(&text) as i64))
        })?;
    }
    {
        let cache: Mutex<HashMap<String, regex::Regex>> = Mutex::new(HashMap::new());
        conn.create_scalar_function("tb_regexp_ci", 2, det, move |ctx| {
            let (Some(pattern), Some(text)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
            let re = compile(&cache, &format!("(?i){pattern}"))?;
            Ok(Some(re.is_match(&text) as i64))
        })?;
    }
    {
        let cache: Mutex<HashMap<String, regex::Regex>> = Mutex::new(HashMap::new());
        conn.create_scalar_function("tb_regexp_replace", -1, det, move |ctx| {
            let (Some(src), Some(pattern), Some(rep)) = (text_arg(ctx, 0), text_arg(ctx, 1), text_arg(ctx, 2)) else { return Ok(None) };
            let flags = if ctx.len() > 3 { text_arg(ctx, 3).unwrap_or_default() } else { String::new() };
            let pat = if flags.contains('i') { format!("(?i){pattern}") } else { pattern };
            let re = compile(&cache, &pat)?;
            // PostgreSQL uses \1 for groups; the regex crate uses ${1}.
            let rep = regex::Regex::new(r"\\(\d)").unwrap().replace_all(&rep, "$${$1}").into_owned();
            Ok(Some(if flags.contains('g') {
                re.replace_all(&src, rep.as_str()).into_owned()
            } else {
                re.replace(&src, rep.as_str()).into_owned()
            }))
        })?;
    }

    // --- arrays and JSON ---------------------------------------------------------------------------
    conn.create_scalar_function("tb_array", -1, det, |ctx| {
        let mut items = Vec::with_capacity(ctx.len());
        for idx in 0..ctx.len() {
            items.push(json_of(ctx.get_raw(idx)));
        }
        Ok(serde_json::Value::Array(items).to_string())
    })?;
    conn.create_scalar_function("tb_array_contains", 2, det, |ctx| {
        // tb_array_contains(array_json, value): value = ANY(array)
        let Some(arr) = text_arg(ctx, 0).and_then(|s| array_items(&s)) else { return Ok(None) };
        let needle = json_of(ctx.get_raw(1));
        if needle.is_null() {
            return Ok(None);
        }
        Ok(Some(arr.iter().any(|v| json_loose_eq(v, &needle)) as i64))
    })?;
    conn.create_scalar_function("tb_array_length", -1, det, |ctx| {
        Ok(text_arg(ctx, 0).and_then(|s| array_items(&s)).and_then(|a| if a.is_empty() { None } else { Some(a.len() as i64) }))
    })?;
    conn.create_scalar_function("tb_cardinality", 1, det, |ctx| {
        Ok(text_arg(ctx, 0).and_then(|s| array_items(&s)).map(|a| a.len() as i64))
    })?;
    conn.create_scalar_function("tb_array_append", 2, det, |ctx| {
        let mut arr = text_arg(ctx, 0).and_then(|s| array_items(&s)).unwrap_or_default();
        arr.push(json_of(ctx.get_raw(1)));
        Ok(serde_json::Value::Array(arr).to_string())
    })?;
    conn.create_scalar_function("tb_array_cat", 2, det, |ctx| {
        let mut a = text_arg(ctx, 0).and_then(|s| array_items(&s)).unwrap_or_default();
        a.extend(text_arg(ctx, 1).and_then(|s| array_items(&s)).unwrap_or_default());
        Ok(serde_json::Value::Array(a).to_string())
    })?;
    conn.create_scalar_function("tb_array_to_string", -1, det, |ctx| {
        let Some(arr) = text_arg(ctx, 0).and_then(|s| array_items(&s)) else { return Ok(None) };
        let sep = text_arg(ctx, 1).unwrap_or_default();
        Ok(Some(
            arr.iter()
                .filter(|v| !v.is_null())
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(&sep),
        ))
    })?;
    conn.create_scalar_function("tb_json_contains", 2, det, |ctx| {
        let (Some(a), Some(b)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        let a: serde_json::Value = serde_json::from_str(&a).unwrap_or_default();
        let b: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
        Ok(Some(json_contains(&a, &b) as i64))
    })?;
    conn.create_scalar_function("tb_json_has_key", 2, det, |ctx| {
        let (Some(a), Some(k)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        let a: serde_json::Value = serde_json::from_str(&a).unwrap_or_default();
        Ok(Some(match a {
            serde_json::Value::Object(m) => m.contains_key(&k),
            serde_json::Value::Array(items) => items.iter().any(|v| v.as_str() == Some(k.as_str())),
            serde_json::Value::String(s) => s == k,
            _ => false,
        } as i64))
    })?;
    conn.create_scalar_function("tb_jsonb_norm", 1, det, |ctx| {
        Ok(text_arg(ctx, 0).map(|s| match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => value::normalize_json(&v),
            Err(_) => s,
        }))
    })?;
    conn.create_scalar_function("tb_json_path", 2, det, |ctx| {
        // #> operator: path given as PostgreSQL text array '{a,b}'.
        let (Some(doc), Some(path)) = (text_arg(ctx, 0), text_arg(ctx, 1)) else { return Ok(None) };
        let mut v: serde_json::Value = serde_json::from_str(&doc).unwrap_or_default();
        for key in value::parse_array_input(&path).unwrap_or_default().into_iter().flatten() {
            v = match v {
                serde_json::Value::Object(mut m) => m.remove(&key).unwrap_or(serde_json::Value::Null),
                serde_json::Value::Array(mut a) => {
                    key.parse::<usize>().ok().filter(|i| *i < a.len()).map(|i| a.swap_remove(i)).unwrap_or_default()
                }
                _ => serde_json::Value::Null,
            };
        }
        Ok(if v.is_null() { None } else { Some(v.to_string()) })
    })?;

    Ok(())
}

fn compile(cache: &Mutex<HashMap<String, regex::Regex>>, pattern: &str) -> rusqlite::Result<regex::Regex> {
    let mut c = cache.lock();
    if let Some(r) = c.get(pattern) {
        return Ok(r.clone());
    }
    let re = regex::Regex::new(pattern).map_err(|e| err(EngineError::new("2201B", format!("invalid regular expression: {e}"))))?;
    c.insert(pattern.to_owned(), re.clone());
    Ok(re)
}

fn pad(ctx: &Context<'_>, left: bool) -> rusqlite::Result<Option<String>> {
    let (Some(s), Some(n)) = (text_arg(ctx, 0), int_arg(ctx, 1)) else { return Ok(None) };
    let fill = if ctx.len() > 2 { text_arg(ctx, 2).unwrap_or_else(|| " ".into()) } else { " ".into() };
    let n = n.max(0) as usize;
    let len = s.chars().count();
    if len >= n {
        return Ok(Some(s.chars().take(n).collect()));
    }
    if fill.is_empty() {
        return Ok(Some(s));
    }
    let padding: String = fill.chars().cycle().take(n - len).collect();
    Ok(Some(if left { format!("{padding}{s}") } else { format!("{s}{padding}") }))
}

fn extreme(ctx: &Context<'_>, greatest: bool) -> rusqlite::Result<Value> {
    let mut best: Option<Value> = None;
    for idx in 0..ctx.len() {
        let v: Value = crate::session::owned(ctx.get_raw(idx));
        if matches!(v, Value::Null) {
            continue;
        }
        best = Some(match best {
            None => v,
            Some(b) => {
                let ord = compare_values(&v, &b);
                if (greatest && ord.is_gt()) || (!greatest && ord.is_lt()) { v } else { b }
            }
        });
    }
    Ok(best.unwrap_or(Value::Null))
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let num = |v: &Value| match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Real(f) => Some(*f),
        _ => None,
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => {
            let s = |v: &Value| match v {
                Value::Text(t) => t.clone(),
                Value::Integer(i) => i.to_string(),
                Value::Real(f) => f.to_string(),
                _ => String::new(),
            };
            s(a).cmp(&s(b))
        }
    }
}

/// Case-insensitive LIKE (MySQL `SHOW ... LIKE`).
pub(crate) fn like_match_ci(s: &str, p: &str) -> bool {
    like_match(&s.to_lowercase(), &p.to_lowercase())
}

/// SQL LIKE matching with `%`, `_` and backslash escapes.
fn like_match(s: &str, p: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = p.chars().collect();
    fn rec(s: &[char], p: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('%') => (0..=s.len()).any(|i| rec(&s[i..], &p[1..])),
            Some('_') => !s.is_empty() && rec(&s[1..], &p[1..]),
            Some('\\') if p.len() > 1 => !s.is_empty() && s[0] == p[1] && rec(&s[1..], &p[2..]),
            Some(c) => !s.is_empty() && s[0] == *c && rec(&s[1..], &p[1..]),
        }
    }
    rec(&s, &p)
}

fn json_of(v: ValueRef<'_>) -> serde_json::Value {
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or_default(),
        ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => serde_json::Value::String(format!("\\x{}", value::hex(b))),
    }
}

fn array_items(s: &str) -> Option<Vec<serde_json::Value>> {
    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(s) {
        return Some(items);
    }
    value::parse_array_input(s)
        .map(|items| items.into_iter().map(|i| i.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null)).collect())
}

fn json_loose_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if a == b {
        return true;
    }
    let s = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    s(a) == s(b)
}

fn json_contains(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Object(am), serde_json::Value::Object(bm)) => {
            bm.iter().all(|(k, bv)| am.get(k).is_some_and(|av| json_contains(av, bv)))
        }
        (serde_json::Value::Array(aa), serde_json::Value::Array(ba)) => ba.iter().all(|bv| aa.iter().any(|av| json_contains(av, bv))),
        (serde_json::Value::Array(aa), other) => aa.iter().any(|av| av == other),
        (x, y) => x == y,
    }
}

/// `schema.name` from a text argument; unquoted parts fold to lower case in PostgreSQL only.
fn split_qualified(raw: &str, fold: bool) -> (Option<String>, String) {
    let clean = |s: &str| {
        let s = s.trim();
        if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
            s[1..s.len() - 1].replace("\"\"", "\"")
        } else if fold {
            s.to_ascii_lowercase()
        } else {
            s.to_owned()
        }
    };
    match raw.rsplit_once('.') {
        Some((schema, name)) => (Some(clean(schema)), clean(name)),
        None => (None, clean(raw)),
    }
}

fn resolve_seq_key(env: &FnEnv, raw: &str) -> String {
    let (schema, name) = split_qualified(raw, env.dialect == Dialect::Postgres);
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => {
            let cat = env.catalog.read().clone();
            let path = env.shared.lock().search_path.clone();
            for s in &path {
                if cat.schema(s).is_some_and(|sd| sd.sequences.contains_key(&name)) || env.sequences.get(&format!("{s}.{name}")).is_some() {
                    return format!("{s}.{name}");
                }
            }
            format!("{}.{name}", path.first().cloned().unwrap_or_else(|| "public".into()))
        }
    }
}

fn format_type_oid(cat: &Catalog, oid: u32, typmod: i64) -> String {
    for sd in cat.schemas.values() {
        for name in sd.enums.keys() {
            if enum_oid(&sd.name, name) == oid {
                return name.clone();
            }
        }
    }
    let base = match oid {
        16 => "boolean",
        17 => "bytea",
        18 => "\"char\"",
        19 => "name",
        20 => "bigint",
        21 => "smallint",
        23 => "integer",
        25 => "text",
        26 => "oid",
        114 => "json",
        700 => "real",
        701 => "double precision",
        1042 => "character",
        1043 => "character varying",
        1082 => "date",
        1083 => "time without time zone",
        1114 => "timestamp without time zone",
        1184 => "timestamp with time zone",
        1186 => "interval",
        1700 => "numeric",
        2950 => "uuid",
        3802 => "jsonb",
        1000 => "boolean[]",
        1005 => "smallint[]",
        1007 => "integer[]",
        1009 => "text[]",
        1015 => "character varying[]",
        1016 => "bigint[]",
        1022 => "double precision[]",
        2951 => "uuid[]",
        3807 => "jsonb[]",
        _ => "unknown",
    };
    match oid {
        1042 | 1043 if typmod > 4 => format!("{base}({})", typmod - 4),
        1700 if typmod > 4 => {
            let m = typmod - 4;
            format!("numeric({},{})", m >> 16, m & 0xffff)
        }
        _ => base.to_owned(),
    }
}

fn constraint_def(cat: &Catalog, oid: u32) -> Option<String> {
    use crate::catalog::oid_for;
    for sd in cat.schemas.values() {
        for (tn, td) in &sd.tables {
            if let Some(pk) = &td.primary_key
                && oid_for(&["con", &sd.name, tn, &pk.name]) == oid
            {
                return Some(format!("PRIMARY KEY ({})", pk.columns.join(", ")));
            }
            for u in &td.uniques {
                if oid_for(&["con", &sd.name, tn, &u.name]) == oid {
                    return Some(format!("UNIQUE ({})", u.columns.join(", ")));
                }
            }
            for fk in &td.foreign_keys {
                if oid_for(&["con", &sd.name, tn, &fk.name]) == oid {
                    let mut s =
                        format!("FOREIGN KEY ({}) REFERENCES {}({})", fk.columns.join(", "), fk.ref_table, fk.ref_columns.join(", "));
                    if let Some(a) = &fk.on_update {
                        s.push_str(&format!(" ON UPDATE {a}"));
                    }
                    if let Some(a) = &fk.on_delete {
                        s.push_str(&format!(" ON DELETE {a}"));
                    }
                    return Some(s);
                }
            }
            for c in &td.checks {
                if oid_for(&["con", &sd.name, tn, &c.name]) == oid {
                    return Some(format!("CHECK ({})", c.display));
                }
            }
        }
    }
    None
}

/// Subset of PostgreSQL `to_char` patterns.
fn pg_to_char(ts: NaiveDateTime, fmt: &str) -> String {
    const MONTHS: [&str; 12] =
        ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let mut out = String::new();
    let mut rest = fmt;
    let tokens: &[(&str, &dyn Fn(NaiveDateTime) -> String)] = &[
        ("YYYY", &|t| format!("{:04}", t.year())),
        ("HH24", &|t| format!("{:02}", t.hour())),
        ("HH12", &|t| format!("{:02}", t.hour12().1)),
        ("Month", &|t| format!("{:<9}", MONTHS[t.month0() as usize])),
        ("Mon", &|t| MONTHS[t.month0() as usize][..3].to_owned()),
        ("Day", &|t| format!("{:<9}", DAYS[t.weekday().num_days_from_sunday() as usize])),
        ("Dy", &|t| DAYS[t.weekday().num_days_from_sunday() as usize][..3].to_owned()),
        ("YY", &|t| format!("{:02}", t.year() % 100)),
        ("MM", &|t| format!("{:02}", t.month())),
        ("DD", &|t| format!("{:02}", t.day())),
        ("HH", &|t| format!("{:02}", t.hour12().1)),
        ("MI", &|t| format!("{:02}", t.minute())),
        ("SS", &|t| format!("{:02}", t.second())),
        ("MS", &|t| format!("{:03}", t.nanosecond() / 1_000_000)),
        ("US", &|t| format!("{:06}", t.nanosecond() / 1000)),
        ("AM", &|t| if t.hour() < 12 { "AM".into() } else { "PM".into() }),
        ("PM", &|t| if t.hour() < 12 { "AM".into() } else { "PM".into() }),
        ("Q", &|t| ((t.month() - 1) / 3 + 1).to_string()),
        ("D", &|t| (t.weekday().num_days_from_sunday() + 1).to_string()),
    ];
    'outer: while !rest.is_empty() {
        for (tok, f) in tokens {
            if rest.starts_with(tok) {
                out.push_str(&f(ts));
                rest = &rest[tok.len()..];
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// MySQL `DATE_FORMAT` specifiers.
fn mysql_date_format(ts: NaiveDateTime, fmt: &str) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars.next().unwrap_or('%');
        let piece = match spec {
            'Y' => ts.format("%Y").to_string(),
            'y' => ts.format("%y").to_string(),
            'm' => ts.format("%m").to_string(),
            'c' => ts.month().to_string(),
            'd' => ts.format("%d").to_string(),
            'e' => ts.day().to_string(),
            'H' => ts.format("%H").to_string(),
            'h' | 'I' => ts.format("%I").to_string(),
            'i' => ts.format("%M").to_string(),
            's' | 'S' => ts.format("%S").to_string(),
            'f' => format!("{:06}", ts.nanosecond() / 1000),
            'p' => ts.format("%p").to_string(),
            'M' => ts.format("%B").to_string(),
            'b' => ts.format("%b").to_string(),
            'W' => ts.format("%A").to_string(),
            'a' => ts.format("%a").to_string(),
            'j' => ts.format("%j").to_string(),
            'T' => ts.format("%H:%M:%S").to_string(),
            'r' => ts.format("%I:%M:%S %p").to_string(),
            other => other.to_string(),
        };
        out.push_str(&piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_matching() {
        assert!(like_match("hello", "h%o"));
        assert!(like_match("hello", "_ello"));
        assert!(!like_match("Hello", "h%"));
        assert!(like_match("50%", "50\\%"));
    }

    #[test]
    fn to_char_formats() {
        let ts = NaiveDateTime::parse_from_str("2024-03-05 14:07:09", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(pg_to_char(ts, "YYYY-MM-DD HH24:MI:SS"), "2024-03-05 14:07:09");
        assert_eq!(mysql_date_format(ts, "%Y/%m/%d %H:%i"), "2024/03/05 14:07");
    }

    #[test]
    fn json_containment() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":{"c":[1,2]}}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":{"c":[2]}}"#).unwrap();
        assert!(json_contains(&a, &b));
    }
}
