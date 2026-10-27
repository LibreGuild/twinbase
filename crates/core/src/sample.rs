//! Realistic sample data for prototyping: values follow column names and types, respect
//! uniqueness and NOT NULL, and reference existing rows for foreign keys.

use rand::Rng;
use rand::seq::IndexedRandom;
use serde::Serialize;
use serde_json::{Value, json};
use twinbase_client::sqlgen::{literal, qualified, quote_ident};
use twinbase_client::{ColumnInfo, SqlClient, TableSchema};

use crate::error::{CoreError, Result};

const FIRST: &[&str] = &[
    "Ava", "Liam", "Maya", "Noah", "Zara", "Ethan", "Priya", "Lucas", "Amara", "Kenji", "Sofia", "Mateo", "Leila", "Omar", "Hana", "Ivan",
];
const LAST: &[&str] =
    &["Patel", "Garcia", "Nguyen", "Smith", "Kim", "Okafor", "Rossi", "Müller", "Silva", "Cohen", "Tanaka", "Haddad", "Novak", "Reddy"];
const CITIES: &[&str] = &["Lisbon", "Austin", "Pune", "Osaka", "Nairobi", "Toronto", "Berlin", "Bogotá", "Melbourne", "Seoul", "Dublin"];
const COUNTRIES: &[&str] =
    &["Portugal", "United States", "India", "Japan", "Kenya", "Canada", "Germany", "Colombia", "Australia", "Korea", "Ireland"];
const WORDS: &[&str] = &[
    "alpha", "bright", "cedar", "delta", "ember", "falcon", "garden", "harbor", "island", "juniper", "kepler", "lumen", "meadow", "nova",
    "orbit", "pixel", "quartz", "river", "summit", "tundra", "umbra", "vertex", "willow", "zenith",
];
const STATUSES: &[&str] = &["active", "pending", "inactive"];

#[derive(Debug, Clone, Serialize)]
pub struct GenerateResult {
    pub table: String,
    pub inserted: u64,
}

