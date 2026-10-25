//! SQL Server mock: the TDS 7.4 protocol on top of the TwinLite engine in T-SQL mode.
//!
//! Supported: PRELOGIN with TLS as SQL Server negotiates it (login-only encryption for
//! `Encrypt=False`, full encryption for `Encrypt=True`, none when the client does not support it;
//! the certificate is self-signed, so clients need `TrustServerCertificate=True`), LOGIN7 with SQL
//! authentication, SQL batches, RPC calls `sp_executesql`, `sp_prepare`,
//! `sp_prepexec`, `sp_execute` and `sp_unprepare` (parameters are bound as typed literals),
//! transaction-manager requests, and attention (cancel). Results are typed TDS columns (INTN, BITN,
//! FLTN, DECIMALN, NVARCHAR, VARBINARY, GUID, DATE, TIME, DATETIME2, DATETIMEOFFSET).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use twinbase_engine::tsql::{bind, declared_names, error_number};
use twinbase_engine::{Engine, OutColumn, Session, SqlType, Value};

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};

const PACKET_SIZE: usize = 4096;
/// SQL_Latin1_General_CP1_CI_AS.
const COLLATION: [u8; 5] = [0x09, 0x04, 0xD0, 0x00, 0x34];

// Packet types.
const PT_BATCH: u8 = 0x01;
const PT_RPC: u8 = 0x03;
const PT_REPLY: u8 = 0x04;
const PT_ATTENTION: u8 = 0x06;
const PT_LOGIN7: u8 = 0x10;
const PT_TM: u8 = 0x0E;
const PT_PRELOGIN: u8 = 0x12;

// DONE status bits.
const DONE_MORE: u16 = 0x01;
const DONE_ERROR: u16 = 0x02;
const DONE_COUNT: u16 = 0x10;
const DONE_ATTN: u16 = 0x20;

// ------------------------------------------------------------------ encoding helpers

struct Out(Vec<u8>);

impl Out {
    fn new() -> Self {
        Out(Vec::with_capacity(512))
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    /// B_VARCHAR: char count byte + UTF-16LE.
    fn b_varchar(&mut self, s: &str) -> &mut Self {
        let units: Vec<u16> = s.encode_utf16().take(255).collect();
        self.u8(units.len() as u8);
        for u in units {
            self.u16(u);
        }
        self
    }
    /// US_VARCHAR: char count u16 + UTF-16LE.
    fn us_varchar(&mut self, s: &str) -> &mut Self {
        let units: Vec<u16> = s.encode_utf16().take(65535).collect();
        self.u16(units.len() as u16);
        for u in units {
            self.u16(u);
        }
        self
    }
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn from_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

fn env_change(out: &mut Out, kind: u8, new: &str, old: &str) {
    let mut body = Out::new();
    body.u8(kind).b_varchar(new).b_varchar(old);
    out.u8(0xE3).u16(body.0.len() as u16).bytes(&body.0);
}

fn env_change_bytes(out: &mut Out, kind: u8, new: &[u8], old: &[u8]) {
    let mut body = Out::new();
    body.u8(kind).u8(new.len() as u8).bytes(new).u8(old.len() as u8).bytes(old);
    out.u8(0xE3).u16(body.0.len() as u16).bytes(&body.0);
}

/// INFO (0xAB) or ERROR (0xAA) token.
fn message(out: &mut Out, token: u8, number: u32, class: u8, text: &str) {
    let mut body = Out::new();
    body.u32(number).u8(1).u8(class).us_varchar(text).b_varchar("twinbase").b_varchar("").u32(1);
    out.u8(token).u16(body.0.len() as u16).bytes(&body.0);
}

fn done(out: &mut Out, token: u8, status: u16, rows: u64) {
    done_cmd(out, token, status, 0xC1, rows);
}

/// DONE with the statement kind (CurCmd), which drivers use to report affected rows.
fn done_cmd(out: &mut Out, token: u8, status: u16, cur_cmd: u16, rows: u64) {
    out.u8(token).u16(status).u16(cur_cmd).u64(rows);
}

fn cur_cmd(tag: &str) -> u16 {
    match tag {
        "SELECT" => 0xC1,
        "INSERT" => 0xC3,
        "DELETE" => 0xC4,
        "UPDATE" => 0xC5,
        "MERGE" => 0x0117,
        _ => 0,
    }
}

// ------------------------------------------------------------------ result types

/// How a result column is sent.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Wire {
    Int(u8),
    Bit,
    Float,
    Decimal(u8, u8),
    NVarChar,
    VarBinary,
    Guid,
    Date,
    Time,
    DateTime2,
    DateTimeOffset,
}

fn wire_for(c: &OutColumn, rows: &[Vec<Value>], i: usize) -> Wire {
    match &c.ty {
        SqlType::Bool => Wire::Bit,
        SqlType::Int1 | SqlType::Int2 => Wire::Int(2),
        SqlType::Int4 | SqlType::Year => Wire::Int(4),
        SqlType::Int8 => Wire::Int(8),
        SqlType::Float4 | SqlType::Float8 => Wire::Float,
        SqlType::Numeric { scale: Some(s), precision } => Wire::Decimal(precision.unwrap_or(38).clamp(1, 38) as u8, (*s).min(38) as u8),
        SqlType::Uuid => Wire::Guid,
        SqlType::Bytea => Wire::VarBinary,
        SqlType::Date => Wire::Date,
        SqlType::Time => Wire::Time,
        SqlType::Timestamp => Wire::DateTime2,
        SqlType::TimestampTz => Wire::DateTimeOffset,
        SqlType::Text
        | SqlType::Varchar { .. }
        | SqlType::Char { .. }
        | SqlType::Json
        | SqlType::Jsonb
        | SqlType::Enum { .. }
        | SqlType::Interval => Wire::NVarChar,
        // Expressions without a declared type: decide by the values (integers are int, as
        // SQL Server types literals, unless one needs bigint).
        _ => {
            let mut kind = None;
            for r in rows {
                match r.get(i) {
                    Some(Value::Integer(n)) if i32::try_from(*n).is_err() => kind = Some(Wire::Int(8)),
                    Some(Value::Integer(_)) if kind.is_none() => kind = Some(Wire::Int(4)),
                    Some(Value::Real(_)) => return Wire::Float,
                    Some(Value::Text(_)) => return Wire::NVarChar,
                    Some(Value::Blob(_)) => return Wire::VarBinary,
                    _ => {}
                }
            }
            kind.unwrap_or(Wire::NVarChar)
        }
    }
}

fn col_metadata(out: &mut Out, columns: &[OutColumn], wires: &[Wire]) {
    out.u8(0x81).u16(columns.len() as u16);
    for (c, w) in columns.iter().zip(wires) {
        out.u32(0).u16(0x0001 | 0x0008); // nullable, updatable unknown
        match w {
            Wire::Int(n) => {
                out.u8(0x26).u8(*n);
            }
            Wire::Bit => {
                out.u8(0x68).u8(1);
            }
            Wire::Float => {
                out.u8(0x6D).u8(8);
            }
            Wire::Decimal(p, s) => {
                out.u8(0x6A).u8(17).u8(*p).u8(*s);
            }
            Wire::NVarChar => {
                out.u8(0xE7).u16(0xFFFF).bytes(&COLLATION);
            }
            Wire::VarBinary => {
                out.u8(0xA5).u16(0xFFFF);
            }
            Wire::Guid => {
                out.u8(0x24).u8(16);
            }
            Wire::Date => {
                out.u8(0x28);
            }
            Wire::Time => {
                out.u8(0x29).u8(7);
            }
            Wire::DateTime2 => {
                out.u8(0x2A).u8(7);
            }
            Wire::DateTimeOffset => {
                out.u8(0x2B).u8(7);
            }
        }
        out.b_varchar(&c.name);
    }
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Integer(i) => Some(i.to_string()),
        Value::Real(f) => Some(f.to_string()),
        Value::Text(t) => Some(t.clone()),
        Value::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
    }
}

