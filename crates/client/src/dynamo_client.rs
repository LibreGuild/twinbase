//! Amazon DynamoDB driver: the JSON 1.0 HTTP API signed with AWS Signature Version 4, over a
//! kept-alive HTTP/1.1 connection (TLS for AWS endpoints).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::HttpConn;
use crate::{ClientError, ConnectionConfig, Result, TlsMode};

#[derive(Clone)]
struct Credentials {
    access_key: String,
    secret: String,
    session_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DynamoEndpoint {
    pub url: String,
    pub region: String,
}

pub struct DynamoClient {
    http: HttpConn,
    region: String,
    creds: Credentials,
    tunnel: Option<Arc<crate::ssh::Tunnel>>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// `aws_access_key_id` etc. from `~/.aws/credentials` (profile from `AWS_PROFILE`, else `default`).
fn profile_credentials() -> Option<Credentials> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let path = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(home).join(".aws").join("credentials"));
    let text = std::fs::read_to_string(path).ok()?;
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
    let mut in_profile = false;
    let (mut ak, mut sk, mut tok) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_profile = line.trim_matches(['[', ']']).trim() == profile;
            continue;
        }
        if !in_profile {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = Some(v.trim().to_owned());
            match k.trim() {
                "aws_access_key_id" => ak = v,
                "aws_secret_access_key" => sk = v,
                "aws_session_token" => tok = v,
                _ => {}
            }
        }
    }
    Some(Credentials { access_key: ak?, secret: sk?, session_token: tok })
}

fn credentials(config: &ConnectionConfig) -> Result<Credentials> {
    if !config.user.is_empty() {
        return Ok(Credentials {
            access_key: config.user.clone(),
            secret: config.password.clone().unwrap_or_default(),
            session_token: None,
        });
    }
    if let (Ok(ak), Ok(sk)) = (std::env::var("AWS_ACCESS_KEY_ID"), std::env::var("AWS_SECRET_ACCESS_KEY")) {
        return Ok(Credentials { access_key: ak, secret: sk, session_token: std::env::var("AWS_SESSION_TOKEN").ok() });
    }
    profile_credentials().ok_or_else(|| ClientError::new("no AWS credentials: enter an access key ID and secret, or set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY, or add them to ~/.aws/credentials"))
}

/// Endpoint from the connection fields: a URL in `host`, `host` + `port`, or AWS for the region.
pub fn endpoint(config: &ConnectionConfig) -> (bool, String, u16, String) {
    let region = config.database.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| "us-east-1".into());
    let host = config.host.trim();
    if host.is_empty() {
        return (true, format!("dynamodb.{region}.amazonaws.com"), 443, region);
    }
    if let Some((scheme, rest)) = host.split_once("://") {
        let https = scheme.eq_ignore_ascii_case("https");
        let authority = rest.split('/').next().unwrap_or(rest);
        let (h, p) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_owned(), p.parse().unwrap_or(if https { 443 } else { 80 })),
            _ => (authority.to_owned(), if https { 443 } else { 80 }),
        };
        return (https, h, p, region);
    }
    let https = config.tls != TlsMode::Disable;
    let port = if config.port == 0 { if https { 443 } else { 8000 } } else { config.port };
    (https, host.to_owned(), port, region)
}

fn error_from(status: u16, body: &[u8]) -> ClientError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let kind = v.get("__type").and_then(Value::as_str).map(|t| t.rsplit('#').next().unwrap_or(t).to_owned());
    let message = v.get("message").or_else(|| v.get("Message")).and_then(Value::as_str).unwrap_or("").to_owned();
    let mut e = ClientError::new(match (&kind, message.is_empty()) {
        (Some(k), false) => format!("{k}: {message}"),
        (Some(k), true) => k.clone(),
        (None, _) => format!("HTTP {status}: {}", String::from_utf8_lossy(body).chars().take(300).collect::<String>()),
    });
    e.code = kind;
    if let Some(r) = v.get("CancellationReasons") {
        e.detail = Some(r.to_string());
    }
    e
}

/// Operations that only read (for read-only connections and agent access).
pub fn is_read_operation(op: &str) -> bool {
    matches!(
        op,
        "ListTables"
            | "DescribeTable"
            | "GetItem"
            | "BatchGetItem"
            | "Query"
            | "Scan"
            | "TransactGetItems"
            | "DescribeTimeToLive"
            | "ListTagsOfResource"
            | "DescribeLimits"
            | "DescribeEndpoints"
            | "DescribeContinuousBackups"
            | "ListBackups"
            | "ListGlobalTables"
            | "DescribeGlobalTable"
    )
}

