//! DynamoDB tables, items and indexes, the API operations on them, and JSON snapshots.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use parking_lot::RwLock;
use serde_json::{Map, Value, json};

use super::DynError;
use super::expr::{self, Cond, Placeholders};
use super::value::{Av, Item, item_from_json, item_json, item_size};

const MAX_ITEM: usize = 400 * 1024;
const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct KeyDef {
    pub hash: String,
    pub range: Option<String>,
}

impl KeyDef {
    fn names(&self) -> Vec<String> {
        std::iter::once(self.hash.clone()).chain(self.range.clone()).collect()
    }

    fn schema_json(&self) -> Value {
        let mut v = vec![json!({ "AttributeName": self.hash, "KeyType": "HASH" })];
        if let Some(r) = &self.range {
            v.push(json!({ "AttributeName": r, "KeyType": "RANGE" }));
        }
        Value::Array(v)
    }
}

#[derive(Debug, Clone)]
pub struct IndexDef {
    pub name: String,
    pub key: KeyDef,
    /// `ALL`, `KEYS_ONLY` or `INCLUDE`.
    pub projection: String,
    pub non_key: Vec<String>,
    pub global: bool,
    pub throughput: Option<Value>,
}

/// Sort-key order inside a partition.
#[derive(Debug, Clone)]
struct SortKey(Av);

impl PartialEq for SortKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for SortKey {}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.key_cmp(&other.0).unwrap_or(Ordering::Equal)
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub key: KeyDef,
    pub attr_defs: BTreeMap<String, String>,
    pub gsis: Vec<IndexDef>,
    pub lsis: Vec<IndexDef>,
    pub billing: String,
    pub throughput: Value,
    pub created: f64,
    pub ttl_attr: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub stream: Option<Value>,
    pub deletion_protection: bool,
    partitions: BTreeMap<Vec<u8>, BTreeMap<SortKey, Item>>,
    count: usize,
    bytes: usize,
}

fn not_found(table: &str) -> DynError {
    DynError::new("ResourceNotFoundException", format!("Requested resource not found: Table: {table} not found"))
}

fn str_of<'a>(req: &'a Value, k: &str) -> Option<&'a str> {
    req.get(k).and_then(Value::as_str)
}

fn need_str<'a>(req: &'a Value, k: &str) -> Result<&'a str, DynError> {
    str_of(req, k).filter(|s| !s.is_empty()).ok_or_else(|| {
        DynError::validation(format!(
            "1 validation error detected: Value null at '{}' failed to satisfy constraint: Member must not be null",
            lower_first(k)
        ))
    })
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn key_def(schema: &Value) -> Result<KeyDef, DynError> {
    let a = schema.as_array().filter(|a| !a.is_empty() && a.len() <= 2).ok_or_else(|| DynError::validation("1 validation error detected: Value at 'keySchema' failed to satisfy constraint: Member must have length less than or equal to 2"))?;
    let get = |kt: &str| a.iter().find(|k| str_of(k, "KeyType") == Some(kt)).and_then(|k| str_of(k, "AttributeName")).map(str::to_owned);
    let hash = get("HASH").ok_or_else(|| DynError::validation("Invalid KeySchema: The first KeySchemaElement is not a HASH key type"))?;
    if str_of(&a[0], "KeyType") != Some("HASH") {
        return Err(DynError::validation("Invalid KeySchema: The first KeySchemaElement is not a HASH key type"));
    }
    let range = get("RANGE");
    if a.len() == 2 && range.is_none() {
        return Err(DynError::validation("Invalid KeySchema: The second KeySchemaElement is not a RANGE key type"));
    }
    Ok(KeyDef { hash, range })
}

