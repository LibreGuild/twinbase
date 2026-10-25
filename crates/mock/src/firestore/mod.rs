//! Google Cloud Firestore mock: the `google.firestore.v1.Firestore` gRPC service, compatible with
//! client SDKs pointed at it through `FIRESTORE_EMULATOR_HOST` — documents, queries, aggregations,
//! transactions, batch writes and realtime listeners (`Listen`).

pub mod query;
pub mod store;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::Stream;
use parking_lot::Mutex;
use prost_types::Timestamp;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};
use twinbase_firestore_proto::v1::firestore_server::{Firestore, FirestoreServer};
use twinbase_firestore_proto::v1::structured_query::filter::FilterType;
use twinbase_firestore_proto::v1::*;

pub use store::FirestoreStore;

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};
use store::{check_doc_path, doc_name, split_name};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Snapshot file used for a Firestore mock data directory.
pub fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join("firestore.json")
}

struct Service {
    store: Arc<FirestoreStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    peers: Arc<Mutex<HashMap<SocketAddr, u64>>>,
}

fn status_from_fault(code: Option<String>, message: String) -> Status {
    use tonic::Code;
    let code = match code.as_deref().map(|c| c.trim().to_ascii_uppercase()).as_deref() {
        Some("CANCELLED" | "1") => Code::Cancelled,
        Some("INVALID_ARGUMENT" | "3") => Code::InvalidArgument,
        Some("DEADLINE_EXCEEDED" | "4") => Code::DeadlineExceeded,
        Some("NOT_FOUND" | "5") => Code::NotFound,
        Some("ALREADY_EXISTS" | "6") => Code::AlreadyExists,
        Some("PERMISSION_DENIED" | "7") => Code::PermissionDenied,
        Some("RESOURCE_EXHAUSTED" | "8") => Code::ResourceExhausted,
        Some("FAILED_PRECONDITION" | "9") => Code::FailedPrecondition,
        Some("ABORTED" | "10") => Code::Aborted,
        Some("UNIMPLEMENTED" | "12") => Code::Unimplemented,
        Some("INTERNAL" | "13") => Code::Internal,
        Some("UNAVAILABLE" | "14") => Code::Unavailable,
        Some("UNAUTHENTICATED" | "16") => Code::Unauthenticated,
        _ => Code::Unavailable,
    };
    Status::new(code, message)
}

impl Service {
    fn connection<T>(&self, req: &Request<T>) -> u64 {
        req.remote_addr().and_then(|a| self.peers.lock().get(&a).copied()).unwrap_or(0)
    }

    fn check_auth<T>(&self, req: &Request<T>) -> Result<(), Status> {
        if let AuthMode::Password { password, .. } = &self.auth {
            let header = req.metadata().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            if header.strip_prefix("Bearer ").map(str::trim) != Some(password.as_str()) {
                return Err(Status::unauthenticated(
                    "This TwinBase Firestore mock requires \"Authorization: Bearer <token>\" with the mock's password as the token",
                ));
            }
        }
        Ok(())
    }

    /// Auth, fault injection and traffic logging around one call.
    async fn guard<T, R>(&self, req: &Request<T>, text: String, f: impl FnOnce() -> Result<(R, u64), Status>) -> Result<R, Status> {
        let conn = self.connection(req);
        let started = Instant::now();
        if let Err(e) = self.check_auth(req) {
            self.traffic.query(conn, &text, started, 0, Some(e.message().to_owned()));
            return Err(e);
        }
        match self.faults.before(&text).await {
            Decision::Proceed => {}
            Decision::Fail { code, message } => {
                self.traffic.fault(conn, &text, &message);
                return Err(status_from_fault(code, message));
            }
            Decision::Drop => {
                self.traffic.fault(conn, &text, "connection dropped");
                return Err(Status::unavailable("connection dropped (fault injection)"));
            }
        }
        let r = f();
        match &r {
            Ok((_, rows)) => self.traffic.query(conn, &text, started, *rows, None),
            Err(e) => self.traffic.query(conn, &text, started, 0, Some(format!("{:?}: {}", e.code(), e.message()))),
        }
        r.map(|(v, _)| v)
    }
}

// ------------------------------------------------------------------ traffic text

fn rel(name: &str) -> String {
    split_name(name).map(|(_, r)| r).unwrap_or_else(|_| name.to_owned())
}

fn value_text(v: &Option<Value>) -> String {
    v.as_ref().map(|v| twinbase_firestore_proto::json::to_plain(v).to_string()).unwrap_or_default()
}