/// A console script: blocks that start with an operation name followed by its JSON request.
pub fn parse_script(src: &str) -> Result<Vec<(String, Value)>> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        let word: String = t.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if word.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && word.len() >= 4 && !t[word.len()..].trim_start().starts_with(':')
        {
            blocks.push((word.clone(), t[word.len()..].to_owned()));
        } else if let Some(last) = blocks.last_mut() {
            last.1.push('\n');
            last.1.push_str(line);
        } else if !t.is_empty() {
            return Err(ClientError::new("start each request with an operation name, e.g. Query { \"TableName\": \"orders\", … }"));
        }
    }
    blocks
        .into_iter()
        .map(|(op, body)| {
            let body = body.trim();
            let v = if body.is_empty() {
                json!({})
            } else {
                serde_json::from_str(body).map_err(|e| ClientError::new(format!("{op}: invalid JSON: {e}")))?
            };
            if !v.is_object() {
                return Err(ClientError::new(format!("{op}: the request must be a JSON object")));
            }
            Ok((op, v))
        })
        .collect()
}

impl DynamoClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<DynamoClient> {
        let (https, host, port, region) = endpoint(config);
        let creds = credentials(config)?;
        let http = HttpConn::new(https, host, port, config.via, config.tls, Duration::from_secs(config.timeout_secs.max(1)));
        let client = DynamoClient { http, region, creds, tunnel: None };
        client.request("ListTables", &json!({ "Limit": 1 })).await?;
        Ok(client)
    }

    pub fn with_tunnel(mut self, t: Arc<crate::ssh::Tunnel>) -> Self {
        self.tunnel = Some(t);
        self
    }

    pub fn endpoint(&self) -> DynamoEndpoint {
        DynamoEndpoint { url: self.http.base_url(), region: self.region.clone() }
    }

    fn sign(&self, target: &str, body: &[u8], amz_date: &str) -> Vec<(String, String)> {
        let date = &amz_date[..8];
        let mut headers = vec![
            ("content-type".to_owned(), "application/x-amz-json-1.0".to_owned()),
            ("host".to_owned(), self.http.host_header()),
            ("x-amz-date".to_owned(), amz_date.to_owned()),
            ("x-amz-target".to_owned(), target.to_owned()),
        ];
        if let Some(t) = &self.creds.session_token {
            headers.push(("x-amz-security-token".to_owned(), t.clone()));
        }
        headers.sort();
        let signed: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
        let canonical = format!("POST\n/\n\n{canonical_headers}\n{signed}\n{}", hex(&Sha256::digest(body)));
        let scope = format!("{date}/{}/dynamodb/aws4_request", self.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
        let k = hmac(format!("AWS4{}", self.creds.secret).as_bytes(), date.as_bytes());
        let k = hmac(&k, self.region.as_bytes());
        let k = hmac(&k, b"dynamodb");
        let k = hmac(&k, b"aws4_request");
        let signature = hex(&hmac(&k, to_sign.as_bytes()));
        headers.push((
            "authorization".to_owned(),
            format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", self.creds.access_key),
        ));
        headers
    }

    /// Call one API operation with its JSON request.
    pub async fn request(&self, op: &str, body: &Value) -> Result<Value> {
        if !op.chars().all(|c| c.is_ascii_alphanumeric()) || op.is_empty() {
            return Err(ClientError::new(format!("invalid operation name: {op}")));
        }
        let bytes = Bytes::from(serde_json::to_vec(body).map_err(|e| ClientError::new(e.to_string()))?);
        let target = format!("DynamoDB_20120810.{op}");
        let res = self
            .http
            .send(op, self.http.timeout.max(Duration::from_secs(30)), || {
                // Signed per attempt: the signature covers the current time.
                let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let mut req = hyper::Request::builder().method("POST").uri("/");
                for (k, v) in self.sign(&target, &bytes, &amz_date) {
                    req = req.header(k, v);
                }
                req.body(Full::new(bytes.clone())).map_err(|e| ClientError::new(e.to_string()))
            })
            .await?;
        if res.status != 200 {
            return Err(error_from(res.status, &res.body));
        }
        serde_json::from_slice(&res.body).map_err(|e| ClientError::new(format!("invalid response: {e}")))
    }

    pub async fn list_tables(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut req = json!({ "Limit": 100 });
            if let Some(s) = &start {
                req["ExclusiveStartTableName"] = json!(s);
            }
            let r = self.request("ListTables", &req).await?;
            out.extend(r["TableNames"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| v.as_str().map(str::to_owned)));
            match r.get("LastEvaluatedTableName").and_then(Value::as_str) {
                Some(s) if out.len() < 10_000 => start = Some(s.to_owned()),
                _ => break,
            }
        }
        Ok(out)
    }

    pub async fn describe_table(&self, table: &str) -> Result<Value> {
        Ok(self.request("DescribeTable", &json!({ "TableName": table })).await?["Table"].take())
    }
}

