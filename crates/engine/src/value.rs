//! Value coercion (what `tb_cast` does on every write and comparison) and text formatting
//! (what each protocol sends back), following PostgreSQL or MySQL rules.

use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use rusqlite::types::{Value, ValueRef};

use crate::Dialect;
use crate::error::{EngineError, state};
use crate::types::SqlType;

/// Canonical storage format for timestamps: fixed width so text ordering matches time ordering.
pub const TS_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.6f";

/// Coerce a value for storage in (or comparison with) a column of type `ty`.
pub fn coerce(v: ValueRef<'_>, ty: &SqlType, dialect: Dialect) -> Result<Value, EngineError> {
    if let ValueRef::Null = v {
        return Ok(Value::Null);
    }
    let my = dialect == Dialect::MySql;
    match ty {
        SqlType::Bool if my => coerce_int(v, -128, 127, ty, dialect),
        SqlType::Bool => coerce_bool(v),
        SqlType::Int1 => coerce_int(v, -128, 127, ty, dialect),
        SqlType::Int2 => coerce_int(v, i16::MIN as i64, i16::MAX as i64, ty, dialect),
        SqlType::Int4 => coerce_int(v, i32::MIN as i64, i32::MAX as i64, ty, dialect),
        SqlType::Int8 => coerce_int(v, i64::MIN, i64::MAX, ty, dialect),
        SqlType::Year => {
            let n = match coerce_int(v, i64::MIN, i64::MAX, ty, dialect)? {
                Value::Integer(n) => n,
                _ => 0,
            };
            let year = match n {
                0 => 0,
                1..=69 => 2000 + n,
                70..=99 => 1900 + n,
                1901..=2155 => n,
                _ => return Err(out_of_range(ty, dialect)),
            };
            Ok(Value::Integer(year))
        }
        SqlType::Float4 | SqlType::Float8 => coerce_float(v, ty, dialect).map(Value::Real),
        SqlType::Numeric { precision, scale } => {
            let mut f = coerce_float(v, ty, dialect)?;
            if let Some(s) = scale {
                let factor = 10f64.powi(*s as i32);
                f = (f * factor).round() / factor;
            }
            if let (Some(p), Some(s)) = (precision, scale) {
                let limit = 10f64.powi((*p as i32) - (*s as i32));
                if f.abs() >= limit {
                    return Err(if my {
                        EngineError::new(state::NUMERIC_VALUE_OUT_OF_RANGE, "Out of range value for column").with_mysql(1264)
                    } else {
                        EngineError::new(state::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow").with_detail(format!(
                            "A field with precision {p}, scale {s} must round to an absolute value less than 10^{}.",
                            p - s
                        ))
                    });
                }
            }
            Ok(Value::Real(f))
        }
        SqlType::Text | SqlType::Other { .. } => Ok(Value::Text(as_text(v))),
        SqlType::Varchar { len } => {
            let s = as_text(v);
            check_len(&s, *len, ty, dialect)?;
            Ok(Value::Text(s))
        }
        SqlType::Char { len } => {
            let mut s = as_text(v);
            if my {
                s.truncate(s.trim_end_matches(' ').len());
            }
            check_len(s.trim_end_matches(' '), *len, ty, dialect)?;
            if !my {
                let n = len.unwrap_or(1) as usize;
                let count = s.chars().count();
                if count < n {
                    s.extend(std::iter::repeat_n(' ', n - count));
                } else if count > n {
                    s = s.chars().take(n).collect();
                }
            }
            Ok(Value::Text(s))
        }
        SqlType::Bytea => Ok(Value::Blob(match v {
            ValueRef::Blob(b) => b.to_vec(),
            ValueRef::Text(t) if !my => decode_pg_bytea(t)?,
            other => as_text(other).into_bytes(),
        })),
        SqlType::Date => {
            let text = temporal_input(v);
            let (date, _, _) = parse_datetime(&text).ok_or_else(|| bad_datetime("date", &text, dialect))?;
            Ok(Value::Text(date.format("%Y-%m-%d").to_string()))
        }
        SqlType::Time => {
            let text = temporal_input(v);
            let time = parse_time_only(&text)
                .or_else(|| parse_datetime(&text).and_then(|(_, t, _)| t))
                .ok_or_else(|| bad_datetime("time without time zone", &text, dialect))?;
            Ok(Value::Text(format_time(time)))
        }
        SqlType::Timestamp => {
            let text = temporal_input(v);
            let (date, time, _) = parse_datetime(&text).ok_or_else(|| bad_datetime("timestamp without time zone", &text, dialect))?;
            let ts = date.and_time(time.unwrap_or(NaiveTime::MIN));
            Ok(Value::Text(ts.format(TS_FORMAT).to_string()))
        }
        SqlType::TimestampTz => {
            let text = temporal_input(v);
            let (date, time, offset) = parse_datetime(&text).ok_or_else(|| bad_datetime("timestamp with time zone", &text, dialect))?;
            let naive = date.and_time(time.unwrap_or(NaiveTime::MIN));
            let utc = match offset {
                Some(off) => off.from_local_datetime(&naive).single().map(|d| d.naive_utc()).unwrap_or(naive),
                None => naive,
            };
            Ok(Value::Text(utc.format(TS_FORMAT).to_string()))
        }
        SqlType::Interval => {
            let text = as_text(v);
            let iv = Interval::parse(&text).ok_or_else(|| {
                EngineError::new(state::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type interval: \"{text}\""))
            })?;
            Ok(Value::Text(iv.to_pg_string()))
        }
        SqlType::Uuid => {
            let parsed = match v {
                ValueRef::Blob(b) if b.len() == 16 => uuid::Uuid::from_slice(b).ok(),
                other => uuid::Uuid::parse_str(as_text(other).trim()).ok(),
            };
            match parsed {
                Some(u) => Ok(Value::Text(u.hyphenated().to_string())),
                None => Err(EngineError::new(
                    state::INVALID_TEXT_REPRESENTATION,
                    format!("invalid input syntax for type uuid: \"{}\"", as_text(v)),
                )),
            }
        }
        SqlType::Json => {
            let text = as_text(v);
            let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| invalid_json(&e, dialect))?;
            Ok(Value::Text(if my { normalize_json(&parsed) } else { text }))
        }
        SqlType::Jsonb => {
            let text = as_text(v);
            let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| invalid_json(&e, dialect))?;
            Ok(Value::Text(normalize_json(&parsed)))
        }
        SqlType::Enum { name, values } => {
            let text = as_text(v);
            let found = if my { values.iter().find(|x| x.eq_ignore_ascii_case(&text)) } else { values.iter().find(|x| **x == text) };
            match found {
                Some(val) => Ok(Value::Text(val.clone())),
                None if my => Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, "Data truncated for column").with_mysql(1265)),
                None => {
                    Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("invalid input value for enum {name}: \"{text}\"")))
                }
            }
        }
        SqlType::Array { element } => {
            let text = as_text(v);
            let items = parse_array_input(&text)
                .ok_or_else(|| EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("malformed array literal: \"{text}\"")))?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let coerced = match item {
                    None => Value::Null,
                    Some(s) => coerce(ValueRef::Text(s.as_bytes()), element, dialect)?,
                };
                out.push(match coerced {
                    Value::Null => serde_json::Value::Null,
                    Value::Integer(i) => serde_json::Value::from(i),
                    Value::Real(f) => serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or_default(),
                    Value::Text(t) => serde_json::Value::String(t),
                    Value::Blob(b) => serde_json::Value::String(format!("\\x{}", hex(&b))),
                });
            }
            Ok(Value::Text(serde_json::Value::Array(out).to_string()))
        }
    }
}

