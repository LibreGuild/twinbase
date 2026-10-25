//! Aggregation: expression evaluation (`$add`, `$cond`, `$map`, …) and pipeline stages
//! (`$match`, `$group`, `$lookup`, `$unwind`, …).

use std::cmp::Ordering;
use std::collections::HashMap;

use bson::{Bson, Document, doc};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use indexmap::IndexMap;

use super::query::{
    MResult, MongoError, as_f64, bson_type_name, compare, is_number, key_of, lookup, matches, project, set_path, sort_docs, unset_path,
    values_equal,
};

/// Variables visible to an expression (`$$ROOT`, `$$this`, `$let` bindings…).
struct Ctx<'a> {
    current: &'a Document,
    vars: HashMap<String, Bson>,
}

/// Evaluate an expression against a document; a missing value is `Null`.
pub fn eval(expr: &Bson, doc: &Document) -> MResult<Bson> {
    let ctx = Ctx { current: doc, vars: HashMap::new() };
    Ok(ev(expr, &ctx)?.unwrap_or(Bson::Null))
}

pub fn truthy(v: &Bson) -> bool {
    match v {
        Bson::Boolean(b) => *b,
        Bson::Null | Bson::Undefined => false,
        x if is_number(x) => as_f64(x).is_some_and(|n| n != 0.0),
        _ => true,
    }
}

fn err(msg: impl Into<String>) -> MongoError {
    MongoError::new(16020, "Location16020", msg)
}

/// Value of a field path; a path through an array maps over its elements, as in MongoDB.
fn field_of(v: &Bson, path: &str) -> Option<Bson> {
    match v {
        Bson::Document(d) => doc_field(d, path),
        Bson::Array(items) => Some(Bson::Array(items.iter().filter_map(|i| field_of(i, path)).collect())),
        _ => None,
    }
}

fn doc_field(d: &Document, path: &str) -> Option<Bson> {
    match path.split_once('.') {
        None => d.get(path).cloned(),
        Some((head, rest)) => field_of(d.get(head)?, rest),
    }
}

/// `None` is "missing" (dropped from objects; `$ifNull` treats it like null).
fn ev(expr: &Bson, ctx: &Ctx) -> MResult<Option<Bson>> {
    match expr {
        Bson::String(s) if s.starts_with("$$") => {
            let (name, path) = s[2..].split_once('.').map(|(a, b)| (a, Some(b))).unwrap_or((&s[2..], None));
            let base = match name {
                "ROOT" | "CURRENT" => Bson::Document(ctx.current.clone()),
                "REMOVE" => return Ok(None),
                "NOW" => Bson::DateTime(bson::DateTime::now()),
                other => match ctx.vars.get(other) {
                    Some(v) => v.clone(),
                    None => return Err(MongoError::new(17276, "Location17276", format!("Use of undefined variable: {other}"))),
                },
            };
            Ok(match path {
                Some(p) => field_of(&base, p),
                None => Some(base),
            })
        }
        Bson::String(s) if s.starts_with('$') && s.len() > 1 => {
            // Field paths evaluate against $$CURRENT; a path through arrays maps over them.
            Ok(doc_field(ctx.current, &s[1..]))
        }
        Bson::Array(items) => {
            Ok(Some(Bson::Array(items.iter().map(|i| ev(i, ctx).map(|v| v.unwrap_or(Bson::Null))).collect::<MResult<_>>()?)))
        }
        Bson::Document(d) => {
            if let Some((op, arg)) = d.iter().next().filter(|(k, _)| k.starts_with('$')) {
                if d.len() > 1 {
                    return Err(MongoError::new(
                        15983,
                        "Location15983",
                        format!(
                            "an expression specification must contain exactly one field, the name of the expression. Found {} fields",
                            d.len()
                        ),
                    ));
                }
                return operator(op, arg, ctx);
            }
            let mut out = Document::new();
            for (k, v) in d {
                if let Some(val) = ev(v, ctx)? {
                    out.insert(k.clone(), val);
                }
            }
            Ok(Some(Bson::Document(out)))
        }
        other => Ok(Some(other.clone())),
    }
}

fn args(arg: &Bson, ctx: &Ctx) -> MResult<Vec<Bson>> {
    match arg {
        Bson::Array(items) => items.iter().map(|i| ev(i, ctx).map(|v| v.unwrap_or(Bson::Null))).collect(),
        other => Ok(vec![ev(other, ctx)?.unwrap_or(Bson::Null)]),
    }
}

fn arg_n<const N: usize>(op: &str, arg: &Bson, ctx: &Ctx) -> MResult<[Bson; N]> {
    let v = args(arg, ctx)?;
    v.try_into().map_err(|v: Vec<Bson>| err(format!("Expression {op} takes exactly {N} arguments. {} were passed in.", v.len())))
}

fn num(v: &Bson, op: &str) -> MResult<f64> {
    as_f64(v).ok_or_else(|| err(format!("{op} only supports numeric types, not {}", bson_type_name(v))))
}

fn is_nullish(v: &Bson) -> bool {
    matches!(v, Bson::Null | Bson::Undefined)
}

/// Numeric result keeping integer types when possible.
fn number(f: f64, ints: bool) -> Bson {
    if ints && f.fract() == 0.0 && f.abs() < 9.0e18 {
        let i = f as i64;
        i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i))
    } else {
        Bson::Double(f)
    }
}

fn all_ints(vals: &[Bson]) -> bool {
    vals.iter().all(|v| matches!(v, Bson::Int32(_) | Bson::Int64(_)))
}

fn as_str(v: &Bson) -> Option<String> {
    match v {
        Bson::String(s) | Bson::Symbol(s) => Some(s.clone()),
        Bson::Null | Bson::Undefined => Some(String::new()),
        x if is_number(x) => Some(x.to_string()),
        _ => None,
    }
}

fn to_datetime(v: &Bson) -> Option<chrono::DateTime<Utc>> {
    match v {
        Bson::DateTime(d) => Utc.timestamp_millis_opt(d.timestamp_millis()).single(),
        Bson::ObjectId(o) => Utc.timestamp_millis_opt(o.timestamp().timestamp_millis()).single(),
        Bson::Timestamp(t) => Utc.timestamp_opt(t.time as i64, 0).single(),
        _ => None,
    }
}

