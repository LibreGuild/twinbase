//! MongoDB query semantics on BSON documents: value ordering, filters, update operators,
//! projections and sorting.

use std::cmp::Ordering;

use bson::{Bson, Document, doc};

/// A command error with a MongoDB error code.
#[derive(Debug, Clone, PartialEq)]
pub struct MongoError {
    pub code: i32,
    pub code_name: &'static str,
    pub message: String,
    /// Extra fields reported with the error (e.g. `keyPattern` / `keyValue` for duplicates).
    pub details: Option<Box<Document>>,
}

impl MongoError {
    pub fn new(code: i32, code_name: &'static str, message: impl Into<String>) -> Self {
        Self { code, code_name, message: message.into(), details: None }
    }

    pub fn bad_value(message: impl Into<String>) -> Self {
        Self::new(2, "BadValue", message)
    }

    pub fn failed_to_parse(message: impl Into<String>) -> Self {
        Self::new(9, "FailedToParse", message)
    }

    pub fn to_doc(&self) -> Document {
        let mut d = doc! { "ok": 0.0, "errmsg": self.message.clone(), "code": self.code, "codeName": self.code_name };
        if let Some(x) = &self.details {
            d.extend((**x).clone());
        }
        d
    }

    /// The entry of a `writeErrors` array.
    pub fn write_error(&self, index: usize) -> Document {
        let mut d = doc! { "index": index as i32, "code": self.code, "errmsg": self.message.clone() };
        if let Some(x) = &self.details {
            d.extend((**x).clone());
        }
        d
    }
}

pub type MResult<T> = Result<T, MongoError>;

// ------------------------------------------------------------------ ordering

/// Canonical type order used for sorting and comparisons across types.
fn type_rank(v: &Bson) -> u8 {
    match v {
        Bson::MinKey => 0,
        Bson::Null | Bson::Undefined => 1,
        Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => 2,
        Bson::Symbol(_) | Bson::String(_) => 3,
        Bson::Document(_) => 4,
        Bson::Array(_) => 5,
        Bson::Binary(_) => 6,
        Bson::ObjectId(_) => 7,
        Bson::Boolean(_) => 8,
        Bson::DateTime(_) => 9,
        Bson::Timestamp(_) => 10,
        Bson::RegularExpression(_) => 11,
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) | Bson::DbPointer(_) => 12,
        Bson::MaxKey => 13,
    }
}

