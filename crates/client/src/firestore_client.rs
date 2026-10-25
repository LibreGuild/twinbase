//! Google Cloud Firestore driver over gRPC (the same API the SDKs use): Google endpoints with
//! service-account credentials (self-signed JWTs), or emulators / TwinBase mocks; plus a small
//! statement language for the console.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value as Json, json};
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use twinbase_firestore_proto::json as fjson;
use twinbase_firestore_proto::v1::firestore_client::FirestoreClient as Grpc;
use twinbase_firestore_proto::v1::structured_query::{self as sq, CollectionSelector, FieldReference};
use twinbase_firestore_proto::v1::*;

use crate::google_auth::GoogleAuth;
use crate::{ClientError, ConnectionConfig, Result, TlsMode};

#[derive(Clone)]
struct BearerInterceptor(GoogleAuth);

impl Interceptor for BearerInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        let token = self.0.token_sync("https://firestore.googleapis.com/").map_err(|e| tonic::Status::unauthenticated(e.message))?;
        let v = MetadataValue::try_from(format!("Bearer {token}")).map_err(|_| tonic::Status::unauthenticated("invalid token"))?;
        req.metadata_mut().insert("authorization", v);
        Ok(req)
    }
}

type Client = Grpc<tonic::service::interceptor::InterceptedService<Channel, BearerInterceptor>>;

pub struct FirestoreClient {
    grpc: Client,
    /// `projects/{p}/databases/{d}`.
    pub database: String,
    pub endpoint: String,
    tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
}

fn status_err(s: tonic::Status) -> ClientError {
    let mut e = ClientError::new(if s.message().is_empty() { format!("{:?}", s.code()) } else { s.message().to_owned() });
    // Canonical gRPC code names (NOT_FOUND, ABORTED, …).
    let debug = format!("{:?}", s.code());
    let mut name = String::new();
    for (i, c) in debug.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            name.push('_');
        }
        name.push(c.to_ascii_uppercase());
    }
    e.code = Some(name);
    e
}

/// Project and database id from the connection's database field (`project` or `project/db`).
fn database_name(config: &ConnectionConfig, credentials_project: Option<String>) -> Result<String> {
    let field = config
        .database
        .clone()
        .filter(|d| !d.trim().is_empty())
        .or(credentials_project)
        .ok_or_else(|| ClientError::new("enter the Google Cloud project id"))?;
    let (project, db) = field.split_once('/').unwrap_or((field.as_str(), "(default)"));
    Ok(format!("projects/{}/databases/{}", project.trim(), if db.trim().is_empty() { "(default)" } else { db.trim() }))
}