/// Partially length-prefixed value (NVARCHAR(MAX), VARBINARY(MAX)).
fn plp(out: &mut Out, data: Option<&[u8]>) {
    match data {
        None => {
            out.u64(u64::MAX);
        }
        Some(d) => {
            out.u64(d.len() as u64);
            for chunk in d.chunks(8000) {
                out.u32(chunk.len() as u32).bytes(chunk);
            }
            out.u32(0);
        }
    }
}

fn date_days(d: chrono::NaiveDate) -> u32 {
    (d.signed_duration_since(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap()).num_days()) as u32
}

fn time_ticks(t: chrono::NaiveTime) -> u64 {
    use chrono::Timelike;
    (t.num_seconds_from_midnight() as u64) * 10_000_000 + (t.nanosecond() as u64 / 100)
}

fn parse_datetime(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Some(d);
        }
        if f == "%Y-%m-%d"
            && let Ok(d) = chrono::NaiveDate::parse_from_str(s, f)
        {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.naive_utc())
}

fn row(out: &mut Out, values: &[Value], wires: &[Wire]) {
    out.u8(0xD1);
    for (v, w) in values.iter().zip(wires) {
        match w {
            Wire::Int(n) => match v {
                Value::Null => {
                    out.u8(0);
                }
                other => {
                    let i = match other {
                        Value::Integer(i) => *i,
                        Value::Real(f) => *f as i64,
                        Value::Text(t) => t.trim().parse().unwrap_or(0),
                        _ => 0,
                    };
                    out.u8(*n);
                    match n {
                        2 => out.bytes(&(i as i16).to_le_bytes()),
                        4 => out.bytes(&(i as i32).to_le_bytes()),
                        _ => out.bytes(&i.to_le_bytes()),
                    };
                }
            },
            Wire::Bit => match v {
                Value::Null => {
                    out.u8(0);
                }
                Value::Integer(i) => {
                    out.u8(1).u8((*i != 0) as u8);
                }
                other => {
                    let t = text_of(other).unwrap_or_default().to_ascii_lowercase();
                    out.u8(1).u8(matches!(t.as_str(), "1" | "t" | "true") as u8);
                }
            },
            Wire::Float => match v {
                Value::Null => {
                    out.u8(0);
                }
                other => {
                    let f = match other {
                        Value::Integer(i) => *i as f64,
                        Value::Real(f) => *f,
                        Value::Text(t) => t.trim().parse().unwrap_or(0.0),
                        _ => 0.0,
                    };
                    out.u8(8).bytes(&f.to_le_bytes());
                }
            },
            Wire::Decimal(_, scale) => match text_of(v) {
                None => {
                    out.u8(0);
                }
                Some(t) => {
                    let d: bigdecimal::BigDecimal = t.trim().parse().unwrap_or_default();
                    let scaled = d.with_scale(*scale as i64);
                    let (digits, _) = scaled.as_bigint_and_exponent();
                    let negative = digits.sign() == bigdecimal::num_bigint::Sign::Minus;
                    let (_, mag) = digits.to_bytes_le();
                    let mut m = [0u8; 16];
                    m[..mag.len().min(16)].copy_from_slice(&mag[..mag.len().min(16)]);
                    out.u8(17).u8(if negative { 0 } else { 1 }).bytes(&m);
                }
            },
            Wire::NVarChar => plp(out, text_of(v).map(|t| utf16(&t)).as_deref()),
            Wire::VarBinary => match v {
                Value::Null => plp(out, None),
                Value::Blob(b) => plp(out, Some(b)),
                other => plp(out, text_of(other).map(|t| t.into_bytes()).as_deref()),
            },
            Wire::Guid => match text_of(v).and_then(|t| uuid::Uuid::parse_str(t.trim()).ok()) {
                None => {
                    out.u8(0);
                }
                Some(u) => {
                    // The first three groups are little-endian on the wire.
                    let b = u.as_bytes();
                    out.u8(16).bytes(&[b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6]]).bytes(&b[8..]);
                }
            },
            Wire::Date => match text_of(v).and_then(|t| parse_datetime(&t)) {
                None => {
                    out.u8(0);
                }
                Some(d) => {
                    out.u8(3).bytes(&date_days(d.date()).to_le_bytes()[..3]);
                }
            },
            Wire::Time => match text_of(v).and_then(|t| chrono::NaiveTime::parse_from_str(t.trim(), "%H:%M:%S%.f").ok()) {
                None => {
                    out.u8(0);
                }
                Some(t) => {
                    out.u8(5).bytes(&time_ticks(t).to_le_bytes()[..5]);
                }
            },
            Wire::DateTime2 => match text_of(v).and_then(|t| parse_datetime(&t)) {
                None => {
                    out.u8(0);
                }
                Some(d) => {
                    out.u8(8).bytes(&time_ticks(d.time()).to_le_bytes()[..5]).bytes(&date_days(d.date()).to_le_bytes()[..3]);
                }
            },
            Wire::DateTimeOffset => {
                let parsed = text_of(v).and_then(|t| {
                    let t = t.trim().to_owned();
                    chrono::DateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f%#z")
                        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&t))
                        .ok()
                        .or_else(|| parse_datetime(&t).map(|n| n.and_utc().fixed_offset()))
                });
                match parsed {
                    None => {
                        out.u8(0);
                    }
                    Some(d) => {
                        let utc = d.naive_utc();
                        let offset = (d.offset().local_minus_utc() / 60) as i16;
                        out.u8(10)
                            .bytes(&time_ticks(utc.time()).to_le_bytes()[..5])
                            .bytes(&date_days(utc.date()).to_le_bytes()[..3])
                            .bytes(&offset.to_le_bytes());
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ parameters

/// A decoded RPC parameter: name and value as a T-SQL literal.
struct Param {
    name: String,
    literal: String,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn b_varchar(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        self.take(n * 2).map(from_utf16)
    }
    fn plp(&mut self) -> Option<Option<Vec<u8>>> {
        let total = self.u64()?;
        if total == u64::MAX {
            return Some(None);
        }
        let mut data = Vec::new();
        loop {
            let n = self.u32()? as usize;
            if n == 0 {
                break;
            }
            data.extend_from_slice(self.take(n)?);
        }
        Some(Some(data))
    }
}

fn nstring(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// MONEY values are integers in ten-thousandths.
fn money_text(raw: i64) -> String {
    format!("{}{}.{:04}", if raw < 0 { "-" } else { "" }, (raw / 10_000).abs(), (raw % 10_000).abs())
}

/// Read one RPC parameter (name, status, TYPE_INFO, value) as a literal.
fn read_param(r: &mut Reader) -> Option<Param> {
    let name = r.b_varchar()?;
    let status = r.u8()?;
    let ty = r.u8()?;
    let literal = match ty {
        // INTN / BITN / FLTN / MONEYN with a length byte, then len + value.
        0x26 | 0x68 | 0x6D | 0x6E => {
            let _max = r.u8()?;
            let n = r.u8()? as usize;
            let v = r.take(n)?;
            match (ty, n) {
                (_, 0) => "NULL".into(),
                (0x26, 1) => v[0].to_string(),
                (0x26, 2) => i16::from_le_bytes([v[0], v[1]]).to_string(),
                (0x26, 4) => i32::from_le_bytes(v.try_into().ok()?).to_string(),
                (0x26, 8) => i64::from_le_bytes(v.try_into().ok()?).to_string(),
                (0x68, _) => if v[0] != 0 { "1" } else { "0" }.into(),
                (0x6D, 4) => f32::from_le_bytes(v.try_into().ok()?).to_string(),
                (0x6D, 8) => format!("{:?}", f64::from_le_bytes(v.try_into().ok()?)),
                (0x6E, 8) => {
                    let hi = i32::from_le_bytes(v[..4].try_into().ok()?) as i64;
                    let lo = u32::from_le_bytes(v[4..].try_into().ok()?) as i64;
                    money_text((hi << 32) | lo)
                }
                (0x6E, 4) => money_text(i32::from_le_bytes(v.try_into().ok()?) as i64),
                _ => "NULL".into(),
            }
        }
        // Fixed-length types (0x1F: a NULL without a type).
        0x1F => "NULL".into(),
        0x30 => r.u8()?.to_string(),
        0x3B => f32::from_bits(r.u32()?).to_string(),
        0x3C => {
            let hi = r.u32()? as i32 as i64;
            let lo = r.u32()? as i64;
            money_text((hi << 32) | lo)
        }
        0x7A => money_text(r.u32()? as i32 as i64),
        0x34 => (r.u16()? as i16).to_string(),
        0x38 => (r.u32()? as i32).to_string(),
        0x7F => (r.u64()? as i64).to_string(),
        0x32 => {
            if r.u8()? != 0 {
                "1".into()
            } else {
                "0".into()
            }
        }
        0x3E => format!("{:?}", f64::from_bits(r.u64()?)),
        // DECIMALN / NUMERICN: max len, precision, scale; value len, sign, magnitude.
        0x6A | 0x6C => {
            let _max = r.u8()?;
            let _p = r.u8()?;
            let scale = r.u8()?;
            let n = r.u8()? as usize;
            if n == 0 {
                "NULL".into()
            } else {
                // Sign byte plus at most 16 magnitude bytes.
                if n > 17 {
                    return None;
                }
                let v = r.take(n)?;
                let mut mag = [0u8; 16];
                mag[..n - 1].copy_from_slice(&v[1..]);
                let m = u128::from_le_bytes(mag).to_string();
                let s = scale as usize;
                let padded = format!("{m:0>width$}", width = s + 1);
                let (i, f) = padded.split_at(padded.len() - s);
                format!("{}{i}{}{f}", if v[0] == 0 { "-" } else { "" }, if s > 0 { "." } else { "" })
            }
        }
        // NVARCHAR / NCHAR / VARCHAR / CHAR / NTEXT (max len 2 bytes + collation).
        0xE7 | 0xEF | 0xA7 | 0xAF => {
            let max = r.u16()?;
            r.take(5)?;
            let unicode = matches!(ty, 0xE7 | 0xEF);
            let data = if max == 0xFFFF {
                r.plp()?
            } else {
                let n = r.u16()?;
                if n == 0xFFFF { None } else { Some(r.take(n as usize)?.to_vec()) }
            };
            match data {
                None => "NULL".into(),
                Some(d) => nstring(&if unicode { from_utf16(&d) } else { String::from_utf8_lossy(&d).into_owned() }),
            }
        }
        // VARBINARY / BINARY.
        0xA5 | 0xAD => {
            let max = r.u16()?;
            let data = if max == 0xFFFF {
                r.plp()?
            } else {
                let n = r.u16()?;
                if n == 0xFFFF { None } else { Some(r.take(n as usize)?.to_vec()) }
            };
            match data {
                None => "NULL".into(),
                Some(d) => format!("'\\x{}'", d.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            }
        }
        // GUID.
        0x24 => {
            let _max = r.u8()?;
            let n = r.u8()? as usize;
            if n == 0 {
                "NULL".into()
            } else {
                let b = r.take(16)?;
                let bytes = [b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]];
                format!("'{}'", uuid::Uuid::from_bytes(bytes).to_string().to_uppercase())
            }
        }
        // DATE.
        0x28 => {
            let n = r.u8()? as usize;
            if n == 0 {
                "NULL".into()
            } else {
                let b = r.take(3)?;
                let days = u32::from_le_bytes([b[0], b[1], b[2], 0]) as i64;
                let d = chrono::NaiveDate::from_ymd_opt(1, 1, 1)? + chrono::Duration::days(days);
                format!("'{d}'")
            }
        }
        // TIME / DATETIME2 / DATETIMEOFFSET (scale byte).
        0x29..=0x2B => {
            let scale = r.u8()? as u32;
            let n = r.u8()? as usize;
            if n == 0 {
                "NULL".into()
            } else {
                let b = r.take(n)?;
                let tlen = match scale {
                    0..=2 => 3,
                    3..=4 => 4,
                    _ => 5,
                };
                let needed = match ty {
                    0x29 => tlen,
                    0x2A => tlen + 3,
                    _ => tlen + 5,
                };
                if b.len() < needed || scale > 7 {
                    return None;
                }
                let mut t = [0u8; 8];
                t[..tlen].copy_from_slice(&b[..tlen]);
                let units = u64::from_le_bytes(t) * 10u64.pow(7 - scale.min(7));
                let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                    (units / 10_000_000) as u32,
                    ((units % 10_000_000) * 100) as u32,
                )?;
                match ty {
                    0x29 => format!("'{}'", time.format("%H:%M:%S%.f")),
                    _ => {
                        let db = &b[tlen..tlen + 3];
                        let days = u32::from_le_bytes([db[0], db[1], db[2], 0]) as i64;
                        let date = chrono::NaiveDate::from_ymd_opt(1, 1, 1)? + chrono::Duration::days(days);
                        let dt = date.and_time(time);
                        if ty == 0x2B {
                            let off = i16::from_le_bytes([b[tlen + 3], b[tlen + 4]]) as i32;
                            let fixed = chrono::FixedOffset::east_opt(off * 60)?;
                            format!(
                                "'{}'",
                                (dt.and_utc() + chrono::Duration::minutes(off as i64))
                                    .naive_utc()
                                    .and_local_timezone(fixed)
                                    .single()?
                                    .format("%Y-%m-%d %H:%M:%S%.f%:z")
                            )
                        } else {
                            format!("'{}'", dt.format("%Y-%m-%d %H:%M:%S%.f"))
                        }
                    }
                }
            }
        }
        // DATETIMEN / DATETIME.
        0x6F | 0x3D => {
            let n = if ty == 0x6F {
                let _max = r.u8()?;
                r.u8()? as usize
            } else {
                8
            };
            match n {
                0 => "NULL".into(),
                8 => {
                    let days = r.u32()? as i32 as i64;
                    let ticks = r.u32()? as i64; // 1/300 s
                    let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)?.and_hms_opt(0, 0, 0)?;
                    let dt = base + chrono::Duration::days(days) + chrono::Duration::milliseconds(ticks * 10 / 3);
                    format!("'{}'", dt.format("%Y-%m-%d %H:%M:%S%.3f"))
                }
                _ => {
                    let days = r.u16()? as i64;
                    let mins = r.u16()? as i64;
                    let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)?.and_hms_opt(0, 0, 0)?;
                    format!("'{}'", (base + chrono::Duration::days(days) + chrono::Duration::minutes(mins)).format("%Y-%m-%d %H:%M:%S"))
                }
            }
        }
        _ => return None,
    };
    let _ = status;
    Some(Param { name, literal })
}

fn unquote_nstring(lit: &str) -> String {
    let t = lit.strip_prefix('N').unwrap_or(lit);
    t.trim_matches('\'').replace("''", "'")
}

// ------------------------------------------------------------------ connection

struct Conn {
    engine: Engine,
    session: Option<Session>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    id: u64,
    prepared: HashMap<i32, (String, Vec<String>)>,
    next_handle: i32,
    database: String,
    tx_descriptor: u64,
}

async fn read_message<S: AsyncRead + Unpin>(s: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let mut payload = Vec::new();
    loop {
        let mut h = [0u8; 8];
        s.read_exact(&mut h).await?;
        let len = u16::from_be_bytes([h[2], h[3]]) as usize;
        if len < 8 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad TDS packet"));
        }
        let mut body = vec![0u8; len - 8];
        s.read_exact(&mut body).await?;
        payload.extend_from_slice(&body);
        if payload.len() > 64 * 1024 * 1024 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "TDS message too large"));
        }
        if h[1] & 0x01 != 0 {
            return Ok((h[0], payload));
        }
    }
}

