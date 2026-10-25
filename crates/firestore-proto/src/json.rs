//! Firestore values as JSON: the REST API's typed form (`{"integerValue": "1"}`) and a plain form
//! for people and seeds, where JSON types map directly and special values use markers:
//! `{"__timestamp": "2024-01-01T00:00:00Z"}`, `{"__ref": "users/alice"}`,
//! `{"__geo": [lat, lng]}`, `{"__bytes": "<base64>"}`, `{"__double": "NaN"}`.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as Json, json};

use crate::google::r#type::LatLng;
use crate::v1::value::ValueType;
use crate::v1::{ArrayValue, MapValue, Value};

pub fn timestamp_text(t: &prost_types::Timestamp) -> String {
    chrono::DateTime::from_timestamp(t.seconds, t.nanos.max(0) as u32)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_default()
}

pub fn parse_timestamp(s: &str) -> Result<prost_types::Timestamp, String> {
    let d = chrono::DateTime::parse_from_rfc3339(s).map_err(|e| format!("invalid timestamp \"{s}\": {e}"))?;
    Ok(prost_types::Timestamp { seconds: d.timestamp(), nanos: d.timestamp_subsec_nanos() as i32 })
}

fn v(t: ValueType) -> Value {
    Value { value_type: Some(t) }
}

pub fn null() -> Value {
    v(ValueType::NullValue(0))
}

// ------------------------------------------------------------------ REST form

pub fn to_rest(value: &Value) -> Json {
    match &value.value_type {
        None | Some(ValueType::NullValue(_)) => json!({ "nullValue": null }),
        Some(ValueType::BooleanValue(b)) => json!({ "booleanValue": b }),
        Some(ValueType::IntegerValue(i)) => json!({ "integerValue": i.to_string() }),
        Some(ValueType::DoubleValue(d)) => {
            if d.is_finite() {
                json!({ "doubleValue": d })
            } else {
                json!({ "doubleValue": if d.is_nan() { "NaN" } else if *d > 0.0 { "Infinity" } else { "-Infinity" } })
            }
        }
        Some(ValueType::TimestampValue(t)) => json!({ "timestampValue": timestamp_text(t) }),
        Some(ValueType::StringValue(s)) => json!({ "stringValue": s }),
        Some(ValueType::BytesValue(b)) => json!({ "bytesValue": B64.encode(b) }),
        Some(ValueType::ReferenceValue(r)) => json!({ "referenceValue": r }),
        Some(ValueType::GeoPointValue(g)) => json!({ "geoPointValue": { "latitude": g.latitude, "longitude": g.longitude } }),
        Some(ValueType::ArrayValue(a)) => json!({ "arrayValue": { "values": a.values.iter().map(to_rest).collect::<Vec<_>>() } }),
        Some(ValueType::MapValue(m)) => json!({ "mapValue": { "fields": fields_to_rest(&m.fields) } }),
        Some(ValueType::FieldReferenceValue(f)) => json!({ "fieldReferenceValue": f }),
        Some(other) => json!({ "unsupported": format!("{other:?}") }),
    }
}

pub fn fields_to_rest(fields: &HashMap<String, Value>) -> Json {
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    Json::Object(keys.into_iter().map(|k| (k.clone(), to_rest(&fields[k]))).collect())
}