/// Format a stored value as protocol text. `None` means SQL NULL.
pub fn format_text(v: &Value, ty: &SqlType, dialect: Dialect) -> Option<String> {
    // SQL Server shows bits as 1/0, like MySQL.
    let my = dialect != Dialect::Postgres;
    Some(match (v, ty) {
        (Value::Null, _) => return None,
        (Value::Integer(i), SqlType::Bool) if !my => {
            if *i != 0 {
                "t".into()
            } else {
                "f".into()
            }
        }
        (Value::Text(t), SqlType::Bool) if !my => match t.as_str() {
            "1" | "t" | "true" => "t".into(),
            _ => "f".into(),
        },
        (Value::Real(f), t) if t.is_integer() || matches!(t, SqlType::Bool) => format!("{}", f.round() as i64),
        (Value::Real(f), SqlType::Float4) => format_float(*f as f32 as f64, true, dialect),
        (Value::Real(f), SqlType::Numeric { scale: Some(s), .. }) => format!("{:.*}", *s as usize, f),
        (Value::Integer(i), SqlType::Numeric { scale: Some(s), .. }) if *s > 0 => format!("{:.*}", *s as usize, *i as f64),
        (Value::Real(f), _) => format_float(*f, false, dialect),
        (Value::Integer(i), SqlType::Float4 | SqlType::Float8) => format_float(*i as f64, false, dialect),
        (Value::Integer(i), _) => i.to_string(),
        (Value::Blob(b), _) if !my => format!("\\x{}", hex(b)),
        (Value::Blob(b), _) => String::from_utf8_lossy(b).into_owned(),
        (Value::Text(t), SqlType::Timestamp) => format_ts_text(t, false, ty_fsp(my)),
        (Value::Text(t), SqlType::TimestampTz) => format_ts_text(t, !my, ty_fsp(my)),
        (Value::Text(t), SqlType::Time) => match parse_time_only(t) {
            Some(tm) if my => tm.format("%H:%M:%S").to_string(),
            Some(tm) => format_time(tm),
            None => t.clone(),
        },
        (Value::Text(t), SqlType::Array { element }) if !my => match serde_json::from_str::<serde_json::Value>(t) {
            Ok(serde_json::Value::Array(items)) => format_pg_array(&items, element),
            _ => t.clone(),
        },
        (Value::Text(t), SqlType::Char { .. }) if my => t.trim_end_matches(' ').to_owned(),
        (Value::Text(t), _) => t.clone(),
    })
}