async fn send<S: AsyncWrite + Unpin>(s: &mut S, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let chunks: Vec<&[u8]> = if payload.is_empty() { vec![&[]] } else { payload.chunks(PACKET_SIZE - 8).collect() };
    let mut buf = Vec::with_capacity(payload.len() + chunks.len() * 8);
    for (i, c) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        buf.push(kind);
        buf.push(if last { 0x01 } else { 0x00 });
        buf.extend_from_slice(&((c.len() + 8) as u16).to_be_bytes());
        buf.extend_from_slice(&[0, 0, ((i + 1) % 256) as u8, 0]);
        buf.extend_from_slice(c);
    }
    s.write_all(&buf).await?;
    s.flush().await
}

/// Skip ALL_HEADERS at the start of batch, RPC and TM payloads.
fn skip_headers(p: &[u8]) -> &[u8] {
    if p.len() >= 4 {
        let total = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
        if total >= 4 && total <= p.len() && total < 4096 {
            return &p[total..];
        }
    }
    p
}

impl Conn {
    /// Run a batch of T-SQL; appends tokens. `proc_done` ends with DONEINPROC tokens (inside RPC).
    async fn run_sql(&mut self, out: &mut Out, sql: &str, in_proc: bool) -> bool {
        let started = Instant::now();
        match self.faults.before(sql).await {
            Decision::Proceed => {}
            Decision::Fail { code, message: msg } => {
                self.traffic.fault(self.id, sql, &msg);
                let number = code.as_deref().and_then(|c| c.parse().ok()).unwrap_or(50000);
                message(out, 0xAA, number, 16, &msg);
                done(out, if in_proc { 0xFF } else { 0xFD }, DONE_ERROR, 0);
                return false;
            }
            Decision::Drop => {
                self.traffic.fault(self.id, sql, "connection dropped");
                return true;
            }
        }
        let mut session = self.session.take().expect("session");
        let text = sql.to_owned();
        let (session, batch) = tokio::task::spawn_blocking(move || {
            let r = session.execute(&text);
            (session, r)
        })
        .await
        .expect("engine task");
        let was_tx = self.tx_descriptor != 0;
        let now_tx = session.in_transaction();
        self.session = Some(session);
        let done_token = if in_proc { 0xFF } else { 0xFD };
        let mut rows_total = 0;
        for o in &batch.outcomes {
            if let Some(m) = &o.message {
                message(out, 0xAB, 0, 0, m);
            }
            match &o.result {
                Some(rs) => {
                    let wires: Vec<Wire> = rs.columns.iter().enumerate().map(|(i, c)| wire_for(c, &rs.rows, i)).collect();
                    col_metadata(out, &rs.columns, &wires);
                    for r in &rs.rows {
                        row(out, r, &wires);
                    }
                    rows_total += rs.rows.len() as u64;
                    done_cmd(out, done_token, DONE_MORE | DONE_COUNT, cur_cmd(&o.tag).max(0xC1), rs.rows.len() as u64);
                }
                None => {
                    rows_total += o.affected;
                    let counted = matches!(o.tag.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE");
                    done_cmd(out, done_token, DONE_MORE | if counted { DONE_COUNT } else { 0 }, cur_cmd(&o.tag), o.affected);
                }
            }
        }
        // Transaction state changes are reported as ENVCHANGE, as SQL Server does.
        if !was_tx && now_tx {
            self.tx_descriptor = 1 + (self.id << 16) + rand_u16() as u64;
            env_change_bytes(out, 8, &self.tx_descriptor.to_le_bytes(), &[]);
        } else if was_tx && !now_tx {
            let old = self.tx_descriptor.to_le_bytes();
            env_change_bytes(out, if batch.error.is_some() { 10 } else { 9 }, &[], &old);
            self.tx_descriptor = 0;
        }
        self.traffic.query(self.id, sql, started, rows_total, batch.error.as_ref().map(|e| e.to_string()));
        if let Some(e) = &batch.error {
            message(out, 0xAA, error_number(e), 16, &e.message);
            done(out, done_token, DONE_ERROR, 0);
        } else if batch.outcomes.is_empty() {
            done(out, done_token, 0, 0);
        }
        false
    }

    /// Handle an RPC request; returns true when the connection should drop.
    async fn rpc(&mut self, out: &mut Out, payload: &[u8]) -> bool {
        let p = skip_headers(payload);
        let mut r = Reader { b: p, p: 0 };
        let Some(name_len) = r.u16() else { return true };
        let proc_name = if name_len == 0xFFFF {
            match r.u16().unwrap_or(0) {
                10 => "sp_executesql".to_owned(),
                11 => "sp_prepare".to_owned(),
                12 => "sp_execute".to_owned(),
                13 => "sp_prepexec".to_owned(),
                15 => "sp_unprepare".to_owned(),
                other => format!("proc#{other}"),
            }
        } else {
            match r.take(name_len as usize * 2) {
                Some(b) => from_utf16(b).to_ascii_lowercase(),
                None => return true,
            }
        };
        let _flags = r.u16();
        let mut params = Vec::new();
        while r.p < p.len() {
            let at = r.p;
            match read_param(&mut r) {
                Some(param) => params.push(param),
                None => {
                    // Name (B_VARCHAR) and status precede the type byte.
                    let ty = p.get(at).and_then(|n| p.get(at + 1 + *n as usize * 2 + 1)).copied().unwrap_or(0);
                    message(
                        out,
                        0xAA,
                        8016,
                        16,
                        &format!(
                            "The incoming RPC request has parameter {} of TDS type 0x{ty:02X}, which the TwinBase mock does not support.",
                            params.len() + 1
                        ),
                    );
                    done(out, 0xFE, DONE_ERROR, 0);
                    return false;
                }
            }
        }
        let bind_values = |names: &[String], values: &[Param]| -> HashMap<String, String> {
            let mut map = HashMap::new();
            for (i, v) in values.iter().enumerate() {
                let key = if v.name.is_empty() { names.get(i).cloned().unwrap_or_default() } else { v.name.to_ascii_lowercase() };
                map.insert(key, v.literal.clone());
            }
            map
        };
        match proc_name.trim_start_matches("sys.").trim_start_matches("dbo.") {
            "sp_executesql" => {
                let stmt = params.first().map(|p| unquote_nstring(&p.literal)).unwrap_or_default();
                let names = params.get(1).map(|p| declared_names(&unquote_nstring(&p.literal))).unwrap_or_default();
                let values = bind_values(&names, params.get(2..).unwrap_or(&[]));
                let sql = bind(&stmt, |n| values.get(n).cloned());
                if self.run_sql(out, &sql, true).await {
                    return true;
                }
            }
            "sp_prepare" | "sp_prepexec" => {
                // @handle OUTPUT, @params, @stmt [, values…]
                let decl = params.get(1).map(|p| unquote_nstring(&p.literal)).unwrap_or_default();
                let stmt = params.get(2).map(|p| unquote_nstring(&p.literal)).unwrap_or_default();
                let names = declared_names(&decl);
                let handle = self.next_handle;
                self.next_handle += 1;
                self.prepared.insert(handle, (stmt.clone(), names.clone()));
                if proc_name.ends_with("prepexec") {
                    let values = bind_values(&names, params.get(3..).unwrap_or(&[]));
                    if self.run_sql(out, &bind(&stmt, |n| values.get(n).cloned()), true).await {
                        return true;
                    }
                }
                return_value(out, params.first().map(|p| p.name.as_str()).unwrap_or("@handle"), handle);
            }
            "sp_execute" => {
                let handle = params.first().and_then(|p| p.literal.parse::<i32>().ok()).unwrap_or(-1);
                match self.prepared.get(&handle).cloned() {
                    Some((stmt, names)) => {
                        let values = bind_values(&names, params.get(1..).unwrap_or(&[]));
                        if self.run_sql(out, &bind(&stmt, |n| values.get(n).cloned()), true).await {
                            return true;
                        }
                    }
                    None => {
                        message(out, 0xAA, 8179, 16, &format!("Could not find prepared statement with handle {handle}."));
                        done(out, 0xFF, DONE_ERROR, 0);
                    }
                }
            }
            "sp_unprepare" => {
                if let Some(h) = params.first().and_then(|p| p.literal.parse::<i32>().ok()) {
                    self.prepared.remove(&h);
                }
            }
            other => {
                // Other procedures: run `EXEC name` as SQL so the engine reports what it supports.
                let args: Vec<String> = params.iter().map(|p| p.literal.clone()).collect();
                let sql = format!("EXEC {other} {}", args.join(", "));
                if self.run_sql(out, &sql, true).await {
                    return true;
                }
            }
        }
        out.u8(0x79).u32(0);
        done(out, 0xFE, 0, 0);
        false
    }
}

fn return_value(out: &mut Out, name: &str, value: i32) {
    out.u8(0xAC).u16(0).b_varchar(name).u8(0x01).u32(0).u16(0).u8(0x26).u8(4).u8(4).bytes(&value.to_le_bytes());
}

fn rand_u16() -> u16 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) & 0xFFFF) as u16
}