fn sentence(rng: &mut impl Rng, words: usize) -> String {
    let mut s: Vec<String> = (0..words).map(|_| WORDS.choose(rng).unwrap().to_string()).collect();
    if let Some(f) = s.first_mut() {
        let mut c = f.chars();
        *f = c.next().map(|x| x.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    }
    s.join(" ")
}

fn max_len(data_type: &str) -> Option<usize> {
    let open = data_type.find('(')?;
    let close = data_type.find(')')?;
    data_type[open + 1..close].split(',').next()?.trim().parse().ok()
}

fn enum_values(data_type: &str) -> Option<Vec<String>> {
    let lower = data_type.to_ascii_lowercase();
    if !lower.starts_with("enum(") {
        return None;
    }
    Some(data_type[5..data_type.len() - 1].split(',').map(|v| v.trim().trim_matches('\'').to_owned()).collect())
}

fn value_for(c: &ColumnInfo, row: usize, rng: &mut impl Rng, unique: bool) -> Value {
    let name = c.name.to_ascii_lowercase();
    let ty = c.data_type.to_ascii_lowercase();
    let first = *FIRST.choose(rng).unwrap();
    let last = *LAST.choose(rng).unwrap();
    let tag: u32 = rng.random_range(1000..9999);
    let text = |s: String| -> Value {
        let s = match max_len(&ty) {
            Some(n) if s.chars().count() > n => s.chars().take(n).collect(),
            _ => s,
        };
        Value::String(s)
    };
    if let Some(values) = enum_values(&c.data_type) {
        return Value::String(values.choose(rng).cloned().unwrap_or_default());
    }
    // pgvector embeddings: unit-length random vectors of the declared size.
    if ty.starts_with("vector") || ty.starts_with("halfvec") || ty.starts_with("sparsevec") {
        let dims = max_len(&ty).unwrap_or(3).clamp(1, 4096);
        let raw: Vec<f64> = (0..dims).map(|_| rng.random_range(-1.0..1.0)).collect();
        let norm = raw.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-9);
        let list = raw.iter().map(|x| format!("{:.4}", x / norm)).collect::<Vec<_>>().join(",");
        return Value::String(format!("[{list}]"));
    }
    if ty.contains("bool") || ty == "tinyint(1)" || name.starts_with("is_") || name.starts_with("has_") {
        return json!(rng.random_bool(0.7));
    }
    if ty.contains("uuid") {
        return Value::String(uuid::Uuid::new_v4().to_string());
    }
    if ty.contains("json") {
        return json!({ "source": "twinbase", "n": row + 1, "tags": [WORDS.choose(rng).unwrap()] }).to_string().into();
    }
    if ty.contains("timestamp") || ty.contains("datetime") || ty == "date" {
        // Deadlines and expiries lie ahead; everything else (created, updated, …) in the past.
        let days_back = if name.contains("birth") || name == "dob" {
            rng.random_range(18 * 365..70 * 365)
        } else if name.contains("due") || name.contains("expire") || name.contains("deadline") || name.starts_with("valid_until") {
            -rng.random_range(1..120)
        } else {
            rng.random_range(0..365)
        };
        let ts = chrono::Utc::now() - chrono::Duration::days(days_back) - chrono::Duration::seconds(rng.random_range(0..86_400));
        return Value::String(if ty == "date" { ts.format("%Y-%m-%d").to_string() } else { ts.format("%Y-%m-%d %H:%M:%S").to_string() });
    }
    if ty.starts_with("time") {
        return Value::String(format!("{:02}:{:02}:00", rng.random_range(8..20), rng.random_range(0..60)));
    }
    let numeric = ty.contains("int")
        || ty.contains("serial")
        || ty.contains("numeric")
        || ty.contains("decimal")
        || ty.contains("float")
        || ty.contains("double")
        || ty.contains("real")
        || ty == "year";
    if numeric {
        let integer = ty.contains("int") || ty.contains("serial") || ty == "year";
        if unique {
            return json!(1_000_000 + row as i64 * 7 + rng.random_range(0..7));
        }
        let v: f64 = if name.contains("price")
            || name.contains("amount")
            || name.contains("total")
            || name.contains("cost")
            || name.contains("balance")
        {
            (rng.random_range(100..50_000) as f64) / 100.0
        } else if name.contains("age") {
            rng.random_range(18..80) as f64
        } else if name.contains("rating") || name.contains("stars") {
            rng.random_range(1..=5) as f64
        } else if name.contains("percent") || name.contains("score") {
            rng.random_range(0..=100) as f64
        } else if name.contains("year") {
            rng.random_range(1990..2026) as f64
        } else if name.contains("lat") {
            rng.random_range(-60_000..60_000) as f64 / 1000.0
        } else if name.contains("lon") || name.contains("lng") {
            rng.random_range(-170_000..170_000) as f64 / 1000.0
        } else {
            rng.random_range(0..1000) as f64
        };
        return if integer || ty.contains("int") { json!(v.round() as i64) } else { json!((v * 100.0).round() / 100.0) };
    }
    let v = if name.contains("email") {
        format!(
            "{}.{}{}@example.com",
            first.to_lowercase(),
            last.to_lowercase().replace('ü', "u"),
            if unique { format!(".{row}") } else { String::new() }
        )
    } else if name == "first_name" || name == "firstname" || name == "given_name" {
        first.to_owned()
    } else if name == "last_name" || name == "lastname" || name == "surname" || name == "family_name" {
        last.to_owned()
    } else if name.contains("username") || name == "login" || name == "handle" {
        format!("{}{}", first.to_lowercase(), tag)
    } else if name == "name"
        || name.contains("full_name")
        || name.contains("display_name")
        || name.ends_with("_name") && name.contains("contact")
    {
        format!("{first} {last}")
    } else if name.contains("phone") || name.contains("mobile") {
        format!("+1-555-{:04}", rng.random_range(0..10_000))
    } else if name.contains("city") {
        CITIES.choose(rng).unwrap().to_string()
    } else if name.contains("country") {
        COUNTRIES.choose(rng).unwrap().to_string()
    } else if name.contains("address") || name.contains("street") {
        format!("{} {} Street", rng.random_range(1..999), WORDS.choose(rng).unwrap())
    } else if name.contains("zip") || name.contains("postal") {
        format!("{:05}", rng.random_range(10_000..99_999))
    } else if name.contains("url") || name.contains("website") || name.contains("link") {
        format!("https://example.com/{}/{}", WORDS.choose(rng).unwrap(), tag)
    } else if name.contains("color") || name.contains("colour") {
        format!("#{:06x}", rng.random_range(0..0xFFFFFF))
    } else if name == "status" || name.ends_with("_status") || name == "state" {
        STATUSES.choose(rng).unwrap().to_string()
    } else if name.contains("description")
        || name.contains("bio")
        || name.contains("body")
        || name.contains("content")
        || name.contains("notes")
        || name.contains("comment")
    {
        let n = rng.random_range(6..14);
        format!("{}.", sentence(rng, n))
    } else if name.contains("title") || name.contains("subject") || name.contains("headline") {
        let n = rng.random_range(2..5);
        sentence(rng, n)
    } else if name.contains("sku") || name.contains("code") || name.contains("slug") {
        format!("{}-{tag}", WORDS.choose(rng).unwrap())
    } else {
        format!("{} {}", WORDS.choose(rng).unwrap(), tag)
    };
    let v = if unique && !name.contains("email") { format!("{v}-{row}") } else { v };
    text(v)
}