fn convert(v: &Bson, to: &str) -> MResult<Bson> {
    if is_nullish(v) {
        return Ok(Bson::Null);
    }
    let fail = || MongoError::new(241, "ConversionFailure", format!("Unsupported conversion from {} to {to}", bson_type_name(v)));
    Ok(match to {
        "string" => match v {
            Bson::String(s) => Bson::String(s.clone()),
            Bson::ObjectId(o) => Bson::String(o.to_hex()),
            Bson::Boolean(b) => Bson::String(b.to_string()),
            Bson::DateTime(d) => Bson::String(d.try_to_rfc3339_string().unwrap_or_default()),
            x if is_number(x) => Bson::String(match x {
                Bson::Double(d) if d.fract() == 0.0 => format!("{d}"),
                other => other.to_string(),
            }),
            _ => return Err(fail()),
        },
        "int" | "long" | "double" | "decimal" => {
            let f = match v {
                Bson::String(s) => s
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| MongoError::new(241, "ConversionFailure", format!("Failed to parse number '{s}' in $convert")))?,
                Bson::Boolean(b) => *b as i32 as f64,
                Bson::DateTime(d) => d.timestamp_millis() as f64,
                x => as_f64(x).ok_or_else(fail)?,
            };
            match to {
                "int" => Bson::Int32(f.trunc() as i32),
                "long" => Bson::Int64(f.trunc() as i64),
                _ => Bson::Double(f),
            }
        }
        "bool" => Bson::Boolean(truthy(v)),
        "objectId" => match v {
            Bson::ObjectId(o) => Bson::ObjectId(*o),
            Bson::String(s) => Bson::ObjectId(
                bson::oid::ObjectId::parse_str(s)
                    .map_err(|_| MongoError::new(241, "ConversionFailure", format!("Failed to parse objectId '{s}' in $convert")))?,
            ),
            _ => return Err(fail()),
        },
        "date" => match v {
            Bson::DateTime(d) => Bson::DateTime(*d),
            Bson::String(s) => {
                let d = chrono::DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .or_else(|_| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc()))
                    .map_err(|_| MongoError::new(241, "ConversionFailure", format!("Error parsing date string '{s}'")))?;
                Bson::DateTime(bson::DateTime::from_millis(d.timestamp_millis()))
            }
            x if is_number(x) => Bson::DateTime(bson::DateTime::from_millis(as_f64(x).unwrap_or(0.0) as i64)),
            Bson::ObjectId(o) => Bson::DateTime(o.timestamp()),
            _ => return Err(fail()),
        },
        other => return Err(MongoError::bad_value(format!("Unknown type name: {other}"))),
    })
}

fn date_part(op: &str, d: chrono::DateTime<Utc>) -> Bson {
    Bson::Int32(match op {
        "$year" => d.year(),
        "$month" => d.month() as i32,
        "$dayOfMonth" => d.day() as i32,
        "$hour" => d.hour() as i32,
        "$minute" => d.minute() as i32,
        "$second" => d.second() as i32,
        "$millisecond" => (d.timestamp_subsec_millis()) as i32,
        "$dayOfWeek" => d.weekday().num_days_from_sunday() as i32 + 1,
        "$dayOfYear" => d.ordinal() as i32,
        "$week" => (d.ordinal0() as i32 + 7 - d.weekday().num_days_from_sunday() as i32) / 7,
        _ => 0,
    })
}

fn named<'a>(arg: &'a Bson, op: &str) -> MResult<&'a Document> {
    arg.as_document().ok_or_else(|| err(format!("{op} requires an object as an argument")))
}