/// PRELOGIN reply: version 16.0 and the encryption the server agreed to.
fn prelogin_reply(encryption: u8) -> Vec<u8> {
    let options = [(0x00u8, 6u16), (0x01, 1), (0x02, 1), (0x03, 0), (0x04, 1)];
    let mut header = Vec::new();
    let mut data = Vec::new();
    let mut offset = (options.len() * 5 + 1) as u16;
    for (t, len) in options {
        header.push(t);
        header.extend_from_slice(&offset.to_be_bytes());
        header.extend_from_slice(&len.to_be_bytes());
        offset += len;
        match t {
            0x00 => data.extend_from_slice(&[16, 0, 0x03, 0xE8, 0, 0]),
            0x01 => data.push(encryption),
            0x02 => data.push(0),
            0x04 => data.push(0),
            _ => {}
        }
    }
    header.push(0xFF);
    header.extend_from_slice(&data);
    header
}

/// The encryption the client asked for in PRELOGIN.
fn requested_encryption(p: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i + 5 <= p.len() && p[i] != 0xFF {
        if p[i] == 0x01 {
            let off = u16::from_be_bytes([p[i + 1], p[i + 2]]) as usize;
            return p.get(off).copied();
        }
        i += 5;
    }
    None
}

struct Login {
    user: String,
    password: String,
    database: String,
}