fn filter_text(f: &structured_query::Filter) -> String {
    use structured_query::{composite_filter, field_filter, unary_filter};
    match &f.filter_type {
        Some(FilterType::CompositeFilter(c)) => {
            let op = if c.op == composite_filter::Operator::Or as i32 { " or " } else { " and " };
            let parts: Vec<String> = c.filters.iter().map(filter_text).collect();
            if parts.len() > 1 { format!("({})", parts.join(op)) } else { parts.join(op) }
        }
        Some(FilterType::FieldFilter(ff)) => {
            let op = match field_filter::Operator::try_from(ff.op).unwrap_or(field_filter::Operator::Unspecified) {
                field_filter::Operator::LessThan => "<",
                field_filter::Operator::LessThanOrEqual => "<=",
                field_filter::Operator::GreaterThan => ">",
                field_filter::Operator::GreaterThanOrEqual => ">=",
                field_filter::Operator::Equal => "==",
                field_filter::Operator::NotEqual => "!=",
                field_filter::Operator::ArrayContains => "array-contains",
                field_filter::Operator::In => "in",
                field_filter::Operator::ArrayContainsAny => "array-contains-any",
                field_filter::Operator::NotIn => "not-in",
                field_filter::Operator::Unspecified => "?",
            };
            format!("{} {op} {}", ff.field.as_ref().map(|f| f.field_path.as_str()).unwrap_or(""), value_text(&ff.value))
        }
        Some(FilterType::UnaryFilter(u)) => {
            let field = match &u.operand_type {
                Some(unary_filter::OperandType::Field(f)) => f.field_path.clone(),
                None => String::new(),
            };
            let op = match unary_filter::Operator::try_from(u.op).unwrap_or(unary_filter::Operator::Unspecified) {
                unary_filter::Operator::IsNan => "is NaN",
                unary_filter::Operator::IsNull => "is null",
                unary_filter::Operator::IsNotNan => "is not NaN",
                unary_filter::Operator::IsNotNull => "is not null",
                unary_filter::Operator::Unspecified => "?",
            };
            format!("{field} {op}")
        }
        None => String::new(),
    }
}

fn query_text(parent: &str, q: &StructuredQuery) -> String {
    let mut s = String::new();
    let base = rel(parent);
    for f in &q.from {
        if f.all_descendants {
            s.push_str(&format!("group {}", f.collection_id));
        } else if base.is_empty() {
            s.push_str(&f.collection_id);
        } else {
            s.push_str(&format!("{base}/{}", f.collection_id));
        }
    }
    if let Some(w) = &q.r#where {
        s.push_str(&format!(" where {}", filter_text(w)));
    }
    for o in &q.order_by {
        s.push_str(&format!(
            " order by {}{}",
            o.field.as_ref().map(|f| f.field_path.as_str()).unwrap_or(""),
            if o.direction == structured_query::Direction::Descending as i32 { " desc" } else { "" }
        ));
    }
    if let Some(l) = q.limit {
        s.push_str(&format!(" limit {l}"));
    }
    s
}

fn write_text(w: &Write) -> String {
    match &w.operation {
        Some(write::Operation::Update(d)) if w.update_mask.is_some() => {
            format!("update {} {}", rel(&d.name), twinbase_firestore_proto::json::fields_to_plain(&d.fields))
        }
        Some(write::Operation::Update(d)) => format!("set {} {}", rel(&d.name), twinbase_firestore_proto::json::fields_to_plain(&d.fields)),
        Some(write::Operation::Delete(n)) => format!("delete {}", rel(n)),
        Some(write::Operation::Transform(t)) => format!("transform {}", rel(&t.document)),
        None => "?".into(),
    }
}

// ------------------------------------------------------------------ helpers

fn mask_doc(mut d: Document, mask: &Option<DocumentMask>) -> Result<Document, Status> {
    if let Some(m) = mask {
        let sel = structured_query::Projection {
            fields: m.field_paths.iter().map(|p| structured_query::FieldReference { field_path: p.clone() }).collect(),
        };
        d.fields = query::project(&d.fields, &sel)?;
    }
    Ok(d)
}

enum ReadTxn {
    None,
    Existing(Vec<u8>),
    New(Vec<u8>),
}

impl ReadTxn {
    fn id(&self) -> Option<&[u8]> {
        match self {
            ReadTxn::None => None,
            ReadTxn::Existing(t) | ReadTxn::New(t) => Some(t),
        }
    }
    fn new_id(&self) -> Vec<u8> {
        match self {
            ReadTxn::New(t) => t.clone(),
            _ => vec![],
        }
    }
}

