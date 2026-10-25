//! Amazon DynamoDB mock: the JSON 1.0 HTTP protocol (`X-Amz-Target: DynamoDB_20120810.*`) with
//! SigV4 request signing, over an in-memory table store with JSON snapshots.

pub mod expr;
pub mod store;
pub mod value;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use store::DynamoStore;

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};

const MAX_BODY: usize = 16 * 1024 * 1024;

/// An error the service reports as `{"__type": "...#Kind", "message": "..."}`.
#[derive(Debug, Clone)]
pub struct DynError {
    pub kind: Cow<'static, str>,
    pub message: String,
    pub status: u16,
    /// Extra fields (CancellationReasons, Item).
    pub extra: Option<Box<Value>>,
}

impl DynError {
    pub fn new(kind: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        let kind = kind.into();
        let status = match kind.as_ref() {
            "InternalServerError" => 500,
            "ServiceUnavailable" => 503,
            _ => 400,
        };
        DynError { kind, message: message.into(), status, extra: None }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("ValidationException", message)
    }

    fn body(&self) -> Value {
        let prefix = match self.kind.as_ref() {
            "ValidationException" => "com.amazon.coral.validate",
            "UnknownOperationException" | "SerializationException" => "com.amazon.coral.service",
            "MissingAuthenticationTokenException" | "UnrecognizedClientException" | "InvalidSignatureException" => {
                "com.amazon.coral.service"
            }
            _ => "com.amazonaws.dynamodb.v20120810",
        };
        let mut v = json!({ "__type": format!("{prefix}#{}", self.kind), "message": self.message });
        if let Some(Value::Object(x)) = self.extra.as_deref() {
            for (k, val) in x {
                v[k] = val.clone();
            }
        }
        v
    }
}

/// Snapshot file used for a DynamoDB mock data directory.
pub fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join("dynamodb.json")
}

struct Server {
    store: Arc<DynamoStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
}

/// Serve DynamoDB clients until `cancel` fires. Snapshots are written every 2 s when data
/// changed and once more on shutdown; expired TTL items are removed every 2 s.
pub async fn serve(
    listener: TcpListener,
    store: Arc<DynamoStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cancel: CancellationToken,
) {
    let server = Arc::new(Server { store: store.clone(), auth, traffic, faults });
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                let st = store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    st.sweep_expired();
                    st.save_if_dirty()
                }).await;
            }
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { continue };
                if server.faults.refuse_connections() {
                    server.traffic.fault(server.traffic.new_connection_id(), "(new connection)", "connection refused");
                    drop(socket);
                    continue;
                }
                let _ = socket.set_nodelay(true);
                let server = server.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let id = server.traffic.new_connection_id();
                    server.traffic.record(id, TrafficKind::Connect { peer: peer.to_string(), user: None, database: None });
                    let traffic = server.traffic.clone();
                    let svc = hyper::service::service_fn(move |req| {
                        let server = server.clone();
                        async move { handle(server, id, req).await }
                    });
                    let conn = hyper::server::conn::http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(socket), svc);
                    tokio::select! {
                        _ = conn => {}
                        _ = cancel.cancelled() => {}
                    }
                    traffic.record(id, TrafficKind::Disconnect);
                });
            }
        }
    }
    let _ = tokio::task::spawn_blocking(move || store.save_if_dirty()).await;
}

/// Returned by the service to make hyper close the connection without a response.
#[derive(Debug)]
struct Dropped;
impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection dropped by fault injection")
    }
}
impl std::error::Error for Dropped {}

fn respond(status: u16, body: &Value) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let crc = crc32fast::hash(&bytes);
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST))
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amzn-requestid", uuid::Uuid::new_v4().simple().to_string().to_uppercase())
        .header("x-amz-crc32", crc.to_string())
        .body(Full::new(Bytes::from(bytes)))
        .unwrap()
}