impl FirestoreClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<FirestoreClient> {
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let google = config.host.trim().is_empty();
        // Emulators accept "owner"; a TwinBase mock with a password expects it as the token.
        let (auth, cred_project) = crate::google_auth::for_connection(config.path.as_deref(), config.password.as_deref(), !google)?;
        if matches!(auth, GoogleAuth::AuthorizedUser { .. }) {
            return Err(ClientError::new(
                "Firestore needs a service-account key file (gcloud user credentials are not supported for Firestore yet)",
            ));
        }
        let (https, host, port) = if google {
            (true, "firestore.googleapis.com".to_owned(), 443)
        } else if let Some((scheme, rest)) = config.host.trim().split_once("://") {
            let authority = rest.split('/').next().unwrap_or(rest);
            let https = scheme.eq_ignore_ascii_case("https");
            match authority.rsplit_once(':') {
                Some((h, p)) => (https, h.to_owned(), p.parse().unwrap_or(if https { 443 } else { 80 })),
                None => (https, authority.to_owned(), if https { 443 } else { 80 }),
            }
        } else {
            (config.tls != TlsMode::Disable, config.host.trim().to_owned(), if config.port == 0 { 8080 } else { config.port })
        };
        let connect_host = config.via.map(|v| (v.ip().to_string(), v.port())).unwrap_or((host.clone(), port));
        let uri = format!("{}://{}:{}", if https { "https" } else { "http" }, connect_host.0, connect_host.1);
        let mut ep = Endpoint::from_shared(uri)
            .map_err(|e| ClientError::new(e.to_string()))?
            .connect_timeout(timeout)
            .timeout(Duration::from_secs(60))
            .tcp_nodelay(true);
        if https {
            let tls = ClientTlsConfig::new().with_webpki_roots().domain_name(host.clone());
            ep = ep.tls_config(tls).map_err(|e| ClientError::new(format!("TLS: {e}")))?;
        }
        let channel = ep.connect().await.map_err(|e| ClientError::new(format!("cannot connect to {host}:{port}: {e}")))?;
        let database = database_name(config, cred_project)?;
        let grpc = Grpc::with_interceptor(channel, BearerInterceptor(auth)).max_decoding_message_size(64 * 1024 * 1024);
        let client =
            FirestoreClient { grpc, database, endpoint: format!("{}://{host}:{port}", if https { "https" } else { "http" }), tunnel: None };
        client.collection_ids("").await?;
        Ok(client)
    }

    pub fn with_tunnel(mut self, t: std::sync::Arc<crate::ssh::Tunnel>) -> Self {
        self.tunnel = Some(t);
        self
    }

    fn root(&self) -> String {
        format!("{}/documents", self.database)
    }

    fn name(&self, path: &str) -> String {
        format!("{}/{}", self.root(), path.trim_matches('/'))
    }

    fn parent(&self, doc: &str) -> String {
        if doc.trim_matches('/').is_empty() { self.root() } else { self.name(doc) }
    }

    /// Collection ids under a document (or the database root).
    pub async fn collection_ids(&self, doc: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let r = self
                .grpc
                .clone()
                .list_collection_ids(ListCollectionIdsRequest {
                    parent: self.parent(doc),
                    page_size: 300,
                    page_token: token,
                    ..Default::default()
                })
                .await
                .map_err(status_err)?
                .into_inner();
            out.extend(r.collection_ids);
            if r.next_page_token.is_empty() || out.len() > 10_000 {
                break;
            }
            token = r.next_page_token;
        }
        Ok(out)
    }

    pub async fn get(&self, path: &str) -> Result<Option<FsDoc>> {
        match self.grpc.clone().get_document(GetDocumentRequest { name: self.name(path), ..Default::default() }).await {
            Ok(d) => Ok(Some(FsDoc::from(d.into_inner()))),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(status_err(s)),
        }
    }

    pub async fn query(&self, q: &FsQuery) -> Result<Vec<FsDoc>> {
        let (parent, sq) = q.to_structured(self)?;
        let mut stream = self
            .grpc
            .clone()
            .run_query(RunQueryRequest {
                parent,
                query_type: Some(run_query_request::QueryType::StructuredQuery(sq)),
                ..Default::default()
            })
            .await
            .map_err(status_err)?
            .into_inner();
        let mut out = Vec::new();
        while let Some(r) = stream.message().await.map_err(status_err)? {
            if let Some(d) = r.document {
                out.push(FsDoc::from(d));
            }
        }
        Ok(out)
    }

    pub async fn count(&self, q: &FsQuery) -> Result<i64> {
        let (parent, sq) = q.to_structured(self)?;
        let agg = StructuredAggregationQuery {
            aggregations: vec![structured_aggregation_query::Aggregation {
                alias: "count".into(),
                operator: Some(structured_aggregation_query::aggregation::Operator::Count(Default::default())),
            }],
            query_type: Some(structured_aggregation_query::QueryType::StructuredQuery(sq)),
        };
        let mut s = self
            .grpc
            .clone()
            .run_aggregation_query(RunAggregationQueryRequest {
                parent,
                query_type: Some(run_aggregation_query_request::QueryType::StructuredAggregationQuery(agg)),
                ..Default::default()
            })
            .await
            .map_err(status_err)?
            .into_inner();
        let r = s.message().await.map_err(status_err)?.and_then(|r| r.result).ok_or_else(|| ClientError::new("no aggregation result"))?;
        Ok(match r.aggregate_fields.get("count").and_then(|v| v.value_type.clone()) {
            Some(value::ValueType::IntegerValue(n)) => n,
            _ => 0,
        })
    }

    /// Write a whole document (`merge`: only the given top-level or dotted fields).
    pub async fn set(&self, path: &str, data: &Json, merge: bool, must_exist: Option<bool>) -> Result<FsDoc> {
        let fields = if merge {
            // Dotted keys address nested fields, as in the SDKs' update().
            let mut nested = Json::Object(Default::default());
            let o = data.as_object().ok_or_else(|| ClientError::new("the data must be a JSON object"))?;
            for (k, v) in o {
                let mut cur = &mut nested;
                let segs: Vec<&str> = k.split('.').collect();
                for (i, s) in segs.iter().enumerate() {
                    if i + 1 == segs.len() {
                        cur[*s] = v.clone();
                    } else {
                        if !cur[*s].is_object() {
                            cur[*s] = json!({});
                        }
                        cur = &mut cur[*s];
                    }
                }
            }
            fjson::fields_from_plain(&nested, &self.database).map_err(ClientError::new)?
        } else {
            fjson::fields_from_plain(data, &self.database).map_err(ClientError::new)?
        };
        let mask = merge.then(|| DocumentMask {
            field_paths: data
                .as_object()
                .map(|o| o.keys().map(|k| k.split('.').map(quote_segment).collect::<Vec<_>>().join(".")).collect())
                .unwrap_or_default(),
        });
        let w = Write {
            operation: Some(write::Operation::Update(Document { name: self.name(path), fields, ..Default::default() })),
            update_mask: mask,
            current_document: must_exist.map(|e| Precondition { condition_type: Some(precondition::ConditionType::Exists(e)) }),
            ..Default::default()
        };
        self.grpc
            .clone()
            .commit(CommitRequest { database: self.database.clone(), writes: vec![w], ..Default::default() })
            .await
            .map_err(status_err)?;
        self.get(path).await?.ok_or_else(|| ClientError::new("the document disappeared after writing"))
    }

    pub async fn add(&self, collection: &str, data: &Json) -> Result<FsDoc> {
        let fields = fjson::fields_from_plain(data, &self.database).map_err(ClientError::new)?;
        let (parent, coll) = match collection.trim_matches('/').rsplit_once('/') {
            Some((p, c)) => (self.name(p), c.to_owned()),
            None => (self.root(), collection.trim_matches('/').to_owned()),
        };
        let d = self
            .grpc
            .clone()
            .create_document(CreateDocumentRequest {
                parent,
                collection_id: coll,
                document: Some(Document { fields, ..Default::default() }),
                ..Default::default()
            })
            .await
            .map_err(status_err)?;
        Ok(FsDoc::from(d.into_inner()))
    }

    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let writes =
            paths.iter().map(|p| Write { operation: Some(write::Operation::Delete(self.name(p))), ..Default::default() }).collect();
        self.grpc
            .clone()
            .commit(CommitRequest { database: self.database.clone(), writes, ..Default::default() })
            .await
            .map_err(status_err)?;
        Ok(paths.len())
    }

    /// Run console statements; each gives one output (errors stop the run).
    pub async fn run(&self, script: &str, max_docs: usize) -> Result<Vec<FsOutput>> {
        let statements = parse_script(script)?;
        if statements.is_empty() {
            return Err(ClientError::new("nothing to run"));
        }
        let mut out = Vec::new();
        for (st, text) in statements {
            let started = Instant::now();
            let mut o = FsOutput { statement: text, ..Default::default() };
            let r: Result<()> = async {
                match st {
                    Statement::Get(p) => o.documents = Some(self.get(&p).await?.into_iter().collect()),
                    Statement::Collections(p) => o.value = Some(json!(self.collection_ids(&p).await?)),
                    Statement::Query(mut q) => {
                        let cap = q.limit.map(|l| l.min(max_docs as i32)).unwrap_or(max_docs as i32);
                        q.limit = Some(cap + 1);
                        let mut docs = self.query(&q).await?;
                        o.truncated = docs.len() > cap as usize;
                        docs.truncate(cap as usize);
                        o.documents = Some(docs);
                    }
                    Statement::Count(q) => o.value = Some(json!(self.count(&q).await?)),
                    Statement::Set(p, data) => {
                        o.documents = Some(vec![self.set(&p, &data, false, None).await?]);
                        o.message = Some(format!("Set {p}"));
                    }
                    Statement::Update(p, data) => {
                        o.documents = Some(vec![self.set(&p, &data, true, Some(true)).await?]);
                        o.message = Some(format!("Updated {p}"));
                    }
                    Statement::Add(c, data) => {
                        let d = self.add(&c, &data).await?;
                        o.message = Some(format!("Added {}", d.path));
                        o.documents = Some(vec![d]);
                    }
                    Statement::Delete(p) => {
                        self.delete(std::slice::from_ref(&p)).await?;
                        o.message = Some(format!("Deleted {p}"));
                    }
                }
                Ok(())
            }
            .await;
            o.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
            let failed = r.is_err();
            if let Err(e) = r {
                o.error = Some(e);
            }
            out.push(o);
            if failed {
                break;
            }
        }
        Ok(out)
    }
}

