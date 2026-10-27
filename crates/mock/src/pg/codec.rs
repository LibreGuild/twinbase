//! PostgreSQL wire encoding of TwinLite values (text and binary formats) and decoding of
//! bound parameters.

use bytes::{BufMut, BytesMut};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use pgwire::api::Type;
use twinbase_engine::value::{self, Interval, parse_any_timestamp, parse_time_only};
use twinbase_engine::{Dialect, Param, SqlType, Value};

fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
}

pub fn pg_type(ty: &SqlType) -> Type {
    if let SqlType::Vector { family, .. } = ty {
        return Type::new(family.name().into(), family.oid(), postgres_types::Kind::Simple, "public".into());
    }
    Type::from_oid(ty.pg_oid()).unwrap_or(Type::TEXT)
}

/// Append one field (length-prefixed) to a DataRow buffer.
pub fn encode_cell(buf: &mut BytesMut, v: &Value, ty: &SqlType, binary: bool) {
    if matches!(v, Value::Null) {
        buf.put_i32(-1);
        return;
    }
    let bytes = if binary { encode_binary(v, ty) } else { None };
    match bytes {
        Some(b) => {
            buf.put_i32(b.len() as i32);
            buf.put_slice(&b);
        }
        None => {
            let text = value::format_text(v, ty, Dialect::Postgres).unwrap_or_default();
            buf.put_i32(text.len() as i32);
            buf.put_slice(text.as_bytes());
        }
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Real(f) => Some(f.round() as i64),
        Value::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Real(f) => Some(*f),
        Value::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn text_of(v: &Value, ty: &SqlType) -> String {
    value::format_text(v, ty, Dialect::Postgres).unwrap_or_default()
}

pub fn encode_binary(v: &Value, ty: &SqlType) -> Option<Vec<u8>> {
    Some(match ty {
        SqlType::Bool => vec![(as_i64(v).unwrap_or(0) != 0) as u8],
        SqlType::Int1 | SqlType::Int2 => (as_i64(v)? as i16).to_be_bytes().to_vec(),
        SqlType::Int4 | SqlType::Year => (as_i64(v)? as i32).to_be_bytes().to_vec(),
        SqlType::Int8 => as_i64(v)?.to_be_bytes().to_vec(),
        SqlType::Float4 => (as_f64(v)? as f32).to_be_bytes().to_vec(),
        SqlType::Float8 => as_f64(v)?.to_be_bytes().to_vec(),
        SqlType::Numeric { .. } => encode_numeric(&text_of(v, ty))?,
        SqlType::Bytea => match v {
            Value::Blob(b) => b.clone(),
            Value::Text(t) => t.as_bytes().to_vec(),
            other => text_of(other, ty).into_bytes(),
        },
        SqlType::Jsonb => {
            let mut out = vec![1u8];
            out.extend(text_of(v, ty).into_bytes());
            out
        }
        SqlType::Date => {
            let d = parse_any_timestamp(&text_of(v, ty))?.date();
            ((d - epoch().date()).num_days() as i32).to_be_bytes().to_vec()
        }
        SqlType::Time => {
            let t = parse_time_only(&text_of(v, ty))?;
            let micros = t.num_seconds_from_midnight() as i64 * 1_000_000 + (t.nanosecond() / 1000) as i64;
            micros.to_be_bytes().to_vec()
        }
        SqlType::Timestamp | SqlType::TimestampTz => {
            let raw = match v {
                Value::Text(t) => t.clone(),
                other => text_of(other, ty),
            };
            let ts = parse_any_timestamp(&raw)?;
            (ts - epoch()).num_microseconds()?.to_be_bytes().to_vec()
        }
        SqlType::Interval => {
            let iv = Interval::parse(&text_of(v, ty))?;
            let mut out = Vec::with_capacity(16);
            out.extend(iv.micros.to_be_bytes());
            out.extend(iv.days.to_be_bytes());
            out.extend(iv.months.to_be_bytes());
            out
        }
        SqlType::Uuid => uuid::Uuid::parse_str(&text_of(v, ty)).ok()?.as_bytes().to_vec(),
        SqlType::Array { element } => {
            let items: Vec<serde_json::Value> = match v {
                Value::Text(t) => serde_json::from_str(t).ok()?,
                _ => return None,
            };
            encode_array(&items, element)?
        }
        SqlType::Vector { family, .. } => {
            let text = text_of(v, ty);
            twinbase_engine::vector::Vector::parse(&text, *family).ok()?.to_binary(*family)
        }
        SqlType::Other { name } if matches!(name.as_str(), "oid" | "regclass" | "regtype" | "regproc" | "xid") => {
            (as_i64(v)? as u32).to_be_bytes().to_vec()
        }
        _ => text_of(v, ty).into_bytes(),
    })
}

fn encode_array(items: &[serde_json::Value], element: &SqlType) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let has_null = items.iter().any(|i| i.is_null());
    out.extend((if items.is_empty() { 0i32 } else { 1 }).to_be_bytes());
    out.extend((has_null as i32).to_be_bytes());
    out.extend((element.pg_oid() as i32).to_be_bytes());
    if !items.is_empty() {
        out.extend((items.len() as i32).to_be_bytes());
        out.extend(1i32.to_be_bytes());
    }
    for item in items {
        let cell = match item {
            serde_json::Value::Null => {
                out.extend((-1i32).to_be_bytes());
                continue;
            }
            serde_json::Value::Number(n) => n.as_i64().map(Value::Integer).unwrap_or_else(|| Value::Real(n.as_f64().unwrap_or(0.0))),
            serde_json::Value::String(s) => Value::Text(s.clone()),
            serde_json::Value::Bool(b) => Value::Integer(*b as i64),
            other => Value::Text(other.to_string()),
        };
        let b = encode_binary(&cell, element)?;
        out.extend((b.len() as i32).to_be_bytes());
        out.extend(b);
    }
    Some(out)
}

/// PostgreSQL binary NUMERIC from a decimal string.
pub fn encode_numeric(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    let mut out = Vec::new();
    if s.eq_ignore_ascii_case("nan") {
        out.extend(0i16.to_be_bytes());
        out.extend(0i16.to_be_bytes());
        out.extend(0xC000u16.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        return Some(out);
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    // Expand exponent notation through f64 formatting.
    let body = if body.contains(['e', 'E']) { format!("{}", body.parse::<f64>().ok()?) } else { body.to_owned() };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((&body, ""));
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let dscale = frac_part.len() as u16;
    let int_part = int_part.trim_start_matches('0');
    // Integer groups (from the right) and fraction groups (from the left).
    let pad = (4 - int_part.len() % 4) % 4;
    let int_padded = format!("{}{int_part}", "0".repeat(pad));
    let mut digits: Vec<i16> = int_padded.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap().parse().unwrap()).collect();
    let mut weight = digits.len() as i16 - 1;
    let frac_pad = (4 - frac_part.len() % 4) % 4;
    let frac_padded = format!("{frac_part}{}", "0".repeat(frac_pad));
    digits.extend(frac_padded.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap().parse::<i16>().unwrap()));
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        weight = 0;
    }
    out.extend((digits.len() as i16).to_be_bytes());
    out.extend(weight.to_be_bytes());
    out.extend((if neg && !digits.is_empty() { 0x4000u16 } else { 0 }).to_be_bytes());
    out.extend(dscale.to_be_bytes());
    for d in digits {
        out.extend(d.to_be_bytes());
    }
    Some(out)
}