fn ty_fsp(my: bool) -> bool {
    // MySQL DATETIME without fractional seconds precision drops fractions.
    my
}

/// Convert a stored value to `serde_json` for UI/MCP consumers.
pub fn to_json(v: &Value, ty: &SqlType, dialect: Dialect) -> serde_json::Value {
    match (v, ty) {
        (Value::Null, _) => serde_json::Value::Null,
        (Value::Integer(i), SqlType::Bool) if dialect != Dialect::MySql => serde_json::Value::Bool(*i != 0),
        (Value::Integer(i), _) => serde_json::Value::from(*i),
        (Value::Real(_), SqlType::Numeric { .. }) => serde_json::Value::String(format_text(v, ty, dialect).unwrap_or_default()),
        (Value::Real(f), _) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(format_float(*f, false, dialect))),
        _ => serde_json::Value::String(format_text(v, ty, dialect).unwrap_or_default()),
    }
}

fn as_text(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => format_float(f, false, Dialect::Postgres),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn temporal_input(v: ValueRef<'_>) -> String {
    match v {
        // MySQL numeric dates: 20240131 or 20240131103000.
        ValueRef::Integer(i) => {
            let s = i.to_string();
            match s.len() {
                8 => format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]),
                14 => format!("{}-{}-{} {}:{}:{}", &s[0..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12], &s[12..14]),
                _ => s,
            }
        }
        other => as_text(other),
    }
}

fn coerce_bool(v: ValueRef<'_>) -> Result<Value, EngineError> {
    match v {
        ValueRef::Integer(i) => Ok(Value::Integer((i != 0) as i64)),
        ValueRef::Real(f) => Ok(Value::Integer((f != 0.0) as i64)),
        other => {
            let s = as_text(other);
            match s.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "y" | "yes" | "on" | "1" => Ok(Value::Integer(1)),
                "f" | "false" | "n" | "no" | "off" | "0" => Ok(Value::Integer(0)),
                _ => Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type boolean: \"{s}\""))),
            }
        }
    }
}

fn int_type_name(ty: &SqlType) -> &'static str {
    match ty {
        SqlType::Int2 | SqlType::Int1 => "smallint",
        SqlType::Int8 => "bigint",
        _ => "integer",
    }
}

fn out_of_range(ty: &SqlType, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::NUMERIC_VALUE_OUT_OF_RANGE, "Out of range value for column").with_mysql(1264)
    } else {
        EngineError::new(state::NUMERIC_VALUE_OUT_OF_RANGE, format!("{} out of range", int_type_name(ty)))
    }
}

fn coerce_int(v: ValueRef<'_>, min: i64, max: i64, ty: &SqlType, dialect: Dialect) -> Result<Value, EngineError> {
    let n: i64 = match v {
        ValueRef::Integer(i) => i,
        ValueRef::Real(f) => {
            let r = f.round();
            if !r.is_finite() || r < i64::MIN as f64 || r > i64::MAX as f64 {
                return Err(out_of_range(ty, dialect));
            }
            r as i64
        }
        other => {
            let s = as_text(other);
            let t = s.trim();
            match t.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    // MySQL accepts numeric-looking strings with fractions; PostgreSQL does not.
                    let as_float = t.parse::<f64>().ok().filter(|_| dialect == Dialect::MySql);
                    match as_float {
                        Some(f) => f.round() as i64,
                        None if t.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '+') && !t.is_empty() => {
                            return Err(out_of_range(ty, dialect));
                        }
                        None if dialect == Dialect::MySql && matches!(ty, SqlType::Bool) => match t.to_ascii_lowercase().as_str() {
                            "true" => 1,
                            "false" => 0,
                            _ => {
                                return Err(EngineError::new(
                                    state::INVALID_TEXT_REPRESENTATION,
                                    format!("Incorrect integer value: '{s}'"),
                                )
                                .with_mysql(1366));
                            }
                        },
                        None if dialect == Dialect::MySql => {
                            return Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("Incorrect integer value: '{s}'"))
                                .with_mysql(1366));
                        }
                        None => {
                            return Err(EngineError::new(
                                state::INVALID_TEXT_REPRESENTATION,
                                format!("invalid input syntax for type {}: \"{s}\"", int_type_name(ty)),
                            ));
                        }
                    }
                }
            }
        }
    };
    if n < min || n > max {
        return Err(out_of_range(ty, dialect));
    }
    Ok(Value::Integer(n))
}