fn parse_login(p: &[u8]) -> Option<Login> {
    let field = |idx: usize| -> Option<(usize, usize)> {
        let at = 36 + idx * 4;
        Some((u16::from_le_bytes([*p.get(at)?, *p.get(at + 1)?]) as usize, u16::from_le_bytes([*p.get(at + 2)?, *p.get(at + 3)?]) as usize))
    };
    let string = |idx: usize| -> Option<String> {
        let (off, len) = field(idx)?;
        Some(from_utf16(p.get(off..off + len * 2)?))
    };
    let user = string(1)?;
    let (off, len) = field(2)?;
    // Passwords are obfuscated: XOR 0xA5, then swap nibbles.
    let pw: Vec<u8> = p.get(off..off + len * 2)?.iter().map(|b| b ^ 0xA5).map(|b| b.rotate_left(4)).collect();
    let database = string(8).unwrap_or_default();
    Some(Login { user, password: from_utf16(&pw), database })
}

fn login_reply(database: &str) -> Vec<u8> {
    let mut out = Out::new();
    env_change(&mut out, 1, database, "master");
    message(&mut out, 0xAB, 5701, 0, &format!("Changed database context to '{database}'."));
    env_change_bytes(&mut out, 7, &COLLATION, &[]);
    env_change(&mut out, 2, "us_english", "");
    message(&mut out, 0xAB, 5703, 0, "Changed language setting to us_english.");
    let mut ack = Out::new();
    ack.u8(1).bytes(&[0x74, 0x00, 0x00, 0x04]).b_varchar("Microsoft SQL Server").bytes(&[16, 0, 0x03, 0xE8]);
    out.u8(0xAD).u16(ack.0.len() as u16).bytes(&ack.0);
    env_change(&mut out, 4, &PACKET_SIZE.to_string(), &PACKET_SIZE.to_string());
    done(&mut out, 0xFD, 0, 0);
    out.0
}

