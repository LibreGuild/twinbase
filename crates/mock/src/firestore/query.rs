//! Firestore query semantics: value ordering, field paths, filters, implicit ordering, cursors,
//! projections and aggregations.

use std::cmp::Ordering;
use std::collections::HashMap;

use prost_types::Timestamp;
use tonic::Status;
use twinbase_firestore_proto::v1::structured_query::{self as sq, Direction, FieldFilter, FieldReference, Filter, Order, UnaryFilter};
use twinbase_firestore_proto::v1::value::ValueType;
use twinbase_firestore_proto::v1::{ArrayValue, Cursor, MapValue, StructuredQuery, Value};

pub type Fields = HashMap<String, Value>;

/// A stored document as queries see it.
pub struct Row<'a> {
    /// Path relative to the database's documents root (`users/alice`).
    pub path: &'a str,
    pub fields: &'a Fields,
}

// ------------------------------------------------------------------ field paths

/// Split a field path (`a.b`, `` `odd.name`.c ``) into segments.
pub fn split_field_path(p: &str) -> Result<Vec<String>, Status> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '.' if !quoted => {
                if cur.is_empty() {
                    return Err(Status::invalid_argument(format!("Invalid field path: {p}")));
                }
                out.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    if quoted || cur.is_empty() {
        return Err(Status::invalid_argument(format!("Invalid field path: {p}")));
    }
    out.push(cur);
    Ok(out)
}

pub fn get_field<'a>(fields: &'a Fields, path: &[String]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut cur = fields.get(first)?;
    for seg in rest {
        match &cur.value_type {
            Some(ValueType::MapValue(m)) => cur = m.fields.get(seg)?,
            _ => return None,
        }
    }
    Some(cur)
}

pub fn set_field(fields: &mut Fields, path: &[String], value: Value) {
    let Some((first, rest)) = path.split_first() else { return };
    if rest.is_empty() {
        fields.insert(first.clone(), value);
        return;
    }
    let slot = fields.entry(first.clone()).or_insert_with(|| Value { value_type: Some(ValueType::MapValue(MapValue::default())) });
    if !matches!(slot.value_type, Some(ValueType::MapValue(_))) {
        *slot = Value { value_type: Some(ValueType::MapValue(MapValue::default())) };
    }
    if let Some(ValueType::MapValue(m)) = &mut slot.value_type {
        set_field(&mut m.fields, rest, value);
    }
}

pub fn remove_field(fields: &mut Fields, path: &[String]) {
    let Some((first, rest)) = path.split_first() else { return };
    if rest.is_empty() {
        fields.remove(first);
        return;
    }
    if let Some(Value { value_type: Some(ValueType::MapValue(m)) }) = fields.get_mut(first) {
        remove_field(&mut m.fields, rest);
    }
}

// ------------------------------------------------------------------ ordering

fn type_rank(v: &Value) -> u8 {
    match &v.value_type {
        None | Some(ValueType::NullValue(_)) => 0,
        Some(ValueType::BooleanValue(_)) => 1,
        Some(ValueType::IntegerValue(_) | ValueType::DoubleValue(_)) => 2,
        Some(ValueType::TimestampValue(_)) => 3,
        Some(ValueType::StringValue(_)) => 4,
        Some(ValueType::BytesValue(_)) => 5,
        Some(ValueType::ReferenceValue(_)) => 6,
        Some(ValueType::GeoPointValue(_)) => 7,
        Some(ValueType::ArrayValue(_)) => 8,
        Some(ValueType::MapValue(m)) if is_vector(m) => 9,
        Some(ValueType::MapValue(_)) => 10,
        Some(_) => 11,
    }
}

fn is_vector(m: &MapValue) -> bool {
    matches!(m.fields.get("__type__").and_then(|v| v.value_type.as_ref()), Some(ValueType::StringValue(s)) if s == "__vector__")
}

fn number(v: &Value) -> Option<f64> {
    match v.value_type {
        Some(ValueType::IntegerValue(i)) => Some(i as f64),
        Some(ValueType::DoubleValue(d)) => Some(d),
        _ => None,
    }
}

pub fn ts_cmp(a: &Timestamp, b: &Timestamp) -> Ordering {
    (a.seconds, a.nanos).cmp(&(b.seconds, b.nanos))
}

fn cmp_numbers(a: &Value, b: &Value) -> Ordering {
    match (&a.value_type, &b.value_type) {
        (Some(ValueType::IntegerValue(x)), Some(ValueType::IntegerValue(y))) => x.cmp(y),
        _ => {
            let (x, y) = (number(a).unwrap_or(f64::NAN), number(b).unwrap_or(f64::NAN));
            match (x.is_nan(), y.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            }
        }
    }
}