fn coerce_float(v: ValueRef<'_>, ty: &SqlType, dialect: Dialect) -> Result<f64, EngineError> {
    match v {
        ValueRef::Integer(i) => Ok(i as f64),
        ValueRef::Real(f) => Ok(f),
        other => {
            let s = as_text(other);
            let t = s.trim();
            let parsed = match t.to_ascii_lowercase().as_str() {
                "nan" => Some(f64::NAN),
                "infinity" | "inf" | "+infinity" | "+inf" => Some(f64::INFINITY),
                "-infinity" | "-inf" => Some(f64::NEG_INFINITY),
                _ => t.parse::<f64>().ok(),
            };
            parsed.ok_or_else(|| {
                let name = match ty {
                    SqlType::Numeric { .. } => "numeric",
                    SqlType::Float4 => "real",
                    _ => "double precision",
                };
                if dialect == Dialect::MySql {
                    EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("Incorrect decimal value: '{s}'")).with_mysql(1366)
                } else {
                    EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type {name}: \"{s}\""))
                }
            })
        }
    }
}

fn check_len(s: &str, len: Option<u32>, ty: &SqlType, dialect: Dialect) -> Result<(), EngineError> {
    if let Some(n) = len
        && s.chars().count() > n as usize
    {
        return Err(if dialect == Dialect::MySql {
            EngineError::new(state::STRING_DATA_RIGHT_TRUNCATION, "Data too long for column").with_mysql(1406)
        } else {
            EngineError::new(state::STRING_DATA_RIGHT_TRUNCATION, format!("value too long for type {}", ty.pg_format_type()))
        });
    }
    Ok(())
}

fn bad_datetime(type_name: &str, text: &str, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::INVALID_DATETIME_FORMAT, format!("Incorrect datetime value: '{text}'")).with_mysql(1292)
    } else {
        EngineError::new(state::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type {type_name}: \"{text}\""))
    }
}

fn invalid_json(e: &serde_json::Error, dialect: Dialect) -> EngineError {
    if dialect == Dialect::MySql {
        EngineError::new(state::INVALID_TEXT_REPRESENTATION, format!("Invalid JSON text: \"{e}\"")).with_mysql(3140)
    } else {
        EngineError::new(state::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type json").with_detail(e.to_string())
    }
}

fn decode_pg_bytea(t: &[u8]) -> Result<Vec<u8>, EngineError> {
    if let Some(hexpart) = t.strip_prefix(b"\\x") {
        let s = std::str::from_utf8(hexpart).unwrap_or("");
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        if !clean.len().is_multiple_of(2) {
            return Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, "invalid hexadecimal data: odd number of digits"));
        }
        return (0..clean.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&clean[i..i + 2], 16)
                    .map_err(|_| EngineError::new(state::INVALID_TEXT_REPRESENTATION, "invalid hexadecimal digit"))
            })
            .collect();
    }
    // Escape format: `\\` and `\ooo`.
    let mut out = Vec::with_capacity(t.len());
    let mut i = 0;
    while i < t.len() {
        if t[i] == b'\\' {
            if i + 1 < t.len() && t[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if i + 3 < t.len() && t[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (t[i + 1] - b'0') * 64 + (t[i + 2] - b'0') * 8 + (t[i + 3] - b'0');
                out.push(v);
                i += 4;
                continue;
            }
            return Err(EngineError::new(state::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type bytea"));
        }
        out.push(t[i]);
        i += 1;
    }
    Ok(out)
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Format a float the way PostgreSQL (shortest round-trip) or MySQL do.
pub fn format_float(f: f64, single: bool, dialect: Dialect) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let abs = f.abs();
    let use_exp = abs != 0.0 && (abs >= 1e15 || abs < 1e-4);
    let s = if single {
        let g = f as f32;
        if use_exp { format!("{g:e}") } else { format!("{g}") }
    } else if use_exp {
        format!("{f:e}")
    } else {
        format!("{f}")
    };
    if use_exp && dialect != Dialect::MySql {
        // Rust: 1e20 / 1.5e-5  ->  PostgreSQL: 1e+20 / 1.5e-05
        if let Some((mantissa, exp)) = s.split_once('e') {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            return format!("{mantissa}e{sign}{digits:0>2}");
        }
    }
    s
}

fn format_time(t: NaiveTime) -> String {
    if t.nanosecond() == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        let s = t.format("%H:%M:%S%.6f").to_string();
        s.trim_end_matches('0').to_owned()
    }
}

/// Render a stored canonical timestamp. Fractions keep PostgreSQL's trailing-zero trimming.
fn format_ts_text(t: &str, with_utc_suffix: bool, drop_fraction: bool) -> String {
    let base = match NaiveDateTime::parse_from_str(t, TS_FORMAT).or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S")) {
        Ok(ts) => {
            if drop_fraction || ts.nanosecond() == 0 {
                ts.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string().trim_end_matches('0').to_owned()
            }
        }
        Err(_) => return t.to_owned(),
    };
    if with_utc_suffix { format!("{base}+00") } else { base }
}