pub fn decode_numeric(b: &[u8]) -> Option<String> {
    if b.len() < 8 {
        return None;
    }
    let rd = |i: usize| i16::from_be_bytes([b[i], b[i + 1]]);
    let ndigits = rd(0) as usize;
    let weight = rd(2) as i32;
    let sign = u16::from_be_bytes([b[4], b[5]]);
    let dscale = u16::from_be_bytes([b[6], b[7]]) as usize;
    if sign == 0xC000 {
        return Some("NaN".into());
    }
    let digits: Vec<i16> = (0..ndigits).map(|i| rd(8 + i * 2)).collect();
    let mut int_part = String::new();
    let mut frac_part = String::new();
    for (i, d) in digits.iter().enumerate() {
        let pos = weight - i as i32;
        if pos >= 0 {
            if int_part.is_empty() { int_part.push_str(&d.to_string()) } else { int_part.push_str(&format!("{d:04}")) }
        } else {
            while frac_part.len() < ((-pos - 1) * 4) as usize {
                frac_part.push_str("0000");
            }
            frac_part.push_str(&format!("{d:04}"));
        }
    }
    // Zero groups between the last digit and the decimal point.
    if weight >= 0 && !digits.is_empty() {
        let present = (0..=weight).filter(|p| (*p as usize) < digits.len()).count() as i32;
        for _ in present..=weight {
            int_part.push_str("0000");
        }
    }
    if int_part.is_empty() {
        int_part.push('0');
    }
    frac_part.truncate(dscale);
    while frac_part.len() < dscale {
        frac_part.push('0');
    }
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    s.push_str(&int_part);
    if dscale > 0 {
        s.push('.');
        s.push_str(&frac_part);
    }
    Some(s)
}