fn quote_segment(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.starts_with(|c: char| c.is_ascii_digit()) {
        s.to_owned()
    } else {
        format!("`{}`", s.replace('`', "\\`"))
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FsDoc {
    pub id: String,
    /// Path below the database root (`users/alice`).
    pub path: String,
    pub data: Json,
    pub create_time: Option<String>,
    pub update_time: Option<String>,
}

impl From<Document> for FsDoc {
    fn from(d: Document) -> Self {
        let path = d.name.split_once("/documents/").map(|(_, p)| p.to_owned()).unwrap_or_default();
        FsDoc {
            id: path.rsplit('/').next().unwrap_or_default().to_owned(),
            path,
            data: fjson::fields_to_plain(&d.fields),
            create_time: d.create_time.as_ref().map(fjson::timestamp_text),
            update_time: d.update_time.as_ref().map(fjson::timestamp_text),
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FsOutput {
    pub statement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<Vec<FsDoc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub truncated: bool,
    pub duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientError>,
}

// ------------------------------------------------------------------ queries and statements

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct FsCondition {
    pub field: String,
    /// `==`, `!=`, `<`, `<=`, `>`, `>=`, `in`, `not-in`, `array-contains`, `array-contains-any`.
    pub op: String,
    pub value: Json,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct FsQuery {
    /// Collection path (`users` or `users/alice/orders`), or a collection id for group queries.
    pub collection: String,
    #[serde(default)]
    pub group: bool,
    #[serde(default)]
    pub conditions: Vec<FsCondition>,
    /// Field and descending flag.
    #[serde(default)]
    pub order: Vec<(String, bool)>,
    #[serde(default)]
    pub limit: Option<i32>,
    /// Start after this document path (paging by name order).
    #[serde(default)]
    pub start_after: Option<String>,
}

impl FsQuery {
    fn to_structured(&self, c: &FirestoreClient) -> Result<(String, StructuredQuery)> {
        let coll = self.collection.trim_matches('/');
        if coll.is_empty() {
            return Err(ClientError::new("choose a collection"));
        }
        let (parent, id) = if self.group {
            (c.root(), coll.to_owned())
        } else {
            match coll.rsplit_once('/') {
                Some((p, id)) => (c.name(p), id.to_owned()),
                None => (c.root(), coll.to_owned()),
            }
        };
        let mut filters = Vec::new();
        for cond in &self.conditions {
            use sq::field_filter::Operator as Op;
            let op = match cond.op.as_str() {
                "==" | "=" => Op::Equal,
                "!=" => Op::NotEqual,
                "<" => Op::LessThan,
                "<=" => Op::LessThanOrEqual,
                ">" => Op::GreaterThan,
                ">=" => Op::GreaterThanOrEqual,
                "in" => Op::In,
                "not-in" => Op::NotIn,
                "array-contains" => Op::ArrayContains,
                "array-contains-any" => Op::ArrayContainsAny,
                other => return Err(ClientError::new(format!("unknown operator {other}"))),
            };
            let value = fjson::from_plain(&cond.value, &c.database).map_err(ClientError::new)?;
            let field = FieldReference { field_path: cond.field.split('.').map(quote_segment).collect::<Vec<_>>().join(".") };
            // == null / == NaN are unary filters in Firestore.
            let unary = match (&op, &value.value_type) {
                (Op::Equal, Some(value::ValueType::NullValue(_))) => Some(sq::unary_filter::Operator::IsNull),
                (Op::NotEqual, Some(value::ValueType::NullValue(_))) => Some(sq::unary_filter::Operator::IsNotNull),
                _ => None,
            };
            filters.push(sq::Filter {
                filter_type: Some(match unary {
                    Some(u) => sq::filter::FilterType::UnaryFilter(sq::UnaryFilter {
                        op: u as i32,
                        operand_type: Some(sq::unary_filter::OperandType::Field(field)),
                    }),
                    None => sq::filter::FilterType::FieldFilter(sq::FieldFilter { field: Some(field), op: op as i32, value: Some(value) }),
                }),
            });
        }
        let r#where = match filters.len() {
            0 => None,
            1 => filters.pop(),
            _ => Some(sq::Filter {
                filter_type: Some(sq::filter::FilterType::CompositeFilter(sq::CompositeFilter {
                    op: sq::composite_filter::Operator::And as i32,
                    filters,
                })),
            }),
        };
        let mut order_by: Vec<sq::Order> = self
            .order
            .iter()
            .map(|(f, desc)| sq::Order {
                field: Some(FieldReference {
                    field_path: if f == "__name__" { f.clone() } else { f.split('.').map(quote_segment).collect::<Vec<_>>().join(".") },
                }),
                direction: if *desc { sq::Direction::Descending } else { sq::Direction::Ascending } as i32,
            })
            .collect();
        let start_at = self.start_after.as_ref().map(|p| {
            if !order_by.iter().any(|o| o.field.as_ref().is_some_and(|f| f.field_path == "__name__")) {
                order_by.push(sq::Order {
                    field: Some(FieldReference { field_path: "__name__".into() }),
                    direction: sq::Direction::Ascending as i32,
                });
            }
            Cursor { values: vec![Value { value_type: Some(value::ValueType::ReferenceValue(c.name(p))) }], before: false }
        });
        Ok((
            parent,
            StructuredQuery {
                from: vec![CollectionSelector { collection_id: id, all_descendants: self.group }],
                r#where,
                order_by,
                limit: self.limit,
                start_at,
                ..Default::default()
            },
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Get(String),
    Collections(String),
    Query(FsQuery),
    Count(FsQuery),
    Set(String, Json),
    Update(String, Json),
    Add(String, Json),
    Delete(String),
}

impl Statement {
    pub fn writes(&self) -> bool {
        matches!(self, Statement::Set(..) | Statement::Update(..) | Statement::Add(..) | Statement::Delete(_))
    }
}

const KEYWORDS: &[&str] = &["get", "collections", "query", "count", "set", "update", "add", "delete", "list"];

/// Statements start at lines beginning with a keyword; other lines continue the previous one.
pub fn parse_script(src: &str) -> Result<Vec<(Statement, String)>> {
    let mut blocks: Vec<String> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        let first = t.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if KEYWORDS.contains(&first.as_str()) || blocks.is_empty() {
            blocks.push(t.to_owned());
        } else {
            let last = blocks.last_mut().unwrap();
            last.push('\n');
            last.push_str(t);
        }
    }
    blocks.into_iter().map(|b| parse_statement(&b).map(|s| (s, b))).collect()
}

struct Lexer<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Lexer<'a> {
    fn ws(&mut self) {
        while self.s[self.i..].starts_with(char::is_whitespace) {
            self.i += self.s[self.i..].chars().next().unwrap().len_utf8();
        }
    }
    fn done(&mut self) -> bool {
        self.ws();
        self.i >= self.s.len()
    }
    fn word(&mut self) -> Option<String> {
        self.ws();
        let rest = &self.s[self.i..];
        if let Some(stripped) = rest.strip_prefix('`') {
            let end = stripped.find('`')?;
            self.i += end + 2;
            return Some(stripped[..end].to_owned());
        }
        let len = rest.find(|c: char| c.is_whitespace() || c == ',').unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        self.i += len;
        Some(rest[..len].to_owned())
    }
    fn peek_kw(&mut self, kw: &str) -> bool {
        self.ws();
        let rest = &self.s[self.i..];
        rest.len() >= kw.len()
            && rest[..kw.len()].eq_ignore_ascii_case(kw)
            && rest[kw.len()..].chars().next().is_none_or(|c| c.is_whitespace() || c == ',')
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.peek_kw(kw) {
            self.i += kw.len();
            true
        } else {
            false
        }
    }
    fn json(&mut self) -> Result<Json> {
        self.ws();
        let mut it = serde_json::Deserializer::from_str(&self.s[self.i..]).into_iter::<Json>();
        match it.next() {
            Some(Ok(v)) => {
                self.i += it.byte_offset();
                Ok(v)
            }
            Some(Err(e)) => Err(ClientError::new(format!("invalid JSON value: {e} (strings need double quotes)"))),
            None => Err(ClientError::new("a value is missing")),
        }
    }
}

fn parse_query(lx: &mut Lexer, kw: &str) -> Result<FsQuery> {
    let group = lx.eat_kw("group");
    let collection = lx.word().ok_or_else(|| ClientError::new(format!("{kw} needs a collection, e.g. {kw} users where age > 30")))?;
    let mut q = FsQuery { collection: collection.trim_matches('/').to_owned(), group, ..Default::default() };
    if lx.eat_kw("where") {
        loop {
            let field = lx.word().ok_or_else(|| ClientError::new("where needs a field"))?;
            let op = lx.word().ok_or_else(|| ClientError::new("where needs an operator"))?.to_ascii_lowercase();
            let value = lx.json()?;
            q.conditions.push(FsCondition { field, op, value });
            if !lx.eat_kw("and") {
                break;
            }
        }
    }
    if lx.eat_kw("order") {
        if !lx.eat_kw("by") {
            return Err(ClientError::new("expected 'order by'"));
        }
        loop {
            let field = lx.word().ok_or_else(|| ClientError::new("order by needs a field"))?;
            let field = field.trim_end_matches(',').to_owned();
            let desc = lx.eat_kw("desc");
            if !desc {
                lx.eat_kw("asc");
            }
            q.order.push((field, desc));
            lx.ws();
            if lx.s[lx.i..].starts_with(',') {
                lx.i += 1;
            } else {
                break;
            }
        }
    }
    if lx.eat_kw("limit") {
        let n = lx.word().and_then(|w| w.parse().ok()).ok_or_else(|| ClientError::new("limit needs a number"))?;
        q.limit = Some(n);
    }
    if !lx.done() {
        return Err(ClientError::new(format!("unexpected text: {}", &lx.s[lx.i..])));
    }
    Ok(q)
}

pub fn parse_statement(src: &str) -> Result<Statement> {
    let mut lx = Lexer { s: src, i: 0 };
    let kw = lx.word().unwrap_or_default().to_ascii_lowercase();
    let path = |lx: &mut Lexer, what: &str| {
        lx.word().map(|p| p.trim_matches('/').to_owned()).ok_or_else(|| ClientError::new(format!("{what} needs a path")))
    };
    let st = match kw.as_str() {
        "get" => Statement::Get(path(&mut lx, "get")?),
        "delete" => Statement::Delete(path(&mut lx, "delete")?),
        "collections" => Statement::Collections(lx.word().unwrap_or_default().trim_matches('/').to_owned()),
        "list" => {
            let c = path(&mut lx, "list")?;
            let limit = if lx.eat_kw("limit") { lx.word().and_then(|w| w.parse().ok()) } else { None };
            Statement::Query(FsQuery { collection: c, limit, ..Default::default() })
        }
        "query" => Statement::Query(parse_query(&mut lx, "query")?),
        "count" => Statement::Count(parse_query(&mut lx, "count")?),
        "set" | "update" | "add" => {
            let p = path(&mut lx, &kw)?;
            let data = lx.json()?;
            if !data.is_object() {
                return Err(ClientError::new(format!("{kw} needs a JSON object of fields")));
            }
            match kw.as_str() {
                "set" => Statement::Set(p, data),
                "update" => Statement::Update(p, data),
                _ => Statement::Add(p, data),
            }
        }
        other => {
            return Err(ClientError::new(format!(
                "unknown statement '{other}': use get, list, query, count, collections, set, update, add or delete"
            )));
        }
    };
    if !lx.done() {
        return Err(ClientError::new(format!("unexpected text: {}", &lx.s[lx.i..])));
    }
    if let Statement::Get(p) | Statement::Set(p, _) | Statement::Update(p, _) | Statement::Delete(p) = &st
        && p.split('/').count() % 2 != 0
    {
        return Err(ClientError::new(format!("{p} is a collection; documents have an even number of path segments (users/alice)")));
    }
    Ok(st)
}

/// Does the script change data? Unparseable scripts count as writes.
pub fn script_writes(src: &str) -> bool {
    parse_script(src).map(|s| s.iter().any(|(st, _)| st.writes())).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements() {
        let s = parse_script("query users where age >= 30 and city == \"Lisbon\" and tags array-contains \"x\" order by age desc, name limit 10\ncount group orders where status in [\"open\", \"paid\"]\nset users/ann {\n  \"name\": \"Ann\",\n  \"joined\": {\"__timestamp\": \"2024-01-01T00:00:00Z\"}\n}\ncollections\ndelete users/ann").unwrap();
        assert_eq!(s.len(), 5);
        let Statement::Query(q) = &s[0].0 else { panic!() };
        assert_eq!(q.conditions.len(), 3);
        assert_eq!(q.conditions[1], FsCondition { field: "city".into(), op: "==".into(), value: json!("Lisbon") });
        assert_eq!(q.order, vec![("age".to_owned(), true), ("name".to_owned(), false)]);
        assert_eq!(q.limit, Some(10));
        let Statement::Count(c) = &s[1].0 else { panic!() };
        assert!(c.group);
        assert!(matches!(&s[2].0, Statement::Set(p, d) if p == "users/ann" && d["joined"]["__timestamp"].is_string()));
        assert!(script_writes("delete users/x") && !script_writes("query users"));
        assert!(parse_statement("get users").unwrap_err().message.contains("collection"));
        assert!(parse_statement("query users where age > thirty").is_err());
        assert!(parse_statement("frobnicate x").is_err());
    }
}