/// Parse an ISO-ish date/time string. Returns date, optional time, optional zone offset.
pub fn parse_datetime(input: &str) -> Option<(NaiveDate, Option<NaiveTime>, Option<FixedOffset>)> {
    let s = input.trim();
    let lower = s.to_ascii_lowercase();
    let now = Utc::now().naive_utc();
    match lower.as_str() {
        "now" | "current_timestamp" => return Some((now.date(), Some(now.time()), Some(FixedOffset::east_opt(0)?))),
        "today" => return Some((now.date(), None, None)),
        "tomorrow" => return Some((now.date() + Duration::days(1), None, None)),
        "yesterday" => return Some((now.date() - Duration::days(1), None, None)),
        "epoch" => return Some((NaiveDate::from_ymd_opt(1970, 1, 1)?, None, Some(FixedOffset::east_opt(0)?))),
        _ => {}
    }
    // RFC 3339 / ISO 8601 with offset.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some((dt.naive_local().date(), Some(dt.naive_local().time()), Some(*dt.offset())));
    }
    let (date_part, rest) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let date = parse_date_only(date_part)?;
    if rest.is_empty() {
        return Some((date, None, None));
    }
    let (time_text, offset) = split_zone(rest);
    let time = parse_time_only(time_text)?;
    Some((date, Some(time), offset))
}

fn parse_date_only(s: &str) -> Option<NaiveDate> {
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y", "%d-%b-%Y", "%Y%m%d", "%b %d %Y", "%Y.%m.%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    None
}

pub fn parse_time_only(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    for fmt in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M", "%H%M%S"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Some(t);
        }
    }
    None
}

/// Split a trailing time-zone designator: `Z`, `+05`, `+0530`, `+05:30`, ` UTC`.
fn split_zone(s: &str) -> (&str, Option<FixedOffset>) {
    let trimmed = s.trim();
    for suffix in [" UTC", " GMT", "UTC", "GMT", "Z", "z"] {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            return (rest.trim(), FixedOffset::east_opt(0));
        }
    }
    if let Some(pos) = trimmed.rfind(['+', '-'])
        && pos >= 5
    {
        let (time, zone) = trimmed.split_at(pos);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let digits: String = zone[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        let parsed = match digits.len() {
            1 | 2 => digits.parse::<i32>().ok().map(|h| (h, 0)),
            4 => digits[..2].parse::<i32>().ok().zip(digits[2..].parse::<i32>().ok()),
            _ => None,
        };
        let Some((h, m)) = parsed else { return (trimmed, None) };
        return (time.trim(), FixedOffset::east_opt(sign * (h * 3600 + m * 60)));
    }
    (trimmed, None)
}

/// Normalize JSON like PostgreSQL `jsonb` / MySQL `JSON`: duplicate keys removed (last wins),
/// keys ordered by length then bytes, `", "` and `": "` separators.
pub fn normalize_json(v: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(v, &mut out);
    out
}

fn write_json(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes())));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&serde_json::Value::String((*k).clone()).to_string());
                out.push_str(": ");
                write_json(&map[k.as_str()], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Parse array input: PostgreSQL literal `{1,2,"a b",NULL}` or JSON `[1,2]`.
pub fn parse_array_input(s: &str) -> Option<Vec<Option<String>>> {
    let t = s.trim();
    if t.starts_with('[') {
        let v: serde_json::Value = serde_json::from_str(t).ok()?;
        return v.as_array().map(|items| {
            items
                .iter()
                .map(|i| match i {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
                .collect()
        });
    }
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    let mut items = Vec::new();
    if inner.trim().is_empty() {
        return Some(items);
    }
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        let mut quoted = false;
        if chars.peek() == Some(&'"') {
            quoted = true;
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            value.push(n);
                        }
                    }
                    '"' => break,
                    other => value.push(other),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
            value = value.trim().to_owned();
        }
        if !quoted && value.eq_ignore_ascii_case("null") {
            items.push(None);
        } else {
            items.push(Some(value));
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            Some(',') => continue,
            None => break,
            Some(_) => return None,
        }
    }
    Some(items)
}