/// Decode a bound parameter.
pub fn decode_param(raw: Option<&[u8]>, binary: bool, oid: u32) -> Param {
    let Some(b) = raw else { return Param::Null };
    if !binary {
        return Param::Text(String::from_utf8_lossy(b).into_owned());
    }
    let be_i64 = |b: &[u8]| -> Option<i64> { Some(i64::from_be_bytes(b.get(..8)?.try_into().ok()?)) };
    let be_i32 = |b: &[u8]| -> Option<i32> { Some(i32::from_be_bytes(b.get(..4)?.try_into().ok()?)) };
    let decoded = match oid {
        16 => b.first().map(|v| Param::Bool(*v != 0)),
        21 => b.get(..2).map(|x| Param::Int(i16::from_be_bytes([x[0], x[1]]) as i64)),
        23 => be_i32(b).map(|v| Param::Int(v as i64)),
        26 => b.get(..4).map(|x| Param::Int(u32::from_be_bytes([x[0], x[1], x[2], x[3]]) as i64)),
        20 => be_i64(b).map(Param::Int),
        700 => b.get(..4).map(|x| Param::Float(f32::from_be_bytes([x[0], x[1], x[2], x[3]]) as f64)),
        701 => b.get(..8).map(|x| Param::Float(f64::from_be_bytes(x.try_into().unwrap()))),
        1700 => decode_numeric(b).map(Param::Text),
        17 => Some(Param::Bytes(b.to_vec())),
        3802 => Some(Param::Text(String::from_utf8_lossy(b.get(1..).unwrap_or_default()).into_owned())),
        1082 => be_i32(b).map(|d| Param::Text((epoch().date() + chrono::Duration::days(d as i64)).format("%Y-%m-%d").to_string())),
        1083 => be_i64(b).map(|m| {
            let t =
                NaiveTime::from_num_seconds_from_midnight_opt((m / 1_000_000) as u32, ((m % 1_000_000) * 1000) as u32).unwrap_or_default();
            Param::Text(t.format("%H:%M:%S%.6f").to_string())
        }),
        1114 => be_i64(b).map(|m| Param::Text((epoch() + chrono::Duration::microseconds(m)).format(value::TS_FORMAT).to_string())),
        1184 => be_i64(b).map(|m| Param::Text(format!("{}+00", (epoch() + chrono::Duration::microseconds(m)).format(value::TS_FORMAT)))),
        1186 => {
            if b.len() >= 16 {
                let micros = be_i64(b).unwrap_or(0);
                let days = be_i32(&b[8..]).unwrap_or(0);
                let months = be_i32(&b[12..]).unwrap_or(0);
                Some(Param::Text(Interval { months, days, micros }.to_pg_string()))
            } else {
                None
            }
        }
        2950 => uuid::Uuid::from_slice(b).ok().map(|u| Param::Text(u.hyphenated().to_string())),
        16390 | 16392 | 16394 => {
            use twinbase_engine::vector::{Vector, VectorKind};
            [VectorKind::Vector, VectorKind::Halfvec, VectorKind::Sparsevec]
                .into_iter()
                .find(|k| k.oid() == oid)
                .and_then(|kind| Vector::from_binary(b, kind).ok())
                .map(|v| Param::Text(v.to_text()))
        }
        1000 | 1005 | 1007 | 1016 | 1021 | 1022 | 1009 | 1015 | 2951 | 1182 | 1115 | 1185 | 1231 | 3807 | 199 => {
            decode_array(b).map(|items| Param::Text(serde_json::Value::Array(items).to_string()))
        }
        _ => Some(Param::Text(String::from_utf8_lossy(b).into_owned())),
    };
    decoded.unwrap_or_else(|| Param::Text(String::from_utf8_lossy(b).into_owned()))
}

fn decode_array(b: &[u8]) -> Option<Vec<serde_json::Value>> {
    let rd = |i: usize| -> Option<i32> { Some(i32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
    let ndim = rd(0)?;
    let elem_oid = rd(8)? as u32;
    if ndim == 0 {
        return Some(vec![]);
    }
    let len = rd(12)? as usize;
    let mut pos = 12 + ndim as usize * 8;
    // Each element takes at least its 4-byte length, so the input bounds the count.
    let mut out = Vec::with_capacity(len.min(b.len() / 4));
    for _ in 0..len {
        let l = rd(pos)?;
        pos += 4;
        if l < 0 {
            out.push(serde_json::Value::Null);
            continue;
        }
        let item = b.get(pos..pos + l as usize)?;
        pos += l as usize;
        out.push(match decode_param(Some(item), true, elem_oid) {
            Param::Null => serde_json::Value::Null,
            Param::Bool(v) => serde_json::Value::Bool(v),
            Param::Int(i) => serde_json::Value::from(i),
            Param::Float(f) => serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or_default(),
            Param::Text(s) => serde_json::Value::String(s),
            Param::Bytes(b) => serde_json::Value::String(format!("\\x{}", value::hex(&b))),
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_round_trip() {
        for s in ["0", "1", "12.50", "-12345.678", "10000", "0.0001", "99999999.99", "100000000"] {
            let enc = encode_numeric(s).unwrap();
            assert_eq!(decode_numeric(&enc).unwrap(), s.trim_start_matches('+'), "{s}");
        }
    }
}