fn sorted_keys(m: &HashMap<String, Value>) -> Vec<&String> {
    let mut k: Vec<&String> = m.keys().collect();
    k.sort();
    k
}

/// Firestore's total order over values.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    let (ra, rb) = (type_rank(a), type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (&a.value_type, &b.value_type) {
        (Some(ValueType::BooleanValue(x)), Some(ValueType::BooleanValue(y))) => x.cmp(y),
        (Some(ValueType::IntegerValue(_) | ValueType::DoubleValue(_)), _) => cmp_numbers(a, b),
        (Some(ValueType::TimestampValue(x)), Some(ValueType::TimestampValue(y))) => ts_cmp(x, y),
        (Some(ValueType::StringValue(x)), Some(ValueType::StringValue(y))) => x.as_bytes().cmp(y.as_bytes()),
        (Some(ValueType::BytesValue(x)), Some(ValueType::BytesValue(y))) => x.cmp(y),
        (Some(ValueType::ReferenceValue(x)), Some(ValueType::ReferenceValue(y))) => x.split('/').cmp(y.split('/')),
        (Some(ValueType::GeoPointValue(x)), Some(ValueType::GeoPointValue(y))) => x
            .latitude
            .partial_cmp(&y.latitude)
            .unwrap_or(Ordering::Equal)
            .then(x.longitude.partial_cmp(&y.longitude).unwrap_or(Ordering::Equal)),
        (Some(ValueType::ArrayValue(x)), Some(ValueType::ArrayValue(y))) => {
            for (p, q) in x.values.iter().zip(&y.values) {
                let o = compare(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.values.len().cmp(&y.values.len())
        }
        (Some(ValueType::MapValue(x)), Some(ValueType::MapValue(y))) => {
            let (kx, ky) = (sorted_keys(&x.fields), sorted_keys(&y.fields));
            for (p, q) in kx.iter().zip(&ky) {
                let o = p.cmp(q).then_with(|| compare(&x.fields[*p], &y.fields[*q]));
                if o != Ordering::Equal {
                    return o;
                }
            }
            kx.len().cmp(&ky.len())
        }
        _ => Ordering::Equal,
    }
}

pub fn equal(a: &Value, b: &Value) -> bool {
    if is_nan(a) || is_nan(b) {
        return false;
    }
    type_rank(a) == type_rank(b) && compare(a, b) == Ordering::Equal
}

fn is_nan(v: &Value) -> bool {
    matches!(v.value_type, Some(ValueType::DoubleValue(d)) if d.is_nan())
}

fn is_null(v: &Value) -> bool {
    matches!(v.value_type, None | Some(ValueType::NullValue(_)))
}

// ------------------------------------------------------------------ filters

fn field_path(f: &Option<FieldReference>) -> Result<Vec<String>, Status> {
    let p = f.as_ref().map(|f| f.field_path.as_str()).unwrap_or("");
    if p == "__name__" {
        return Ok(vec!["__name__".into()]);
    }
    split_field_path(p)
}

/// The value a query sees at a field path; `__name__` is the document reference.
fn query_value(row: &Row, path: &[String], db: &str) -> Option<Value> {
    if path.len() == 1 && path[0] == "__name__" {
        return Some(Value { value_type: Some(ValueType::ReferenceValue(format!("{db}/documents/{}", row.path))) });
    }
    get_field(row.fields, path).cloned()
}

pub fn matches(filter: &Filter, row: &Row, db: &str) -> Result<bool, Status> {
    use sq::filter::FilterType;
    match &filter.filter_type {
        None => Ok(true),
        Some(FilterType::CompositeFilter(c)) => {
            let or = c.op == sq::composite_filter::Operator::Or as i32;
            for f in &c.filters {
                let m = matches(f, row, db)?;
                if or && m {
                    return Ok(true);
                }
                if !or && !m {
                    return Ok(false);
                }
            }
            Ok(!or || c.filters.is_empty())
        }
        Some(FilterType::FieldFilter(f)) => field_filter(f, row, db),
        Some(FilterType::UnaryFilter(u)) => unary_filter(u, row, db),
    }
}