fn read_txn(store: &FirestoreStore, database: &str, transaction: Option<Vec<u8>>, new_transaction: Option<TransactionOptions>) -> ReadTxn {
    if let Some(t) = transaction {
        return ReadTxn::Existing(t);
    }
    if let Some(opts) = new_transaction {
        let read_only = matches!(opts.mode, Some(transaction_options::Mode::ReadOnly(_)));
        return ReadTxn::New(store.begin(database, read_only));
    }
    ReadTxn::None
}

fn resume_token(t: &Timestamp) -> Vec<u8> {
    let micros = t.seconds * 1_000_000 + (t.nanos / 1000) as i64;
    micros.to_be_bytes().to_vec()
}

fn database_of(parent: &str) -> Result<(String, String), Status> {
    split_name(parent)
}

fn stream<T: Send + 'static>(items: Vec<Result<T, Status>>) -> BoxStream<T> {
    Box::pin(futures::stream::iter(items))
}

// ------------------------------------------------------------------ service

#[tonic::async_trait]
impl Firestore for Service {
    async fn get_document(&self, req: Request<GetDocumentRequest>) -> Result<Response<Document>, Status> {
        let r = req.get_ref().clone();
        let text = format!("GetDocument {}", rel(&r.name));
        self.guard(&req, text, || {
            let (db, path) = split_name(&r.name)?;
            let txn = match &r.consistency_selector {
                Some(get_document_request::ConsistencySelector::ReadTime(_)) => return Err(read_time_unsupported()),
                Some(get_document_request::ConsistencySelector::Transaction(t)) => Some(t.clone()),
                _ => None,
            };
            let d = self
                .store
                .get(&db, &path, txn.as_deref())?
                .ok_or_else(|| Status::not_found(format!("No document to get: \"{}\"", r.name)))?;
            Ok((Response::new(mask_doc(d, &r.mask)?), 1))
        })
        .await
    }

    async fn list_documents(&self, req: Request<ListDocumentsRequest>) -> Result<Response<ListDocumentsResponse>, Status> {
        let r = req.get_ref().clone();
        let text = format!(
            "ListDocuments {}",
            if rel(&r.parent).is_empty() { r.collection_id.clone() } else { format!("{}/{}", rel(&r.parent), r.collection_id) }
        );
        self.guard(&req, text, || {
            let (db, parent) = database_of(&r.parent)?;
            let coll = if parent.is_empty() { r.collection_id.clone() } else { format!("{parent}/{}", r.collection_id) };
            let mut docs = self.store.list(&db, &coll);
            if !r.page_token.is_empty() {
                docs.retain(|d| d.name.as_str() > r.page_token.as_str());
            }
            let size = if r.page_size > 0 { r.page_size as usize } else { 300 };
            let next = if docs.len() > size { docs[size - 1].name.clone() } else { String::new() };
            docs.truncate(size);
            let docs = docs.into_iter().map(|d| mask_doc(d, &r.mask)).collect::<Result<Vec<_>, _>>()?;
            let n = docs.len() as u64;
            Ok((Response::new(ListDocumentsResponse { documents: docs, next_page_token: next }), n))
        })
        .await
    }

    async fn update_document(&self, req: Request<UpdateDocumentRequest>) -> Result<Response<Document>, Status> {
        let r = req.get_ref().clone();
        let doc = r.document.clone().ok_or_else(|| Status::invalid_argument("document is required"))?;
        let w = Write {
            operation: Some(write::Operation::Update(doc.clone())),
            update_mask: r.update_mask.clone(),
            update_transforms: vec![],
            current_document: r.current_document,
        };
        let text = format!("UpdateDocument {}", write_text(&w));
        self.guard(&req, text, || {
            let (db, path) = split_name(&doc.name)?;
            self.store.commit(&db, &[w], None)?;
            let d = self.store.get(&db, &path, None)?.ok_or_else(|| Status::internal("written document vanished"))?;
            Ok((Response::new(mask_doc(d, &r.mask)?), 1))
        })
        .await
    }

    async fn delete_document(&self, req: Request<DeleteDocumentRequest>) -> Result<Response<()>, Status> {
        let r = req.get_ref().clone();
        let text = format!("DeleteDocument {}", rel(&r.name));
        self.guard(&req, text, || {
            let (db, _) = split_name(&r.name)?;
            self.store.commit(
                &db,
                &[Write {
                    operation: Some(write::Operation::Delete(r.name.clone())),
                    current_document: r.current_document,
                    ..Default::default()
                }],
                None,
            )?;
            Ok((Response::new(()), 1))
        })
        .await
    }