async fn handle(server: Arc<Server>, conn: u64, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Dropped> {
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return Ok(respond(413, &DynError::validation("Request body is too large").body())),
    };
    if parts.method != hyper::Method::POST {
        // DynamoDB answers health checks with a plain 200.
        return Ok(Response::builder().status(200).body(Full::new(Bytes::from_static(b"healthy: dynamodb.twinbase.local"))).unwrap());
    }
    let target = parts.headers.get("x-amz-target").and_then(|v| v.to_str().ok()).unwrap_or("");
    let Some(op) = target.strip_prefix("DynamoDB_20120810.") else {
        return Ok(respond(400, &DynError::new("UnknownOperationException", "").body()));
    };
    let op = op.to_owned();
    let region = match authenticate(&server.auth, &parts, &body) {
        Ok(r) => r,
        Err(e) => {
            server.traffic.query(conn, &format!("{op} (authentication)"), Instant::now(), 0, Some(e.message.clone()));
            return Ok(respond(e.status, &e.body()));
        }
    };
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            return Ok(respond(
                400,
                &DynError::new("SerializationException", "Start of structure or map found where not expected.").body(),
            ));
        }
    };
    let text = describe(&op, &request);
    match server.faults.before(&text).await {
        Decision::Proceed => {}
        Decision::Fail { code, message } => {
            server.traffic.fault(conn, &text, &message);
            let e = DynError::new(code.unwrap_or_else(|| "InternalServerError".into()), message);
            return Ok(respond(e.status, &e.body()));
        }
        Decision::Drop => {
            server.traffic.fault(conn, &text, "connection dropped");
            return Err(Dropped);
        }
    }
    let started = Instant::now();
    let store = server.store.clone();
    let req2 = request.clone();
    let op2 = op.clone();
    let result = tokio::task::spawn_blocking(move || store.handle(&op2, &req2, &region))
        .await
        .unwrap_or_else(|_| Err(DynError::new("InternalServerError", "the request panicked")));
    match result {
        Ok(v) => {
            let rows = v.get("Count").and_then(Value::as_u64).or_else(|| v.get("Item").map(|_| 1)).unwrap_or(0);
            server.traffic.query(conn, &text, started, rows, None);
            Ok(respond(200, &v))
        }
        Err(e) => {
            server.traffic.query(conn, &text, started, 0, Some(format!("{}: {}", e.kind, e.message)));
            Ok(respond(e.status, &e.body()))
        }
    }
}