/// Insert `count` generated rows into `table`.
pub async fn generate(client: &dyn SqlClient, schema: &str, table: &str, count: usize) -> Result<GenerateResult> {
    let count = count.clamp(1, 10_000);
    let info: TableSchema = client.describe(schema, table).await?;
    let d = client.dialect();
    // Existing parent keys for foreign-key columns.
    let mut parents: Vec<(String, Vec<Value>)> = Vec::new();
    for fk in &info.foreign_keys {
        if fk.columns.len() != 1 {
            continue;
        }
        let sql = format!(
            "SELECT DISTINCT {} FROM {} LIMIT 1000",
            quote_ident(d, &fk.ref_columns[0]),
            qualified(d, if fk.ref_schema.is_empty() { schema } else { &fk.ref_schema }, &fk.ref_table)
        );
        let keys: Vec<Value> = client
            .query(&sql, 1000)
            .await?
            .pop()
            .map(|r| r.rows.into_iter().filter_map(|row| row.into_iter().next()).filter(|v| !v.is_null()).collect())
            .unwrap_or_default();
        let nullable = info.columns.iter().find(|c| c.name == fk.columns[0]).is_none_or(|c| c.nullable);
        if keys.is_empty() && !nullable {
            return Err(CoreError::invalid(format!(
                "\"{table}\".\"{}\" references \"{}\", which has no rows yet. Generate data for \"{}\" first.",
                fk.columns[0], fk.ref_table, fk.ref_table
            )));
        }
        parents.push((fk.columns[0].clone(), keys));
    }
    let unique_cols: Vec<String> = info.indexes.iter().filter(|i| i.unique && i.columns.len() == 1).map(|i| i.columns[0].clone()).collect();
    let cols: Vec<&ColumnInfo> = info.columns.iter().filter(|c| !c.auto).collect();
    // Multi-column keys (e.g. order_items(order_id, product_id)): rows must not repeat a
    // combination already in the table or generated earlier in this batch.
    let mut composite: Vec<Vec<String>> =
        info.indexes.iter().filter(|i| i.unique && i.columns.len() > 1).map(|i| i.columns.clone()).collect();
    if info.primary_key.len() > 1 && !composite.contains(&info.primary_key) {
        composite.push(info.primary_key.clone());
    }
    composite.retain(|k| k.iter().all(|c| cols.iter().any(|x| &x.name == c)));
    let mut seen: Vec<std::collections::HashSet<Vec<String>>> = Vec::new();
    for key in &composite {
        let sql = format!(
            "SELECT {} FROM {} LIMIT 20000",
            key.iter().map(|c| quote_ident(d, c)).collect::<Vec<_>>().join(", "),
            qualified(d, schema, table)
        );
        let existing = client.query(&sql, 20_000).await?.pop().map(|r| r.rows).unwrap_or_default();
        seen.push(existing.into_iter().map(|row| row.iter().map(|v| literal(d, v)).collect()).collect());
    }
    let key_positions: Vec<Vec<usize>> =
        composite.iter().map(|k| k.iter().map(|c| cols.iter().position(|x| &x.name == c).unwrap()).collect()).collect();
    // ThreadRng is not Send, so generate everything before the next await.
    let statements = {
        let mut rng = rand::rng();
        let base: usize = rng.random_range(0..1_000_000);
        let mut statements = Vec::new();
        for chunk in (0..count).collect::<Vec<_>>().chunks(100) {
            let mut rows = Vec::new();
            for &i in chunk {
                // A few tries to find a combination not used yet; give up on the row otherwise.
                for _attempt in 0..25 {
                    let vals: Vec<String> = cols
                        .iter()
                        .map(|c| {
                            if let Some((_, keys)) = parents.iter().find(|(col, _)| col == &c.name) {
                                return keys.choose(&mut rng).map(|k| literal(d, k)).unwrap_or_else(|| "NULL".into());
                            }
                            // Columns of a composite key are unique together, not one by one.
                            let unique = unique_cols.contains(&c.name) || (c.primary_key && info.primary_key.len() == 1);
                            if c.nullable && !unique && c.default.is_none() && rng.random_bool(0.08) {
                                return "NULL".into();
                            }
                            literal(d, &value_for(c, base + i, &mut rng, unique))
                        })
                        .collect();
                    let tuples: Vec<Vec<String>> = key_positions.iter().map(|p| p.iter().map(|&j| vals[j].clone()).collect()).collect();
                    if tuples.iter().zip(&seen).any(|(t, s)| s.contains(t)) {
                        continue;
                    }
                    for (t, s) in tuples.into_iter().zip(seen.iter_mut()) {
                        s.insert(t);
                    }
                    rows.push(format!("({})", vals.join(", ")));
                    break;
                }
            }
            if rows.is_empty() {
                continue;
            }
            let names = cols.iter().map(|c| quote_ident(d, &c.name)).collect::<Vec<_>>().join(", ");
            statements.push(if cols.is_empty() {
                format!("INSERT INTO {} DEFAULT VALUES", qualified(d, schema, table))
            } else {
                format!("INSERT INTO {} ({names}) VALUES {}", qualified(d, schema, table), rows.join(", "))
            });
        }
        statements
    };
    let inserted = client.execute_in_transaction(&statements, None).await?;
    Ok(GenerateResult { table: table.to_owned(), inserted })
}