fn operator(op: &str, arg: &Bson, ctx: &Ctx) -> MResult<Option<Bson>> {
    let v = match op {
        "$literal" => arg.clone(),
        // Arithmetic.
        "$add" => {
            let vals = args(arg, ctx)?;
            if vals.iter().any(is_nullish) {
                return Ok(Some(Bson::Null));
            }
            let date = vals.iter().find_map(|v| if let Bson::DateTime(d) = v { Some(d.timestamp_millis()) } else { None });
            let sum: f64 = vals.iter().filter(|v| !matches!(v, Bson::DateTime(_))).map(|v| num(v, "$add")).sum::<MResult<f64>>()?;
            match date {
                Some(ms) => Bson::DateTime(bson::DateTime::from_millis(ms + sum as i64)),
                None => number(sum, all_ints(&vals)),
            }
        }
        "$subtract" => {
            let [a, b] = arg_n::<2>(op, arg, ctx)?;
            match (&a, &b) {
                (x, y) if is_nullish(x) || is_nullish(y) => Bson::Null,
                (Bson::DateTime(x), Bson::DateTime(y)) => Bson::Int64(x.timestamp_millis() - y.timestamp_millis()),
                (Bson::DateTime(x), y) => Bson::DateTime(bson::DateTime::from_millis(x.timestamp_millis() - num(y, op)? as i64)),
                (x, y) => number(num(x, op)? - num(y, op)?, all_ints(&[a.clone(), b.clone()])),
            }
        }
        "$multiply" => {
            let vals = args(arg, ctx)?;
            if vals.iter().any(is_nullish) {
                return Ok(Some(Bson::Null));
            }
            let p = vals.iter().map(|v| num(v, op)).product::<MResult<f64>>()?;
            number(p, all_ints(&vals))
        }
        "$divide" => {
            let [a, b] = arg_n::<2>(op, arg, ctx)?;
            if is_nullish(&a) || is_nullish(&b) {
                return Ok(Some(Bson::Null));
            }
            let d = num(&b, op)?;
            if d == 0.0 {
                return Err(MongoError::new(2, "BadValue", "can't $divide by zero"));
            }
            Bson::Double(num(&a, op)? / d)
        }
        "$mod" => {
            let [a, b] = arg_n::<2>(op, arg, ctx)?;
            if is_nullish(&a) || is_nullish(&b) {
                return Ok(Some(Bson::Null));
            }
            let d = num(&b, op)?;
            if d == 0.0 {
                return Err(MongoError::new(16610, "Location16610", "can't $mod by zero"));
            }
            number(num(&a, op)? % d, all_ints(&[a.clone(), b.clone()]))
        }
        "$abs" | "$ceil" | "$floor" | "$sqrt" | "$exp" | "$ln" | "$log10" => {
            let [a] = arg_n::<1>(op, arg, ctx)?;
            if is_nullish(&a) {
                return Ok(Some(Bson::Null));
            }
            let x = num(&a, op)?;
            match op {
                "$abs" => number(x.abs(), all_ints(&[a])),
                "$ceil" => number(x.ceil(), true),
                "$floor" => number(x.floor(), true),
                "$sqrt" => Bson::Double(x.sqrt()),
                "$exp" => Bson::Double(x.exp()),
                "$ln" => Bson::Double(x.ln()),
                _ => Bson::Double(x.log10()),
            }
        }
        "$pow" => {
            let [a, b] = arg_n::<2>(op, arg, ctx)?;
            if is_nullish(&a) || is_nullish(&b) {
                return Ok(Some(Bson::Null));
            }
            number(num(&a, op)?.powf(num(&b, op)?), all_ints(&[a.clone(), b.clone()]))
        }
        "$round" | "$trunc" => {
            let vals = args(arg, ctx)?;
            let a = vals.first().cloned().unwrap_or(Bson::Null);
            if is_nullish(&a) {
                return Ok(Some(Bson::Null));
            }
            let places = vals.get(1).and_then(as_f64).unwrap_or(0.0) as i32;
            let f = 10f64.powi(places);
            let x = num(&a, op)? * f;
            // MongoDB rounds half to even.
            let r = if op == "$trunc" {
                x.trunc()
            } else {
                let r = x.round();
                if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 { r - x.signum() } else { r }
            };
            number(r / f, all_ints(&[a]) || places <= 0 && !matches!(vals[0], Bson::Double(_)))
        }
        // Comparison.
        "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" | "$cmp" => {
            let [a, b] = arg_n::<2>(op, arg, ctx)?;
            let o = compare(&a, &b);
            match op {
                "$eq" => Bson::Boolean(values_equal(&a, &b)),
                "$ne" => Bson::Boolean(!values_equal(&a, &b)),
                "$gt" => Bson::Boolean(o == Ordering::Greater),
                "$gte" => Bson::Boolean(o != Ordering::Less),
                "$lt" => Bson::Boolean(o == Ordering::Less),
                "$lte" => Bson::Boolean(o != Ordering::Greater),
                _ => Bson::Int32(o as i32),
            }
        }
        // Boolean.
        "$and" => {
            let items = match arg {
                Bson::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            let mut r = true;
            for i in &items {
                if !truthy(&ev(i, ctx)?.unwrap_or(Bson::Null)) {
                    r = false;
                    break;
                }
            }
            Bson::Boolean(r)
        }
        "$or" => {
            let items = match arg {
                Bson::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            let mut r = false;
            for i in &items {
                if truthy(&ev(i, ctx)?.unwrap_or(Bson::Null)) {
                    r = true;
                    break;
                }
            }
            Bson::Boolean(r)
        }
        "$not" => {
            let [a] = arg_n::<1>(op, arg, ctx)?;
            Bson::Boolean(!truthy(&a))
        }
        // Conditional.
        "$cond" => {
            let (c, t, e) = match arg {
                Bson::Array(a) if a.len() == 3 => (&a[0], &a[1], &a[2]),
                Bson::Document(d) => {
                    let g = |k: &str| d.get(k).ok_or_else(|| err(format!("Missing '{k}' parameter to $cond")));
                    (g("if")?, g("then")?, g("else")?)
                }
                _ => return Err(err("$cond needs if/then/else")),
            };
            return if truthy(&ev(c, ctx)?.unwrap_or(Bson::Null)) { ev(t, ctx) } else { ev(e, ctx) };
        }
        "$ifNull" => {
            let Bson::Array(items) = arg else { return Err(err("$ifNull needs an array")) };
            for (i, item) in items.iter().enumerate() {
                let v = ev(item, ctx)?;
                if i + 1 == items.len() || v.as_ref().is_some_and(|v| !is_nullish(v)) {
                    return Ok(v);
                }
            }
            Bson::Null
        }
        "$switch" => {
            let d = named(arg, op)?;
            for b in d.get_array("branches").map_err(|_| err("$switch requires at least one branch"))? {
                let b = b.as_document().ok_or_else(|| err("$switch expected each branch to be an object"))?;
                if truthy(&ev(b.get("case").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null)) {
                    return ev(b.get("then").unwrap_or(&Bson::Null), ctx);
                }
            }
            match d.get("default") {
                Some(dv) => return ev(dv, ctx),
                None => {
                    return Err(MongoError::new(
                        40066,
                        "Location40066",
                        "$switch could not find a matching branch for an input, and no default was specified.",
                    ));
                }
            }
        }
        // Strings.
        "$concat" => {
            let vals = args(arg, ctx)?;
            if vals.iter().any(is_nullish) {
                return Ok(Some(Bson::Null));
            }
            let mut s = String::new();
            for v in &vals {
                match v {
                    Bson::String(x) => s.push_str(x),
                    other => return Err(err(format!("$concat only supports strings, not {}", bson_type_name(other)))),
                }
            }
            Bson::String(s)
        }
        "$toUpper" | "$toLower" | "$trim" | "$ltrim" | "$rtrim" | "$strLenCP" | "$strLenBytes" => {
            let a = match (op, arg) {
                ("$trim" | "$ltrim" | "$rtrim", Bson::Document(d)) => ev(d.get("input").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null),
                _ => arg_n::<1>(op, arg, ctx)?.into_iter().next().unwrap(),
            };
            let s = as_str(&a).ok_or_else(|| err(format!("{op} requires a string, found {}", bson_type_name(&a))))?;
            match op {
                "$toUpper" => Bson::String(s.to_uppercase()),
                "$toLower" => Bson::String(s.to_lowercase()),
                "$trim" => Bson::String(s.trim().to_owned()),
                "$ltrim" => Bson::String(s.trim_start().to_owned()),
                "$rtrim" => Bson::String(s.trim_end().to_owned()),
                "$strLenCP" => Bson::Int32(s.chars().count() as i32),
                _ => Bson::Int32(s.len() as i32),
            }
        }
        "$substr" | "$substrCP" | "$substrBytes" => {
            let [s, start, len] = arg_n::<3>(op, arg, ctx)?;
            let s = as_str(&s).unwrap_or_default();
            let start = as_f64(&start).unwrap_or(0.0).max(0.0) as usize;
            let len = as_f64(&len).unwrap_or(-1.0);
            let chars: Vec<char> = s.chars().collect();
            let end = if len < 0.0 { chars.len() } else { (start + len as usize).min(chars.len()) };
            Bson::String(chars.get(start.min(chars.len())..end.max(start.min(chars.len()))).map(|c| c.iter().collect()).unwrap_or_default())
        }
        "$split" => {
            let [s, sep] = arg_n::<2>(op, arg, ctx)?;
            if is_nullish(&s) {
                return Ok(Some(Bson::Null));
            }
            let (Bson::String(s), Bson::String(sep)) = (&s, &sep) else { return Err(err("$split requires strings")) };
            if sep.is_empty() {
                return Err(err("$split requires a non-empty separator"));
            }
            Bson::Array(s.split(sep.as_str()).map(|p| Bson::String(p.to_owned())).collect())
        }
        "$indexOfCP" => {
            let vals = args(arg, ctx)?;
            let (Some(Bson::String(s)), Some(Bson::String(sub))) = (vals.first(), vals.get(1)) else { return Ok(Some(Bson::Null)) };
            let chars: Vec<char> = s.chars().collect();
            let sub: Vec<char> = sub.chars().collect();
            let from = vals.get(2).and_then(as_f64).unwrap_or(0.0) as usize;
            let pos = (from..=chars.len().saturating_sub(sub.len())).find(|&i| chars.get(i..i + sub.len()) == Some(&sub[..]));
            Bson::Int32(pos.map(|p| p as i32).unwrap_or(-1))
        }
        "$regexMatch" | "$regexFind" => {
            let d = named(arg, op)?;
            let input = ev(d.get("input").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let re = ev(d.get("regex").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let opts = d.get_str("options").unwrap_or("");
            let pattern = match &re {
                Bson::RegularExpression(r) => {
                    let flags = if opts.is_empty() { r.options.as_str().replace(['u', 'l'], "") } else { opts.to_owned() };
                    if flags.is_empty() { r.pattern.as_str().to_owned() } else { format!("(?{flags}){}", r.pattern.as_str()) }
                }
                Bson::String(s) => {
                    if opts.is_empty() {
                        s.clone()
                    } else {
                        format!("(?{opts}){s}")
                    }
                }
                _ => return Err(err("$regexMatch needs a regex")),
            };
            let re = regex::RegexBuilder::new(&pattern)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| MongoError::bad_value(format!("invalid regex: {e}")))?;
            let s = input.as_str().unwrap_or("");
            if op == "$regexMatch" {
                Bson::Boolean(re.is_match(s))
            } else {
                match re.find(s) {
                    Some(m) => Bson::Document(
                        doc! { "match": m.as_str(), "idx": s[..m.start()].chars().count() as i32, "captures": Bson::Array(vec![]) },
                    ),
                    None => Bson::Null,
                }
            }
        }
        "$replaceAll" | "$replaceOne" => {
            let d = named(arg, op)?;
            let g = |k: &str| -> MResult<Bson> { Ok(ev(d.get(k).unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null)) };
            let (input, find, repl) = (g("input")?, g("find")?, g("replacement")?);
            if [&input, &find, &repl].iter().any(|v| is_nullish(v)) {
                return Ok(Some(Bson::Null));
            }
            let (Bson::String(i), Bson::String(f), Bson::String(r)) = (&input, &find, &repl) else {
                return Err(err(format!("{op} requires strings")));
            };
            Bson::String(if op == "$replaceAll" { i.replace(f.as_str(), r) } else { i.replacen(f.as_str(), r, 1) })
        }
        "$toString" => convert(&arg_n::<1>(op, arg, ctx)?[0], "string")?,
        "$toInt" => convert(&arg_n::<1>(op, arg, ctx)?[0], "int")?,
        "$toLong" => convert(&arg_n::<1>(op, arg, ctx)?[0], "long")?,
        "$toDouble" | "$toDecimal" => convert(&arg_n::<1>(op, arg, ctx)?[0], "double")?,
        "$toBool" => convert(&arg_n::<1>(op, arg, ctx)?[0], "bool")?,
        "$toObjectId" => convert(&arg_n::<1>(op, arg, ctx)?[0], "objectId")?,
        "$toDate" => convert(&arg_n::<1>(op, arg, ctx)?[0], "date")?,
        "$convert" => {
            let d = named(arg, op)?;
            let input = ev(d.get("input").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let to = match d.get("to") {
                Some(Bson::String(s)) => s.clone(),
                Some(other) => as_str(&ev(other, ctx)?.unwrap_or(Bson::Null)).unwrap_or_default(),
                None => return Err(err("Missing 'to' parameter to $convert")),
            };
            if is_nullish(&input)
                && let Some(n) = d.get("onNull")
            {
                return ev(n, ctx);
            }
            match convert(&input, &to) {
                Ok(v) => v,
                Err(e) => match d.get("onError") {
                    Some(fallback) => return ev(fallback, ctx),
                    None => return Err(e),
                },
            }
        }
        "$type" => match ev(&arg_n_raw(arg), ctx)? {
            Some(v) => Bson::String(bson_type_name(&v).into()),
            None => Bson::String("missing".into()),
        },
        "$isNumber" => Bson::Boolean(ev(&arg_n_raw(arg), ctx)?.is_some_and(|v| is_number(&v))),
        "$isArray" => Bson::Boolean(matches!(ev(&arg_n_raw(arg), ctx)?, Some(Bson::Array(_)))),
        // Arrays.
        "$size" => match arg_n::<1>(op, arg, ctx)?.into_iter().next().unwrap() {
            Bson::Array(a) => Bson::Int32(a.len() as i32),
            other => {
                return Err(MongoError::new(
                    17124,
                    "Location17124",
                    format!("The argument to $size must be an array. Type of argument: {}", bson_type_name(&other)),
                ));
            }
        },
        "$arrayElemAt" => {
            let [a, i] = arg_n::<2>(op, arg, ctx)?;
            let Bson::Array(a) = a else { return Ok(Some(Bson::Null)) };
            let i = as_f64(&i).unwrap_or(0.0) as i64;
            let idx = if i < 0 { a.len() as i64 + i } else { i };
            return Ok(usize::try_from(idx).ok().and_then(|i| a.get(i).cloned()));
        }
        "$first" | "$last" => {
            let [a] = arg_n::<1>(op, arg, ctx)?;
            let Bson::Array(a) = a else { return Ok(if is_nullish(&a) { Some(Bson::Null) } else { None }) };
            return Ok(if op == "$first" { a.first().cloned() } else { a.last().cloned() });
        }
        "$in" => {
            let [x, a] = arg_n::<2>(op, arg, ctx)?;
            let Bson::Array(a) = a else { return Err(err("$in requires an array as a second argument")) };
            Bson::Boolean(a.iter().any(|v| values_equal(v, &x)))
        }
        "$concatArrays" => {
            let mut out = Vec::new();
            for v in args(arg, ctx)? {
                match v {
                    Bson::Array(a) => out.extend(a),
                    x if is_nullish(&x) => return Ok(Some(Bson::Null)),
                    other => return Err(err(format!("$concatArrays only supports arrays, not {}", bson_type_name(&other)))),
                }
            }
            Bson::Array(out)
        }
        "$reverseArray" => match arg_n::<1>(op, arg, ctx)?.into_iter().next().unwrap() {
            Bson::Array(mut a) => {
                a.reverse();
                Bson::Array(a)
            }
            _ => Bson::Null,
        },
        "$slice" => {
            let vals = args(arg, ctx)?;
            let Some(Bson::Array(a)) = vals.first() else { return Ok(Some(Bson::Null)) };
            let (start, n) = match (vals.get(1).and_then(as_f64), vals.get(2).and_then(as_f64)) {
                (Some(n), None) if n < 0.0 => (a.len().saturating_sub((-n) as usize), a.len()),
                (Some(n), None) => (0, n as usize),
                (Some(p), Some(n)) => {
                    (if p < 0.0 { a.len().saturating_sub((-p) as usize) } else { (p as usize).min(a.len()) }, n.max(0.0) as usize)
                }
                _ => (0, a.len()),
            };
            Bson::Array(a.iter().skip(start).take(n).cloned().collect())
        }
        "$range" => {
            let vals = args(arg, ctx)?;
            let (s, e) = (vals.first().and_then(as_f64).unwrap_or(0.0) as i64, vals.get(1).and_then(as_f64).unwrap_or(0.0) as i64);
            let step = vals.get(2).and_then(as_f64).unwrap_or(1.0) as i64;
            if step == 0 {
                return Err(err("$range requires a non-zero step value"));
            }
            let mut out = Vec::new();
            let mut i = s;
            while (step > 0 && i < e) || (step < 0 && i > e) {
                out.push(Bson::Int32(i as i32));
                if out.len() > 1_000_000 {
                    return Err(err("$range would produce too many values"));
                }
                i += step;
            }
            Bson::Array(out)
        }
        "$filter" | "$map" => {
            let d = named(arg, op)?;
            let input = ev(d.get("input").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let Bson::Array(items) = input else { return Ok(Some(Bson::Null)) };
            let name = d.get_str("as").unwrap_or("this").to_owned();
            let body = d.get(if op == "$map" { "in" } else { "cond" }).cloned().unwrap_or(Bson::Null);
            let mut vars = ctx.vars.clone();
            let mut out = Vec::new();
            for item in items {
                vars.insert(name.clone(), item.clone());
                let inner = Ctx { current: ctx.current, vars: vars.clone() };
                let r = ev(&body, &inner)?.unwrap_or(Bson::Null);
                if op == "$map" {
                    out.push(r);
                } else if truthy(&r) {
                    out.push(item);
                    if d.get("limit").and_then(as_f64).is_some_and(|l| out.len() as f64 >= l) {
                        break;
                    }
                }
            }
            Bson::Array(out)
        }
        "$reduce" => {
            let d = named(arg, op)?;
            let input = ev(d.get("input").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let Bson::Array(items) = input else { return Ok(Some(Bson::Null)) };
            let mut acc = ev(d.get("initialValue").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            let body = d.get("in").cloned().unwrap_or(Bson::Null);
            for item in items {
                let mut vars = ctx.vars.clone();
                vars.insert("this".into(), item);
                vars.insert("value".into(), acc);
                acc = ev(&body, &Ctx { current: ctx.current, vars })?.unwrap_or(Bson::Null);
            }
            acc
        }
        "$sum" | "$avg" | "$min" | "$max" => {
            let vals = args(arg, ctx)?;
            let vals: Vec<Bson> = match vals.as_slice() {
                [Bson::Array(a)] => a.clone(),
                _ => vals,
            };
            fold(op, &vals)
        }
        // Objects.
        "$mergeObjects" => {
            let mut out = Document::new();
            for v in args(arg, ctx)? {
                if let Bson::Document(d) = v {
                    for (k, v) in d {
                        out.insert(k, v);
                    }
                }
            }
            Bson::Document(out)
        }
        "$objectToArray" => match arg_n::<1>(op, arg, ctx)?.into_iter().next().unwrap() {
            Bson::Document(d) => Bson::Array(d.into_iter().map(|(k, v)| Bson::Document(doc! { "k": k, "v": v })).collect()),
            _ => Bson::Null,
        },
        "$arrayToObject" => {
            let [a] = arg_n::<1>(op, arg, ctx)?;
            let Bson::Array(items) = a else { return Ok(Some(Bson::Null)) };
            let mut out = Document::new();
            for i in items {
                match i {
                    Bson::Document(d) => {
                        if let (Ok(k), Some(v)) = (d.get_str("k"), d.get("v")) {
                            out.insert(k, v.clone());
                        }
                    }
                    Bson::Array(pair) if pair.len() == 2 => {
                        if let Bson::String(k) = &pair[0] {
                            out.insert(k.clone(), pair[1].clone());
                        }
                    }
                    _ => return Err(err("$arrayToObject requires an array of key-value pairs")),
                }
            }
            Bson::Document(out)
        }
        "$getField" => {
            let (field, input) = match arg {
                Bson::String(s) => (s.clone(), Bson::Document(ctx.current.clone())),
                Bson::Document(d) => (
                    as_str(&ev(d.get("field").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null)).unwrap_or_default(),
                    match d.get("input") {
                        Some(i) => ev(i, ctx)?.unwrap_or(Bson::Null),
                        None => Bson::Document(ctx.current.clone()),
                    },
                ),
                _ => return Err(err("$getField requires a field name")),
            };
            return Ok(input.as_document().and_then(|d| d.get(&field).cloned()));
        }
        "$let" => {
            let d = named(arg, op)?;
            let mut vars = ctx.vars.clone();
            if let Ok(defs) = d.get_document("vars") {
                for (k, v) in defs {
                    vars.insert(k.clone(), ev(v, ctx)?.unwrap_or(Bson::Null));
                }
            }
            return ev(d.get("in").unwrap_or(&Bson::Null), &Ctx { current: ctx.current, vars });
        }
        // Dates.
        "$year" | "$month" | "$dayOfMonth" | "$hour" | "$minute" | "$second" | "$millisecond" | "$dayOfWeek" | "$dayOfYear" | "$week" => {
            let v = match arg {
                Bson::Document(d) if d.contains_key("date") => ev(d.get("date").unwrap(), ctx)?.unwrap_or(Bson::Null),
                other => arg_n::<1>(op, other, ctx)?.into_iter().next().unwrap(),
            };
            if is_nullish(&v) {
                return Ok(Some(Bson::Null));
            }
            date_part(op, to_datetime(&v).ok_or_else(|| err(format!("can't convert from BSON type {} to Date", bson_type_name(&v))))?)
        }
        "$dateToString" => {
            let d = named(arg, op)?;
            let v = ev(d.get("date").unwrap_or(&Bson::Null), ctx)?.unwrap_or(Bson::Null);
            if is_nullish(&v) {
                return match d.get("onNull") {
                    Some(n) => ev(n, ctx),
                    None => Ok(Some(Bson::Null)),
                };
            }
            let dt = to_datetime(&v).ok_or_else(|| err("$dateToString requires a date"))?;
            let fmt = d.get_str("format").unwrap_or("%Y-%m-%dT%H:%M:%S.%LZ").replace("%L", &format!("{:03}", dt.timestamp_subsec_millis()));
            Bson::String(dt.format(&fmt).to_string())
        }
        other => return Err(MongoError::new(168, "InvalidPipelineOperator", format!("Unrecognized expression '{other}'"))),
    };
    Ok(Some(v))
}

/// The single argument of operators that accept `x` or `[x]`.
fn arg_n_raw(arg: &Bson) -> Bson {
    match arg {
        Bson::Array(a) if a.len() == 1 => a[0].clone(),
        other => other.clone(),
    }
}

fn fold(op: &str, vals: &[Bson]) -> Bson {
    let nums: Vec<&Bson> = vals.iter().filter(|v| is_number(v)).collect();
    match op {
        "$sum" => {
            let total: f64 = nums.iter().filter_map(|v| as_f64(v)).sum();
            number(total, nums.iter().all(|v| matches!(v, Bson::Int32(_) | Bson::Int64(_))))
        }
        "$avg" => {
            if nums.is_empty() {
                Bson::Null
            } else {
                Bson::Double(nums.iter().filter_map(|v| as_f64(v)).sum::<f64>() / nums.len() as f64)
            }
        }
        _ => {
            let present = vals.iter().filter(|v| !is_nullish(v));
            let pick = if op == "$min" { present.min_by(|a, b| compare(a, b)) } else { present.max_by(|a, b| compare(a, b)) };
            pick.cloned().unwrap_or(Bson::Null)
        }
    }
}

// ------------------------------------------------------------------ pipeline

/// Gives `$lookup` and `$unionWith` access to other collections of the same database.
pub trait Collections {
    fn documents(&self, collection: &str) -> Vec<Document>;
}

impl<F: Fn(&str) -> Vec<Document>> Collections for F {
    fn documents(&self, collection: &str) -> Vec<Document> {
        self(collection)
    }
}

fn stage_err(msg: impl Into<String>) -> MongoError {
    MongoError::new(40323, "Location40323", msg)
}

pub fn run_pipeline(mut docs: Vec<Document>, pipeline: &[Document], colls: &dyn Collections) -> MResult<Vec<Document>> {
    for stage in pipeline {
        if stage.len() != 1 {
            return Err(stage_err("A pipeline stage specification object must contain exactly one field."));
        }
        let (name, spec) = stage.iter().next().unwrap();
        docs = run_stage(docs, name, spec, colls)?;
    }
    Ok(docs)
}

fn spec_doc<'a>(spec: &'a Bson, stage: &str) -> MResult<&'a Document> {
    spec.as_document().ok_or_else(|| MongoError::new(15955, "Location15955", format!("{stage} specification must be an object")))
}

fn run_stage(docs: Vec<Document>, name: &str, spec: &Bson, colls: &dyn Collections) -> MResult<Vec<Document>> {
    Ok(match name {
        "$match" => {
            let q = spec_doc(spec, name)?;
            let mut out = Vec::with_capacity(docs.len());
            for d in docs {
                if matches(&d, q)? {
                    out.push(d);
                }
            }
            out
        }
        "$project" => {
            let p = spec_doc(spec, name)?;
            docs.into_iter().map(|d| project_stage(&d, p)).collect::<MResult<_>>()?
        }
        "$addFields" | "$set" => {
            let p = spec_doc(spec, name)?;
            let mut out = Vec::with_capacity(docs.len());
            for mut d in docs {
                let base = d.clone();
                for (k, e) in p {
                    let ctx = Ctx { current: &base, vars: HashMap::new() };
                    match ev(e, &ctx)? {
                        Some(v) => set_path(&mut d, k, v),
                        None => unset_path(&mut d, k),
                    }
                }
                out.push(d);
            }
            out
        }
        "$unset" => {
            let fields: Vec<String> = match spec {
                Bson::String(s) => vec![s.clone()],
                Bson::Array(a) => a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                _ => return Err(stage_err("$unset specification must be a string or an array")),
            };
            docs.into_iter()
                .map(|mut d| {
                    for f in &fields {
                        unset_path(&mut d, f);
                    }
                    d
                })
                .collect()
        }
        "$sort" => {
            let mut docs = docs;
            sort_docs(&mut docs, spec_doc(spec, name)?);
            docs
        }
        "$limit" => {
            let n = as_f64(spec)
                .filter(|n| *n > 0.0)
                .ok_or_else(|| MongoError::new(15958, "Location15958", "the limit must be positive"))? as usize;
            docs.into_iter().take(n).collect()
        }
        "$skip" => {
            let n = as_f64(spec)
                .filter(|n| *n >= 0.0)
                .ok_or_else(|| MongoError::new(15956, "Location15956", "the skip must be non-negative"))? as usize;
            docs.into_iter().skip(n).collect()
        }
        "$count" => {
            let field = spec
                .as_str()
                .filter(|s| !s.is_empty() && !s.starts_with('$'))
                .ok_or_else(|| MongoError::new(40156, "Location40156", "the count field must be a non-empty string"))?;
            if docs.is_empty() { vec![] } else { vec![doc! { field: docs.len() as i32 }] }
        }
        "$group" => group(docs, spec_doc(spec, name)?)?,
        "$unwind" => {
            let (path, index, keep_empty) = match spec {
                Bson::String(s) => (s.clone(), None, false),
                Bson::Document(d) => (
                    d.get_str("path").map_err(|_| stage_err("$unwind requires a path"))?.to_owned(),
                    d.get_str("includeArrayIndex").ok().map(str::to_owned),
                    d.get_bool("preserveNullAndEmptyArrays").unwrap_or(false),
                ),
                _ => return Err(stage_err("$unwind requires a string or document")),
            };
            let path = path
                .strip_prefix('$')
                .ok_or_else(|| MongoError::new(28818, "Location28818", "path option to $unwind stage should be prefixed with a '$'"))?
                .to_owned();
            let mut out = Vec::new();
            for d in docs {
                match lookup(&d, &path).cloned() {
                    Some(Bson::Array(items)) if !items.is_empty() => {
                        for (i, item) in items.into_iter().enumerate() {
                            let mut nd = d.clone();
                            set_path(&mut nd, &path, item);
                            if let Some(f) = &index {
                                set_path(&mut nd, f, Bson::Int64(i as i64));
                            }
                            out.push(nd);
                        }
                    }
                    Some(Bson::Array(_)) | None | Some(Bson::Null) => {
                        if keep_empty {
                            let mut nd = d.clone();
                            if let Some(f) = &index {
                                set_path(&mut nd, f, Bson::Null);
                            }
                            out.push(nd);
                        }
                    }
                    Some(_) => {
                        let mut nd = d;
                        if let Some(f) = &index {
                            set_path(&mut nd, f, Bson::Null);
                        }
                        out.push(nd);
                    }
                }
            }
            out
        }
        "$lookup" => {
            let s = spec_doc(spec, name)?;
            let from = s.get_str("from").map_err(|_| stage_err("$lookup requires 'from'"))?;
            let as_field = s.get_str("as").map_err(|_| stage_err("$lookup requires 'as'"))?.to_owned();
            let foreign = colls.documents(from);
            let pipeline: Option<Vec<Document>> =
                s.get_array("pipeline").ok().map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect());
            let mut out = Vec::with_capacity(docs.len());
            for mut d in docs {
                let mut joined: Vec<Document> = match (s.get_str("localField"), s.get_str("foreignField")) {
                    (Ok(lf), Ok(ff)) => {
                        let local: Vec<Bson> = match lookup(&d, lf) {
                            Some(Bson::Array(a)) => a.clone(),
                            Some(v) => vec![v.clone()],
                            None => vec![Bson::Null],
                        };
                        foreign
                            .iter()
                            .filter(|f| {
                                let fv = super::query::path_values(f, ff);
                                if fv.is_empty() {
                                    return local.iter().any(|l| matches!(l, Bson::Null));
                                }
                                fv.iter().any(|v| match v {
                                    Bson::Array(items) => items.iter().any(|i| local.iter().any(|l| values_equal(i, l))),
                                    v => local.iter().any(|l| values_equal(v, l)),
                                })
                            })
                            .cloned()
                            .collect()
                    }
                    _ => foreign.clone(),
                };
                if let Some(p) = &pipeline {
                    // `let` variables are substituted as literals so `$expr` can use `$$name`.
                    let p = match s.get_document("let") {
                        Ok(lets) => {
                            let mut vars = HashMap::new();
                            for (k, e) in lets {
                                vars.insert(k.clone(), ev(e, &Ctx { current: &d, vars: HashMap::new() })?.unwrap_or(Bson::Null));
                            }
                            p.iter()
                                .map(|st| substitute_vars(&Bson::Document(st.clone()), &vars))
                                .filter_map(|b| b.as_document().cloned())
                                .collect()
                        }
                        Err(_) => p.clone(),
                    };
                    joined = run_pipeline(joined, &p, colls)?;
                }
                set_path(&mut d, &as_field, Bson::Array(joined.into_iter().map(Bson::Document).collect()));
                out.push(d);
            }
            out
        }
        "$unionWith" => {
            let (coll, pipeline) = match spec {
                Bson::String(c) => (c.clone(), vec![]),
                Bson::Document(d) => (
                    d.get_str("coll").map_err(|_| stage_err("$unionWith requires 'coll'"))?.to_owned(),
                    d.get_array("pipeline").map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect()).unwrap_or_default(),
                ),
                _ => return Err(stage_err("$unionWith requires a collection")),
            };
            let mut docs = docs;
            docs.extend(run_pipeline(colls.documents(&coll), &pipeline, colls)?);
            docs
        }
        "$replaceRoot" | "$replaceWith" => {
            let expr = if name == "$replaceRoot" {
                spec_doc(spec, name)?.get("newRoot").cloned().ok_or_else(|| stage_err("no newRoot specified for the $replaceRoot stage"))?
            } else {
                spec.clone()
            };
            docs.into_iter()
                .map(|d| match eval(&expr, &d)? {
                    Bson::Document(nd) => Ok(nd),
                    other => Err(MongoError::new(
                        40228,
                        "Location40228",
                        format!("'replacement document' must evaluate to an object, but resulting value was: {other}"),
                    )),
                })
                .collect::<MResult<_>>()?
        }
        "$sortByCount" => {
            let grouped = group(docs, &doc! { "_id": spec.clone(), "count": { "$sum": 1 } })?;
            let mut grouped = grouped;
            sort_docs(&mut grouped, &doc! { "count": -1 });
            grouped
        }
        "$sample" => {
            use rand::seq::SliceRandom;
            let n = spec_doc(spec, name)?.get("size").and_then(as_f64).unwrap_or(0.0) as usize;
            let mut docs = docs;
            docs.shuffle(&mut rand::rng());
            docs.truncate(n);
            docs
        }
        "$facet" => {
            let s = spec_doc(spec, name)?;
            let mut out = Document::new();
            for (k, p) in s {
                let p: Vec<Document> =
                    p.as_array().map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect()).unwrap_or_default();
                let r = run_pipeline(docs.clone(), &p, colls)?;
                out.insert(k.clone(), Bson::Array(r.into_iter().map(Bson::Document).collect()));
            }
            vec![out]
        }
        "$bucket" => {
            let s = spec_doc(spec, name)?;
            let by = s.get("groupBy").cloned().ok_or_else(|| stage_err("$bucket requires 'groupBy'"))?;
            let bounds = s.get_array("boundaries").map_err(|_| stage_err("$bucket requires 'boundaries'"))?.clone();
            let default = s.get("default").cloned();
            let output = s.get_document("output").cloned().unwrap_or_else(|_| doc! { "count": { "$sum": 1 } });
            let mut tagged = Vec::with_capacity(docs.len());
            for d in docs {
                let v = eval(&by, &d)?;
                let bucket = bounds
                    .windows(2)
                    .find(|w| compare(&v, &w[0]) != Ordering::Less && compare(&v, &w[1]) == Ordering::Less)
                    .map(|w| w[0].clone());
                match bucket.or_else(|| default.clone()) {
                    Some(b) => {
                        let mut d = d;
                        d.insert("__bucket", b);
                        tagged.push(d);
                    }
                    None => {
                        return Err(MongoError::new(
                            40066,
                            "Location40066",
                            "$bucket could not find a matching branch for an input, and no default was specified.",
                        ));
                    }
                }
            }
            let mut g = output;
            g.insert("_id", "$__bucket");
            let mut out = group(tagged, &g)?;
            sort_docs(&mut out, &doc! { "_id": 1 });
            out
        }
        "$redact" | "$out" | "$merge" => return Err(stage_err(format!("{name} is handled by the server, not inside a pipeline"))),
        other => return Err(MongoError::new(40324, "Location40324", format!("Unrecognized pipeline stage name: '{other}'"))),
    })
}

fn substitute_vars(v: &Bson, vars: &HashMap<String, Bson>) -> Bson {
    match v {
        Bson::String(s) if s.starts_with("$$") => {
            let name = s[2..].split('.').next().unwrap_or_default();
            match vars.get(name) {
                Some(val) => {
                    let base = match s[2..].split_once('.') {
                        Some((_, path)) => field_of(val, path).unwrap_or(Bson::Null),
                        None => val.clone(),
                    };
                    Bson::Document(doc! { "$literal": base })
                }
                None => v.clone(),
            }
        }
        Bson::Document(d) => Bson::Document(d.iter().map(|(k, v)| (k.clone(), substitute_vars(v, vars))).collect()),
        Bson::Array(a) => Bson::Array(a.iter().map(|v| substitute_vars(v, vars)).collect()),
        other => other.clone(),
    }
}

/// `$project` with inclusion, exclusion and computed fields.
fn project_stage(d: &Document, spec: &Document) -> MResult<Document> {
    let mut flags = Document::new();
    let mut computed: Vec<(String, Bson)> = Vec::new();
    fn flatten(prefix: &str, spec: &Document, flags: &mut Document, computed: &mut Vec<(String, Bson)>) {
        for (k, v) in spec {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match v {
                Bson::Boolean(_) | Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) => {
                    flags.insert(path, v.clone());
                }
                Bson::Document(sub) if !sub.keys().next().is_some_and(|k| k.starts_with('$')) => flatten(&path, sub, flags, computed),
                other => computed.push((path, other.clone())),
            }
        }
    }
    flatten("", spec, &mut flags, &mut computed);
    if computed.is_empty() {
        return project(d, &flags);
    }
    // Computed fields imply inclusion mode.
    let excluded = flags.iter().any(|(k, v)| k != "_id" && !truthy(v));
    if excluded {
        return Err(MongoError::new(31254, "Location31254", "Cannot do exclusion on a field in inclusion projection"));
    }
    if !flags.iter().any(|(k, v)| k != "_id" && truthy(v)) {
        // Only computed fields (and maybe _id): project _id explicitly.
        flags.insert("__twinbase_none", 1);
    }
    let mut out = project(d, &flags)?;
    for (path, e) in computed {
        match eval_opt(&e, d)? {
            Some(v) => set_path(&mut out, &path, v),
            None => unset_path(&mut out, &path),
        }
    }
    Ok(out)
}

fn eval_opt(expr: &Bson, doc: &Document) -> MResult<Option<Bson>> {
    ev(expr, &Ctx { current: doc, vars: HashMap::new() })
}

enum Acc {
    Sum(Vec<Bson>),
    Avg(f64, usize),
    Min(Option<Bson>),
    Max(Option<Bson>),
    First(Option<Bson>),
    Last(Option<Bson>),
    Push(Vec<Bson>),
    AddToSet(IndexMap<Vec<u8>, Bson>),
    Count(i64),
    Merge(Document),
}

fn group(docs: Vec<Document>, spec: &Document) -> MResult<Vec<Document>> {
    let id_expr =
        spec.get("_id").cloned().ok_or_else(|| MongoError::new(15955, "Location15955", "a group specification must include an _id"))?;
    let fields: Vec<(String, String, Bson)> = spec
        .iter()
        .filter(|(k, _)| k.as_str() != "_id")
        .map(|(k, v)| {
            let d = v
                .as_document()
                .filter(|d| d.len() == 1)
                .ok_or_else(|| MongoError::new(40234, "Location40234", format!("The field '{k}' must be an accumulator object")))?;
            let (op, e) = d.iter().next().unwrap();
            Ok((k.clone(), op.clone(), e.clone()))
        })
        .collect::<MResult<_>>()?;
    let mut groups: IndexMap<Vec<u8>, (Bson, Vec<Acc>)> = IndexMap::new();
    for d in &docs {
        let id = eval(&id_expr, d)?;
        let key = key_of(&id);
        let entry = groups.entry(key).or_insert_with(|| {
            let accs = fields
                .iter()
                .map(|(_, op, _)| match op.as_str() {
                    "$avg" => Acc::Avg(0.0, 0),
                    "$min" => Acc::Min(None),
                    "$max" => Acc::Max(None),
                    "$first" => Acc::First(None),
                    "$last" => Acc::Last(None),
                    "$push" => Acc::Push(vec![]),
                    "$addToSet" => Acc::AddToSet(IndexMap::new()),
                    "$count" => Acc::Count(0),
                    "$mergeObjects" => Acc::Merge(Document::new()),
                    _ => Acc::Sum(vec![]),
                })
                .collect();
            (id.clone(), accs)
        });
        for ((_, op, e), acc) in fields.iter().zip(entry.1.iter_mut()) {
            let v = if op == "$count" { Bson::Null } else { eval_opt(e, d)?.unwrap_or(Bson::Null) };
            match acc {
                Acc::Sum(vals) => {
                    if !matches!(op.as_str(), "$sum") {
                        return Err(MongoError::new(15952, "Location15952", format!("unknown group operator '{op}'")));
                    }
                    match v {
                        Bson::Array(_) => {}
                        x if is_number(&x) => vals.push(x),
                        _ => {}
                    }
                }
                Acc::Avg(sum, n) => {
                    if let Some(x) = as_f64(&v) {
                        *sum += x;
                        *n += 1;
                    }
                }
                Acc::Min(cur) => {
                    if !matches!(v, Bson::Null | Bson::Undefined) && cur.as_ref().is_none_or(|c| compare(&v, c) == Ordering::Less) {
                        *cur = Some(v);
                    }
                }
                Acc::Max(cur) => {
                    if !matches!(v, Bson::Null | Bson::Undefined) && cur.as_ref().is_none_or(|c| compare(&v, c) == Ordering::Greater) {
                        *cur = Some(v);
                    }
                }
                Acc::First(cur) => {
                    if cur.is_none() {
                        *cur = Some(v);
                    }
                }
                Acc::Last(cur) => *cur = Some(v),
                Acc::Push(items) => items.push(v),
                Acc::AddToSet(set) => {
                    set.entry(key_of(&v)).or_insert(v);
                }
                Acc::Count(n) => *n += 1,
                Acc::Merge(m) => {
                    if let Bson::Document(x) = v {
                        for (k, v) in x {
                            m.insert(k, v);
                        }
                    }
                }
            }
        }
    }
    Ok(groups
        .into_values()
        .map(|(id, accs)| {
            let mut out = doc! { "_id": id };
            for ((name, _, _), acc) in fields.iter().zip(accs) {
                let v = match acc {
                    Acc::Sum(vals) => fold("$sum", &vals),
                    Acc::Avg(sum, n) => {
                        if n == 0 {
                            Bson::Null
                        } else {
                            Bson::Double(sum / n as f64)
                        }
                    }
                    Acc::Min(v) | Acc::Max(v) | Acc::First(v) | Acc::Last(v) => v.unwrap_or(Bson::Null),
                    Acc::Push(items) => Bson::Array(items),
                    Acc::AddToSet(set) => Bson::Array(set.into_values().collect()),
                    Acc::Count(n) => number(n as f64, true),
                    Acc::Merge(m) => Bson::Document(m),
                };
                out.insert(name.clone(), v);
            }
            out
        })
        .collect())
}

/// Apply an update written as an aggregation pipeline (`[{$set: …}, {$unset: …}]`).
pub fn apply_pipeline_update(doc: &Document, pipeline: &[Document]) -> MResult<Document> {
    for st in pipeline {
        let name = st.keys().next().map(String::as_str).unwrap_or_default();
        if !matches!(name, "$set" | "$addFields" | "$unset" | "$project" | "$replaceRoot" | "$replaceWith") {
            return Err(MongoError::new(72, "InvalidOptions", format!("{name} is not allowed to be used within an update")));
        }
    }
    let none = |_: &str| Vec::<Document>::new();
    let mut out = run_pipeline(vec![doc.clone()], pipeline, &none)?;
    let mut d = out.pop().unwrap_or_default();
    if let Some(id) = doc.get("_id") {
        if d.get("_id").is_some_and(|nid| !values_equal(nid, id)) {
            return Err(MongoError::new(
                66,
                "ImmutableField",
                "Performing an update on the path '_id' would modify the immutable field '_id'",
            ));
        }
        if !d.contains_key("_id") {
            let mut with_id = doc! { "_id": id.clone() };
            with_id.extend(d);
            d = with_id;
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Vec<Document> {
        vec![]
    }

    #[test]
    fn expressions() {
        let d = doc! { "a": 4, "b": 2.5, "s": "Hello World", "arr": [1, 2, 3], "n": null, "when": bson::DateTime::from_millis(1_700_000_000_000) };
        let e = |x: Bson| eval(&x, &d).unwrap();
        assert_eq!(e(Bson::Document(doc! { "$add": ["$a", 1] })), Bson::Int32(5));
        assert_eq!(e(Bson::Document(doc! { "$multiply": ["$a", "$b"] })), Bson::Double(10.0));
        assert_eq!(e(Bson::Document(doc! { "$toUpper": "$s" })), Bson::String("HELLO WORLD".into()));
        assert_eq!(e(Bson::Document(doc! { "$concat": ["$s", "!"] })), Bson::String("Hello World!".into()));
        assert_eq!(e(Bson::Document(doc! { "$size": "$arr" })), Bson::Int32(3));
        assert_eq!(e(Bson::Document(doc! { "$ifNull": ["$n", "$missing", "x"] })), Bson::String("x".into()));
        assert_eq!(
            e(Bson::Document(doc! { "$cond": { "if": { "$gt": ["$a", 3] }, "then": "big", "else": "small" } })),
            Bson::String("big".into())
        );
        assert_eq!(
            e(Bson::Document(doc! { "$map": { "input": "$arr", "as": "x", "in": { "$multiply": ["$$x", 10] } } })),
            Bson::Array(vec![10.into(), 20.into(), 30.into()])
        );
        assert_eq!(
            e(Bson::Document(doc! { "$filter": { "input": "$arr", "cond": { "$gte": ["$$this", 2] } } })),
            Bson::Array(vec![2.into(), 3.into()])
        );
        assert_eq!(
            e(Bson::Document(doc! { "$reduce": { "input": "$arr", "initialValue": 0, "in": { "$add": ["$$value", "$$this"] } } })),
            Bson::Int32(6)
        );
        assert_eq!(e(Bson::Document(doc! { "$year": "$when" })), Bson::Int32(2023));
        assert_eq!(
            e(Bson::Document(doc! { "$dateToString": { "format": "%Y-%m-%d", "date": "$when" } })),
            Bson::String("2023-11-14".into())
        );
        assert_eq!(e(Bson::Document(doc! { "$sum": "$arr" })), Bson::Int32(6));
        assert_eq!(e(Bson::Document(doc! { "$round": [2.5, 0] })), Bson::Double(2.0));
        assert!(eval(&Bson::Document(doc! { "$divide": [1, 0] }), &d).is_err());
        assert!(eval(&Bson::Document(doc! { "$nope": 1 }), &d).is_err());
    }

    #[test]
    fn pipeline_group_unwind_lookup() {
        let orders = vec![
            doc! { "_id": 1, "customer": "ann", "items": ["a", "b"], "total": 10 },
            doc! { "_id": 2, "customer": "bob", "items": ["a"], "total": 5 },
            doc! { "_id": 3, "customer": "ann", "items": [], "total": 7.5 },
        ];
        let customers = vec![doc! { "_id": "ann", "city": "Lisbon" }, doc! { "_id": "bob", "city": "Porto" }];
        let colls = move |name: &str| if name == "customers" { customers.clone() } else { vec![] };
        let out = run_pipeline(
            orders.clone(),
            &[
                doc! { "$match": { "total": { "$gte": 5 } } },
                doc! { "$group": { "_id": "$customer", "spent": { "$sum": "$total" }, "n": { "$count": {} }, "first": { "$first": "$_id" } } },
                doc! { "$sort": { "spent": -1 } },
                doc! { "$lookup": { "from": "customers", "localField": "_id", "foreignField": "_id", "as": "c" } },
                doc! { "$project": { "spent": 1, "n": 1, "city": { "$first": "$c.city" } } },
            ],
            &colls,
        )
        .unwrap();
        assert_eq!(out[0], doc! { "_id": "ann", "spent": 17.5, "n": 2, "city": "Lisbon" });
        assert_eq!(out[1], doc! { "_id": "bob", "spent": 5, "n": 1, "city": "Porto" });
        let unwound = run_pipeline(orders.clone(), &[doc! { "$unwind": "$items" }, doc! { "$count": "n" }], &none).unwrap();
        assert_eq!(unwound, vec![doc! { "n": 3 }]);
        let kept = run_pipeline(
            orders,
            &[doc! { "$unwind": { "path": "$items", "preserveNullAndEmptyArrays": true } }, doc! { "$count": "n" }],
            &none,
        )
        .unwrap();
        assert_eq!(kept, vec![doc! { "n": 4 }]);
    }

    #[test]
    fn pipeline_updates_keep_id() {
        let d = doc! { "_id": 7, "a": 1 };
        let out = apply_pipeline_update(&d, &[doc! { "$set": { "b": { "$add": ["$a", 1] } } }, doc! { "$unset": "a" }]).unwrap();
        assert_eq!(out, doc! { "_id": 7, "b": 2 });
        assert!(apply_pipeline_update(&d, &[doc! { "$match": {} }]).is_err());
    }
}