/// Short, readable text of a request for the traffic log and fault matching:
/// `Query orders pk = :p {":p":"c#1"}`.
fn describe(op: &str, req: &Value) -> String {
    let mut s = op.to_owned();
    if let Some(t) = req.get("TableName").and_then(Value::as_str) {
        s.push(' ');
        s.push_str(t);
    }
    if let Some(i) = req.get("IndexName").and_then(Value::as_str) {
        s.push_str(&format!(" (index {i})"));
    }
    let plain = |v: &Value| value::item_from_json(v).map(|i| value::plain_item(&i).to_string()).unwrap_or_else(|_| v.to_string());
    for k in ["Key", "Item"] {
        if let Some(v) = req.get(k) {
            s.push(' ');
            s.push_str(&plain(v));
        }
    }
    for k in ["KeyConditionExpression", "UpdateExpression", "ConditionExpression", "FilterExpression"] {
        if let Some(e) = req.get(k).and_then(Value::as_str) {
            s.push_str(&format!(" {e}"));
        }
    }
    if let Some(v) = req.get("ExpressionAttributeValues") {
        s.push(' ');
        s.push_str(&plain(v));
    }
    if let Some(r) = req.get("RequestItems").and_then(Value::as_object) {
        s.push_str(&format!(" {}", r.keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    if let Some(t) = req.get("TransactItems").and_then(Value::as_array) {
        s.push_str(&format!(" ({} items)", t.len()));
    }
    s
}

// ------------------------------------------------------------------ SigV4

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Check the request signature (when the mock requires credentials) and return the region.
fn authenticate(auth: &AuthMode, parts: &hyper::http::request::Parts, body: &[u8]) -> Result<String, DynError> {
    let header = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let field = |name: &str| header.split([',', ' ']).find_map(|p| p.trim().strip_prefix(name)).map(str::to_owned);
    let credential = field("Credential=");
    // Credential=AKID/20240101/us-east-1/dynamodb/aws4_request
    let scope: Vec<String> = credential.as_deref().unwrap_or("").split('/').map(str::to_owned).collect();
    let region = scope.get(2).cloned().unwrap_or_else(|| "us-east-1".into());
    let AuthMode::Password { user, password } = auth else { return Ok(region) };
    if !header.starts_with("AWS4-HMAC-SHA256") || scope.len() != 5 {
        return Err(DynError::new("MissingAuthenticationTokenException", "Request is missing Authentication Token"));
    }
    if &scope[0] != user {
        return Err(DynError::new("UnrecognizedClientException", "The security token included in the request is invalid."));
    }
    let signed: Vec<String> = field("SignedHeaders=").unwrap_or_default().split(';').map(str::to_owned).collect();
    let signature = field("Signature=").unwrap_or_default();
    let amz_date = parts.headers.get("x-amz-date").and_then(|v| v.to_str().ok()).unwrap_or("");
    let payload_hash = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| hex(&Sha256::digest(body)));
    let mut query: Vec<(String, String)> = parts
        .uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_owned(), v.to_owned())
        })
        .collect();
    query.sort();
    let canonical_headers: String = signed
        .iter()
        .map(|h| {
            let v = parts
                .headers
                .get_all(h.as_str())
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(",");
            let v = if h == "host" && v.is_empty() { parts.uri.authority().map(|a| a.to_string()).unwrap_or_default() } else { v };
            format!("{h}:{v}\n")
        })
        .collect();
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        parts.method,
        uri_encode(parts.uri.path(), false),
        query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
        canonical_headers,
        signed.join(";"),
        payload_hash
    );
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{}\n{}", scope[1..].join("/"), hex(&Sha256::digest(canonical.as_bytes())));
    let k_date = hmac(format!("AWS4{password}").as_bytes(), scope[1].as_bytes());
    let k_region = hmac(&k_date, scope[2].as_bytes());
    let k_service = hmac(&k_region, scope[3].as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let expected = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    if expected.len() != signature.len() || expected.bytes().zip(signature.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return Err(DynError::new(
            "InvalidSignatureException",
            "The request signature we calculated does not match the signature you provided. Check your AWS Secret Access Key and signing method.",
        ));
    }
    Ok(region)
}

/// Apply a seed (`{"tables": [...]}`) to the store.
pub fn apply_seed(store: &DynamoStore, seed: &str) -> Result<usize, String> {
    let v: Value = serde_json::from_str(seed).map_err(|e| format!("seed is not valid JSON: {e}"))?;
    store.load_seed(&v).map_err(|e| e.message)
}

/// All tables as a seed (pretty JSON).
pub fn export_seed(store: &DynamoStore) -> String {
    serde_json::to_string_pretty(&store.export()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from the AWS Signature Version 4 documentation (IAM ListUsers).
    #[test]
    fn sigv4_matches_the_aws_documentation_example() {
        let req = hyper::Request::builder()
            .method("GET")
            .uri("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08")
            .header("content-type", "application/x-www-form-urlencoded; charset=utf-8")
            .header("host", "iam.amazonaws.com")
            .header("x-amz-date", "20150830T123600Z")
            .header(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
            )
            .body(())
            .unwrap();
        let (parts, _) = req.into_parts();
        let auth = AuthMode::Password { user: "AKIDEXAMPLE".into(), password: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into() };
        assert_eq!(authenticate(&auth, &parts, b"").unwrap(), "us-east-1");
        let wrong = AuthMode::Password { user: "AKIDEXAMPLE".into(), password: "other".into() };
        assert_eq!(authenticate(&wrong, &parts, b"").unwrap_err().kind, "InvalidSignatureException");
        let other_user = AuthMode::Password { user: "SOMEONE".into(), password: "x".into() };
        assert_eq!(authenticate(&other_user, &parts, b"").unwrap_err().kind, "UnrecognizedClientException");
        assert_eq!(authenticate(&AuthMode::Any, &parts, b"").unwrap(), "us-east-1");
    }
}