    type BatchGetDocumentsStream = BoxStream<BatchGetDocumentsResponse>;

    async fn batch_get_documents(&self, req: Request<BatchGetDocumentsRequest>) -> Result<Response<Self::BatchGetDocumentsStream>, Status> {
        let r = req.get_ref().clone();
        let text = format!("BatchGetDocuments {}", r.documents.iter().map(|d| rel(d)).collect::<Vec<_>>().join(", "));
        self.guard(&req, text, || {
            let (tx, new) = match r.consistency_selector.clone() {
                Some(batch_get_documents_request::ConsistencySelector::ReadTime(_)) => return Err(read_time_unsupported()),
                Some(batch_get_documents_request::ConsistencySelector::Transaction(t)) => (Some(t), None),
                Some(batch_get_documents_request::ConsistencySelector::NewTransaction(o)) => (None, Some(o)),
                _ => (None, None),
            };
            let txn = read_txn(&self.store, &r.database, tx, new);
            let read_time = self.store.read_time();
            let mut out = Vec::new();
            let mut found = 0;
            for (i, name) in r.documents.iter().enumerate() {
                let (db, path) = split_name(name)?;
                let result = match self.store.get(&db, &path, txn.id())? {
                    Some(d) => {
                        found += 1;
                        batch_get_documents_response::Result::Found(mask_doc(d, &r.mask)?)
                    }
                    None => batch_get_documents_response::Result::Missing(name.clone()),
                };
                out.push(Ok(BatchGetDocumentsResponse {
                    result: Some(result),
                    read_time: Some(read_time),
                    transaction: if i == 0 { txn.new_id() } else { vec![] },
                }));
            }
            if out.is_empty() && matches!(txn, ReadTxn::New(_)) {
                out.push(Ok(BatchGetDocumentsResponse { result: None, read_time: Some(read_time), transaction: txn.new_id() }));
            }
            Ok((Response::new(stream(out)), found))
        })
        .await
    }

    async fn begin_transaction(&self, req: Request<BeginTransactionRequest>) -> Result<Response<BeginTransactionResponse>, Status> {
        let r = req.get_ref().clone();
        self.guard(&req, "BeginTransaction".into(), || {
            let read_only = matches!(r.options.as_ref().and_then(|o| o.mode.as_ref()), Some(transaction_options::Mode::ReadOnly(_)));
            Ok((Response::new(BeginTransactionResponse { transaction: self.store.begin(&r.database, read_only) }), 0))
        })
        .await
    }

    async fn commit(&self, req: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        let r = req.get_ref().clone();
        let text = format!(
            "Commit{} {}",
            if r.transaction.is_empty() { "" } else { " (transaction)" },
            r.writes.iter().map(write_text).collect::<Vec<_>>().join("; ")
        );
        self.guard(&req, text, || {
            let (time, results) =
                self.store.commit(&r.database, &r.writes, (!r.transaction.is_empty()).then_some(r.transaction.as_slice()))?;
            let n = results.len() as u64;
            Ok((Response::new(CommitResponse { write_results: results, commit_time: Some(time) }), n))
        })
        .await
    }

    async fn rollback(&self, req: Request<RollbackRequest>) -> Result<Response<()>, Status> {
        let r = req.get_ref().clone();
        self.guard(&req, "Rollback".into(), || {
            self.store.rollback(&r.transaction);
            Ok((Response::new(()), 0))
        })
        .await
    }

    type RunQueryStream = BoxStream<RunQueryResponse>;

    async fn run_query(&self, req: Request<RunQueryRequest>) -> Result<Response<Self::RunQueryStream>, Status> {
        let r = req.get_ref().clone();
        let Some(run_query_request::QueryType::StructuredQuery(q)) = r.query_type.clone() else {
            return Err(Status::invalid_argument("structured_query is required"));
        };
        let text = format!("RunQuery {}", query_text(&r.parent, &q));
        self.guard(&req, text, || {
            let (db, parent) = database_of(&r.parent)?;
            let (tx, new) = match r.consistency_selector.clone() {
                Some(run_query_request::ConsistencySelector::ReadTime(_)) => return Err(read_time_unsupported()),
                Some(run_query_request::ConsistencySelector::Transaction(t)) => (Some(t), None),
                Some(run_query_request::ConsistencySelector::NewTransaction(o)) => (None, Some(o)),
                _ => (None, None),
            };
            let txn = read_txn(&self.store, &r.parent, tx, new);
            let read_time = self.store.read_time();
            let docs = self.store.run_query(&db, &parent, &q, txn.id())?;
            let n = docs.len() as u64;
            let mut out: Vec<Result<RunQueryResponse, Status>> = Vec::with_capacity(docs.len().max(1));
            let total = docs.len();
            for (i, d) in docs.into_iter().enumerate() {
                out.push(Ok(RunQueryResponse {
                    document: Some(d),
                    read_time: Some(read_time),
                    transaction: if i == 0 { txn.new_id() } else { vec![] },
                    continuation_selector: (i + 1 == total).then_some(run_query_response::ContinuationSelector::Done(true)),
                    ..Default::default()
                }));
            }
            if out.is_empty() {
                out.push(Ok(RunQueryResponse {
                    read_time: Some(read_time),
                    transaction: txn.new_id(),
                    continuation_selector: Some(run_query_response::ContinuationSelector::Done(true)),
                    ..Default::default()
                }));
            }
            Ok((Response::new(stream(out)), n))
        })
        .await
    }