fn format_pg_array(items: &[serde_json::Value], element: &SqlType) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            serde_json::Value::Null => "NULL".into(),
            serde_json::Value::Bool(b) => {
                if *b {
                    "t".into()
                } else {
                    "f".into()
                }
            }
            serde_json::Value::Number(n) => {
                let v = if let Some(i) = n.as_i64() { Value::Integer(i) } else { Value::Real(n.as_f64().unwrap_or(0.0)) };
                format_text(&v, element, Dialect::Postgres).unwrap_or_default()
            }
            serde_json::Value::String(s) => {
                let shown = if matches!(element, SqlType::Bool) {
                    if s == "1" || s == "t" { "t".to_owned() } else { "f".to_owned() }
                } else {
                    format_text(&Value::Text(s.clone()), element, Dialect::Postgres).unwrap_or_default()
                };
                quote_array_element(&shown)
            }
            other => quote_array_element(&other.to_string()),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn quote_array_element(s: &str) -> String {
    let needs =
        s.is_empty() || s.eq_ignore_ascii_case("null") || s.chars().any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if needs { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_owned() }
}

/// PostgreSQL interval value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    pub fn parse(input: &str) -> Option<Interval> {
        let s = input.trim().trim_start_matches('@').trim();
        let lower = s.to_ascii_lowercase();
        if let Some(iso) = lower.strip_prefix('p') {
            return Self::parse_iso(iso);
        }
        let mut iv = Interval::default();
        let ago = lower.ends_with(" ago");
        let body = lower.trim_end_matches(" ago");
        let tokens: Vec<&str> = body.split_whitespace().collect();
        let mut i = 0;
        let mut any = false;
        while i < tokens.len() {
            let tok = tokens[i];
            if tok.contains(':') {
                let neg = tok.starts_with('-');
                let t = tok.trim_start_matches(['-', '+']);
                let parts: Vec<&str> = t.split(':').collect();
                let h: i64 = parts.first()?.parse().ok()?;
                let m: i64 = parts.get(1).map(|p| p.parse().ok()).unwrap_or(Some(0))?;
                let sec: f64 = parts.get(2).map(|p| p.parse().ok()).unwrap_or(Some(0.0))?;
                let micros = h * 3_600_000_000 + m * 60_000_000 + (sec * 1_000_000.0).round() as i64;
                iv.micros += if neg { -micros } else { micros };
                any = true;
                i += 1;
                continue;
            }
            let value: f64 = tok.parse().ok()?;
            let unit = tokens.get(i + 1).copied().unwrap_or("second");
            let unit = unit.trim_end_matches(',');
            match unit {
                "year" | "years" | "y" | "yr" | "yrs" => iv.months += (value * 12.0) as i32,
                "month" | "months" | "mon" | "mons" => iv.months += value as i32,
                "week" | "weeks" | "w" => iv.days += (value * 7.0) as i32,
                "day" | "days" | "d" => iv.days += value as i32,
                "hour" | "hours" | "h" | "hr" | "hrs" => iv.micros += (value * 3_600_000_000.0) as i64,
                "minute" | "minutes" | "min" | "mins" | "m" => iv.micros += (value * 60_000_000.0) as i64,
                "second" | "seconds" | "sec" | "secs" | "s" => iv.micros += (value * 1_000_000.0) as i64,
                "millisecond" | "milliseconds" | "ms" => iv.micros += (value * 1000.0) as i64,
                "microsecond" | "microseconds" | "us" => iv.micros += value as i64,
                _ => return None,
            }
            any = true;
            i += 2;
        }
        if !any {
            return None;
        }
        if ago {
            iv = iv.negate();
        }
        Some(iv)
    }

    fn parse_iso(s: &str) -> Option<Interval> {
        let mut iv = Interval::default();
        let mut in_time = false;
        let mut num = String::new();
        for c in s.chars() {
            match c {
                't' => in_time = true,
                '0'..='9' | '.' | '-' => num.push(c),
                unit => {
                    let v: f64 = num.parse().ok()?;
                    num.clear();
                    match (unit, in_time) {
                        ('y', false) => iv.months += (v * 12.0) as i32,
                        ('m', false) => iv.months += v as i32,
                        ('w', false) => iv.days += (v * 7.0) as i32,
                        ('d', false) => iv.days += v as i32,
                        ('h', true) => iv.micros += (v * 3_600_000_000.0) as i64,
                        ('m', true) => iv.micros += (v * 60_000_000.0) as i64,
                        ('s', true) => iv.micros += (v * 1_000_000.0) as i64,
                        _ => return None,
                    }
                }
            }
        }
        Some(iv)
    }

    pub fn negate(self) -> Interval {
        Interval { months: -self.months, days: -self.days, micros: -self.micros }
    }

    pub fn to_pg_string(self) -> String {
        let mut parts = Vec::new();
        let years = self.months / 12;
        let months = self.months % 12;
        let plural = |n: i32, unit: &str| if n.abs() == 1 { format!("{n} {unit}") } else { format!("{n} {unit}s") };
        if years != 0 {
            parts.push(plural(years, "year"));
        }
        if months != 0 {
            parts.push(if months.abs() == 1 { format!("{months} mon") } else { format!("{months} mons") });
        }
        if self.days != 0 {
            parts.push(plural(self.days, "day"));
        }
        if self.micros != 0 || parts.is_empty() {
            let neg = self.micros < 0;
            let total = self.micros.unsigned_abs();
            let h = total / 3_600_000_000;
            let m = (total / 60_000_000) % 60;
            let s = (total / 1_000_000) % 60;
            let frac = total % 1_000_000;
            let mut t = format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, h, m, s);
            if frac != 0 {
                let f = format!("{frac:06}");
                t.push('.');
                t.push_str(f.trim_end_matches('0'));
            }
            parts.push(t);
        }
        parts.join(" ")
    }

    /// Add this interval to a timestamp.
    pub fn add_to(self, ts: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut out = ts;
        if self.months > 0 {
            out = out.checked_add_months(Months::new(self.months as u32))?;
        } else if self.months < 0 {
            out = out.checked_sub_months(Months::new(self.months.unsigned_abs()))?;
        }
        out += Duration::days(self.days as i64);
        out += Duration::microseconds(self.micros);
        Some(out)
    }

    /// Interval between two timestamps (days + time, like PostgreSQL `ts - ts`).
    pub fn between(a: NaiveDateTime, b: NaiveDateTime) -> Interval {
        let d = a - b;
        let total = d.num_microseconds().unwrap_or(0);
        let days = total / 86_400_000_000;
        Interval { months: 0, days: days as i32, micros: total - days * 86_400_000_000 }
    }
}