pub fn from_rest(j: &Json) -> Result<Value, String> {
    let o = j
        .as_object()
        .filter(|o| o.len() == 1)
        .ok_or_else(|| format!("a value must be an object with one key such as stringValue, got {j}"))?;
    let (k, x) = o.iter().next().unwrap();
    Ok(v(match k.as_str() {
        "nullValue" => ValueType::NullValue(0),
        "booleanValue" => ValueType::BooleanValue(x.as_bool().ok_or("booleanValue must be true or false")?),
        "integerValue" => ValueType::IntegerValue(match x {
            Json::String(s) => s.parse().map_err(|_| format!("invalid integerValue {s}"))?,
            other => other.as_i64().ok_or_else(|| format!("invalid integerValue {other}"))?,
        }),
        "doubleValue" => ValueType::DoubleValue(match x {
            Json::String(s) => match s.as_str() {
                "NaN" => f64::NAN,
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                other => other.parse().map_err(|_| format!("invalid doubleValue {other}"))?,
            },
            other => other.as_f64().ok_or_else(|| format!("invalid doubleValue {other}"))?,
        }),
        "timestampValue" => ValueType::TimestampValue(parse_timestamp(x.as_str().ok_or("timestampValue must be a string")?)?),
        "stringValue" => ValueType::StringValue(x.as_str().ok_or("stringValue must be a string")?.to_owned()),
        "bytesValue" => ValueType::BytesValue(B64.decode(x.as_str().unwrap_or_default()).map_err(|_| "bytesValue must be base64")?),
        "referenceValue" => ValueType::ReferenceValue(x.as_str().ok_or("referenceValue must be a string")?.to_owned()),
        "geoPointValue" => ValueType::GeoPointValue(LatLng {
            latitude: x["latitude"].as_f64().unwrap_or(0.0),
            longitude: x["longitude"].as_f64().unwrap_or(0.0),
        }),
        "arrayValue" => ValueType::ArrayValue(ArrayValue {
            values: x
                .get("values")
                .and_then(Json::as_array)
                .map(|a| a.iter().map(from_rest).collect::<Result<_, _>>())
                .transpose()?
                .unwrap_or_default(),
        }),
        "mapValue" => ValueType::MapValue(MapValue { fields: fields_from_rest(x.get("fields").unwrap_or(&json!({})))? }),
        other => return Err(format!("unknown value type {other}")),
    }))
}

pub fn fields_from_rest(j: &Json) -> Result<HashMap<String, Value>, String> {
    let o = j.as_object().ok_or("fields must be an object")?;
    o.iter().map(|(k, x)| Ok((k.clone(), from_rest(x)?))).collect()
}

// ------------------------------------------------------------------ plain form

/// Relative path of a reference (`users/alice`) from a full document name.
fn relative(name: &str) -> &str {
    name.split_once("/documents/").map(|(_, p)| p).unwrap_or(name)
}

pub fn to_plain(value: &Value) -> Json {
    match &value.value_type {
        None | Some(ValueType::NullValue(_)) => Json::Null,
        Some(ValueType::BooleanValue(b)) => json!(b),
        Some(ValueType::IntegerValue(i)) => json!(i),
        Some(ValueType::DoubleValue(d)) => {
            if d.is_finite() {
                // Keep doubles distinguishable from integers: 1.0 stays 1.0.
                serde_json::Number::from_f64(*d).map(Json::Number).unwrap_or(Json::Null)
            } else {
                json!({ "__double": if d.is_nan() { "NaN" } else if *d > 0.0 { "Infinity" } else { "-Infinity" } })
            }
        }
        Some(ValueType::TimestampValue(t)) => json!({ "__timestamp": timestamp_text(t) }),
        Some(ValueType::StringValue(s)) => json!(s),
        Some(ValueType::BytesValue(b)) => json!({ "__bytes": B64.encode(b) }),
        Some(ValueType::ReferenceValue(r)) => json!({ "__ref": relative(r) }),
        Some(ValueType::GeoPointValue(g)) => json!({ "__geo": [g.latitude, g.longitude] }),
        Some(ValueType::ArrayValue(a)) => Json::Array(a.values.iter().map(to_plain).collect()),
        Some(ValueType::MapValue(m)) => fields_to_plain(&m.fields),
        Some(other) => json!(format!("{other:?}")),
    }
}

pub fn fields_to_plain(fields: &HashMap<String, Value>) -> Json {
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    Json::Object(keys.into_iter().map(|k| (k.clone(), to_plain(&fields[k]))).collect())
}