    type ExecutePipelineStream = BoxStream<ExecutePipelineResponse>;

    async fn execute_pipeline(&self, _req: Request<ExecutePipelineRequest>) -> Result<Response<Self::ExecutePipelineStream>, Status> {
        Err(Status::unimplemented("Pipelines are not supported by the TwinBase Firestore mock"))
    }

    type RunAggregationQueryStream = BoxStream<RunAggregationQueryResponse>;

    async fn run_aggregation_query(
        &self,
        req: Request<RunAggregationQueryRequest>,
    ) -> Result<Response<Self::RunAggregationQueryStream>, Status> {
        let r = req.get_ref().clone();
        let Some(run_aggregation_query_request::QueryType::StructuredAggregationQuery(aq)) = r.query_type.clone() else {
            return Err(Status::invalid_argument("structured_aggregation_query is required"));
        };
        let Some(structured_aggregation_query::QueryType::StructuredQuery(q)) = aq.query_type.clone() else {
            return Err(Status::invalid_argument("the aggregation needs a structured query"));
        };
        let text = format!(
            "RunAggregationQuery {} on {}",
            aq.aggregations.iter().map(|a| a.alias.clone()).collect::<Vec<_>>().join(", "),
            query_text(&r.parent, &q)
        );
        self.guard(&req, text, || {
            use structured_aggregation_query::aggregation::Operator;
            let (db, parent) = database_of(&r.parent)?;
            let txn = match r.consistency_selector.clone() {
                Some(run_aggregation_query_request::ConsistencySelector::ReadTime(_)) => return Err(read_time_unsupported()),
                Some(run_aggregation_query_request::ConsistencySelector::Transaction(t)) => ReadTxn::Existing(t),
                Some(run_aggregation_query_request::ConsistencySelector::NewTransaction(o)) => {
                    read_txn(&self.store, &r.parent, None, Some(o))
                }
                _ => ReadTxn::None,
            };
            let docs = self.store.run_query(&db, &parent, &q, txn.id())?;
            let mut fields = HashMap::new();
            for (i, a) in aq.aggregations.iter().enumerate() {
                let alias = if a.alias.is_empty() { format!("field_{}", i + 1) } else { a.alias.clone() };
                let value = match &a.operator {
                    Some(Operator::Count(c)) => {
                        let n = docs.len() as i64;
                        Value { value_type: Some(value::ValueType::IntegerValue(c.up_to.map(|u| n.min(u)).unwrap_or(n))) }
                    }
                    Some(Operator::Sum(s)) => {
                        let path = query::split_field_path(&s.field.as_ref().map(|f| f.field_path.clone()).unwrap_or_default())?;
                        let vals: Vec<&Value> = docs.iter().filter_map(|d| query::get_field(&d.fields, &path)).collect();
                        query::sum(&vals)
                    }
                    Some(Operator::Avg(s)) => {
                        let path = query::split_field_path(&s.field.as_ref().map(|f| f.field_path.clone()).unwrap_or_default())?;
                        let vals: Vec<&Value> = docs.iter().filter_map(|d| query::get_field(&d.fields, &path)).collect();
                        query::avg(&vals)
                    }
                    None => return Err(Status::invalid_argument("aggregation operator is required")),
                };
                fields.insert(alias, value);
            }
            let resp = RunAggregationQueryResponse {
                result: Some(AggregationResult { aggregate_fields: fields }),
                transaction: txn.new_id(),
                read_time: Some(self.store.read_time()),
                explain_metrics: None,
            };
            Ok((Response::new(stream(vec![Ok(resp)])), 1))
        })
        .await
    }