/// Parse a stored or literal timestamp into a naive UTC datetime.
pub fn parse_any_timestamp(s: &str) -> Option<NaiveDateTime> {
    let (date, time, offset) = parse_datetime(s)?;
    let naive = date.and_time(time.unwrap_or(NaiveTime::MIN));
    Some(match offset {
        Some(off) => off.from_local_datetime(&naive).single().map(|d| d.naive_utc()).unwrap_or(naive),
        None => naive,
    })
}

/// Current transaction-independent timestamp in canonical storage format.
pub fn now_text() -> String {
    Utc::now().naive_utc().format(TS_FORMAT).to_string()
}

/// Extract a field (PostgreSQL `EXTRACT`/`date_part`) from a timestamp.
pub fn extract_field(field: &str, ts: NaiveDateTime) -> Option<f64> {
    Some(match field.to_ascii_lowercase().as_str() {
        "year" | "years" => ts.year() as f64,
        "month" | "months" | "mon" => ts.month() as f64,
        "day" | "days" => ts.day() as f64,
        "hour" | "hours" => ts.hour() as f64,
        "minute" | "minutes" => ts.minute() as f64,
        "second" | "seconds" => ts.second() as f64 + ts.nanosecond() as f64 / 1e9,
        "milliseconds" => (ts.second() as f64 * 1000.0) + ts.nanosecond() as f64 / 1e6,
        "microseconds" => (ts.second() as f64 * 1e6) + ts.nanosecond() as f64 / 1e3,
        "dow" => ts.weekday().num_days_from_sunday() as f64,
        "isodow" => ts.weekday().number_from_monday() as f64,
        "doy" => ts.ordinal() as f64,
        "week" => ts.iso_week().week() as f64,
        "quarter" => ((ts.month() - 1) / 3 + 1) as f64,
        "epoch" => ts.and_utc().timestamp_micros() as f64 / 1e6,
        "decade" => (ts.year() / 10) as f64,
        "century" => ((ts.year() - 1) / 100 + 1) as f64,
        _ => return None,
    })
}