fn field_filter(f: &FieldFilter, row: &Row, db: &str) -> Result<bool, Status> {
    use sq::field_filter::Operator as Op;
    let path = field_path(&f.field)?;
    let target = f.value.clone().unwrap_or_default();
    let Some(v) = query_value(row, &path, db) else { return Ok(false) };
    let same_type = type_rank(&v) == type_rank(&target);
    let op = Op::try_from(f.op).unwrap_or(Op::Unspecified);
    let list = |t: &Value| match &t.value_type {
        Some(ValueType::ArrayValue(a)) => Ok(a.values.clone()),
        _ => Err(Status::invalid_argument("A non-empty array is required for 'IN', 'NOT_IN' and 'ARRAY_CONTAINS_ANY' filters.")),
    };
    Ok(match op {
        Op::Equal => equal(&v, &target),
        Op::NotEqual => !is_null(&v) && !is_nan(&v) && !equal(&v, &target),
        Op::LessThan => same_type && !is_nan(&v) && compare(&v, &target) == Ordering::Less,
        Op::LessThanOrEqual => same_type && !is_nan(&v) && compare(&v, &target) != Ordering::Greater,
        Op::GreaterThan => same_type && !is_nan(&v) && compare(&v, &target) == Ordering::Greater,
        Op::GreaterThanOrEqual => same_type && !is_nan(&v) && compare(&v, &target) != Ordering::Less,
        Op::ArrayContains => matches!(&v.value_type, Some(ValueType::ArrayValue(a)) if a.values.iter().any(|x| equal(x, &target))),
        Op::In => list(&target)?.iter().any(|t| equal(&v, t)),
        Op::ArrayContainsAny => {
            let wanted = list(&target)?;
            matches!(&v.value_type, Some(ValueType::ArrayValue(a)) if a.values.iter().any(|x| wanted.iter().any(|t| equal(x, t))))
        }
        Op::NotIn => !is_null(&v) && !list(&target)?.iter().any(|t| equal(&v, t)),
        Op::Unspecified => return Err(Status::invalid_argument("Filter operator is unspecified")),
    })
}

fn unary_filter(u: &UnaryFilter, row: &Row, db: &str) -> Result<bool, Status> {
    use sq::unary_filter::{OperandType, Operator as Op};
    let Some(OperandType::Field(f)) = &u.operand_type else { return Err(Status::invalid_argument("Unary filter needs a field")) };
    let path = field_path(&Some(f.clone()))?;
    let v = query_value(row, &path, db);
    Ok(match Op::try_from(u.op).unwrap_or(Op::Unspecified) {
        Op::IsNan => v.as_ref().is_some_and(is_nan),
        Op::IsNull => v.as_ref().is_some_and(is_null),
        Op::IsNotNan => v.as_ref().is_some_and(|x| !is_nan(x) && !is_null(x)),
        Op::IsNotNull => v.as_ref().is_some_and(|x| !is_null(x)),
        Op::Unspecified => return Err(Status::invalid_argument("Filter operator is unspecified")),
    })
}

fn inequality_fields(filter: &Filter, out: &mut Vec<String>) {
    use sq::field_filter::Operator as Op;
    use sq::filter::FilterType;
    match &filter.filter_type {
        Some(FilterType::CompositeFilter(c)) => c.filters.iter().for_each(|f| inequality_fields(f, out)),
        Some(FilterType::FieldFilter(f)) => {
            let ineq = matches!(
                Op::try_from(f.op),
                Ok(Op::LessThan | Op::LessThanOrEqual | Op::GreaterThan | Op::GreaterThanOrEqual | Op::NotEqual | Op::NotIn)
            );
            if ineq
                && let Some(p) = f.field.as_ref().map(|f| f.field_path.clone())
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
        Some(FilterType::UnaryFilter(u)) => {
            use sq::unary_filter::{OperandType, Operator as UOp};
            if matches!(UOp::try_from(u.op), Ok(UOp::IsNotNan | UOp::IsNotNull))
                && let Some(OperandType::Field(f)) = &u.operand_type
                && !out.contains(&f.field_path)
            {
                out.push(f.field_path.clone());
            }
        }
        None => {}
    }
}

/// Explicit orders, then inequality fields not yet ordered, then `__name__`.
pub fn effective_orders(q: &StructuredQuery) -> Vec<(String, bool)> {
    let mut orders: Vec<(String, bool)> = q
        .order_by
        .iter()
        .map(|o: &Order| (o.field.as_ref().map(|f| f.field_path.clone()).unwrap_or_default(), o.direction == Direction::Descending as i32))
        .collect();
    let mut ineq = Vec::new();
    if let Some(w) = &q.r#where {
        inequality_fields(w, &mut ineq);
    }
    ineq.sort();
    let last_desc = orders.last().map(|(_, d)| *d).unwrap_or(false);
    for f in ineq {
        if !orders.iter().any(|(p, _)| *p == f) {
            orders.push((f, last_desc));
        }
    }
    if !orders.iter().any(|(p, _)| p == "__name__") {
        orders.push(("__name__".into(), last_desc));
    }
    orders
}