fn index_def(v: &Value, global: bool) -> Result<IndexDef, DynError> {
    let name = need_str(v, "IndexName")?.to_owned();
    let key = key_def(v.get("KeySchema").unwrap_or(&Value::Null))?;
    let proj = v.get("Projection").cloned().unwrap_or(json!({ "ProjectionType": "ALL" }));
    let projection = str_of(&proj, "ProjectionType").unwrap_or("ALL").to_owned();
    if !matches!(projection.as_str(), "ALL" | "KEYS_ONLY" | "INCLUDE") {
        return Err(DynError::validation(format!("Unknown ProjectionType: {projection}")));
    }
    let non_key = proj
        .get("NonKeyAttributes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    Ok(IndexDef { name, key, projection, non_key, global, throughput: v.get("ProvisionedThroughput").cloned() })
}

fn segment_of(bytes: &[u8], total: u64) -> u64 {
    // FNV-1a: stable across runs, so segments split the same way every time.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h % total
}

impl Table {
    fn key_names(&self) -> Vec<String> {
        self.key.names()
    }

    fn index(&self, name: &str) -> Result<&IndexDef, DynError> {
        self.gsis
            .iter()
            .chain(self.lsis.iter())
            .find(|i| i.name == name)
            .ok_or_else(|| DynError::validation(format!("The table does not have the specified index: {name}")))
    }

    fn check_key_attr(&self, item: &Item, attr: &str, index: Option<&str>) -> Result<Option<Av>, DynError> {
        let Some(v) = item.get(attr) else { return Ok(None) };
        let want = self.attr_defs.get(attr).map(String::as_str).unwrap_or("S");
        if v.type_name() != want {
            return Err(DynError::validation(match index {
                None => format!(
                    "One or more parameter values were invalid: Type mismatch for key {attr} expected: {want} actual: {}",
                    v.type_name()
                ),
                Some(i) => format!(
                    "One or more parameter values were invalid: Type mismatch for Index Key {attr} Expected: {want} Actual: {} IndexName: {i}",
                    v.type_name()
                ),
            }));
        }
        if matches!(v, Av::S(s) if s.is_empty()) || matches!(v, Av::B(b) if b.is_empty()) {
            return Err(DynError::validation(match index {
                None => format!(
                    "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty string value. Key: {attr}"
                ),
                Some(i) => format!(
                    "One or more parameter values are not valid. A value specified for a secondary index key is not supported. The AttributeValue for a key attribute cannot contain an empty string value. IndexName: {i}, IndexKey: {attr}"
                ),
            }));
        }
        Ok(Some(v.clone()))
    }

    /// Partition and sort position of an item, validating key and index-key attributes.
    fn locate(&self, item: &Item) -> Result<(Vec<u8>, SortKey), DynError> {
        let hash = self.check_key_attr(item, &self.key.hash, None)?.ok_or_else(|| {
            DynError::validation(format!("One or more parameter values were invalid: Missing the key {} in the item", self.key.hash))
        })?;
        let range = match &self.key.range {
            Some(r) => self.check_key_attr(item, r, None)?.ok_or_else(|| {
                DynError::validation(format!("One or more parameter values were invalid: Missing the key {r} in the item"))
            })?,
            None => Av::Null,
        };
        for ix in self.gsis.iter().chain(self.lsis.iter()) {
            for a in ix.key.names() {
                self.check_key_attr(item, &a, Some(&ix.name))?;
            }
        }
        Ok((hash.key_bytes(), SortKey(range)))
    }

    /// Position of a `Key` parameter (exactly the key attributes).
    fn locate_key(&self, key: &Item) -> Result<(Vec<u8>, SortKey), DynError> {
        let names = self.key_names();
        if key.len() != names.len() || !names.iter().all(|n| key.contains_key(n)) {
            return Err(DynError::validation("The provided key element does not match the schema"));
        }
        let hash = self.check_key_attr(key, &self.key.hash, None)?.unwrap();
        let range = match &self.key.range {
            Some(r) => self.check_key_attr(key, r, None)?.unwrap(),
            None => Av::Null,
        };
        Ok((hash.key_bytes(), SortKey(range)))
    }

    fn get(&self, pos: &(Vec<u8>, SortKey)) -> Option<&Item> {
        self.partitions.get(&pos.0)?.get(&pos.1)
    }

    fn put(&mut self, pos: (Vec<u8>, SortKey), item: Item) -> Option<Item> {
        self.bytes += item_size(&item);
        let old = self.partitions.entry(pos.0).or_default().insert(pos.1, item);
        match &old {
            Some(o) => self.bytes -= item_size(o),
            None => self.count += 1,
        }
        old
    }

    fn remove(&mut self, pos: &(Vec<u8>, SortKey)) -> Option<Item> {
        let part = self.partitions.get_mut(&pos.0)?;
        let old = part.remove(&pos.1)?;
        if part.is_empty() {
            self.partitions.remove(&pos.0);
        }
        self.count -= 1;
        self.bytes -= item_size(&old);
        Some(old)
    }

    fn all_items(&self) -> impl Iterator<Item = &Item> {
        self.partitions.values().flat_map(|p| p.values())
    }

    fn key_of(&self, item: &Item, index: Option<&IndexDef>) -> Item {
        let mut names = self.key_names();
        if let Some(ix) = index {
            names.extend(ix.key.names());
        }
        names.into_iter().filter_map(|n| item.get(&n).map(|v| (n, v.clone()))).collect()
    }

    fn project_index(&self, item: &Item, ix: &IndexDef) -> Item {
        match ix.projection.as_str() {
            "ALL" => item.clone(),
            _ => {
                let mut keep: HashSet<String> = self.key_names().into_iter().chain(ix.key.names()).collect();
                if ix.projection == "INCLUDE" {
                    keep.extend(ix.non_key.iter().cloned());
                }
                item.iter().filter(|(k, _)| keep.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect()
            }
        }
    }

    pub fn describe(&self, region: &str, status: &str) -> Value {
        let arn = format!("arn:aws:dynamodb:{region}:000000000000:table/{}", self.name);
        let index_json = |ix: &IndexDef| {
            let items: Vec<&Item> = self.all_items().filter(|i| ix.key.names().iter().all(|k| i.contains_key(k))).collect();
            let mut proj = json!({ "ProjectionType": ix.projection });
            if !ix.non_key.is_empty() {
                proj["NonKeyAttributes"] = json!(ix.non_key);
            }
            let mut v = json!({
                "IndexName": ix.name, "KeySchema": ix.key.schema_json(), "Projection": proj,
                "IndexSizeBytes": items.iter().map(|i| item_size(i)).sum::<usize>(), "ItemCount": items.len(),
                "IndexArn": format!("{arn}/index/{}", ix.name),
            });
            if ix.global {
                v["IndexStatus"] = json!("ACTIVE");
                v["ProvisionedThroughput"] = ix.throughput.clone().map(|t| json!({ "ReadCapacityUnits": t["ReadCapacityUnits"], "WriteCapacityUnits": t["WriteCapacityUnits"], "NumberOfDecreasesToday": 0 })).unwrap_or(json!({ "ReadCapacityUnits": 0, "WriteCapacityUnits": 0, "NumberOfDecreasesToday": 0 }));
            }
            v
        };
        let mut d = json!({
            "TableName": self.name,
            "TableStatus": status,
            "KeySchema": self.key.schema_json(),
            "AttributeDefinitions": self.attr_defs.iter().map(|(n, t)| json!({ "AttributeName": n, "AttributeType": t })).collect::<Vec<_>>(),
            "CreationDateTime": self.created,
            "ProvisionedThroughput": { "ReadCapacityUnits": self.throughput["ReadCapacityUnits"].as_i64().unwrap_or(0), "WriteCapacityUnits": self.throughput["WriteCapacityUnits"].as_i64().unwrap_or(0), "NumberOfDecreasesToday": 0 },
            "TableSizeBytes": self.bytes,
            "ItemCount": self.count,
            "TableArn": arn,
            "TableId": format!("{:08x}-0000-4000-8000-000000000000", segment_of(self.name.as_bytes(), u32::MAX as u64)),
            "BillingModeSummary": { "BillingMode": self.billing },
            "DeletionProtectionEnabled": self.deletion_protection,
        });
        if !self.gsis.is_empty() {
            d["GlobalSecondaryIndexes"] = Value::Array(self.gsis.iter().map(index_json).collect());
        }
        if !self.lsis.is_empty() {
            d["LocalSecondaryIndexes"] = Value::Array(self.lsis.iter().map(index_json).collect());
        }
        if let Some(s) = &self.stream {
            d["StreamSpecification"] = s.clone();
        }
        d
    }

    /// Definition for snapshots and seeds (a CreateTable request).
    fn definition(&self) -> Value {
        let index = |ix: &IndexDef| {
            let mut proj = json!({ "ProjectionType": ix.projection });
            if !ix.non_key.is_empty() {
                proj["NonKeyAttributes"] = json!(ix.non_key);
            }
            let mut v = json!({ "IndexName": ix.name, "KeySchema": ix.key.schema_json(), "Projection": proj });
            if let Some(t) = &ix.throughput {
                v["ProvisionedThroughput"] = t.clone();
            }
            v
        };
        let mut d = json!({
            "TableName": self.name,
            "KeySchema": self.key.schema_json(),
            "AttributeDefinitions": self.attr_defs.iter().map(|(n, t)| json!({ "AttributeName": n, "AttributeType": t })).collect::<Vec<_>>(),
            "BillingMode": self.billing,
        });
        if self.billing == "PROVISIONED" {
            d["ProvisionedThroughput"] = self.throughput.clone();
        }
        if !self.gsis.is_empty() {
            d["GlobalSecondaryIndexes"] = Value::Array(self.gsis.iter().map(index).collect());
        }
        if !self.lsis.is_empty() {
            d["LocalSecondaryIndexes"] = Value::Array(self.lsis.iter().map(index).collect());
        }
        if let Some(t) = &self.ttl_attr {
            d["TimeToLiveAttribute"] = json!(t);
        }
        if !self.tags.is_empty() {
            d["Tags"] = Value::Array(self.tags.iter().map(|(k, v)| json!({ "Key": k, "Value": v })).collect());
        }
        d
    }
}

fn cmp_order(a: &[(Av, bool)], b: &[(Av, bool)]) -> Ordering {
    for ((x, hash), (y, _)) in a.iter().zip(b) {
        let o = if *hash { x.key_bytes().cmp(&y.key_bytes()) } else { x.key_cmp(y).unwrap_or(Ordering::Equal) };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// Read and write capacity units, roughly as DynamoDB charges them.
fn capacity(bytes: usize, write: bool, consistent: bool) -> f64 {
    if write {
        (bytes.max(1)).div_ceil(1024) as f64
    } else {
        let units = (bytes.max(1)).div_ceil(4096) as f64;
        if consistent { units } else { units / 2.0 }
    }
}

pub struct DynamoStore {
    tables: RwLock<BTreeMap<String, Table>>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

struct Request<'a> {
    req: &'a Value,
    ph: Placeholders<'a>,
}

impl<'a> Request<'a> {
    fn new(req: &'a Value) -> Self {
        Request { req, ph: Placeholders::new(req) }
    }

    fn condition(&self, key: &str, kind: &'static str) -> Result<Option<Cond>, DynError> {
        match str_of(self.req, key) {
            Some(src) => Ok(Some(expr::parse_condition(kind, src, &self.ph)?)),
            None => Ok(None),
        }
    }

    fn projection(&self) -> Result<Option<Vec<expr::Path>>, DynError> {
        match str_of(self.req, "ProjectionExpression") {
            Some(src) => Ok(Some(expr::parse_projection(src, &self.ph)?)),
            None => Ok(None),
        }
    }

    fn finish(&self) -> Result<(), DynError> {
        if (self.req.get("ExpressionAttributeNames").is_some() || self.req.get("ExpressionAttributeValues").is_some())
            && ["ConditionExpression", "FilterExpression", "KeyConditionExpression", "ProjectionExpression", "UpdateExpression"]
                .iter()
                .all(|k| self.req.get(*k).is_none())
        {
            let which =
                if self.req.get("ExpressionAttributeNames").is_some() { "ExpressionAttributeNames" } else { "ExpressionAttributeValues" };
            return Err(DynError::validation(format!("{which} can only be specified when using expressions")));
        }
        self.ph.check_unused()
    }
}

fn legacy_check(req: &Value) -> Result<(), DynError> {
    for k in ["Expected", "AttributeUpdates", "KeyConditions", "QueryFilter", "ScanFilter", "AttributesToGet", "ConditionalOperator"] {
        if req.get(k).is_some() {
            return Err(DynError::validation(format!(
                "{k} (a legacy parameter) is not supported by the TwinBase mock; use the expression parameters instead"
            )));
        }
    }
    Ok(())
}

fn return_values(req: &Value, allowed: &[&str]) -> Result<String, DynError> {
    let rv = str_of(req, "ReturnValues").unwrap_or("NONE").to_owned();
    if !allowed.contains(&rv.as_str()) {
        return Err(DynError::validation(format!("Return values set to invalid value: {rv}")));
    }
    Ok(rv)
}

fn conditional_failed(req: &Value, old: Option<&Item>) -> DynError {
    let mut e = DynError::new("ConditionalCheckFailedException", "The conditional request failed");
    if str_of(req, "ReturnValuesOnConditionCheckFailure") == Some("ALL_OLD")
        && let Some(o) = old
    {
        e.extra = Some(Box::new(json!({ "Item": item_json(o) })));
    }
    e
}

impl DynamoStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let store = DynamoStore { tables: RwLock::new(BTreeMap::new()), path, dirty: AtomicBool::new(false) };
        if let Some(p) = &store.path
            && p.exists()
            && let Err(e) = store.load(p)
        {
            let aside = p.with_extension(format!("json.unreadable-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
            tracing::warn!("cannot read DynamoDB snapshot {}: {e}; moved to {}", p.display(), aside.display());
            let _ = std::fs::rename(p, aside);
            store.tables.write().clear();
        }
        store
    }

    fn touch(&self) {
        self.dirty.store(true, AtomicOrdering::SeqCst);
    }

    // ---------------------------------------------------------- persistence

    fn load(&self, path: &std::path::Path) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        self.load_seed(&v).map(|_| ()).map_err(|e| e.message)?;
        self.dirty.store(false, AtomicOrdering::SeqCst);
        Ok(())
    }

    /// Create tables and put items from a seed / snapshot:
    /// `{"tables": [{<CreateTable request>, "TimeToLiveAttribute": "...", "Items": [<DynamoDB JSON>]}]}`.
    pub fn load_seed(&self, seed: &Value) -> Result<usize, DynError> {
        let tables = seed.get("tables").and_then(Value::as_array).ok_or_else(|| DynError::validation("seed needs a \"tables\" array"))?;
        let mut n = 0;
        for t in tables {
            let name = need_str(t, "TableName")?.to_owned();
            if self.tables.read().contains_key(&name) {
                self.op_delete_table(&json!({ "TableName": name }), "local")?;
            }
            self.op_create_table(t, "local")?;
            if let Some(attr) = str_of(t, "TimeToLiveAttribute")
                && let Some(tb) = self.tables.write().get_mut(&name)
            {
                tb.ttl_attr = Some(attr.to_owned());
            }
            for item in t.get("Items").and_then(Value::as_array).cloned().unwrap_or_default() {
                self.op_put_item(&json!({ "TableName": name, "Item": item }))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// All tables as a seed document (definitions and items in DynamoDB JSON).
    pub fn export(&self) -> Value {
        let tables = self.tables.read();
        json!({
            "tables": tables.values().map(|t| {
                let mut d = t.definition();
                d["Items"] = Value::Array(t.all_items().map(item_json).collect());
                d
            }).collect::<Vec<_>>()
        })
    }

    pub fn save_if_dirty(&self) -> std::io::Result<()> {
        if self.dirty.swap(false, AtomicOrdering::SeqCst)
            && let Err(e) = self.save()
        {
            self.dirty.store(true, AtomicOrdering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let text = serde_json::to_vec(&self.export()).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    /// Delete items whose TTL attribute (epoch seconds) has passed. Returns how many.
    pub fn sweep_expired(&self) -> usize {
        let now = now_secs();
        let mut removed = 0;
        let mut tables = self.tables.write();
        for t in tables.values_mut() {
            let Some(attr) = t.ttl_attr.clone() else { continue };
            let expired: Vec<(Vec<u8>, SortKey)> = t
                .partitions
                .iter()
                .flat_map(|(h, p)| p.iter().map(move |(s, i)| (h, s, i)))
                .filter(|(_, _, i)| matches!(i.get(&attr), Some(Av::N(n)) if n.to_string().parse::<f64>().is_ok_and(|x| x < now)))
                .map(|(h, s, _)| (h.clone(), s.clone()))
                .collect();
            for pos in expired {
                t.remove(&pos);
                removed += 1;
            }
        }
        if removed > 0 {
            self.touch();
        }
        removed
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }

    // ---------------------------------------------------------- dispatch

    pub fn handle(&self, op: &str, req: &Value, region: &str) -> Result<Value, DynError> {
        match op {
            "CreateTable" => self.op_create_table(req, region),
            "DeleteTable" => self.op_delete_table(req, region),
            "DescribeTable" => {
                let name = need_str(req, "TableName")?;
                let tables = self.tables.read();
                let t = tables.get(name).ok_or_else(|| not_found(name))?;
                Ok(json!({ "Table": t.describe(region, "ACTIVE") }))
            }
            "ListTables" => {
                let tables = self.tables.read();
                let start = str_of(req, "ExclusiveStartTableName");
                let limit = req.get("Limit").and_then(Value::as_u64).unwrap_or(100).clamp(1, 100) as usize;
                let names: Vec<&String> = tables.keys().filter(|n| start.is_none_or(|s| n.as_str() > s)).collect();
                let page: Vec<&String> = names.iter().take(limit).copied().collect();
                let mut out = json!({ "TableNames": page });
                if names.len() > limit {
                    out["LastEvaluatedTableName"] = json!(page.last());
                }
                Ok(out)
            }
            "UpdateTable" => self.op_update_table(req, region),
            "UpdateTimeToLive" => {
                let name = need_str(req, "TableName")?;
                let spec = req.get("TimeToLiveSpecification").cloned().unwrap_or_default();
                let mut tables = self.tables.write();
                let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
                t.ttl_attr =
                    if spec["Enabled"].as_bool().unwrap_or(false) { spec["AttributeName"].as_str().map(str::to_owned) } else { None };
                self.touch();
                Ok(json!({ "TimeToLiveSpecification": spec }))
            }
            "DescribeTimeToLive" => {
                let name = need_str(req, "TableName")?;
                let tables = self.tables.read();
                let t = tables.get(name).ok_or_else(|| not_found(name))?;
                Ok(json!({ "TimeToLiveDescription": match &t.ttl_attr {
                    Some(a) => json!({ "TimeToLiveStatus": "ENABLED", "AttributeName": a }),
                    None => json!({ "TimeToLiveStatus": "DISABLED" }),
                } }))
            }
            "TagResource" | "UntagResource" | "ListTagsOfResource" => {
                let arn = need_str(req, "ResourceArn")?;
                let name = arn.rsplit("table/").next().unwrap_or_default().split('/').next().unwrap_or_default();
                let mut tables = self.tables.write();
                let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
                match op {
                    "TagResource" => {
                        for tag in req.get("Tags").and_then(Value::as_array).cloned().unwrap_or_default() {
                            t.tags.insert(
                                tag["Key"].as_str().unwrap_or_default().to_owned(),
                                tag["Value"].as_str().unwrap_or_default().to_owned(),
                            );
                        }
                        self.touch();
                        Ok(json!({}))
                    }
                    "UntagResource" => {
                        for k in req.get("TagKeys").and_then(Value::as_array).cloned().unwrap_or_default() {
                            t.tags.remove(k.as_str().unwrap_or_default());
                        }
                        self.touch();
                        Ok(json!({}))
                    }
                    _ => Ok(json!({ "Tags": t.tags.iter().map(|(k, v)| json!({ "Key": k, "Value": v })).collect::<Vec<_>>() })),
                }
            }
            "DescribeLimits" => Ok(
                json!({ "AccountMaxReadCapacityUnits": 80000, "AccountMaxWriteCapacityUnits": 80000, "TableMaxReadCapacityUnits": 40000, "TableMaxWriteCapacityUnits": 40000 }),
            ),
            "DescribeContinuousBackups" => {
                let name = need_str(req, "TableName")?;
                self.tables.read().get(name).ok_or_else(|| not_found(name))?;
                Ok(
                    json!({ "ContinuousBackupsDescription": { "ContinuousBackupsStatus": "ENABLED", "PointInTimeRecoveryDescription": { "PointInTimeRecoveryStatus": "DISABLED" } } }),
                )
            }
            "PutItem" => self.op_put_item(req),
            "GetItem" => self.op_get_item(req),
            "DeleteItem" => self.op_delete_item(req),
            "UpdateItem" => self.op_update_item(req),
            "Query" => self.op_query(req),
            "Scan" => self.op_scan(req),
            "BatchGetItem" => self.op_batch_get(req),
            "BatchWriteItem" => self.op_batch_write(req),
            "TransactWriteItems" => self.op_transact_write(req),
            "TransactGetItems" => self.op_transact_get(req),
            other => {
                Err(DynError::new("UnknownOperationException", format!("Operation {other} is not supported by the TwinBase DynamoDB mock")))
            }
        }
    }

    // ---------------------------------------------------------- tables

    fn op_create_table(&self, req: &Value, region: &str) -> Result<Value, DynError> {
        let name = need_str(req, "TableName")?.to_owned();
        if name.len() < 3 || name.len() > 255 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)) {
            return Err(DynError::validation(format!(
                "TableName must be at least 3 characters long and at most 255 characters long, and contain only a-z, A-Z, 0-9, '_', '-' and '.': {name}"
            )));
        }
        let key = key_def(req.get("KeySchema").unwrap_or(&Value::Null))?;
        let mut attr_defs = BTreeMap::new();
        for d in req.get("AttributeDefinitions").and_then(Value::as_array).cloned().unwrap_or_default() {
            let t = str_of(&d, "AttributeType").unwrap_or("");
            if !matches!(t, "S" | "N" | "B") {
                return Err(DynError::validation(format!("Member must satisfy enum value set: [B, N, S]; got {t}")));
            }
            attr_defs.insert(str_of(&d, "AttributeName").unwrap_or_default().to_owned(), t.to_owned());
        }
        let gsis: Vec<IndexDef> = req
            .get("GlobalSecondaryIndexes")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| index_def(v, true)).collect())
            .transpose()?
            .unwrap_or_default();
        let lsis: Vec<IndexDef> = req
            .get("LocalSecondaryIndexes")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| index_def(v, false)).collect())
            .transpose()?
            .unwrap_or_default();
        for l in &lsis {
            if l.key.hash != key.hash || l.key.range.is_none() {
                return Err(DynError::validation(format!(
                    "One or more parameter values were invalid: Table KeySchema does not have a range key, which is required when specifying a LocalSecondaryIndex; or index {} does not share the table's hash key",
                    l.name
                )));
            }
        }
        let mut used: HashSet<String> = key.names().into_iter().collect();
        for ix in gsis.iter().chain(lsis.iter()) {
            used.extend(ix.key.names());
        }
        for u in &used {
            if !attr_defs.contains_key(u) {
                return Err(DynError::validation(format!(
                    "One or more parameter values were invalid: Some index key attributes are not defined in AttributeDefinitions. Keys: [{u}], AttributeDefinitions: [{}]",
                    attr_defs.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        if attr_defs.len() != used.len() {
            return Err(DynError::validation("One or more parameter values were invalid: Number of attributes in KeySchema does not exactly match number of attributes defined in AttributeDefinitions".to_string()));
        }
        let mut names = HashSet::new();
        for ix in gsis.iter().chain(lsis.iter()) {
            if !names.insert(ix.name.clone()) {
                return Err(DynError::validation(format!("One or more parameter values were invalid: Duplicate index name: {}", ix.name)));
            }
        }
        let billing = str_of(req, "BillingMode").unwrap_or("PROVISIONED").to_owned();
        let throughput = req.get("ProvisionedThroughput").cloned().unwrap_or(json!({ "ReadCapacityUnits": 0, "WriteCapacityUnits": 0 }));
        if billing == "PROVISIONED" && req.get("ProvisionedThroughput").is_none() {
            return Err(DynError::validation(
                "One or more parameter values were invalid: ReadCapacityUnits and WriteCapacityUnits must both be specified when BillingMode is PROVISIONED",
            ));
        }
        let tags = req
            .get("Tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|t| (t["Key"].as_str().unwrap_or_default().to_owned(), t["Value"].as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let table = Table {
            name: name.clone(),
            key,
            attr_defs,
            gsis,
            lsis,
            billing,
            throughput,
            created: now_secs(),
            ttl_attr: None,
            tags,
            stream: req.get("StreamSpecification").cloned(),
            deletion_protection: req.get("DeletionProtectionEnabled").and_then(Value::as_bool).unwrap_or(false),
            partitions: BTreeMap::new(),
            count: 0,
            bytes: 0,
        };
        let mut tables = self.tables.write();
        if tables.contains_key(&name) {
            return Err(DynError::new("ResourceInUseException", format!("Table already exists: {name}")));
        }
        let desc = table.describe(region, "ACTIVE");
        tables.insert(name, table);
        self.touch();
        Ok(json!({ "TableDescription": desc }))
    }

    fn op_delete_table(&self, req: &Value, region: &str) -> Result<Value, DynError> {
        let name = need_str(req, "TableName")?;
        let mut tables = self.tables.write();
        let t = tables.get(name).ok_or_else(|| not_found(name))?;
        if t.deletion_protection {
            return Err(DynError::validation(
                "Resource cannot be deleted as it is currently protected against deletion. Disable deletion protection first.".to_string(),
            ));
        }
        let desc = t.describe(region, "DELETING");
        tables.remove(name);
        self.touch();
        Ok(json!({ "TableDescription": desc }))
    }

    fn op_update_table(&self, req: &Value, region: &str) -> Result<Value, DynError> {
        let name = need_str(req, "TableName")?;
        let mut tables = self.tables.write();
        let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
        for d in req.get("AttributeDefinitions").and_then(Value::as_array).cloned().unwrap_or_default() {
            t.attr_defs
                .insert(str_of(&d, "AttributeName").unwrap_or_default().to_owned(), str_of(&d, "AttributeType").unwrap_or("S").to_owned());
        }
        if let Some(b) = str_of(req, "BillingMode") {
            t.billing = b.to_owned();
        }
        if let Some(p) = req.get("ProvisionedThroughput") {
            t.throughput = p.clone();
        }
        if let Some(d) = req.get("DeletionProtectionEnabled").and_then(Value::as_bool) {
            t.deletion_protection = d;
        }
        for u in req.get("GlobalSecondaryIndexUpdates").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(c) = u.get("Create") {
                let ix = index_def(c, true)?;
                if t.gsis.iter().any(|g| g.name == ix.name) {
                    return Err(DynError::validation(format!("Attempting to create an index which already exists: {}", ix.name)));
                }
                for a in ix.key.names() {
                    if !t.attr_defs.contains_key(&a) {
                        return Err(DynError::validation(format!(
                            "One or more parameter values were invalid: Global Secondary Index key attribute {a} is not defined in AttributeDefinitions"
                        )));
                    }
                }
                // Existing items with a wrong-typed index key would be rejected by DynamoDB too.
                for item in t.all_items() {
                    for a in ix.key.names() {
                        t.check_key_attr(item, &a, Some(&ix.name))?;
                    }
                }
                t.gsis.push(ix);
            }
            if let Some(d) = u.get("Delete") {
                let n = need_str(d, "IndexName")?;
                if !t.gsis.iter().any(|g| g.name == n) {
                    return Err(DynError::new("ResourceNotFoundException", format!("Requested resource not found: Index: {n} not found")));
                }
                t.gsis.retain(|g| g.name != n);
            }
            if let Some(up) = u.get("Update") {
                let n = need_str(up, "IndexName")?;
                if let Some(g) = t.gsis.iter_mut().find(|g| g.name == n) {
                    g.throughput = up.get("ProvisionedThroughput").cloned();
                }
            }
        }
        let desc = t.describe(region, "ACTIVE");
        self.touch();
        Ok(json!({ "TableDescription": desc }))
    }

    // ---------------------------------------------------------- items

    fn op_put_item(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let item = item_from_json(req.get("Item").unwrap_or(&Value::Null))?;
        let cond = r.condition("ConditionExpression", "ConditionExpression")?;
        r.finish()?;
        let rv = return_values(req, &["NONE", "ALL_OLD"])?;
        if item_size(&item) > MAX_ITEM {
            return Err(DynError::validation("Item size has exceeded the maximum allowed size"));
        }
        let mut tables = self.tables.write();
        let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
        let pos = t.locate(&item)?;
        let old = t.get(&pos).cloned();
        if let Some(c) = &cond
            && !expr::eval(c, old.as_ref().unwrap_or(&Item::new()))
        {
            return Err(conditional_failed(req, old.as_ref()));
        }
        let size = item_size(&item);
        t.put(pos, item);
        self.touch();
        let mut out = json!({});
        if rv == "ALL_OLD"
            && let Some(o) = &old
        {
            out["Attributes"] = item_json(o);
        }
        consumed(&mut out, req, name, capacity(size, true, true));
        Ok(out)
    }

    fn op_get_item(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let key = item_from_json(req.get("Key").unwrap_or(&Value::Null))?;
        let proj = r.projection()?;
        r.finish()?;
        let tables = self.tables.read();
        let t = tables.get(name).ok_or_else(|| not_found(name))?;
        let pos = t.locate_key(&key)?;
        let mut out = json!({});
        let consistent = req.get("ConsistentRead").and_then(Value::as_bool).unwrap_or(false);
        let mut size = 0;
        if let Some(item) = t.get(&pos) {
            size = item_size(item);
            let shown = match &proj {
                Some(p) => expr::project(item, p),
                None => item.clone(),
            };
            out["Item"] = item_json(&shown);
        }
        consumed(&mut out, req, name, capacity(size, false, consistent));
        Ok(out)
    }

    fn op_delete_item(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let key = item_from_json(req.get("Key").unwrap_or(&Value::Null))?;
        let cond = r.condition("ConditionExpression", "ConditionExpression")?;
        r.finish()?;
        let rv = return_values(req, &["NONE", "ALL_OLD"])?;
        let mut tables = self.tables.write();
        let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
        let pos = t.locate_key(&key)?;
        let old = t.get(&pos).cloned();
        if let Some(c) = &cond
            && !expr::eval(c, old.as_ref().unwrap_or(&Item::new()))
        {
            return Err(conditional_failed(req, old.as_ref()));
        }
        let mut out = json!({});
        if let Some(o) = t.remove(&pos) {
            self.touch();
            if rv == "ALL_OLD" {
                out["Attributes"] = item_json(&o);
            }
            consumed(&mut out, req, name, capacity(item_size(&o), true, true));
        }
        Ok(out)
    }

    fn op_update_item(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let key = item_from_json(req.get("Key").unwrap_or(&Value::Null))?;
        let cond = r.condition("ConditionExpression", "ConditionExpression")?;
        let update = match str_of(req, "UpdateExpression") {
            Some(src) => Some(expr::parse_update(src, &r.ph)?),
            None => None,
        };
        r.finish()?;
        let rv = return_values(req, &["NONE", "ALL_OLD", "UPDATED_OLD", "ALL_NEW", "UPDATED_NEW"])?;
        let mut tables = self.tables.write();
        let t = tables.get_mut(name).ok_or_else(|| not_found(name))?;
        let pos = t.locate_key(&key)?;
        let old = t.get(&pos).cloned();
        if let Some(c) = &cond
            && !expr::eval(c, old.as_ref().unwrap_or(&Item::new()))
        {
            return Err(conditional_failed(req, old.as_ref()));
        }
        let mut item = old.clone().unwrap_or_else(|| key.clone());
        let touched = match &update {
            Some(u) => expr::apply_update(&mut item, u, &t.key_names())?,
            None => vec![],
        };
        if item_size(&item) > MAX_ITEM {
            return Err(DynError::validation("Item size to update has exceeded the maximum allowed size"));
        }
        t.locate(&item)?;
        let size = item_size(&item);
        t.put(pos, item.clone());
        self.touch();
        let pick = |src: &Item| -> Item { touched.iter().filter_map(|k| src.get(k).map(|v| (k.clone(), v.clone()))).collect() };
        let attrs: Option<Item> = match rv.as_str() {
            "ALL_OLD" => old.clone(),
            "UPDATED_OLD" => old.as_ref().map(pick),
            "ALL_NEW" => Some(item.clone()),
            "UPDATED_NEW" => Some(pick(&item)),
            _ => None,
        };
        let mut out = json!({});
        if let Some(a) = attrs.filter(|a| !a.is_empty()) {
            out["Attributes"] = item_json(&a);
        }
        consumed(&mut out, req, name, capacity(size, true, true));
        Ok(out)
    }

    // ---------------------------------------------------------- reads

    fn op_query(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let key_cond = r.condition("KeyConditionExpression", "KeyConditionExpression")?.ok_or_else(|| {
            DynError::validation("Either the KeyConditions or KeyConditionExpression parameter must be specified in the request.")
        })?;
        let filter = r.condition("FilterExpression", "FilterExpression")?;
        let proj = r.projection()?;
        r.finish()?;
        let tables = self.tables.read();
        let t = tables.get(name).ok_or_else(|| not_found(name))?;
        let index = str_of(req, "IndexName").map(|n| t.index(n)).transpose()?;
        let key = index.map(|i| &i.key).unwrap_or(&t.key);
        let (hash_value, range_cond) = split_key_condition(&key_cond, key)?;
        let forward = req.get("ScanIndexForward").and_then(Value::as_bool).unwrap_or(true);
        // Candidates in key order.
        let mut items: Vec<(Vec<(Av, bool)>, &Item)> = match index {
            None => t
                .partitions
                .get(&hash_value.key_bytes())
                .map(|p| p.values().map(|i| (order_key(t, i, None), i)).collect())
                .unwrap_or_default(),
            Some(ix) => {
                let mut v: Vec<(Vec<(Av, bool)>, &Item)> = t
                    .all_items()
                    .filter(|i| {
                        i.get(&ix.key.hash).is_some_and(|h| *h == hash_value) && ix.key.range.as_ref().is_none_or(|r| i.contains_key(r))
                    })
                    .map(|i| (order_key(t, i, Some(ix)), i))
                    .collect();
                v.sort_by(|a, b| cmp_order(&a.0, &b.0));
                v
            }
        };
        if let Some(rc) = &range_cond {
            items.retain(|(_, i)| expr::eval(rc, i));
        }
        if !forward {
            items.reverse();
        }
        self.page(req, t, index, items, filter.as_ref(), proj.as_deref(), forward)
    }

    fn op_scan(&self, req: &Value) -> Result<Value, DynError> {
        legacy_check(req)?;
        let r = Request::new(req);
        let name = need_str(req, "TableName")?;
        let filter = r.condition("FilterExpression", "FilterExpression")?;
        let proj = r.projection()?;
        r.finish()?;
        let tables = self.tables.read();
        let t = tables.get(name).ok_or_else(|| not_found(name))?;
        let index = str_of(req, "IndexName").map(|n| t.index(n)).transpose()?;
        let segments = match (req.get("Segment").and_then(Value::as_u64), req.get("TotalSegments").and_then(Value::as_u64)) {
            (Some(s), Some(n)) if n >= 1 && s < n => Some((s, n)),
            (None, None) => None,
            _ => {
                return Err(DynError::validation(
                    "The Segment parameter is required but was not present in the request when parameter TotalSegments is present",
                ));
            }
        };
        let mut items: Vec<(Vec<(Av, bool)>, &Item)> = t
            .all_items()
            .filter(|i| index.is_none_or(|ix| ix.key.names().iter().all(|k| i.contains_key(k))))
            .filter(|i| segments.is_none_or(|(s, n)| i.get(&t.key.hash).is_some_and(|h| segment_of(&h.key_bytes(), n) == s)))
            .map(|i| (order_key(t, i, index), i))
            .collect();
        if index.is_some() {
            items.sort_by(|a, b| cmp_order(&a.0, &b.0));
        }
        self.page(req, t, index, items, filter.as_ref(), proj.as_deref(), true)
    }

    /// One page of a query or scan: start after `ExclusiveStartKey`, evaluate up to `Limit` items
    /// (and 1 MB), filter, project, and report `LastEvaluatedKey` when more items remain.
    #[allow(clippy::too_many_arguments)]
    fn page(
        &self,
        req: &Value,
        t: &Table,
        index: Option<&IndexDef>,
        items: Vec<(Vec<(Av, bool)>, &Item)>,
        filter: Option<&Cond>,
        proj: Option<&[expr::Path]>,
        forward: bool,
    ) -> Result<Value, DynError> {
        let start = match req.get("ExclusiveStartKey") {
            Some(k) => {
                let k = item_from_json(k)?;
                let mut want = t.key_names();
                if let Some(ix) = index {
                    want.extend(ix.key.names());
                }
                want.dedup();
                if !want.iter().all(|n| k.contains_key(n)) {
                    return Err(DynError::validation(
                        "The provided starting key is invalid: The provided key element does not match the schema",
                    ));
                }
                Some(order_key(t, &k, index))
            }
            None => None,
        };
        let limit = match req.get("Limit") {
            Some(l) => {
                let l = l.as_u64().filter(|l| *l >= 1).ok_or_else(|| DynError::validation("1 validation error detected: Value at 'limit' failed to satisfy constraint: Member must have value greater than or equal to 1"))?;
                l as usize
            }
            None => usize::MAX,
        };
        let select = str_of(req, "Select").unwrap_or(if proj.is_some() { "SPECIFIC_ATTRIBUTES" } else { "ALL_ATTRIBUTES" });
        if select == "COUNT" && proj.is_some() {
            return Err(DynError::validation("Cannot specify the ProjectionExpression when choosing to get only the Count"));
        }
        let consistent = req.get("ConsistentRead").and_then(Value::as_bool).unwrap_or(false);
        if consistent && index.is_some_and(|i| i.global) {
            return Err(DynError::validation("Consistent reads are not supported on global secondary indexes"));
        }
        let mut out_items = Vec::new();
        let (mut scanned, mut count, mut bytes) = (0usize, 0usize, 0usize);
        let mut last: Option<&Item> = None;
        let mut more = false;
        for (ok, item) in items.iter().filter(|(ok, _)| match &start {
            Some(s) => {
                let o = cmp_order(ok, s);
                if forward { o == Ordering::Greater } else { o == Ordering::Less }
            }
            None => true,
        }) {
            let _ = ok;
            if scanned >= limit || bytes >= PAGE_BYTES {
                more = true;
                break;
            }
            scanned += 1;
            bytes += item_size(item);
            last = Some(item);
            if filter.is_none_or(|f| expr::eval(f, item)) {
                count += 1;
                if select != "COUNT" {
                    let base = match index {
                        Some(ix) => t.project_index(item, ix),
                        None => (*item).clone(),
                    };
                    let shown = match proj {
                        Some(p) => expr::project(&base, p),
                        None => base,
                    };
                    out_items.push(item_json(&shown));
                }
            }
        }
        let mut out = json!({ "Count": count, "ScannedCount": scanned });
        if select != "COUNT" {
            out["Items"] = Value::Array(out_items);
        }
        if more && let Some(l) = last {
            out["LastEvaluatedKey"] = item_json(&t.key_of(l, index));
        }
        consumed(&mut out, req, &t.name, capacity(bytes, false, consistent));
        Ok(out)
    }

    fn op_batch_get(&self, req: &Value) -> Result<Value, DynError> {
        let requests = req
            .get("RequestItems")
            .and_then(Value::as_object)
            .ok_or_else(|| DynError::validation("The requestItems parameter is required for BatchGetItem"))?;
        let total: usize = requests.values().map(|r| r.get("Keys").and_then(Value::as_array).map(Vec::len).unwrap_or(0)).sum();
        if total > 100 {
            return Err(DynError::validation("Too many items requested for the BatchGetItem call"));
        }
        let mut responses = Map::new();
        for (table, spec) in requests {
            let mut items = Vec::new();
            for key in spec.get("Keys").and_then(Value::as_array).cloned().unwrap_or_default() {
                let mut get = json!({ "TableName": table, "Key": key });
                for k in ["ProjectionExpression", "ExpressionAttributeNames", "ConsistentRead"] {
                    if let Some(v) = spec.get(k) {
                        get[k] = v.clone();
                    }
                }
                if let Some(item) = self.op_get_item(&get)?.get("Item") {
                    items.push(item.clone());
                }
            }
            responses.insert(table.clone(), Value::Array(items));
        }
        Ok(json!({ "Responses": responses, "UnprocessedKeys": {} }))
    }

    fn op_batch_write(&self, req: &Value) -> Result<Value, DynError> {
        let requests = req
            .get("RequestItems")
            .and_then(Value::as_object)
            .ok_or_else(|| DynError::validation("The requestItems parameter is required for BatchWriteItem"))?;
        let total: usize = requests.values().map(|r| r.as_array().map(Vec::len).unwrap_or(0)).sum();
        if total == 0 || total > 25 {
            return Err(DynError::validation(
                "1 validation error detected: Value at 'requestItems' failed to satisfy constraint: Map value must satisfy constraint: [Member must have length less than or equal to 25, Member must have length greater than or equal to 1]",
            ));
        }
        // Validate everything first so a bad request writes nothing.
        {
            let tables = self.tables.read();
            for (table, writes) in requests {
                let t = tables.get(table).ok_or_else(|| not_found(table))?;
                let mut seen = HashSet::new();
                for w in writes.as_array().cloned().unwrap_or_default() {
                    let pos = if let Some(p) = w.get("PutRequest") {
                        t.locate(&item_from_json(p.get("Item").unwrap_or(&Value::Null))?)?
                    } else if let Some(d) = w.get("DeleteRequest") {
                        t.locate_key(&item_from_json(d.get("Key").unwrap_or(&Value::Null))?)?
                    } else {
                        return Err(DynError::validation("Supplied AttributeValue has neither PutRequest nor DeleteRequest"));
                    };
                    if !seen.insert((pos.0.clone(), format!("{:?}", pos.1.0.key_bytes()))) {
                        return Err(DynError::validation("Provided list of item keys contains duplicates"));
                    }
                }
            }
        }
        for (table, writes) in requests {
            for w in writes.as_array().cloned().unwrap_or_default() {
                if let Some(p) = w.get("PutRequest") {
                    self.op_put_item(&json!({ "TableName": table, "Item": p["Item"] }))?;
                } else if let Some(d) = w.get("DeleteRequest") {
                    self.op_delete_item(&json!({ "TableName": table, "Key": d["Key"] }))?;
                }
            }
        }
        Ok(json!({ "UnprocessedItems": {} }))
    }

    fn op_transact_get(&self, req: &Value) -> Result<Value, DynError> {
        let items = req.get("TransactItems").and_then(Value::as_array).cloned().unwrap_or_default();
        if items.is_empty() || items.len() > 100 {
            return Err(DynError::validation(
                "1 validation error detected: Value at 'transactItems' failed to satisfy constraint: Member must have length less than or equal to 100",
            ));
        }
        let mut responses = Vec::new();
        for i in items {
            let g = i.get("Get").ok_or_else(|| DynError::validation("TransactGetItems entries need a Get"))?;
            let r = self.op_get_item(g)?;
            responses.push(match r.get("Item") {
                Some(it) => json!({ "Item": it }),
                None => json!({}),
            });
        }
        Ok(json!({ "Responses": responses }))
    }

    /// All-or-nothing writes: every condition is checked under one lock before anything changes.
    fn op_transact_write(&self, req: &Value) -> Result<Value, DynError> {
        let items = req.get("TransactItems").and_then(Value::as_array).cloned().unwrap_or_default();
        if items.is_empty() || items.len() > 100 {
            return Err(DynError::validation(
                "1 validation error detected: Value at 'transactItems' failed to satisfy constraint: Member must have length less than or equal to 100",
            ));
        }
        let mut tables = self.tables.write();
        // Parse and validate.
        struct Op {
            kind: &'static str,
            table: String,
            pos: (Vec<u8>, SortKey),
            item: Option<Item>,
            update: Option<expr::Update>,
            cond: Option<Cond>,
            key: Item,
            return_old: bool,
        }
        let mut ops = Vec::new();
        let mut seen = HashSet::new();
        for entry in &items {
            let (kind, body) = ["ConditionCheck", "Put", "Delete", "Update"]
                .iter()
                .find_map(|k| entry.get(*k).map(|b| (*k, b)))
                .ok_or_else(|| DynError::validation("TransactItems entries need one of ConditionCheck, Put, Delete, Update"))?;
            let r = Request::new(body);
            let table = need_str(body, "TableName")?.to_owned();
            let t = tables.get(&table).ok_or_else(|| not_found(&table))?;
            let cond = r.condition("ConditionExpression", "ConditionExpression")?;
            let (pos, item, key) = if kind == "Put" {
                let it = item_from_json(body.get("Item").unwrap_or(&Value::Null))?;
                let pos = t.locate(&it)?;
                let key = t.key_of(&it, None);
                (pos, Some(it), key)
            } else {
                let key = item_from_json(body.get("Key").unwrap_or(&Value::Null))?;
                (t.locate_key(&key)?, None, key)
            };
            let update =
                if kind == "Update" { Some(expr::parse_update(str_of(body, "UpdateExpression").unwrap_or(""), &r.ph)?) } else { None };
            if kind == "ConditionCheck" && cond.is_none() {
                return Err(DynError::validation("The ConditionCheck needs a ConditionExpression"));
            }
            r.finish()?;
            if !seen.insert((table.clone(), pos.0.clone(), format!("{:?}", pos.1.0.key_bytes()))) {
                return Err(DynError::validation("Transaction request cannot include multiple operations on one item"));
            }
            let return_old = str_of(body, "ReturnValuesOnConditionCheckFailure") == Some("ALL_OLD");
            ops.push(Op { kind, table, pos, item, update, cond, key, return_old });
        }
        // Check every condition.
        let mut reasons = Vec::new();
        let mut failed = false;
        for op in &ops {
            let t = &tables[&op.table];
            let old = t.get(&op.pos);
            let ok = op.cond.as_ref().is_none_or(|c| expr::eval(c, old.unwrap_or(&Item::new())));
            if ok {
                reasons.push(json!({ "Code": "None" }));
            } else {
                failed = true;
                let mut r = json!({ "Code": "ConditionalCheckFailed", "Message": "The conditional request failed" });
                if op.return_old
                    && let Some(o) = old
                {
                    r["Item"] = item_json(o);
                }
                reasons.push(r);
            }
        }
        if failed {
            let mut e = DynError::new(
                "TransactionCanceledException",
                format!(
                    "Transaction cancelled, please refer cancellation reasons for specific reasons [{}]",
                    reasons.iter().map(|r| r["Code"].as_str().unwrap_or("None")).collect::<Vec<_>>().join(", ")
                ),
            );
            e.extra = Some(Box::new(json!({ "CancellationReasons": reasons })));
            return Err(e);
        }
        // Compute every new item before changing anything.
        // (table, key, new item or None for a delete)
        #[allow(clippy::type_complexity)]
        let mut results: Vec<(String, (Vec<u8>, SortKey), Option<Item>)> = Vec::new();
        for op in &ops {
            let t = &tables[&op.table];
            match op.kind {
                "Put" => results.push((op.table.clone(), op.pos.clone(), op.item.clone())),
                "Delete" => results.push((op.table.clone(), op.pos.clone(), None)),
                "Update" => {
                    let mut it = t.get(&op.pos).cloned().unwrap_or_else(|| op.key.clone());
                    expr::apply_update(&mut it, op.update.as_ref().unwrap(), &t.key_names())?;
                    t.locate(&it)?;
                    results.push((op.table.clone(), op.pos.clone(), Some(it)));
                }
                _ => {}
            }
        }
        for (table, pos, item) in results {
            let t = tables.get_mut(&table).unwrap();
            match item {
                Some(i) => {
                    t.put(pos, i);
                }
                None => {
                    t.remove(&pos);
                }
            }
        }
        self.touch();
        Ok(json!({}))
    }
}

fn consumed(out: &mut Value, req: &Value, table: &str, units: f64) {
    if matches!(str_of(req, "ReturnConsumedCapacity"), Some("TOTAL" | "INDEXES")) {
        out["ConsumedCapacity"] = json!({ "TableName": table, "CapacityUnits": units });
    }
}

/// Sort-order key of an item for a table or index scan.
fn order_key(t: &Table, item: &Item, index: Option<&IndexDef>) -> Vec<(Av, bool)> {
    let get = |n: &String| item.get(n).cloned().unwrap_or(Av::Null);
    let mut k = Vec::new();
    if let Some(ix) = index {
        k.push((get(&ix.key.hash), true));
        if let Some(r) = &ix.key.range {
            k.push((get(r), false));
        }
    }
    k.push((get(&t.key.hash), true));
    if let Some(r) = &t.key.range {
        k.push((get(r), false));
    }
    k
}

/// Split a key condition into the partition value and the optional sort-key condition.
fn split_key_condition(c: &Cond, key: &KeyDef) -> Result<(Av, Option<Cond>), DynError> {
    let parts: Vec<&Cond> = match c {
        Cond::And(a, b) => vec![a, b],
        other => vec![other],
    };
    let attr_of = |o: &expr::Operand| match o {
        expr::Operand::Path(p) if p.len() == 1 => match &p[0] {
            expr::Elem::Name(n) => Some(n.clone()),
            _ => None,
        },
        _ => None,
    };
    let mut hash = None;
    let mut range = None;
    for part in parts {
        let target = match part {
            Cond::Cmp(a, _, _) | Cond::Between(a, _, _) | Cond::BeginsWith(a, _) => attr_of(a),
            _ => None,
        };
        let Some(attr) = target else {
            return Err(DynError::validation("Invalid KeyConditionExpression: Invalid operator used in KeyConditionExpression"));
        };
        if attr == key.hash {
            match part {
                Cond::Cmp(_, expr::CmpOp::Eq, expr::Operand::Value(v)) => hash = Some(v.clone()),
                _ => return Err(DynError::validation("Query key condition not supported")),
            }
        } else if Some(&attr) == key.range.as_ref() {
            if matches!(part, Cond::Cmp(_, expr::CmpOp::Ne, _)) {
                return Err(DynError::validation("Invalid KeyConditionExpression: Unsupported operator on KeyCondition: <>"));
            }
            range = Some(part.clone());
        } else {
            return Err(DynError::validation(format!("Query condition missed key schema element: {}", key.hash)));
        }
    }
    let hash = hash.ok_or_else(|| DynError::validation(format!("Query condition missed key schema element: {}", key.hash)))?;
    Ok((hash, range))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> DynamoStore {
        let s = DynamoStore::new(None);
        s.handle(
            "CreateTable",
            &json!({
                "TableName": "orders", "BillingMode": "PAY_PER_REQUEST",
                "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }],
                "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }, { "AttributeName": "sk", "AttributeType": "N" }, { "AttributeName": "status", "AttributeType": "S" }],
                "GlobalSecondaryIndexes": [{ "IndexName": "by-status", "KeySchema": [{ "AttributeName": "status", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }], "Projection": { "ProjectionType": "KEYS_ONLY" } }]
            }),
            "us-east-1",
        )
        .unwrap();
        for i in 1..=10 {
            s.handle("PutItem", &json!({ "TableName": "orders", "Item": { "pk": { "S": format!("c#{}", i % 2) }, "sk": { "N": i.to_string() }, "status": { "S": if i % 3 == 0 { "open" } else { "paid" } }, "amount": { "N": (i * 10).to_string() } } }), "us-east-1").unwrap();
        }
        s
    }

    #[test]
    fn query_pagination_index_and_filters() {
        let s = store();
        let q = |extra: Value| {
            let mut r = json!({ "TableName": "orders", "KeyConditionExpression": "pk = :p AND sk BETWEEN :a AND :b", "ExpressionAttributeValues": { ":p": { "S": "c#0" }, ":a": { "N": "2" }, ":b": { "N": "8" } } });
            for (k, v) in extra.as_object().unwrap() {
                r[k] = v.clone();
            }
            s.handle("Query", &r, "us-east-1").unwrap()
        };
        let all = q(json!({}));
        assert_eq!(all["Count"], 4);
        assert_eq!(all["Items"][0]["sk"]["N"], "2");
        let back = q(json!({ "ScanIndexForward": false, "Limit": 2 }));
        assert_eq!(back["Items"].as_array().unwrap().iter().map(|i| i["sk"]["N"].as_str().unwrap()).collect::<Vec<_>>(), vec!["8", "6"]);
        let next = q(json!({ "ScanIndexForward": false, "Limit": 2, "ExclusiveStartKey": back["LastEvaluatedKey"] }));
        assert_eq!(next["Items"].as_array().unwrap().iter().map(|i| i["sk"]["N"].as_str().unwrap()).collect::<Vec<_>>(), vec!["4", "2"]);
        assert!(next.get("LastEvaluatedKey").is_none());
        let by_status = s.handle("Query", &json!({ "TableName": "orders", "IndexName": "by-status", "KeyConditionExpression": "#s = :o", "ExpressionAttributeNames": { "#s": "status" }, "ExpressionAttributeValues": { ":o": { "S": "open" } } }), "us-east-1").unwrap();
        assert_eq!(by_status["Count"], 3);
        assert!(by_status["Items"][0].get("amount").is_none(), "KEYS_ONLY projection");
        let filtered = s.handle("Scan", &json!({ "TableName": "orders", "FilterExpression": "amount >= :t", "ExpressionAttributeValues": { ":t": { "N": "50" } }, "Select": "COUNT" }), "us-east-1").unwrap();
        assert_eq!((filtered["Count"].as_u64(), filtered["ScannedCount"].as_u64()), (Some(6), Some(10)));
        let missing = s
            .handle(
                "Query",
                &json!({ "TableName": "orders", "KeyConditionExpression": "sk = :a", "ExpressionAttributeValues": { ":a": { "N": "1" } } }),
                "us-east-1",
            )
            .unwrap_err();
        assert!(missing.message.contains("missed key schema element: pk"));
    }

    #[test]
    fn conditions_updates_and_transactions() {
        let s = store();
        let r = |op: &str, v: Value| s.handle(op, &v, "us-east-1");
        let dup = r("PutItem", json!({ "TableName": "orders", "Item": { "pk": { "S": "c#1" }, "sk": { "N": "1" } }, "ConditionExpression": "attribute_not_exists(pk)" })).unwrap_err();
        assert_eq!(dup.kind, "ConditionalCheckFailedException");
        let u = r("UpdateItem", json!({ "TableName": "orders", "Key": { "pk": { "S": "c#1" }, "sk": { "N": "1" } }, "UpdateExpression": "SET amount = amount + :d, notes = list_append(if_not_exists(notes, :e), :n)", "ExpressionAttributeValues": { ":d": { "N": "5" }, ":e": { "L": [] }, ":n": { "L": [{ "S": "x" }] } }, "ReturnValues": "UPDATED_NEW" })).unwrap();
        assert_eq!(u["Attributes"]["amount"]["N"], "15");
        assert_eq!(u["Attributes"]["notes"]["L"][0]["S"], "x");
        let wrong_type =
            r("PutItem", json!({ "TableName": "orders", "Item": { "pk": { "S": "c#1" }, "sk": { "N": "99" }, "status": { "N": "1" } } }))
                .unwrap_err();
        assert!(wrong_type.message.contains("Index Key status"), "{}", wrong_type.message);
        let tx = r("TransactWriteItems", json!({ "TransactItems": [
            { "Put": { "TableName": "orders", "Item": { "pk": { "S": "c#9" }, "sk": { "N": "1" } } } },
            { "ConditionCheck": { "TableName": "orders", "Key": { "pk": { "S": "c#1" }, "sk": { "N": "3" } }, "ConditionExpression": "#s = :x", "ExpressionAttributeNames": { "#s": "status" }, "ExpressionAttributeValues": { ":x": { "S": "nope" } } } }
        ] })).unwrap_err();
        assert_eq!(tx.kind, "TransactionCanceledException");
        assert_eq!(tx.extra.unwrap()["CancellationReasons"][1]["Code"], "ConditionalCheckFailed");
        assert!(
            r("GetItem", json!({ "TableName": "orders", "Key": { "pk": { "S": "c#9" }, "sk": { "N": "1" } } }))
                .unwrap()
                .get("Item")
                .is_none(),
            "nothing written"
        );
        r("BatchWriteItem", json!({ "RequestItems": { "orders": [{ "PutRequest": { "Item": { "pk": { "S": "b" }, "sk": { "N": "1" } } } }, { "DeleteRequest": { "Key": { "pk": { "S": "c#1" }, "sk": { "N": "1" } } } }] } })).unwrap();
        let got = r("BatchGetItem", json!({ "RequestItems": { "orders": { "Keys": [{ "pk": { "S": "b" }, "sk": { "N": "1" } }, { "pk": { "S": "c#1" }, "sk": { "N": "1" } }] } } })).unwrap();
        assert_eq!(got["Responses"]["orders"].as_array().unwrap().len(), 1);
        let bad_key = r("GetItem", json!({ "TableName": "orders", "Key": { "pk": { "S": "b" } } })).unwrap_err();
        assert!(bad_key.message.contains("does not match the schema"));
    }

    #[test]
    fn ttl_snapshot_and_seed_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dynamodb.json");
        let s = DynamoStore::new(Some(path.clone()));
        s.load_seed(&store().export()).unwrap();
        s.handle(
            "UpdateTimeToLive",
            &json!({ "TableName": "orders", "TimeToLiveSpecification": { "Enabled": true, "AttributeName": "expires" } }),
            "x",
        )
        .unwrap();
        s.handle(
            "PutItem",
            &json!({ "TableName": "orders", "Item": { "pk": { "S": "t" }, "sk": { "N": "1" }, "expires": { "N": "1000" } } }),
            "x",
        )
        .unwrap();
        assert_eq!(s.sweep_expired(), 1);
        s.save_if_dirty().unwrap();
        let again = DynamoStore::new(Some(path));
        let d = again.handle("DescribeTable", &json!({ "TableName": "orders" }), "x").unwrap();
        assert_eq!(d["Table"]["ItemCount"], 10);
        assert_eq!(d["Table"]["GlobalSecondaryIndexes"][0]["IndexName"], "by-status");
        assert_eq!(
            again.handle("DescribeTimeToLive", &json!({ "TableName": "orders" }), "x").unwrap()["TimeToLiveDescription"]["AttributeName"],
            "expires"
        );
    }
}
