//! DynamoDB attribute values (`{"S": "x"}`, `{"N": "1.5"}`, …): parsing, validation, ordering,
//! sizes and JSON encoding.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bigdecimal::BigDecimal;
use serde_json::{Map, Value, json};

use super::DynError;

pub type Item = BTreeMap<String, Av>;

#[derive(Debug, Clone)]
pub enum Av {
    S(String),
    N(BigDecimal),
    B(Vec<u8>),
    Ss(Vec<String>),
    Ns(Vec<BigDecimal>),
    Bs(Vec<Vec<u8>>),
    M(Item),
    L(Vec<Av>),
    Null,
    Bool(bool),
}

impl PartialEq for Av {
    fn eq(&self, other: &Self) -> bool {
        use Av::*;
        match (self, other) {
            (S(a), S(b)) => a == b,
            (N(a), N(b)) => a == b,
            (B(a), B(b)) => a == b,
            (Ss(a), Ss(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            (Ns(a), Ns(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            (Bs(a), Bs(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            (M(a), M(b)) => a == b,
            (L(a), L(b)) => a == b,
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            _ => false,
        }
    }
}

fn invalid(msg: impl Into<String>) -> DynError {
    DynError::validation(msg)
}

/// Parse a DynamoDB number: at most 38 significant digits, magnitude 1E-130 to 9.9…E+125.
pub fn parse_number(s: &str) -> Result<BigDecimal, DynError> {
    let t = s.trim();
    let n = BigDecimal::from_str(t).map_err(|_| invalid(format!("The parameter cannot be converted to a numeric value: {s}")))?;
    let n = n.normalized();
    if n != 0 {
        if n.digits() > 38 {
            return Err(invalid(format!("Attempting to store more than 38 significant digits in a Number: {s}")));
        }
        let (_, scale) = n.as_bigint_and_exponent();
        // value = digits × 10^-scale; its order of magnitude:
        let magnitude = n.digits() as i64 - 1 - scale;
        if magnitude > 125 {
            return Err(invalid(format!("Number overflow. Attempting to store a number with magnitude larger than supported range: {s}")));
        }
        if magnitude < -130 {
            return Err(invalid(format!(
                "Number underflow. Attempting to store a number with magnitude smaller than supported range: {s}"
            )));
        }
    }
    Ok(n)
}

pub fn number_text(n: &BigDecimal) -> String {
    n.normalized().to_plain_string()
}

impl Av {
    pub fn type_name(&self) -> &'static str {
        match self {
            Av::S(_) => "S",
            Av::N(_) => "N",
            Av::B(_) => "B",
            Av::Ss(_) => "SS",
            Av::Ns(_) => "NS",
            Av::Bs(_) => "BS",
            Av::M(_) => "M",
            Av::L(_) => "L",
            Av::Null => "NULL",
            Av::Bool(_) => "BOOL",
        }
    }

    pub fn from_json(v: &Value) -> Result<Av, DynError> {
        let Value::Object(o) = v else {
            return Err(invalid("Supplied AttributeValue is empty, must contain exactly one of the supported datatypes"));
        };
        if o.len() != 1 {
            return Err(invalid(
                "Supplied AttributeValue has more than one datatypes set, must contain exactly one of the supported datatypes",
            ));
        }
        let (t, x) = o.iter().next().unwrap();
        let str_of = |x: &Value| x.as_str().map(str::to_owned).ok_or_else(|| invalid(format!("Invalid {t} value")));
        let list = |x: &Value| x.as_array().cloned().ok_or_else(|| invalid(format!("Invalid {t} value")));
        let bytes = |x: &Value| -> Result<Vec<u8>, DynError> {
            B64.decode(x.as_str().unwrap_or_default()).map_err(|_| invalid("Invalid Binary value: not valid base64"))
        };
        Ok(match t.as_str() {
            "S" => Av::S(str_of(x)?),
            "N" => Av::N(parse_number(x.as_str().ok_or_else(|| invalid("Invalid N value"))?)?),
            "B" => Av::B(bytes(x)?),
            "BOOL" => Av::Bool(x.as_bool().ok_or_else(|| invalid("Invalid BOOL value"))?),
            "NULL" => {
                if x.as_bool() != Some(true) {
                    return Err(invalid(
                        "One or more parameter values were invalid: Null attribute value types must have the value of true",
                    ));
                }
                Av::Null
            }
            "SS" | "NS" | "BS" => {
                let items = list(x)?;
                if items.is_empty() {
                    return Err(invalid("One or more parameter values were invalid: An string set  may not be empty"));
                }
                let set = match t.as_str() {
                    "SS" => Av::Ss(items.iter().map(str_of).collect::<Result<_, _>>()?),
                    "NS" => Av::Ns(items.iter().map(|i| parse_number(i.as_str().unwrap_or_default())).collect::<Result<_, _>>()?),
                    _ => Av::Bs(items.iter().map(bytes).collect::<Result<_, _>>()?),
                };
                if set.set_len() != items.len() || set.has_duplicates() {
                    return Err(invalid("One or more parameter values were invalid: Input collection contains duplicates"));
                }
                set
            }
            "M" => {
                let Value::Object(m) = x else { return Err(invalid("Invalid M value")) };
                Av::M(m.iter().map(|(k, v)| Ok((k.clone(), Av::from_json(v)?))).collect::<Result<_, DynError>>()?)
            }
            "L" => Av::L(list(x)?.iter().map(Av::from_json).collect::<Result<_, _>>()?),
            other => return Err(invalid(format!("Supplied AttributeValue has an unsupported datatype: {other}"))),
        })
    }

    fn set_len(&self) -> usize {
        match self {
            Av::Ss(v) => v.len(),
            Av::Ns(v) => v.len(),
            Av::Bs(v) => v.len(),
            _ => 0,
        }
    }

    fn has_duplicates(&self) -> bool {
        match self {
            Av::Ss(v) => (1..v.len()).any(|i| v[..i].contains(&v[i])),
            Av::Ns(v) => (1..v.len()).any(|i| v[..i].contains(&v[i])),
            Av::Bs(v) => (1..v.len()).any(|i| v[..i].contains(&v[i])),
            _ => false,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Av::S(s) => json!({ "S": s }),
            Av::N(n) => json!({ "N": number_text(n) }),
            Av::B(b) => json!({ "B": B64.encode(b) }),
            Av::Ss(v) => json!({ "SS": v }),
            Av::Ns(v) => json!({ "NS": v.iter().map(number_text).collect::<Vec<_>>() }),
            Av::Bs(v) => json!({ "BS": v.iter().map(|b| B64.encode(b)).collect::<Vec<_>>() }),
            Av::M(m) => json!({ "M": item_json(m) }),
            Av::L(l) => json!({ "L": l.iter().map(Av::to_json).collect::<Vec<_>>() }),
            Av::Null => json!({ "NULL": true }),
            Av::Bool(b) => json!({ "BOOL": b }),
        }
    }

    /// Order of scalar key values (S, N, B); `None` for other types or mixed types.
    pub fn key_cmp(&self, other: &Av) -> Option<Ordering> {
        match (self, other) {
            (Av::S(a), Av::S(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Av::N(a), Av::N(b)) => Some(a.cmp(b)),
            (Av::B(a), Av::B(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// Approximate stored size in bytes (name sizes are added by [`item_size`]).
    pub fn size(&self) -> usize {
        match self {
            Av::S(s) => s.len(),
            Av::N(n) => (n.digits() as usize).div_ceil(2) + 1,
            Av::B(b) => b.len(),
            Av::Ss(v) => v.iter().map(String::len).sum(),
            Av::Ns(v) => v.iter().map(|n| (n.digits() as usize).div_ceil(2) + 1).sum(),
            Av::Bs(v) => v.iter().map(Vec::len).sum(),
            Av::M(m) => 3 + m.iter().map(|(k, v)| k.len() + v.size() + 1).sum::<usize>(),
            Av::L(l) => 3 + l.iter().map(|v| v.size() + 1).sum::<usize>(),
            Av::Null | Av::Bool(_) => 1,
        }
    }

    /// Canonical bytes of a key value, for maps keyed by partition key.
    pub fn key_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Av::S(s) => {
                out.push(b'S');
                out.extend_from_slice(s.as_bytes());
            }
            Av::N(n) => {
                out.push(b'N');
                out.extend_from_slice(number_text(n).as_bytes());
            }
            Av::B(b) => {
                out.push(b'B');
                out.extend_from_slice(b);
            }
            other => out.extend_from_slice(other.to_json().to_string().as_bytes()),
        }
        out
    }
}

pub fn item_from_json(v: &Value) -> Result<Item, DynError> {
    let Value::Object(o) = v else { return Err(invalid("Item must be a map of attribute values")) };
    o.iter().map(|(k, v)| Ok((k.clone(), Av::from_json(v)?))).collect()
}

pub fn item_json(item: &Item) -> Value {
    Value::Object(item.iter().map(|(k, v)| (k.clone(), v.to_json())).collect::<Map<_, _>>())
}

pub fn item_size(item: &Item) -> usize {
    item.iter().map(|(k, v)| k.len() + v.size()).sum()
}

/// A DynamoDB-typed value as plain JSON, for traffic text.
pub fn plain(v: &Av) -> Value {
    match v {
        Av::S(s) => json!(s),
        Av::N(n) => serde_json::from_str(&number_text(n)).unwrap_or_else(|_| json!(number_text(n))),
        Av::B(b) => json!(format!("<{} bytes>", b.len())),
        Av::Ss(v) => json!(v),
        Av::Ns(v) => Value::Array(v.iter().map(|n| plain(&Av::N(n.clone()))).collect()),
        Av::Bs(v) => json!(format!("<{} binary values>", v.len())),
        Av::M(m) => plain_item(m),
        Av::L(l) => Value::Array(l.iter().map(plain).collect()),
        Av::Null => Value::Null,
        Av::Bool(b) => json!(b),
    }
}

pub fn plain_item(item: &Item) -> Value {
    Value::Object(item.iter().map(|(k, v)| (k.clone(), plain(v))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_sets_and_round_trips() {
        assert_eq!(number_text(&parse_number("01.500").unwrap()), "1.5");
        assert_eq!(number_text(&parse_number("-0").unwrap()), "0");
        assert_eq!(number_text(&parse_number("1e3").unwrap()), "1000");
        assert!(parse_number("1".repeat(39).as_str()).is_err());
        assert!(parse_number("1e126").is_err());
        assert!(parse_number("1e-131").is_err());
        assert!(parse_number("abc").is_err());
        let v = json!({ "M": { "a": { "NS": ["1", "2.50"] }, "b": { "L": [{ "S": "x" }, { "NULL": true }, { "BOOL": false }] }, "c": { "B": "aGk=" } } });
        let av = Av::from_json(&v).unwrap();
        assert_eq!(
            av.to_json(),
            json!({ "M": { "a": { "NS": ["1", "2.5"] }, "b": { "L": [{ "S": "x" }, { "NULL": true }, { "BOOL": false }] }, "c": { "B": "aGk=" } } })
        );
        assert!(Av::from_json(&json!({ "SS": ["a", "a"] })).is_err());
        assert!(Av::from_json(&json!({ "SS": [] })).is_err());
        assert!(Av::from_json(&json!({ "S": "a", "N": "1" })).is_err());
        assert!(Av::from_json(&json!({ "NULL": false })).is_err());
        assert_eq!(Av::from_json(&json!({ "SS": ["a", "b"] })).unwrap(), Av::from_json(&json!({ "SS": ["b", "a"] })).unwrap());
        assert_eq!(Av::N(parse_number("2").unwrap()).key_cmp(&Av::N(parse_number("10").unwrap())), Some(Ordering::Less));
        assert_eq!(Av::S("2".into()).key_cmp(&Av::S("10".into())), Some(Ordering::Greater));
    }
}