/// Plain JSON to a Firestore value. `database` (`projects/p/databases/d`) completes `__ref` paths.
pub fn from_plain(j: &Json, database: &str) -> Result<Value, String> {
    Ok(match j {
        Json::Null => null(),
        Json::Bool(b) => v(ValueType::BooleanValue(*b)),
        Json::Number(n) => match n.as_i64() {
            Some(i) if !n.to_string().contains(['.', 'e', 'E']) => v(ValueType::IntegerValue(i)),
            _ => v(ValueType::DoubleValue(n.as_f64().unwrap_or(0.0))),
        },
        Json::String(s) => v(ValueType::StringValue(s.clone())),
        Json::Array(a) => {
            v(ValueType::ArrayValue(ArrayValue { values: a.iter().map(|x| from_plain(x, database)).collect::<Result<_, _>>()? }))
        }
        Json::Object(o) => {
            if o.len() == 1 {
                let (k, x) = o.iter().next().unwrap();
                match k.as_str() {
                    "__timestamp" => {
                        return Ok(v(ValueType::TimestampValue(parse_timestamp(
                            x.as_str().ok_or("__timestamp needs an RFC 3339 string")?,
                        )?)));
                    }
                    "__ref" => {
                        let p = x.as_str().ok_or("__ref needs a document path")?;
                        let full =
                            if p.starts_with("projects/") { p.to_owned() } else { format!("{database}/documents/{}", p.trim_matches('/')) };
                        return Ok(v(ValueType::ReferenceValue(full)));
                    }
                    "__geo" => {
                        let a = x.as_array().filter(|a| a.len() == 2).ok_or("__geo needs [latitude, longitude]")?;
                        return Ok(v(ValueType::GeoPointValue(LatLng {
                            latitude: a[0].as_f64().unwrap_or(0.0),
                            longitude: a[1].as_f64().unwrap_or(0.0),
                        })));
                    }
                    "__bytes" => {
                        return Ok(v(ValueType::BytesValue(
                            B64.decode(x.as_str().unwrap_or_default()).map_err(|_| "__bytes needs base64")?,
                        )));
                    }
                    "__double" => {
                        let d = match x {
                            Json::String(s) if s == "NaN" => f64::NAN,
                            Json::String(s) if s == "Infinity" => f64::INFINITY,
                            Json::String(s) if s == "-Infinity" => f64::NEG_INFINITY,
                            other => other.as_f64().ok_or("__double needs a number or NaN/Infinity")?,
                        };
                        return Ok(v(ValueType::DoubleValue(d)));
                    }
                    _ => {}
                }
            }
            v(ValueType::MapValue(MapValue { fields: fields_from_plain(j, database)? }))
        }
    })
}

pub fn fields_from_plain(j: &Json, database: &str) -> Result<HashMap<String, Value>, String> {
    let o: &Map<String, Json> = j.as_object().ok_or("a document must be a JSON object")?;
    o.iter().map(|(k, x)| Ok((k.clone(), from_plain(x, database)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_and_plain_round_trips() {
        let db = "projects/p/databases/(default)";
        let plain = json!({
            "name": "Ann", "age": 31, "score": 1.0, "nan": { "__double": "NaN" }, "joined": { "__timestamp": "2024-05-01T10:00:00Z" },
            "friend": { "__ref": "users/bob" }, "where": { "__geo": [38.7, -9.1] }, "blob": { "__bytes": "aGk=" },
            "tags": ["a", 1, null], "nested": { "deep": { "x": true } }
        });
        let fields = fields_from_plain(&plain, db).unwrap();
        assert!(matches!(fields["age"].value_type, Some(ValueType::IntegerValue(31))));
        assert!(matches!(fields["score"].value_type, Some(ValueType::DoubleValue(d)) if d == 1.0));
        assert!(
            matches!(&fields["friend"].value_type, Some(ValueType::ReferenceValue(r)) if r == "projects/p/databases/(default)/documents/users/bob")
        );
        let back = fields_to_plain(&fields);
        assert_eq!(back["score"].to_string(), "1.0");
        assert_eq!(back["friend"], json!({ "__ref": "users/bob" }));
        assert_eq!(back["joined"], json!({ "__timestamp": "2024-05-01T10:00:00Z" }));
        let rest = fields_to_rest(&fields);
        assert_eq!(rest["age"], json!({ "integerValue": "31" }));
        assert_eq!(fields_from_rest(&rest).unwrap()["nested"], fields["nested"]);
        assert!(from_plain(&json!({ "__geo": [1] }), db).is_err());
    }
}