/// PostgreSQL `date_trunc`.
pub fn date_trunc(field: &str, ts: NaiveDateTime) -> Option<NaiveDateTime> {
    let d = ts.date();
    let midnight = NaiveTime::MIN;
    Some(match field.to_ascii_lowercase().as_str() {
        "microseconds" | "microsecond" => ts,
        "milliseconds" | "millisecond" => ts.with_nanosecond(ts.nanosecond() / 1_000_000 * 1_000_000)?,
        "second" | "seconds" => ts.with_nanosecond(0)?,
        "minute" | "minutes" => ts.with_nanosecond(0)?.with_second(0)?,
        "hour" | "hours" => ts.with_nanosecond(0)?.with_second(0)?.with_minute(0)?,
        "day" | "days" => d.and_time(midnight),
        "week" | "weeks" => (d - Duration::days(d.weekday().num_days_from_monday() as i64)).and_time(midnight),
        "month" | "months" => d.with_day(1)?.and_time(midnight),
        "quarter" => NaiveDate::from_ymd_opt(d.year(), (d.month() - 1) / 3 * 3 + 1, 1)?.and_time(midnight),
        "year" | "years" => NaiveDate::from_ymd_opt(d.year(), 1, 1)?.and_time(midnight),
        "decade" => NaiveDate::from_ymd_opt(d.year() / 10 * 10, 1, 1)?.and_time(midnight),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(v: &str, ty: SqlType) -> Result<Value, EngineError> {
        coerce(ValueRef::Text(v.as_bytes()), &ty, Dialect::Postgres)
    }

    #[test]
    fn integers_follow_postgres_rules() {
        assert_eq!(pg(" 42 ", SqlType::Int4).unwrap(), Value::Integer(42));
        let err = pg("abc", SqlType::Int4).unwrap_err();
        assert_eq!(err.sqlstate, "22P02");
        assert_eq!(err.message, "invalid input syntax for type integer: \"abc\"");
        assert_eq!(pg("99999", SqlType::Int2).unwrap_err().message, "smallint out of range");
        assert_eq!(pg("1.5", SqlType::Int4).unwrap_err().sqlstate, "22P02");
    }

    #[test]
    fn booleans_and_text_formatting() {
        assert_eq!(pg("yes", SqlType::Bool).unwrap(), Value::Integer(1));
        assert_eq!(format_text(&Value::Integer(1), &SqlType::Bool, Dialect::Postgres).unwrap(), "t");
        assert_eq!(format_text(&Value::Integer(1), &SqlType::Bool, Dialect::MySql).unwrap(), "1");
    }

    #[test]
    fn timestamps_normalize_and_format() {
        let v = pg("2024-03-01T10:00:00+02:00", SqlType::TimestampTz).unwrap();
        assert_eq!(v, Value::Text("2024-03-01 08:00:00.000000".into()));
        assert_eq!(format_text(&v, &SqlType::TimestampTz, Dialect::Postgres).unwrap(), "2024-03-01 08:00:00+00");
        let v = pg("2024-03-01 10:00:00.5", SqlType::Timestamp).unwrap();
        assert_eq!(format_text(&v, &SqlType::Timestamp, Dialect::Postgres).unwrap(), "2024-03-01 10:00:00.5");
        assert_eq!(pg("2024-03-01", SqlType::Date).unwrap(), Value::Text("2024-03-01".into()));
        assert_eq!(pg("nope", SqlType::Date).unwrap_err().sqlstate, "22007");
    }

    #[test]
    fn varchar_length_and_uuid() {
        let err = pg("abcdef", SqlType::Varchar { len: Some(3) }).unwrap_err();
        assert_eq!(err.message, "value too long for type character varying(3)");
        let u = pg("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11", SqlType::Uuid).unwrap();
        assert_eq!(u, Value::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()));
    }

    #[test]
    fn jsonb_normalizes_like_postgres() {
        let v = pg(r#"{"bb":1,"a":[1,2],"a":3}"#, SqlType::Jsonb).unwrap();
        assert_eq!(v, Value::Text(r#"{"a": 3, "bb": 1}"#.into()));
    }

    #[test]
    fn numeric_scale_and_overflow() {
        let ty = SqlType::Numeric { precision: Some(5), scale: Some(2) };
        let v = pg("12.345", ty.clone()).unwrap();
        assert_eq!(format_text(&v, &ty, Dialect::Postgres).unwrap(), "12.35");
        assert_eq!(pg("1234.5", ty).unwrap_err().message, "numeric field overflow");
    }

    #[test]
    fn arrays_round_trip() {
        let ty = SqlType::Array { element: Box::new(SqlType::Text) };
        let v = pg(r#"{a,"b c",NULL}"#, ty.clone()).unwrap();
        assert_eq!(format_text(&v, &ty, Dialect::Postgres).unwrap(), r#"{a,"b c",NULL}"#);
        let ity = SqlType::Array { element: Box::new(SqlType::Int4) };
        let v = pg("{1,2,3}", ity.clone()).unwrap();
        assert_eq!(v, Value::Text("[1,2,3]".into()));
        assert_eq!(format_text(&v, &ity, Dialect::Postgres).unwrap(), "{1,2,3}");
    }

    #[test]
    fn intervals_parse_and_print() {
        assert_eq!(Interval::parse("1 day 2 hours").unwrap().to_pg_string(), "1 day 02:00:00");
        assert_eq!(Interval::parse("14 months").unwrap().to_pg_string(), "1 year 2 mons");
        assert_eq!(Interval::parse("PT1H30M").unwrap().to_pg_string(), "01:30:00");
        assert_eq!(Interval::parse("3 days ago").unwrap().to_pg_string(), "-3 days");
    }

    #[test]
    fn floats_format_like_postgres() {
        assert_eq!(format_float(1e20, false, Dialect::Postgres), "1e+20");
        assert_eq!(format_float(1.5e-5, false, Dialect::Postgres), "1.5e-05");
        assert_eq!(format_float(0.1, false, Dialect::Postgres), "0.1");
    }
}