pub async fn serve(listener: TcpListener, engine: Engine, auth: AuthMode, traffic: TrafficLog, faults: Faults, cancel: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            a = listener.accept() => a,
        };
        let Ok((socket, peer)) = accepted else { continue };
        let id = traffic.new_connection_id();
        if faults.refuse_connections() {
            traffic.fault(id, "(new connection)", "connection refused");
            continue;
        }
        let _ = socket.set_nodelay(true);
        let conn = Conn {
            engine: engine.clone(),
            session: None,
            auth: auth.clone(),
            traffic: traffic.clone(),
            faults: faults.clone(),
            id,
            prepared: HashMap::new(),
            next_handle: 1,
            database: String::new(),
            tx_descriptor: 0,
        };
        let cancel = cancel.clone();
        let traffic = traffic.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                r = handle(conn, socket, peer) => {
                    if let Err(e) = r {
                        tracing::debug!("TDS connection ended: {e}");
                    }
                }
            }
            traffic.record(id, TrafficKind::Disconnect);
        });
    }
}

// ------------------------------------------------------------------ TLS

const ENCRYPT_OFF: u8 = 0x00;
const ENCRYPT_ON: u8 = 0x01;
const ENCRYPT_NOT_SUP: u8 = 0x02;

/// A self-signed certificate for the process, made on first use.
fn tls_acceptor() -> std::io::Result<tokio_rustls::TlsAcceptor> {
    static CONFIG: OnceLock<Result<Arc<rustls::ServerConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).map_err(|e| e.to_string())?;
        let cert = key.cert.der().clone();
        let private = rustls::pki_types::PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into());
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(vec![cert], private)
            .map(|mut c| {
                // Tickets would be written after the client has stopped unwrapping PRELOGIN packets.
                c.send_tls13_tickets = 0;
                Arc::new(c)
            })
            .map_err(|e| e.to_string())
    });
    match config {
        Ok(c) => Ok(tokio_rustls::TlsAcceptor::from(c.clone())),
        Err(e) => Err(std::io::Error::other(format!("TLS setup: {e}"))),
    }
}