/// Order values of a row; `None` when an ordered field is missing (such rows are excluded).
pub fn order_key(row: &Row, orders: &[(String, bool)], db: &str) -> Result<Option<Vec<Value>>, Status> {
    let mut out = Vec::with_capacity(orders.len());
    for (p, _) in orders {
        let path = if p == "__name__" { vec!["__name__".into()] } else { split_field_path(p)? };
        match query_value(row, &path, db) {
            Some(v) => out.push(v),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

pub fn cmp_keys(a: &[Value], b: &[Value], orders: &[(String, bool)]) -> Ordering {
    for ((x, y), (_, desc)) in a.iter().zip(b).zip(orders) {
        let o = compare(x, y);
        let o = if *desc { o.reverse() } else { o };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Position of a key relative to a cursor (compared on the cursor's values only).
fn cmp_cursor(key: &[Value], cursor: &Cursor, orders: &[(String, bool)]) -> Ordering {
    let n = cursor.values.len().min(key.len());
    cmp_keys(&key[..n], &cursor.values[..n], &orders[..n])
}

pub fn after_start(key: &[Value], start: Option<&Cursor>, orders: &[(String, bool)]) -> bool {
    match start {
        None => true,
        Some(c) => {
            let o = cmp_cursor(key, c, orders);
            if c.before { o != Ordering::Less } else { o == Ordering::Greater }
        }
    }
}

pub fn before_end(key: &[Value], end: Option<&Cursor>, orders: &[(String, bool)]) -> bool {
    match end {
        None => true,
        Some(c) => {
            let o = cmp_cursor(key, c, orders);
            if c.before { o == Ordering::Less } else { o != Ordering::Greater }
        }
    }
}

/// Keep only the selected field paths (a `select` with no fields keeps none).
pub fn project(fields: &Fields, select: &sq::Projection) -> Result<Fields, Status> {
    let mut out = Fields::new();
    for f in &select.fields {
        if f.field_path == "__name__" {
            continue;
        }
        let path = split_field_path(&f.field_path)?;
        if let Some(v) = get_field(fields, &path) {
            set_field(&mut out, &path, v.clone());
        }
    }
    Ok(out)
}

/// `sum` keeps integers while they fit; otherwise it is a double, as in Firestore.
pub fn sum(values: &[&Value]) -> Value {
    let mut int: Option<i64> = Some(0);
    let mut dbl = 0.0f64;
    for v in values {
        match v.value_type {
            Some(ValueType::IntegerValue(i)) => {
                int = int.and_then(|s| s.checked_add(i));
                dbl += i as f64;
            }
            Some(ValueType::DoubleValue(d)) => {
                int = None;
                dbl += d;
            }
            _ => {}
        }
    }
    Value {
        value_type: Some(match int {
            Some(i) => ValueType::IntegerValue(i),
            None => ValueType::DoubleValue(dbl),
        }),
    }
}

pub fn avg(values: &[&Value]) -> Value {
    let nums: Vec<f64> = values.iter().filter_map(|v| number(v)).collect();
    if nums.is_empty() {
        return twinbase_firestore_proto::json::null();
    }
    Value { value_type: Some(ValueType::DoubleValue(nums.iter().sum::<f64>() / nums.len() as f64)) }
}

pub fn array(values: Vec<Value>) -> Value {
    Value { value_type: Some(ValueType::ArrayValue(ArrayValue { values })) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twinbase_firestore_proto::json::fields_from_plain;

    fn val(j: serde_json::Value) -> Value {
        twinbase_firestore_proto::json::from_plain(&j, "projects/p/databases/(default)").unwrap()
    }

    #[test]
    fn ordering_and_paths() {
        assert_eq!(compare(&val(serde_json::json!(1)), &val(serde_json::json!(1.5))), Ordering::Less);
        assert!(equal(&val(serde_json::json!(2)), &val(serde_json::json!(2.0))));
        assert_eq!(compare(&val(serde_json::json!(null)), &val(serde_json::json!(false))), Ordering::Less);
        assert_eq!(compare(&val(serde_json::json!({ "__double": "NaN" })), &val(serde_json::json!(-1e300))), Ordering::Less);
        assert_eq!(compare(&val(serde_json::json!("b")), &val(serde_json::json!(5))), Ordering::Greater);
        assert_eq!(split_field_path("a.`b.c`.d").unwrap(), vec!["a", "b.c", "d"]);
        assert!(split_field_path("a..b").is_err());
        let mut f = fields_from_plain(&serde_json::json!({ "a": { "b": 1 } }), "x").unwrap();
        set_field(&mut f, &["a".into(), "c".into()], val(serde_json::json!(2)));
        remove_field(&mut f, &["a".into(), "b".into()]);
        assert_eq!(twinbase_firestore_proto::json::fields_to_plain(&f), serde_json::json!({ "a": { "c": 2 } }));
        assert!(matches!(sum(&[&val(serde_json::json!(1)), &val(serde_json::json!(2))]).value_type, Some(ValueType::IntegerValue(3))));
        assert!(
            matches!(sum(&[&val(serde_json::json!(1)), &val(serde_json::json!(0.5))]).value_type, Some(ValueType::DoubleValue(d)) if d == 1.5)
        );
    }
}