pub fn as_f64(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(i) => Some(*i as f64),
        Bson::Int64(i) => Some(*i as f64),
        Bson::Double(d) => Some(*d),
        Bson::Decimal128(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

pub fn is_number(v: &Bson) -> bool {
    matches!(v, Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_))
}

/// MongoDB's total order over BSON values.
pub fn compare(a: &Bson, b: &Bson) -> Ordering {
    let (ra, rb) = (type_rank(a), type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (x, y) if is_number(x) && is_number(y) => {
            if let (Bson::Int64(i), Bson::Int64(j)) = (x, y) {
                return i.cmp(j);
            }
            as_f64(x).unwrap_or(0.0).partial_cmp(&as_f64(y).unwrap_or(0.0)).unwrap_or(Ordering::Equal)
        }
        (Bson::String(x) | Bson::Symbol(x), Bson::String(y) | Bson::Symbol(y)) => x.cmp(y),
        (Bson::Document(x), Bson::Document(y)) => {
            for ((ka, va), (kb, vb)) in x.iter().zip(y.iter()) {
                let c = compare(va, vb).then_with(|| ka.cmp(kb));
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (Bson::Array(x), Bson::Array(y)) => {
            for (va, vb) in x.iter().zip(y.iter()) {
                let c = compare(va, vb);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (Bson::Binary(x), Bson::Binary(y)) => x.bytes.len().cmp(&y.bytes.len()).then_with(|| x.bytes.cmp(&y.bytes)),
        (Bson::ObjectId(x), Bson::ObjectId(y)) => x.bytes().cmp(&y.bytes()),
        (Bson::Boolean(x), Bson::Boolean(y)) => x.cmp(y),
        (Bson::DateTime(x), Bson::DateTime(y)) => x.timestamp_millis().cmp(&y.timestamp_millis()),
        (Bson::Timestamp(x), Bson::Timestamp(y)) => (x.time, x.increment).cmp(&(y.time, y.increment)),
        (Bson::RegularExpression(x), Bson::RegularExpression(y)) => {
            (x.pattern.as_str(), x.options.as_str()).cmp(&(y.pattern.as_str(), y.options.as_str()))
        }
        _ => Ordering::Equal,
    }
}

pub fn values_equal(a: &Bson, b: &Bson) -> bool {
    type_rank(a) == type_rank(b) && compare(a, b) == Ordering::Equal
}

/// Canonical bytes of a value for hashing (`_id` lookups, unique indexes, `$group` keys):
/// values that compare equal produce the same key (1, 1.0 and NumberLong(1) are one key).
pub fn key_of(v: &Bson) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    write_key(v, &mut out);
    out
}

fn write_key(v: &Bson, out: &mut Vec<u8>) {
    out.push(type_rank(v));
    match v {
        x if is_number(x) => {
            let f = as_f64(x).unwrap_or(0.0);
            // Integers stay exact beyond 2^53 when they are 64-bit.
            match x {
                Bson::Int64(i) => out.extend_from_slice(&i.to_be_bytes()),
                _ if f.fract() == 0.0 && f.abs() < 9.0e18 => out.extend_from_slice(&(f as i64).to_be_bytes()),
                _ => out.extend_from_slice(&f.to_bits().to_be_bytes()),
            }
        }
        Bson::String(s) | Bson::Symbol(s) => {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        Bson::Document(d) => {
            for (k, v) in d {
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                write_key(v, out);
            }
            out.push(0xff);
        }
        Bson::Array(a) => {
            for v in a {
                write_key(v, out);
            }
            out.push(0xff);
        }
        Bson::Binary(b) => {
            out.extend_from_slice(&(b.bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(&b.bytes);
        }
        Bson::ObjectId(o) => out.extend_from_slice(&o.bytes()),
        Bson::Boolean(b) => out.push(*b as u8),
        Bson::DateTime(d) => out.extend_from_slice(&d.timestamp_millis().to_be_bytes()),
        Bson::Timestamp(t) => {
            out.extend_from_slice(&t.time.to_be_bytes());
            out.extend_from_slice(&t.increment.to_be_bytes());
        }
        Bson::RegularExpression(r) => {
            out.extend_from_slice(r.pattern.as_str().as_bytes());
            out.push(0);
            out.extend_from_slice(r.options.as_str().as_bytes());
        }
        other => out.extend_from_slice(format!("{other:?}").as_bytes()),
    }
}

// ------------------------------------------------------------------ paths

/// Values reachable at a dotted path. Arrays on the way are traversed element by element; a
/// numeric segment also indexes into arrays.
pub fn path_values<'a>(doc: &'a Document, path: &str) -> Vec<&'a Bson> {
    fn walk<'a>(v: &'a Bson, parts: &[&str], out: &mut Vec<&'a Bson>) {
        let Some((head, rest)) = parts.split_first() else {
            out.push(v);
            return;
        };
        match v {
            Bson::Document(d) => {
                if let Some(next) = d.get(*head) {
                    walk(next, rest, out);
                }
            }
            Bson::Array(items) => {
                if let Ok(i) = head.parse::<usize>() {
                    if let Some(next) = items.get(i) {
                        walk(next, rest, out);
                    }
                } else {
                    for item in items {
                        if matches!(item, Bson::Document(_)) {
                            walk(item, parts, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = Vec::new();
    if let Some(first) = doc.get(parts[0]) {
        walk(first, &parts[1..], &mut out);
    }
    out
}

/// The single value at a path (first match), as used by sort and projection.
pub fn lookup<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut cur = doc;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        let v = cur.get(*p)?;
        if i + 1 == parts.len() {
            return Some(v);
        }
        match v {
            Bson::Document(d) => cur = d,
            Bson::Array(a) => {
                let idx: usize = parts.get(i + 1)?.parse().ok()?;
                let item = a.get(idx)?;
                if i + 2 == parts.len() {
                    return Some(item);
                }
                match item {
                    Bson::Document(d) => {
                        // Skip the index segment.
                        return lookup(d, &parts[i + 2..].join("."));
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    None
}

// ------------------------------------------------------------------ filters

pub fn bson_type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::Symbol(_) => "symbol",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

fn type_number(name: &str) -> Option<i32> {
    Some(match name {
        "double" => 1,
        "string" => 2,
        "object" => 3,
        "array" => 4,
        "binData" => 5,
        "undefined" => 6,
        "objectId" => 7,
        "bool" => 8,
        "date" => 9,
        "null" => 10,
        "regex" => 11,
        "javascript" => 13,
        "int" => 16,
        "timestamp" => 17,
        "long" => 18,
        "decimal" => 19,
        "minKey" => -1,
        "maxKey" => 127,
        _ => return None,
    })
}

fn type_matches(v: &Bson, spec: &Bson) -> bool {
    let name = bson_type_name(v);
    match spec {
        Bson::String(s) if s == "number" => is_number(v),
        Bson::String(s) => s == name,
        Bson::Array(a) => a.iter().any(|s| type_matches(v, s)),
        other => as_f64(other).map(|n| type_number(name) == Some(n as i32)).unwrap_or(false),
    }
}

fn regex_from(spec: &Bson, options: Option<&str>) -> MResult<regex::Regex> {
    let (pattern, opts) = match spec {
        Bson::RegularExpression(r) => (r.pattern.to_string(), r.options.to_string()),
        Bson::String(s) => (s.clone(), String::new()),
        _ => return Err(MongoError::bad_value("$regex has to be a string")),
    };
    let opts = options.map(str::to_owned).unwrap_or(opts);
    let mut b = regex::RegexBuilder::new(&pattern);
    b.case_insensitive(opts.contains('i'))
        .multi_line(opts.contains('m'))
        .dot_matches_new_line(opts.contains('s'))
        .ignore_whitespace(opts.contains('x'));
    b.size_limit(1 << 20).build().map_err(|e| MongoError::bad_value(format!("invalid regex: {e}")))
}

/// Candidate values for leaf comparisons: each value at the path, plus elements of arrays.
fn leaf_candidates<'a>(values: &[&'a Bson]) -> Vec<&'a Bson> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        out.push(*v);
        if let Bson::Array(items) = v {
            out.extend(items.iter());
        }
    }
    out
}

fn is_operator_doc(d: &Document) -> bool {
    d.keys().next().is_some_and(|k| k.starts_with('$'))
}

/// Does `doc` match the query `filter`?
pub fn matches(doc: &Document, filter: &Document) -> MResult<bool> {
    for (key, cond) in filter {
        let ok = match key.as_str() {
            "$and" => logical(doc, cond)?.iter().all(|b| *b),
            "$or" => logical(doc, cond)?.iter().any(|b| *b),
            "$nor" => !logical(doc, cond)?.iter().any(|b| *b),
            "$comment" => true,
            "$expr" => crate::mongo::agg::truthy(&crate::mongo::agg::eval(cond, doc)?),
            k if k.starts_with('$') => return Err(MongoError::bad_value(format!("unknown top level operator: {k}"))),
            path => field_matches(doc, path, cond)?,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn logical(doc: &Document, cond: &Bson) -> MResult<Vec<bool>> {
    let Bson::Array(items) = cond else { return Err(MongoError::bad_value("$and/$or/$nor must be a nonempty array")) };
    items
        .iter()
        .map(|i| match i {
            Bson::Document(d) => matches(doc, d),
            _ => Err(MongoError::bad_value("$and/$or/$nor entries need to be full objects")),
        })
        .collect()
}

fn field_matches(doc: &Document, path: &str, cond: &Bson) -> MResult<bool> {
    let values = path_values(doc, path);
    match cond {
        Bson::Document(ops) if is_operator_doc(ops) => {
            let regex_options = ops.get("$options").and_then(Bson::as_str);
            for (op, arg) in ops {
                if op == "$options" {
                    continue;
                }
                if !operator_matches(&values, op, arg, regex_options)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Bson::RegularExpression(_) => {
            let re = regex_from(cond, None)?;
            Ok(leaf_candidates(&values).iter().any(|v| v.as_str().is_some_and(|s| re.is_match(s))))
        }
        _ => Ok(eq_matches(&values, cond)),
    }
}

fn eq_matches(values: &[&Bson], target: &Bson) -> bool {
    if values.is_empty() {
        return matches!(target, Bson::Null);
    }
    leaf_candidates(values).iter().any(|v| values_equal(v, target))
}

fn cmp_matches(values: &[&Bson], target: &Bson, ok: impl Fn(Ordering) -> bool) -> bool {
    leaf_candidates(values).iter().any(|v| type_rank(v) == type_rank(target) && ok(compare(v, target)))
}

fn operator_matches(values: &[&Bson], op: &str, arg: &Bson, regex_options: Option<&str>) -> MResult<bool> {
    Ok(match op {
        "$eq" => eq_matches(values, arg),
        "$ne" => !eq_matches(values, arg),
        "$gt" => cmp_matches(values, arg, |o| o == Ordering::Greater),
        "$gte" => cmp_matches(values, arg, |o| o != Ordering::Less),
        "$lt" => cmp_matches(values, arg, |o| o == Ordering::Less),
        "$lte" => cmp_matches(values, arg, |o| o != Ordering::Greater),
        "$in" => {
            let Bson::Array(set) = arg else { return Err(MongoError::bad_value("$in needs an array")) };
            set.iter().any(|t| match t {
                Bson::RegularExpression(_) => {
                    regex_from(t, None).is_ok_and(|re| leaf_candidates(values).iter().any(|v| v.as_str().is_some_and(|s| re.is_match(s))))
                }
                _ => eq_matches(values, t),
            })
        }
        "$nin" => {
            let Bson::Array(set) = arg else { return Err(MongoError::bad_value("$nin needs an array")) };
            !set.iter().any(|t| eq_matches(values, t))
        }
        "$exists" => {
            let want = match arg {
                Bson::Boolean(b) => *b,
                other => as_f64(other).map(|n| n != 0.0).unwrap_or(true),
            };
            !values.is_empty() == want
        }
        "$type" => leaf_candidates(values).iter().any(|v| type_matches(v, arg)),
        "$regex" => {
            let re = regex_from(arg, regex_options)?;
            leaf_candidates(values).iter().any(|v| v.as_str().is_some_and(|s| re.is_match(s)))
        }
        "$size" => {
            let n = as_f64(arg).ok_or_else(|| MongoError::bad_value("$size needs a number"))? as usize;
            values.iter().any(|v| matches!(v, Bson::Array(a) if a.len() == n))
        }
        "$all" => {
            let Bson::Array(want) = arg else { return Err(MongoError::bad_value("$all needs an array")) };
            !want.is_empty() && want.iter().all(|w| eq_matches(values, w))
        }
        "$elemMatch" => {
            let Bson::Document(q) = arg else { return Err(MongoError::bad_value("$elemMatch needs an Object")) };
            let mut hit = false;
            for v in values {
                if let Bson::Array(items) = v {
                    for item in items {
                        let ok = match item {
                            Bson::Document(d) if !is_operator_doc(q) => matches(d, q)?,
                            other => {
                                // {$elemMatch: {$gte: 80, $lt: 85}} on scalar elements.
                                let one = [other];
                                let mut all = true;
                                for (o, a) in q {
                                    if !operator_matches(&one, o, a, None)? {
                                        all = false;
                                        break;
                                    }
                                }
                                all
                            }
                        };
                        if ok {
                            hit = true;
                            break;
                        }
                    }
                }
            }
            hit
        }
        "$not" => match arg {
            Bson::Document(inner) => {
                let mut all = true;
                for (o, a) in inner {
                    if !operator_matches(values, o, a, inner.get("$options").and_then(Bson::as_str))? {
                        all = false;
                        break;
                    }
                }
                !all
            }
            Bson::RegularExpression(_) => {
                let re = regex_from(arg, None)?;
                !leaf_candidates(values).iter().any(|v| v.as_str().is_some_and(|s| re.is_match(s)))
            }
            _ => return Err(MongoError::bad_value("$not needs a regex or a document")),
        },
        "$mod" => {
            let Bson::Array(a) = arg else { return Err(MongoError::bad_value("malformed mod, needs to be an array")) };
            let (Some(d), Some(r)) = (a.first().and_then(as_f64), a.get(1).and_then(as_f64)) else {
                return Err(MongoError::bad_value("malformed mod"));
            };
            if d == 0.0 {
                return Err(MongoError::bad_value("divisor cannot be 0"));
            }
            leaf_candidates(values).iter().any(|v| as_f64(v).is_some_and(|n| (n.trunc() as i64) % (d as i64) == r as i64))
        }
        "$comment" => true,
        other => return Err(MongoError::bad_value(format!("unknown operator: {other}"))),
    })
}

// ------------------------------------------------------------------ sort & projection

/// Sort key value: arrays sort by their smallest (ascending) or largest (descending) element.
fn sort_value(doc: &Document, path: &str, descending: bool) -> Bson {
    let values = path_values(doc, path);
    let mut cands =
        leaf_candidates(&values).into_iter().filter(|v| !matches!(v, Bson::Array(a) if !a.is_empty())).cloned().collect::<Vec<_>>();
    if cands.is_empty() {
        return Bson::Null;
    }
    cands.sort_by(compare);
    if descending { cands.pop().unwrap_or(Bson::Null) } else { cands.swap_remove(0) }
}

pub fn sort_docs(docs: &mut [Document], spec: &Document) {
    sort_by_spec(docs, spec, |d| d);
}

/// Stable sort by a MongoDB sort specification; sort keys are computed once per item.
pub fn sort_by_spec<T>(items: &mut [T], spec: &Document, doc: impl Fn(&T) -> &Document) {
    if spec.is_empty() || items.len() < 2 {
        return;
    }
    let keys: Vec<(String, bool)> = spec.iter().map(|(k, v)| (k.clone(), as_f64(v).unwrap_or(1.0) < 0.0)).collect();
    let mut keyed: Vec<(Vec<Bson>, usize)> =
        items.iter().enumerate().map(|(i, it)| (keys.iter().map(|(k, desc)| sort_value(doc(it), k, *desc)).collect(), i)).collect();
    keyed.sort_by(|(a, _), (b, _)| {
        for ((x, y), (_, desc)) in a.iter().zip(b).zip(&keys) {
            let o = compare(x, y);
            let o = if *desc { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    // Apply the permutation in place.
    let mut order: Vec<usize> = keyed.into_iter().map(|(_, i)| i).collect();
    for i in 0..order.len() {
        let mut j = order[i];
        while j < i {
            j = order[j];
        }
        items.swap(i, j);
        order[i] = j;
    }
}

/// Apply a find projection (inclusion or exclusion; `_id` included unless excluded).
pub fn project(doc: &Document, spec: &Document) -> MResult<Document> {
    if spec.is_empty() {
        return Ok(doc.clone());
    }
    let truthy = |v: &Bson| match v {
        Bson::Boolean(b) => *b,
        other => as_f64(other).map(|n| n != 0.0).unwrap_or(true),
    };
    let fields: Vec<(&String, bool)> = spec.iter().map(|(k, v)| (k, truthy(v))).collect();
    let inclusion = fields.iter().any(|(k, inc)| *inc && k.as_str() != "_id");
    let exclusion = fields.iter().any(|(k, inc)| !*inc && k.as_str() != "_id");
    if inclusion && exclusion {
        return Err(MongoError::new(31254, "Location31254", "Cannot do exclusion on a field in inclusion projection"));
    }
    if inclusion {
        let mut out = Document::new();
        let keep_id = spec.get("_id").map(truthy).unwrap_or(true);
        if keep_id && let Some(id) = doc.get("_id") {
            out.insert("_id", id.clone());
        }
        for (k, inc) in &fields {
            if *inc
                && k.as_str() != "_id"
                && let Some(v) = lookup(doc, k)
            {
                set_path(&mut out, k, v.clone());
            }
        }
        Ok(out)
    } else {
        let mut out = doc.clone();
        for (k, inc) in &fields {
            if !*inc {
                unset_path(&mut out, k);
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ updates

pub fn set_path(doc: &mut Document, path: &str, value: Bson) {
    let mut parts = path.splitn(2, '.');
    let head = parts.next().unwrap_or_default();
    match parts.next() {
        None => {
            doc.insert(head, value);
        }
        Some(rest) => {
            let entry = doc.entry(head.to_owned()).or_insert_with(|| Bson::Document(Document::new()));
            match entry {
                Bson::Document(d) => set_path(d, rest, value),
                Bson::Array(a) => set_in_array(a, rest, value),
                other => {
                    let mut d = Document::new();
                    set_path(&mut d, rest, value);
                    *other = Bson::Document(d);
                }
            }
        }
    }
}

fn set_in_array(a: &mut Vec<Bson>, path: &str, value: Bson) {
    let mut parts = path.splitn(2, '.');
    let head = parts.next().unwrap_or_default();
    let Ok(idx) = head.parse::<usize>() else { return };
    while a.len() <= idx {
        a.push(Bson::Null);
    }
    match parts.next() {
        None => a[idx] = value,
        Some(rest) => match &mut a[idx] {
            Bson::Document(d) => set_path(d, rest, value),
            slot => {
                let mut d = Document::new();
                set_path(&mut d, rest, value);
                *slot = Bson::Document(d);
            }
        },
    }
}

pub fn unset_path(doc: &mut Document, path: &str) {
    let mut parts = path.splitn(2, '.');
    let head = parts.next().unwrap_or_default();
    match parts.next() {
        None => {
            doc.remove(head);
        }
        Some(rest) => match doc.get_mut(head) {
            Some(Bson::Document(d)) => unset_path(d, rest),
            Some(Bson::Array(a)) => {
                if let Ok(i) = rest.split('.').next().unwrap_or_default().parse::<usize>() {
                    let tail: Vec<&str> = rest.splitn(2, '.').collect();
                    if tail.len() == 1 {
                        if i < a.len() {
                            a[i] = Bson::Null;
                        }
                    } else if let Some(Bson::Document(d)) = a.get_mut(i) {
                        unset_path(d, tail[1]);
                    }
                }
            }
            _ => {}
        },
    }
}

fn get_path_mut<'a>(doc: &'a mut Document, path: &str) -> Option<&'a mut Bson> {
    let mut parts = path.splitn(2, '.');
    let head = parts.next()?;
    match parts.next() {
        None => doc.get_mut(head),
        Some(rest) => match doc.get_mut(head)? {
            Bson::Document(d) => get_path_mut(d, rest),
            Bson::Array(a) => {
                let mut p = rest.splitn(2, '.');
                let i: usize = p.next()?.parse().ok()?;
                match p.next() {
                    None => a.get_mut(i),
                    Some(r) => match a.get_mut(i)? {
                        Bson::Document(d) => get_path_mut(d, r),
                        _ => None,
                    },
                }
            }
            _ => None,
        },
    }
}

fn add_numbers(a: &Bson, b: &Bson, multiply: bool) -> MResult<Bson> {
    let op = |x: f64, y: f64| if multiply { x * y } else { x + y };
    Ok(match (a, b) {
        (Bson::Int32(x), Bson::Int32(y)) => {
            let r = if multiply { (*x as i64) * (*y as i64) } else { (*x as i64) + (*y as i64) };
            i32::try_from(r).map(Bson::Int32).unwrap_or(Bson::Int64(r))
        }
        (Bson::Int32(_) | Bson::Int64(_), Bson::Int32(_) | Bson::Int64(_)) => {
            let (x, y) = (as_f64(a).unwrap() as i64, as_f64(b).unwrap() as i64);
            Bson::Int64(if multiply { x.wrapping_mul(y) } else { x.wrapping_add(y) })
        }
        (x, y) if is_number(x) && is_number(y) => Bson::Double(op(as_f64(x).unwrap(), as_f64(y).unwrap())),
        _ => return Err(MongoError::new(14, "TypeMismatch", "Cannot apply $inc/$mul to a value of non-numeric type")),
    })
}

fn each_values(arg: &Bson) -> (Vec<Bson>, Option<&Document>) {
    match arg {
        Bson::Document(d) if d.contains_key("$each") => (d.get_array("$each").cloned().unwrap_or_default(), Some(d)),
        other => (vec![other.clone()], None),
    }
}

/// Is this an update with operators (vs. a replacement document)?
pub fn is_update_operators(update: &Document) -> bool {
    update.keys().next().is_some_and(|k| k.starts_with('$'))
}

/// Apply update operators to `doc`. `inserting` enables `$setOnInsert`.
pub fn apply_update(doc: &mut Document, update: &Document, inserting: bool) -> MResult<()> {
    for (op, fields) in update {
        let Bson::Document(fields) = fields else {
            return Err(MongoError::failed_to_parse(format!(
                "Modifiers operate on fields but we found type {} instead",
                bson_type_name(fields)
            )));
        };
        for (path, arg) in fields {
            if path == "_id" && op != "$setOnInsert" && op != "$set" {
                return Err(MongoError::new(
                    66,
                    "ImmutableField",
                    "Performing an update on the path '_id' would modify the immutable field '_id'",
                ));
            }
            match op.as_str() {
                "$set" => {
                    if path == "_id" && doc.get("_id").is_some_and(|id| !values_equal(id, arg)) {
                        return Err(MongoError::new(
                            66,
                            "ImmutableField",
                            "Performing an update on the path '_id' would modify the immutable field '_id'",
                        ));
                    }
                    set_path(doc, path, arg.clone());
                }
                "$setOnInsert" => {
                    if inserting {
                        set_path(doc, path, arg.clone());
                    }
                }
                "$unset" => unset_path(doc, path),
                "$inc" | "$mul" => {
                    let cur = lookup(doc, path).cloned();
                    let new = match cur {
                        None | Some(Bson::Null) if op == "$inc" => arg.clone(),
                        None | Some(Bson::Null) => match arg {
                            Bson::Int32(_) => Bson::Int32(0),
                            Bson::Int64(_) => Bson::Int64(0),
                            _ => Bson::Double(0.0),
                        },
                        Some(v) => add_numbers(&v, arg, op == "$mul")?,
                    };
                    set_path(doc, path, new);
                }
                "$min" | "$max" => {
                    let cur = lookup(doc, path).cloned();
                    let replace = match &cur {
                        None => true,
                        Some(v) => {
                            let o = compare(arg, v);
                            if op == "$min" { o == Ordering::Less } else { o == Ordering::Greater }
                        }
                    };
                    if replace {
                        set_path(doc, path, arg.clone());
                    }
                }
                "$rename" => {
                    let Bson::String(to) = arg else { return Err(MongoError::bad_value("$rename target must be a string")) };
                    if let Some(v) = lookup(doc, path).cloned() {
                        unset_path(doc, path);
                        set_path(doc, to, v);
                    }
                }
                "$currentDate" => {
                    let as_ts = matches!(arg, Bson::Document(d) if d.get_str("$type").is_ok_and(|t| t == "timestamp"));
                    let now = bson::DateTime::now();
                    set_path(
                        doc,
                        path,
                        if as_ts {
                            Bson::Timestamp(bson::Timestamp { time: (now.timestamp_millis() / 1000) as u32, increment: 1 })
                        } else {
                            Bson::DateTime(now)
                        },
                    );
                }
                "$push" | "$addToSet" => {
                    let (mut items, opts) = each_values(arg);
                    let cur = lookup(doc, path).cloned().unwrap_or(Bson::Array(vec![]));
                    let Bson::Array(mut arr) = cur else {
                        return Err(MongoError::bad_value(format!(
                            "The field '{path}' must be an array but is of type {}",
                            bson_type_name(&cur)
                        )));
                    };
                    if op == "$addToSet" {
                        for it in items {
                            if !arr.iter().any(|x| values_equal(x, &it)) {
                                arr.push(it);
                            }
                        }
                    } else {
                        let position = opts.and_then(|o| o.get("$position")).and_then(as_f64);
                        match position {
                            Some(p) => {
                                let at = if p < 0.0 { arr.len().saturating_sub((-p) as usize) } else { (p as usize).min(arr.len()) };
                                for (i, it) in items.drain(..).enumerate() {
                                    arr.insert(at + i, it);
                                }
                            }
                            None => arr.append(&mut items),
                        }
                        if let Some(Bson::Document(sort)) = opts.and_then(|o| o.get("$sort")) {
                            let mut docs: Vec<Document> = arr.iter().filter_map(|x| x.as_document().cloned()).collect();
                            if docs.len() == arr.len() {
                                sort_docs(&mut docs, sort);
                                arr = docs.into_iter().map(Bson::Document).collect();
                            }
                        } else if let Some(dir) = opts.and_then(|o| o.get("$sort")).and_then(as_f64) {
                            arr.sort_by(|a, b| if dir < 0.0 { compare(b, a) } else { compare(a, b) });
                        }
                        if let Some(n) = opts.and_then(|o| o.get("$slice")).and_then(as_f64) {
                            let n = n as i64;
                            if n >= 0 {
                                arr.truncate(n as usize);
                            } else {
                                let keep = (-n) as usize;
                                if arr.len() > keep {
                                    arr.drain(..arr.len() - keep);
                                }
                            }
                        }
                    }
                    set_path(doc, path, Bson::Array(arr));
                }
                "$pull" | "$pullAll" => {
                    if let Some(Bson::Array(arr)) = get_path_mut(doc, path) {
                        match (op.as_str(), arg) {
                            ("$pullAll", Bson::Array(vals)) => arr.retain(|x| !vals.iter().any(|v| values_equal(x, v))),
                            (_, Bson::Document(cond)) => {
                                let mut keep = Vec::with_capacity(arr.len());
                                for x in arr.drain(..) {
                                    let hit = match &x {
                                        Bson::Document(d) if !is_operator_doc(cond) => matches(d, cond)?,
                                        other => {
                                            let one = [other];
                                            let mut all = true;
                                            for (o, a) in cond {
                                                if !operator_matches(&one, o, a, None)? {
                                                    all = false;
                                                    break;
                                                }
                                            }
                                            all
                                        }
                                    };
                                    if !hit {
                                        keep.push(x);
                                    }
                                }
                                *arr = keep;
                            }
                            (_, v) => arr.retain(|x| !values_equal(x, v)),
                        }
                    }
                }
                "$pop" => {
                    if let Some(Bson::Array(arr)) = get_path_mut(doc, path) {
                        if as_f64(arg).unwrap_or(1.0) < 0.0 {
                            if !arr.is_empty() {
                                arr.remove(0);
                            }
                        } else {
                            arr.pop();
                        }
                    }
                }
                other => return Err(MongoError::failed_to_parse(format!("Unknown modifier: {other}"))),
            }
        }
    }
    Ok(())
}

/// Equality fields of a filter, used to seed documents created by upserts.
pub fn upsert_seed(filter: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in filter {
        match k.as_str() {
            "$and" => {
                if let Bson::Array(items) = v {
                    for i in items {
                        if let Bson::Document(d) = i {
                            for (kk, vv) in upsert_seed(d) {
                                set_path(&mut out, &kk, vv);
                            }
                        }
                    }
                }
            }
            k if k.starts_with('$') => {}
            path => match v {
                Bson::Document(d) if is_operator_doc(d) => {
                    if let Some(eq) = d.get("$eq") {
                        set_path(&mut out, path, eq.clone());
                    }
                }
                other => set_path(&mut out, path, other.clone()),
            },
        }
    }
    out
}

// ------------------------------------------------------------------ positional updates

/// Rewrite update paths that use `$`, `$[]` or `$[name]` into concrete array indexes for `doc`.
pub fn resolve_positional(
    doc: &Document,
    update: &Document,
    filter: &Document,
    array_filters: &[Document],
    upserting: bool,
) -> MResult<Document> {
    if !update.values().any(|f| f.as_document().is_some_and(|f| f.keys().any(|k| k.contains('$')))) {
        return Ok(update.clone());
    }
    let mut out = Document::new();
    for (op, fields) in update {
        let Bson::Document(fields) = fields else {
            out.insert(op.clone(), fields.clone());
            continue;
        };
        let mut resolved = Document::new();
        for (path, arg) in fields {
            if !path.contains('$') {
                resolved.insert(path.clone(), arg.clone());
                continue;
            }
            if upserting {
                if path.split('.').any(|p| p == "$") {
                    return Err(MongoError::new(2, "BadValue", "The positional operator did not find the match needed from the query."));
                }
                continue;
            }
            for concrete in expand_path(&Bson::Document(doc.clone()), &path.split('.').collect::<Vec<_>>(), "", filter, array_filters)? {
                resolved.insert(concrete, arg.clone());
            }
        }
        out.insert(op.clone(), resolved);
    }
    Ok(out)
}

fn expand_path(v: &Bson, parts: &[&str], prefix: &str, filter: &Document, array_filters: &[Document]) -> MResult<Vec<String>> {
    let Some((head, rest)) = parts.split_first() else { return Ok(vec![prefix.to_owned()]) };
    let join = |p: &str, seg: &str| if p.is_empty() { seg.to_owned() } else { format!("{p}.{seg}") };
    let positional = *head == "$" || head.starts_with("$[");
    if !positional {
        let next = match v {
            Bson::Document(d) => d.get(*head).cloned().unwrap_or(Bson::Null),
            Bson::Array(a) => head.parse::<usize>().ok().and_then(|i| a.get(i).cloned()).unwrap_or(Bson::Null),
            _ => Bson::Null,
        };
        if !rest.iter().any(|p| p.contains('$')) {
            return Ok(vec![parts.iter().fold(prefix.to_owned(), |acc, p| join(&acc, p))]);
        }
        return expand_path(&next, rest, &join(prefix, head), filter, array_filters);
    }
    let Bson::Array(items) = v else {
        return Err(MongoError::new(
            2,
            "BadValue",
            format!("The path '{prefix}' must exist in the document in order to apply array updates."),
        ));
    };
    let indexes: Vec<usize> = if *head == "$[]" {
        (0..items.len()).collect()
    } else if *head == "$" {
        // First element matched by the query conditions on this array.
        let conds: Document = filter
            .iter()
            .filter(|(k, _)| k.as_str() == prefix || k.starts_with(&format!("{prefix}.")))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut hit = None;
        for (i, item) in items.iter().enumerate() {
            let probe = Document::from_iter([(prefix.to_owned(), Bson::Array(vec![item.clone()]))]);
            let mut nested = Document::new();
            set_path(&mut nested, prefix, Bson::Array(vec![item.clone()]));
            if !conds.is_empty() && (matches(&nested, &conds)? || matches(&probe, &conds)?) {
                hit = Some(i);
                break;
            }
        }
        match hit {
            Some(i) => vec![i],
            None => return Err(MongoError::new(2, "BadValue", "The positional operator did not find the match needed from the query.")),
        }
    } else {
        let name = &head[2..head.len() - 1];
        let cond = array_filters.iter().find(|f| f.keys().any(|k| k == name || k.starts_with(&format!("{name}.")))).ok_or_else(|| {
            MongoError::new(2, "BadValue", format!("No array filter found for identifier '{name}' in path '{}'", join(prefix, head)))
        })?;
        let mut idx = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if matches(&Document::from_iter([(name.to_owned(), item.clone())]), cond)? {
                idx.push(i);
            }
        }
        idx
    };
    let mut out = Vec::new();
    for i in indexes {
        out.extend(expand_path(&items[i], rest, &join(prefix, &i.to_string()), filter, array_filters)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;

    fn m(d: &Document, f: Document) -> bool {
        matches(d, &f).unwrap()
    }

    #[test]
    fn ordering_across_types_and_numbers() {
        assert_eq!(compare(&Bson::Int32(3), &Bson::Double(2.5)), Ordering::Greater);
        assert_eq!(compare(&Bson::Int64(2), &Bson::Int32(2)), Ordering::Equal);
        assert_eq!(compare(&Bson::Null, &Bson::Int32(0)), Ordering::Less);
        assert_eq!(compare(&Bson::String("a".into()), &Bson::Int32(9)), Ordering::Greater);
        assert!(values_equal(&Bson::Int32(1), &Bson::Double(1.0)));
    }

    #[test]
    fn filters_with_operators_paths_and_arrays() {
        let d = doc! { "name": "Ann", "age": 31, "tags": ["red", "blue"], "address": { "city": "Lisbon" },
        "orders": [ { "total": 10, "status": "paid" }, { "total": 99, "status": "open" } ], "nick": null };
        assert!(m(&d, doc! { "name": "Ann", "age": { "$gte": 30, "$lt": 40 } }));
        assert!(m(&d, doc! { "tags": "red" }));
        assert!(m(&d, doc! { "tags": ["red", "blue"] }));
        assert!(!m(&d, doc! { "tags": ["blue", "red"] }));
        assert!(m(&d, doc! { "address.city": "Lisbon" }));
        assert!(m(&d, doc! { "orders.total": { "$gt": 50 } }));
        assert!(m(&d, doc! { "orders": { "$elemMatch": { "total": { "$gt": 50 }, "status": "open" } } }));
        assert!(!m(&d, doc! { "orders": { "$elemMatch": { "total": { "$gt": 50 }, "status": "paid" } } }));
        assert!(m(&d, doc! { "missing": null }));
        assert!(m(&d, doc! { "nick": { "$exists": true } }));
        assert!(m(&d, doc! { "missing": { "$exists": false } }));
        assert!(m(&d, doc! { "$or": [ { "age": 1 }, { "name": { "$regex": "^an", "$options": "i" } } ] }));
        assert!(m(&d, doc! { "tags": { "$size": 2, "$all": ["blue"] } }));
        assert!(m(&d, doc! { "age": { "$in": [1, 31] }, "name": { "$nin": ["Bo"] } }));
        assert!(m(&d, doc! { "age": { "$not": { "$gt": 40 } } }));
        assert!(m(&d, doc! { "age": { "$type": "number" }, "name": { "$type": "string" } }));
        assert!(m(&d, doc! { "age": { "$mod": [10, 1] } }));
        assert!(!m(&d, doc! { "tags": { "$ne": "red" } }));
        assert!(matches(&d, &doc! { "$bogus": 1 }).is_err());
    }

    #[test]
    fn update_operators() {
        let mut d = doc! { "_id": 1, "n": 1, "tags": ["a"], "nested": { "x": 1 }, "list": [1, 2, 3, 2] };
        apply_update(
            &mut d,
            &doc! {
                "$inc": { "n": 2, "fresh": 5 }, "$set": { "nested.y": "y", "deep.a.b": true },
                "$push": { "tags": { "$each": ["b", "c"], "$slice": -2 } }, "$addToSet": { "set": "x" },
                "$pull": { "list": 2 }, "$rename": { "nested.x": "renamed" }, "$min": { "low": 3 }
            },
            false,
        )
        .unwrap();
        assert_eq!(d.get_i32("n").unwrap(), 3);
        assert_eq!(d.get_i32("fresh").unwrap(), 5);
        assert_eq!(lookup(&d, "nested.y"), Some(&Bson::String("y".into())));
        assert_eq!(lookup(&d, "deep.a.b"), Some(&Bson::Boolean(true)));
        assert_eq!(d.get_array("tags").unwrap(), &vec![Bson::String("b".into()), Bson::String("c".into())]);
        assert_eq!(d.get_array("list").unwrap(), &vec![Bson::Int32(1), Bson::Int32(3)]);
        assert_eq!(d.get_i32("renamed").unwrap(), 1);
        assert!(apply_update(&mut d, &doc! { "$set": { "_id": 2 } }, false).is_err());
        assert!(apply_update(&mut d, &doc! { "$inc": { "tags": 1 } }, false).is_err());
    }

    #[test]
    fn sort_permutation_matches_reference() {
        let mut seed = 12345u64;
        for len in [0usize, 1, 2, 3, 7, 50, 257] {
            let mut docs: Vec<Document> = (0..len)
                .map(|i| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    doc! { "k": ((seed >> 33) % 13) as i32, "i": i as i32 }
                })
                .collect();
            let mut expected = docs.clone();
            expected.sort_by_key(|d| (-(d.get_i32("k").unwrap()), d.get_i32("i").unwrap()));
            sort_docs(&mut docs, &doc! { "k": -1 });
            assert_eq!(docs, expected, "len {len}");
        }
    }

    #[test]
    fn positional_updates() {
        let d = doc! { "items": [ { "sku": "a", "qty": 1 }, { "sku": "b", "qty": 5 }, { "sku": "c", "qty": 9 } ], "grades": [80, 95, 70] };
        let u = resolve_positional(&d, &doc! { "$set": { "items.$.qty": 0 } }, &doc! { "items.sku": "b" }, &[], false).unwrap();
        assert_eq!(u, doc! { "$set": { "items.1.qty": 0 } });
        let u = resolve_positional(&d, &doc! { "$inc": { "items.$[].qty": 1 } }, &doc! {}, &[], false).unwrap();
        assert_eq!(u, doc! { "$inc": { "items.0.qty": 1, "items.1.qty": 1, "items.2.qty": 1 } });
        let u = resolve_positional(&d, &doc! { "$set": { "items.$[big].sku": "X" } }, &doc! {}, &[doc! { "big.qty": { "$gt": 4 } }], false)
            .unwrap();
        assert_eq!(u, doc! { "$set": { "items.1.sku": "X", "items.2.sku": "X" } });
        let u = resolve_positional(&d, &doc! { "$set": { "grades.$": 100 } }, &doc! { "grades": { "$gte": 90 } }, &[], false).unwrap();
        assert_eq!(u, doc! { "$set": { "grades.1": 100 } });
        assert!(resolve_positional(&d, &doc! { "$set": { "items.$[x].a": 1 } }, &doc! {}, &[], false).is_err());
        let mut d2 = d.clone();
        apply_update(
            &mut d2,
            &resolve_positional(&d, &doc! { "$set": { "items.$.qty": 0 } }, &doc! { "items.sku": "c" }, &[], false).unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(lookup(&d2, "items.2.qty"), Some(&Bson::Int32(0)));
    }

    #[test]
    fn projection_sort_and_upsert_seed() {
        let d = doc! { "_id": ObjectId::new(), "a": 1, "b": { "c": 2, "d": 3 }, "e": 4 };
        let p = project(&d, &doc! { "a": 1, "b.c": 1, "_id": 0 }).unwrap();
        assert_eq!(p, doc! { "a": 1, "b": { "c": 2 } });
        let x = project(&d, &doc! { "b": 0, "e": 0 }).unwrap();
        assert_eq!(x.keys().collect::<Vec<_>>(), vec!["_id", "a"]);
        assert!(project(&d, &doc! { "a": 1, "e": 0 }).is_err());
        let mut docs = vec![doc! { "n": 2 }, doc! { "n": 1 }, doc! {}, doc! { "n": [5, 0] }];
        sort_docs(&mut docs, &doc! { "n": 1 });
        assert_eq!(docs[0], doc! {});
        assert_eq!(docs[1], doc! { "n": [5, 0] });
        let seed = upsert_seed(&doc! { "email": "a@x.io", "age": { "$gt": 3 }, "$and": [ { "plan": { "$eq": "pro" } } ] });
        assert_eq!(seed, doc! { "email": "a@x.io", "plan": "pro" });
    }
}