/// Plain JSON for a DynamoDB-typed value (sets become arrays, numbers JSON numbers when exact).
pub fn unmarshal(v: &Value) -> Value {
    let Some(o) = v.as_object().filter(|o| o.len() == 1) else { return v.clone() };
    let (t, x) = o.iter().next().unwrap();
    match t.as_str() {
        "S" | "B" => x.clone(),
        "N" => x
            .as_str()
            .and_then(|n| serde_json::from_str::<Value>(n).ok())
            .filter(|n| n.as_f64().is_some_and(|f| f.abs() < 9.0e15) || n.is_i64() || n.is_u64())
            .unwrap_or_else(|| x.clone()),
        "NS" => Value::Array(x.as_array().cloned().unwrap_or_default().iter().map(|n| unmarshal(&json!({ "N": n }))).collect()),
        "SS" | "BS" => x.clone(),
        "BOOL" => x.clone(),
        "NULL" => Value::Null,
        "L" => Value::Array(x.as_array().cloned().unwrap_or_default().iter().map(unmarshal).collect()),
        "M" => unmarshal_item(x),
        _ => v.clone(),
    }
}

pub fn unmarshal_item(item: &Value) -> Value {
    match item.as_object() {
        Some(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), unmarshal(v))).collect()),
        None => item.clone(),
    }
}

/// DynamoDB-typed value for a plain JSON value (strings → S, numbers → N, arrays → L, objects → M).
pub fn marshal(v: &Value) -> Value {
    match v {
        Value::Null => json!({ "NULL": true }),
        Value::Bool(b) => json!({ "BOOL": b }),
        Value::Number(n) => json!({ "N": n.to_string() }),
        Value::String(s) => json!({ "S": s }),
        Value::Array(a) => json!({ "L": a.iter().map(marshal).collect::<Vec<_>>() }),
        Value::Object(o) => json!({ "M": o.iter().map(|(k, v)| (k.clone(), marshal(v))).collect::<serde_json::Map<_, _>>() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_endpoints_and_marshalling() {
        let s = parse_script("// list\nListTables {}\nQuery {\n  \"TableName\": \"orders\",\n  \"KeyConditionExpression\": \"pk = :p\"\n}\nDescribeTable { \"TableName\": \"x\" }").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[1].0, "Query");
        assert_eq!(s[1].1["TableName"], "orders");
        assert!(parse_script("{ }").is_err());
        assert!(parse_script("Query { bad }").unwrap_err().message.contains("invalid JSON"));
        let mut c = ConnectionConfig {
            kind: crate::DbKind::Dynamodb,
            host: String::new(),
            port: 0,
            user: String::new(),
            password: None,
            database: Some("eu-west-1".into()),
            path: None,
            tls: TlsMode::Disable,
            timeout_secs: 5,
            ssh: None,
            via: None,
        };
        assert_eq!(endpoint(&c), (true, "dynamodb.eu-west-1.amazonaws.com".into(), 443, "eu-west-1".into()));
        c.host = "http://localhost:8000".into();
        assert_eq!(endpoint(&c), (false, "localhost".into(), 8000, "eu-west-1".into()));
        c.host = "127.0.0.1".into();
        c.port = 18000;
        assert_eq!(endpoint(&c), (false, "127.0.0.1".into(), 18000, "eu-west-1".into()));
        let typed = json!({ "M": { "a": { "N": "1.5" }, "b": { "SS": ["x"] }, "c": { "L": [{ "NULL": true }, { "BOOL": true }] }, "big": { "N": "12345678901234567890123" } } });
        assert_eq!(unmarshal(&typed), json!({ "a": 1.5, "b": ["x"], "c": [null, true], "big": "12345678901234567890123" }));
        assert_eq!(
            marshal(&json!({ "n": 2, "s": "x", "l": [true] })),
            json!({ "M": { "n": { "N": "2" }, "s": { "S": "x" }, "l": { "L": [{ "BOOL": true }] } } })
        );
        assert!(is_read_operation("Query") && !is_read_operation("PutItem"));
    }
}