/// During the TLS handshake, TDS carries TLS records inside PRELOGIN packets; afterwards TLS runs
/// directly on the socket. The client may send its first encrypted request before the server has
/// left the handshake, so data that is not a PRELOGIN packet ends the wrapping too.
struct PreloginStream<S> {
    inner: S,
    handshake: bool,
    /// Bytes already read that belong to the unwrapped stream.
    raw: Vec<u8>,
    /// Header bytes read so far and payload bytes left in the current packet (handshake only).
    header: [u8; 8],
    header_read: usize,
    payload_left: usize,
    /// Handshake bytes waiting to be sent as one packet on flush.
    out: Vec<u8>,
    sending: Option<(Vec<u8>, usize)>,
}

impl<S> PreloginStream<S> {
    fn new(inner: S) -> Self {
        PreloginStream {
            inner,
            handshake: true,
            raw: Vec::new(),
            header: [0; 8],
            header_read: 0,
            payload_left: 0,
            out: Vec::new(),
            sending: None,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PreloginStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if !this.raw.is_empty() {
            let n = this.raw.len().min(buf.remaining());
            buf.put_slice(&this.raw[..n]);
            this.raw.drain(..n);
            return Poll::Ready(Ok(()));
        }
        if !this.handshake {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        while this.payload_left == 0 {
            let mut hb = ReadBuf::new(&mut this.header[this.header_read..]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut hb))?;
            let n = hb.filled().len();
            if n == 0 {
                return Poll::Ready(Ok(()));
            }
            this.header_read += n;
            if this.header[0] != PT_PRELOGIN {
                this.handshake = false;
                this.raw = this.header[..this.header_read].to_vec();
                this.header_read = 0;
                return Pin::new(this).poll_read(cx, buf);
            }
            if this.header_read == 8 {
                this.header_read = 0;
                this.payload_left = (u16::from_be_bytes([this.header[2], this.header[3]]) as usize).saturating_sub(8);
            }
        }
        let want = this.payload_left.min(buf.remaining());
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(want));
        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut limited))?;
        let n = limited.filled().len();
        buf.advance(n);
        this.payload_left -= n;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PreloginStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        if self.handshake {
            self.out.extend_from_slice(data);
            return Poll::Ready(Ok(data.len()));
        }
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.sending.is_none() && !this.out.is_empty() {
            let payload = std::mem::take(&mut this.out);
            let mut packet = Vec::with_capacity(payload.len() + 8);
            for (i, chunk) in payload.chunks(PACKET_SIZE - 8).enumerate() {
                let last = (i + 1) * (PACKET_SIZE - 8) >= payload.len();
                packet.extend_from_slice(&[PT_PRELOGIN, last as u8]);
                packet.extend_from_slice(&((chunk.len() + 8) as u16).to_be_bytes());
                packet.extend_from_slice(&[0, 0, (i + 1) as u8, 0]);
                packet.extend_from_slice(chunk);
            }
            this.sending = Some((packet, 0));
        }
        if let Some((packet, at)) = &mut this.sending {
            while *at < packet.len() {
                *at += ready!(Pin::new(&mut this.inner).poll_write(cx, &packet[*at..]))?;
            }
            this.sending = None;
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn handle(c: Conn, mut s: TcpStream, peer: std::net::SocketAddr) -> std::io::Result<()> {
    let (t, p) = read_message(&mut s).await?;
    if t != PT_PRELOGIN {
        return Ok(());
    }
    // As SQL Server does: no TLS for clients without it, TLS for the login only when the client
    // turned encryption off, and TLS throughout otherwise.
    let encryption = match requested_encryption(&p).map(|e| e & 0x0F) {
        Some(ENCRYPT_NOT_SUP) => ENCRYPT_NOT_SUP,
        Some(ENCRYPT_OFF) | None => ENCRYPT_OFF,
        _ => ENCRYPT_ON,
    };
    send(&mut s, PT_REPLY, &prelogin_reply(encryption)).await?;
    if encryption == ENCRYPT_NOT_SUP {
        let (t, p) = read_message(&mut s).await?;
        return serve_session(c, s, t, p, peer).await;
    }
    let mut tls = tls_acceptor()?.accept(PreloginStream::new(s)).await?;
    tls.get_mut().0.handshake = false;
    let (t, p) = read_message(&mut tls).await?;
    if encryption == ENCRYPT_OFF {
        let (wrapper, _) = tls.into_inner();
        serve_session(c, wrapper.inner, t, p, peer).await
    } else {
        serve_session(c, tls, t, p, peer).await
    }
}

/// LOGIN7 (already read as `t`/`p`) and the request loop.
async fn serve_session<S: AsyncRead + AsyncWrite + Unpin>(
    mut c: Conn,
    mut s: S,
    t: u8,
    p: Vec<u8>,
    peer: std::net::SocketAddr,
) -> std::io::Result<()> {
    if t != PT_LOGIN7 {
        return Ok(());
    }
    let Some(login) = parse_login(&p) else { return Ok(()) };
    c.traffic.record(
        c.id,
        TrafficKind::Connect { peer: peer.to_string(), user: Some(login.user.clone()), database: Some(login.database.clone()) },
    );
    if let AuthMode::Password { user, password } = &c.auth
        && (&login.user != user || &login.password != password)
    {
        let mut out = Out::new();
        message(&mut out, 0xAA, 18456, 14, &format!("Login failed for user '{}'.", login.user));
        done(&mut out, 0xFD, DONE_ERROR, 0);
        send(&mut s, PT_REPLY, &out.0).await?;
        return Ok(());
    }
    let engine = c.engine.clone();
    let user = login.user.clone();
    // SQL Server's default database is master; the mock serves one database under any name.
    let database = if login.database.is_empty() { "master".to_owned() } else { login.database.clone() };
    let db = database.clone();
    let session = tokio::task::spawn_blocking(move || engine.session(Some(&db), &user)).await.map_err(std::io::Error::other)?;
    let session = match session {
        Ok(s) => s,
        Err(e) => {
            let mut out = Out::new();
            message(&mut out, 0xAA, 4060, 11, &e.to_string());
            done(&mut out, 0xFD, DONE_ERROR, 0);
            send(&mut s, PT_REPLY, &out.0).await?;
            return Ok(());
        }
    };
    c.session = Some(session);
    c.database = database;
    send(&mut s, PT_REPLY, &login_reply(&c.database)).await?;
    loop {
        let (t, p) = read_message(&mut s).await?;
        let mut out = Out::new();
        let drop_conn = match t {
            PT_BATCH => {
                let sql = from_utf16(skip_headers(&p));
                c.run_sql(&mut out, &sql, false).await
            }
            PT_RPC => c.rpc(&mut out, &p).await,
            PT_TM => {
                let body = skip_headers(&p);
                let kind = u16::from_le_bytes([*body.first().unwrap_or(&0), *body.get(1).unwrap_or(&0)]);
                let sql = match kind {
                    5 => "BEGIN TRANSACTION",
                    7 => "COMMIT TRANSACTION",
                    8 => "ROLLBACK TRANSACTION",
                    _ => "",
                };
                if sql.is_empty() {
                    done(&mut out, 0xFD, 0, 0);
                    false
                } else {
                    let stop = c.run_sql(&mut out, sql, false).await;
                    // run_sql ends with DONE tokens per statement; TM replies need just one.
                    stop
                }
            }
            PT_ATTENTION => {
                done(&mut out, 0xFD, DONE_ATTN, 0);
                false
            }
            _ => true,
        };
        if drop_conn {
            return Ok(());
        }
        // The last DONE of a reply must not say "more".
        clear_last_done_more(&mut out.0);
        send(&mut s, PT_REPLY, &out.0).await?;
    }
}

/// Clear DONE_MORE on the final DONE/DONEPROC/DONEINPROC token (13 bytes at the end).
fn clear_last_done_more(b: &mut [u8]) {
    if b.len() >= 13 {
        let at = b.len() - 13;
        if matches!(b[at], 0xFD..=0xFF) {
            b[at + 1] &= !(DONE_MORE as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_parameters_do_not_panic() {
        // name "" (0 chars), status 0, then a type and a hostile value.
        let decimal = [0u8, 0, 0x6A, 17, 38, 2, 200, 1, 2, 3];
        assert!(read_param(&mut Reader { b: &decimal, p: 0 }).is_none());
        let time = [0u8, 0, 0x2A, 7, 2, 1, 2];
        assert!(read_param(&mut Reader { b: &time, p: 0 }).is_none());
        let ok = [0u8, 0, 0x26, 4, 4, 42, 0, 0, 0];
        assert_eq!(read_param(&mut Reader { b: &ok, p: 0 }).unwrap().literal, "42");
    }

    #[test]
    fn login_passwords_are_deobfuscated() {
        // "a" obfuscated: 0x61 -> swap -> 0x16 -> ^0xA5 -> 0xB3; high byte 0x00 -> 0x00 -> 0xA5.
        let mut p = vec![0u8; 94];
        let user = utf16("sa");
        let pw = [0xB3u8, 0xA5];
        let user_off = 94u16;
        let pw_off = user_off + user.len() as u16;
        p[40..42].copy_from_slice(&user_off.to_le_bytes());
        p[42..44].copy_from_slice(&2u16.to_le_bytes());
        p[44..46].copy_from_slice(&pw_off.to_le_bytes());
        p[46..48].copy_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&user);
        p.extend_from_slice(&pw);
        let l = parse_login(&p).unwrap();
        assert_eq!((l.user.as_str(), l.password.as_str()), ("sa", "a"));
    }
}