    async fn partition_query(&self, req: Request<PartitionQueryRequest>) -> Result<Response<PartitionQueryResponse>, Status> {
        self.guard(&req, "PartitionQuery".into(), || {
            Ok((Response::new(PartitionQueryResponse { partitions: vec![], next_page_token: String::new() }), 0))
        })
        .await
    }

    type WriteStream = BoxStream<WriteResponse>;

    async fn write(&self, _req: Request<Streaming<WriteRequest>>) -> Result<Response<Self::WriteStream>, Status> {
        Err(Status::unimplemented("The streaming Write RPC is not supported; current SDKs use Commit"))
    }

    type ListenStream = BoxStream<ListenResponse>;

    async fn listen(&self, req: Request<Streaming<ListenRequest>>) -> Result<Response<Self::ListenStream>, Status> {
        let conn = self.connection(&req);
        self.check_auth(&req)?;
        let mut inbound = req.into_inner();
        let (tx, rx) = mpsc::channel::<Result<ListenResponse, Status>>(256);
        let store = self.store.clone();
        let traffic = self.traffic.clone();
        let mut changes = store.subscribe();
        tokio::spawn(async move {
            let mut targets: BTreeMap<i32, ListenTarget> = BTreeMap::new();
            loop {
                tokio::select! {
                    msg = inbound.message() => {
                        let Ok(Some(m)) = msg else { break };
                        let responses = match m.target_change {
                            Some(listen_request::TargetChange::AddTarget(t)) => {
                                let started = Instant::now();
                                let text = format!("Listen {}", target_text(&t));
                                let r = add_target(&store, &m.database, t, &mut targets);
                                traffic.query(conn, &text, started, r.as_ref().map(|v| v.len().saturating_sub(3) as u64).unwrap_or(0), r.as_ref().err().map(|e| e.message().to_owned()));
                                r
                            }
                            Some(listen_request::TargetChange::RemoveTarget(id)) => {
                                targets.remove(&id);
                                Ok(vec![target_change(target_change::TargetChangeType::Remove, vec![id], None)])
                            }
                            None => Ok(vec![]),
                        };
                        match responses {
                            Ok(rs) => {
                                for r in rs {
                                    if tx.send(Ok(r)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        }
                    }
                    change = changes.recv() => {
                        let change = match change {
                            Ok(c) => c,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => store::Change { database: String::new(), paths: vec![], time: store.read_time() },
                            Err(_) => break,
                        };
                        let responses = refresh_targets(&store, &change, &mut targets);
                        for r in responses {
                            if tx.send(Ok(r)).await.is_err() {
                                return;
                            }
                        }
                    }
                    _ = tx.closed() => break,
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn list_collection_ids(&self, req: Request<ListCollectionIdsRequest>) -> Result<Response<ListCollectionIdsResponse>, Status> {
        let r = req.get_ref().clone();
        let text = format!("ListCollectionIds {}", if rel(&r.parent).is_empty() { "(root)".to_owned() } else { rel(&r.parent) });
        self.guard(&req, text, || {
            let (db, parent) = database_of(&r.parent)?;
            let mut ids = self.store.collection_ids(&db, &parent);
            if !r.page_token.is_empty() {
                ids.retain(|i| i.as_str() > r.page_token.as_str());
            }
            let size = if r.page_size > 0 { r.page_size as usize } else { usize::MAX };
            let next = if ids.len() > size { ids[size - 1].clone() } else { String::new() };
            ids.truncate(size);
            let n = ids.len() as u64;
            Ok((Response::new(ListCollectionIdsResponse { collection_ids: ids, next_page_token: next }), n))
        })
        .await
    }

    async fn batch_write(&self, req: Request<BatchWriteRequest>) -> Result<Response<BatchWriteResponse>, Status> {
        let r = req.get_ref().clone();
        let text = format!("BatchWrite {}", r.writes.iter().map(write_text).collect::<Vec<_>>().join("; "));
        self.guard(&req, text, || {
            // Each write applies on its own (not atomically), as in Firestore.
            let mut results = Vec::new();
            let mut statuses = Vec::new();
            for w in &r.writes {
                match self.store.commit(&r.database, std::slice::from_ref(w), None) {
                    Ok((_, mut wr)) => {
                        results.push(wr.pop().unwrap_or_default());
                        statuses.push(twinbase_firestore_proto::google::rpc::Status::default());
                    }
                    Err(e) => {
                        results.push(WriteResult::default());
                        statuses.push(twinbase_firestore_proto::google::rpc::Status {
                            code: e.code() as i32,
                            message: e.message().to_owned(),
                            details: vec![],
                        });
                    }
                }
            }
            let n = results.len() as u64;
            Ok((Response::new(BatchWriteResponse { write_results: results, status: statuses }), n))
        })
        .await
    }

    async fn create_document(&self, req: Request<CreateDocumentRequest>) -> Result<Response<Document>, Status> {
        let r = req.get_ref().clone();
        let id = if r.document_id.is_empty() { store::auto_id() } else { r.document_id.clone() };
        let parent_rel = rel(&r.parent);
        let path =
            if parent_rel.is_empty() { format!("{}/{id}", r.collection_id) } else { format!("{parent_rel}/{}/{id}", r.collection_id) };
        let text = format!("CreateDocument {path}");
        self.guard(&req, text, || {
            let (db, _) = database_of(&r.parent)?;
            check_doc_path(&path)?;
            let mut doc = r.document.clone().unwrap_or_default();
            doc.name = doc_name(&db, &path);
            let w = Write {
                operation: Some(write::Operation::Update(doc)),
                current_document: Some(Precondition { condition_type: Some(precondition::ConditionType::Exists(false)) }),
                ..Default::default()
            };
            self.store.commit(&db, &[w], None)?;
            let d = self.store.get(&db, &path, None)?.ok_or_else(|| Status::internal("written document vanished"))?;
            Ok((Response::new(mask_doc(d, &r.mask)?), 1))
        })
        .await
    }
}

// ------------------------------------------------------------------ listeners

struct ListenTarget {
    database: String,
    spec: target::TargetType,
    /// Current documents and their update times.
    docs: BTreeMap<String, Timestamp>,
}

fn target_text(t: &Target) -> String {
    match &t.target_type {
        Some(target::TargetType::Documents(d)) => d.documents.iter().map(|n| rel(n)).collect::<Vec<_>>().join(", "),
        Some(target::TargetType::Query(q)) => match &q.query_type {
            Some(target::query_target::QueryType::StructuredQuery(sq)) => query_text(&q.parent, sq),
            None => "?".into(),
        },
        None => "?".into(),
    }
}

fn target_change(kind: target_change::TargetChangeType, ids: Vec<i32>, read_time: Option<Timestamp>) -> ListenResponse {
    ListenResponse {
        response_type: Some(listen_response::ResponseType::TargetChange(TargetChange {
            target_change_type: kind as i32,
            target_ids: ids,
            cause: None,
            resume_token: read_time.as_ref().map(resume_token).unwrap_or_default(),
            read_time,
        })),
    }
}

fn evaluate(store: &FirestoreStore, database: &str, spec: &target::TargetType) -> Result<Vec<Document>, Status> {
    match spec {
        target::TargetType::Documents(d) => {
            let mut out = Vec::new();
            for n in &d.documents {
                let (db, path) = split_name(n)?;
                if let Some(doc) = store.get(&db, &path, None)? {
                    out.push(doc);
                }
            }
            Ok(out)
        }
        target::TargetType::Query(q) => {
            let Some(target::query_target::QueryType::StructuredQuery(sq)) = &q.query_type else {
                return Err(Status::invalid_argument("query target needs a structured query"));
            };
            let (db, parent) = split_name(&q.parent)?;
            let _ = database;
            store.run_query(&db, &parent, sq, None)
        }
    }
}

fn add_target(
    store: &FirestoreStore,
    database: &str,
    t: Target,
    targets: &mut BTreeMap<i32, ListenTarget>,
) -> Result<Vec<ListenResponse>, Status> {
    let spec = t.target_type.ok_or_else(|| Status::invalid_argument("target_type is required"))?;
    let id = t.target_id;
    let read_time = store.read_time();
    let docs = evaluate(store, database, &spec)?;
    let mut out = vec![target_change(target_change::TargetChangeType::Add, vec![id], None)];
    let mut current = BTreeMap::new();
    for d in docs {
        current.insert(d.name.clone(), d.update_time.unwrap_or_default());
        out.push(ListenResponse {
            response_type: Some(listen_response::ResponseType::DocumentChange(DocumentChange {
                document: Some(d),
                target_ids: vec![id],
                removed_target_ids: vec![],
            })),
        });
    }
    out.push(target_change(target_change::TargetChangeType::Current, vec![id], Some(read_time)));
    out.push(target_change(target_change::TargetChangeType::NoChange, vec![], Some(read_time)));
    if t.once {
        out.push(target_change(target_change::TargetChangeType::Remove, vec![id], None));
    } else {
        targets.insert(id, ListenTarget { database: store::db_key(database), spec, docs: current });
    }
    Ok(out)
}

/// After a commit: send what changed for each affected target, then a consistent-snapshot marker.
fn refresh_targets(store: &FirestoreStore, change: &store::Change, targets: &mut BTreeMap<i32, ListenTarget>) -> Vec<ListenResponse> {
    let mut out = Vec::new();
    for (id, t) in targets.iter_mut() {
        if !change.database.is_empty() && t.database != change.database {
            continue;
        }
        let Ok(docs) = evaluate(store, &t.database, &t.spec) else { continue };
        let mut next = BTreeMap::new();
        for d in docs {
            let ut = d.update_time.unwrap_or_default();
            let changed = t.docs.get(&d.name).is_none_or(|old| query::ts_cmp(old, &ut).is_ne());
            next.insert(d.name.clone(), ut);
            if changed {
                out.push(ListenResponse {
                    response_type: Some(listen_response::ResponseType::DocumentChange(DocumentChange {
                        document: Some(d),
                        target_ids: vec![*id],
                        removed_target_ids: vec![],
                    })),
                });
            }
        }
        for gone in t.docs.keys().filter(|k| !next.contains_key(*k)) {
            let exists = split_name(gone).ok().and_then(|(db, p)| store.get(&db, &p, None).ok().flatten()).is_some();
            out.push(ListenResponse {
                response_type: Some(if exists {
                    listen_response::ResponseType::DocumentRemove(DocumentRemove {
                        document: gone.clone(),
                        removed_target_ids: vec![*id],
                        read_time: Some(change.time),
                    })
                } else {
                    listen_response::ResponseType::DocumentDelete(DocumentDelete {
                        document: gone.clone(),
                        removed_target_ids: vec![*id],
                        read_time: Some(change.time),
                    })
                }),
            });
        }
        t.docs = next;
    }
    if !out.is_empty() {
        out.push(target_change(target_change::TargetChangeType::NoChange, vec![], Some(change.time)));
    }
    out
}

// ------------------------------------------------------------------ server

/// Serve Firestore gRPC until `cancel` fires; snapshots every 2 s when changed and on shutdown.
pub async fn serve(
    listener: TcpListener,
    store: Arc<FirestoreStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cancel: CancellationToken,
) {
    let peers = Arc::new(Mutex::new(HashMap::new()));
    let svc = Service { store: store.clone(), auth, traffic: traffic.clone(), faults: faults.clone(), peers: peers.clone() };
    let saver = {
        let store = store.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let st = store.clone();
                        let _ = tokio::task::spawn_blocking(move || st.save_if_dirty()).await;
                    }
                }
            }
        })
    };
    let incoming = futures::stream::unfold((listener, traffic, faults, peers), |(listener, traffic, faults, peers)| async move {
        loop {
            match listener.accept().await {
                Ok((socket, peer)) => {
                    if faults.refuse_connections() {
                        traffic.fault(traffic.new_connection_id(), "(new connection)", "connection refused");
                        drop(socket);
                        continue;
                    }
                    let _ = socket.set_nodelay(true);
                    let id = traffic.new_connection_id();
                    traffic.record(id, TrafficKind::Connect { peer: peer.to_string(), user: None, database: None });
                    let mut p = peers.lock();
                    if p.len() > 10_000 {
                        p.clear();
                    }
                    p.insert(peer, id);
                    drop(p);
                    return Some((Ok::<_, std::io::Error>(socket), (listener, traffic, faults, peers)));
                }
                Err(_) => continue,
            }
        }
    });
    let shutdown = cancel.clone();
    let _ = tonic::transport::Server::builder()
        .add_service(FirestoreServer::new(svc).max_decoding_message_size(16 * 1024 * 1024).max_encoding_message_size(64 * 1024 * 1024))
        .serve_with_incoming_shutdown(incoming, async move { shutdown.cancelled().await })
        .await;
    let _ = saver.await;
    let _ = tokio::task::spawn_blocking(move || store.save_if_dirty()).await;
}

/// Apply a seed (`{"documents": {"users/alice": {...}}}`).
pub fn apply_seed(store: &FirestoreStore, seed: &str) -> Result<usize, String> {
    let v: serde_json::Value = serde_json::from_str(seed).map_err(|e| format!("seed is not valid JSON: {e}"))?;
    store.seed(&v)
}

/// The default database as a seed (pretty JSON).
pub fn export_seed(store: &FirestoreStore) -> String {
    serde_json::to_string_pretty(&store.export()).unwrap_or_default()
}

/// Point-in-time reads would silently return current data; refuse them instead.
fn read_time_unsupported() -> Status {
    Status::unimplemented("point-in-time reads (read_time) are not supported by the TwinBase Firestore mock")
}
