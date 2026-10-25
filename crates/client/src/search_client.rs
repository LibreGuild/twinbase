//! Elasticsearch and OpenSearch driver over the REST API (one kept-alive HTTP connection).
//!
//! The console takes requests the way Kibana Dev Tools and OpenSearch Dashboards do:
//!
//! ```text
//! GET /products/_search
//! { "query": { "match": { "name": "lamp" } } }
//!
//! POST /_bulk
//! { "index": { "_index": "products", "_id": "1" } }
//! { "name": "Lamp" }
//! ```

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use serde::Serialize;
use serde_json::{Value, json};

use crate::http::{HttpConn, url_encode};
use crate::{ClientError, ConnectionConfig, Result, TlsMode};

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    /// `elasticsearch` or `opensearch`.
    pub flavor: String,
    pub version: String,
    pub cluster: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    pub name: String,
    pub health: String,
    pub status: String,
    pub docs: Option<u64>,
    pub size: Option<String>,
    pub primaries: Option<u32>,
    pub replicas: Option<u32>,
    /// Aliases pointing at this index.
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldInfo {
    /// Dotted path, e.g. `address.city` (multi-fields as `name.keyword`).
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// One request of a console script and its response.
#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub request: String,
    pub status: u16,
    pub response: Value,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientError>,
}

pub struct SearchClient {
    http: HttpConn,
    authorization: Option<String>,
    pub info: ServerInfo,
    tunnel: Option<Arc<crate::ssh::Tunnel>>,
}

/// Endpoint from the connection fields: a URL in `host` (Elastic Cloud, AWS OpenSearch), or
/// `host` + `port` (HTTPS when TLS is on).
pub fn endpoint(config: &ConnectionConfig) -> (bool, String, u16) {
    let host = config.host.trim();
    if let Some((scheme, rest)) = host.split_once("://") {
        let https = scheme.eq_ignore_ascii_case("https");
        let authority = rest.split('/').next().unwrap_or(rest);
        let (h, p) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_owned(), p.parse().unwrap_or(if https { 443 } else { 9200 })),
            _ => (authority.to_owned(), if https { 443 } else { 9200 }),
        };
        return (https, h, p);
    }
    let host = if host.is_empty() { "127.0.0.1".to_owned() } else { host.to_owned() };
    (config.tls != TlsMode::Disable, host, if config.port == 0 { 9200 } else { config.port })
}

/// `{"error": {"type": "index_not_found_exception", "reason": "no such index [x]"}, "status": 404}`
fn error_from(status: u16, body: &Value, raw: &[u8]) -> ClientError {
    let err = &body["error"];
    let (kind, reason) = match err {
        Value::Object(_) => {
            let cause = err["root_cause"].get(0).filter(|c| c["reason"].is_string()).unwrap_or(err);
            (err["type"].as_str().map(str::to_owned), cause["reason"].as_str().or(err["reason"].as_str()).map(str::to_owned))
        }
        Value::String(s) => (None, Some(s.clone())),
        _ => (None, None),
    };
    let mut e = ClientError::new(
        reason.unwrap_or_else(|| format!("HTTP {status}: {}", String::from_utf8_lossy(raw).chars().take(300).collect::<String>())),
    );
    e.code = kind.or_else(|| Some(status.to_string()));
    if let Some(caused) = err["caused_by"]["reason"].as_str() {
        e.detail = Some(caused.to_owned());
    }
    e
}

/// Requests that only read (for read-only connections and agents).
pub fn is_read_request(method: &str, path: &str) -> bool {
    let m = method.to_ascii_uppercase();
    if m == "GET" || m == "HEAD" {
        return true;
    }
    if m != "POST" {
        return false;
    }
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // A write endpoint anywhere in the path (`_update_by_query`, `_bulk`, `_doc`, …) wins.
    let reads = [
        "_search",
        "_count",
        "_msearch",
        "_mget",
        "_sql",
        "_explain",
        "_validate",
        "_field_caps",
        "_analyze",
        "_termvectors",
        "_mtermvectors",
        "_rank_eval",
        "_async_search",
        "_eql",
        "_query",
        "_render",
        "_ppl",
    ];
    segments.iter().any(|s| reads.contains(s))
        && !segments
            .iter()
            .any(|s| matches!(*s, "_bulk" | "_doc" | "_create" | "_update" | "_update_by_query" | "_delete_by_query" | "_reindex"))
}

/// A parsed console request.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleRequest {
    pub method: String,
    pub path: String,
    /// JSON text, or NDJSON lines for `_bulk` / `_msearch`.
    pub body: Option<String>,
    pub ndjson: bool,
}

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// Kibana-style `"""…"""` strings become JSON strings.
fn triple_quotes(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("\"\"\"") {
        let Some(end) = rest[start + 3..].find("\"\"\"") else { break };
        out.push_str(&rest[..start]);
        out.push_str(&serde_json::to_string(&rest[start + 3..start + 3 + end]).unwrap_or_default());
        rest = &rest[start + 3 + end + 3..];
    }
    out.push_str(rest);
    out
}

pub fn parse_console(src: &str) -> Result<Vec<ConsoleRequest>> {
    let mut out: Vec<ConsoleRequest> = Vec::new();
    let mut body = String::new();
    let flush = |out: &mut Vec<ConsoleRequest>, body: &mut String| -> Result<()> {
        if let Some(r) = out.last_mut() {
            let text = triple_quotes(body.trim());
            if !text.is_empty() {
                if r.ndjson {
                    // One JSON value per line; reject anything else early with a clear message.
                    for (i, line) in text.lines().map(str::trim).filter(|l| !l.is_empty()).enumerate() {
                        serde_json::from_str::<Value>(line).map_err(|e| {
                            ClientError::new(format!("{} {}: line {} of the body is not JSON: {e}", r.method, r.path, i + 1))
                        })?;
                    }
                    r.body = Some(text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n") + "\n");
                } else {
                    let v: Value = serde_json::from_str(&text)
                        .map_err(|e| ClientError::new(format!("{} {}: the body is not JSON: {e}", r.method, r.path)))?;
                    r.body = Some(v.to_string());
                }
            }
        }
        body.clear();
        Ok(())
    };
    for line in src.lines() {
        let t = line.trim();
        let mut words = t.split_whitespace();
        let first = words.next().unwrap_or("");
        if METHODS.contains(&first.to_ascii_uppercase().as_str())
            && let Some(path) = words.next()
        {
            flush(&mut out, &mut body)?;
            let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
            let bare = path.split('?').next().unwrap_or("");
            let ndjson = ["_bulk", "_msearch", "_mtermvectors"].iter().any(|e| bare.ends_with(e)) || bare.ends_with("/_msearch/template");
            out.push(ConsoleRequest { method: first.to_ascii_uppercase(), path, body: None, ndjson });
            continue;
        }
        if out.is_empty() {
            if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
                continue;
            }
            return Err(ClientError::new(format!("expected a request such as GET /index/_search, found: {t}")));
        }
        if !(body.trim().is_empty() && (t.starts_with('#') || t.starts_with("//"))) {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&mut out, &mut body)?;
    Ok(out)
}

/// Whether a console script changes anything.
pub fn script_writes(src: &str) -> bool {
    match parse_console(src) {
        Ok(reqs) => reqs.iter().any(|r| !is_read_request(&r.method, &r.path)),
        Err(_) => true,
    }
}

impl SearchClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<SearchClient> {
        let (https, host, port) = endpoint(config);
        let http = HttpConn::new(https, host, port, config.via, config.tls, Duration::from_secs(config.timeout_secs.max(1)));
        let password = config.password.clone().unwrap_or_default();
        let authorization = if !config.user.is_empty() {
            use base64::Engine;
            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{password}", config.user))))
        } else if !password.is_empty() {
            // No user: the password field holds an API key (`id:key` or its base64 form).
            use base64::Engine;
            let key = if password.contains(':') { base64::engine::general_purpose::STANDARD.encode(&password) } else { password };
            Some(format!("ApiKey {key}"))
        } else {
            None
        };
        let endpoint = http.base_url();
        let mut client = SearchClient {
            http,
            authorization,
            info: ServerInfo { flavor: String::new(), version: String::new(), cluster: String::new(), endpoint },
            tunnel: None,
        };
        let root = client.request("GET", "/", None).await?;
        client.info.version = root["version"]["number"].as_str().unwrap_or("").to_owned();
        client.info.flavor =
            if root["version"]["distribution"].as_str() == Some("opensearch") { "opensearch".into() } else { "elasticsearch".into() };
        client.info.cluster = root["cluster_name"].as_str().unwrap_or("").to_owned();
        Ok(client)
    }

    pub fn with_tunnel(mut self, t: Arc<crate::ssh::Tunnel>) -> Self {
        self.tunnel = Some(t);
        self
    }

    pub fn product(&self) -> String {
        format!("{} {}", if self.info.flavor == "opensearch" { "OpenSearch" } else { "Elasticsearch" }, self.info.version)
    }

    /// Send one request; the status and parsed body (text bodies such as `_cat` output become a
    /// JSON string).
    pub async fn raw(&self, method: &str, path: &str, body: Option<&str>, ndjson: bool) -> Result<(u16, Value)> {
        if !METHODS.contains(&method) {
            return Err(ClientError::new(format!("unsupported method {method}")));
        }
        if !path.starts_with('/') || path.contains(char::is_whitespace) || path.contains("://") {
            return Err(ClientError::new(format!("invalid path {path}")));
        }
        let bytes = Bytes::from(body.unwrap_or("").to_owned());
        let res = self
            .http
            .send(path, self.http.timeout.max(Duration::from_secs(120)), || {
                let mut req = hyper::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", self.http.host_header())
                    .header("accept", "application/json");
                if body.is_some() {
                    req = req.header("content-type", if ndjson { "application/x-ndjson" } else { "application/json" });
                }
                if let Some(a) = &self.authorization {
                    req = req.header("authorization", a.as_str());
                }
                req.body(Full::new(bytes.clone())).map_err(|e| ClientError::new(e.to_string()))
            })
            .await?;
        let v = if res.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&res.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&res.body).into_owned()))
        };
        Ok((res.status, v))
    }

    /// Send a request and fail on HTTP errors.
    pub async fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let text = body.map(|b| b.to_string());
        let (status, v) = self.raw(method, path, text.as_deref(), false).await?;
        if status >= 400 {
            return Err(error_from(status, &v, v.to_string().as_bytes()));
        }
        Ok(v)
    }

    pub async fn indices(&self, include_hidden: bool) -> Result<Vec<IndexSummary>> {
        let v = self
            .request(
                "GET",
                "/_cat/indices?format=json&bytes=b&h=index,health,status,docs.count,store.size,pri,rep&expand_wildcards=open",
                None,
            )
            .await?;
        let aliases = self.request("GET", "/_cat/aliases?format=json&h=alias,index", None).await.unwrap_or(Value::Null);
        let mut out: Vec<IndexSummary> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let name = i["index"].as_str()?.to_owned();
                if !include_hidden && name.starts_with('.') {
                    return None;
                }
                let num = |k: &str| i[k].as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| i[k].as_u64());
                Some(IndexSummary {
                    aliases: aliases
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|a| a["index"].as_str() == Some(name.as_str()))
                        .filter_map(|a| a["alias"].as_str().map(str::to_owned))
                        .collect(),
                    health: i["health"].as_str().unwrap_or("").to_owned(),
                    status: i["status"].as_str().unwrap_or("").to_owned(),
                    docs: num("docs.count"),
                    size: num("store.size").map(|b| b.to_string()),
                    primaries: num("pri").map(|n| n as u32),
                    replicas: num("rep").map(|n| n as u32),
                    name,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The fields of an index (merged across the indices an alias or pattern matches).
    pub async fn mapping(&self, index: &str) -> Result<(Vec<FieldInfo>, Value)> {
        let v = self.request("GET", &format!("/{}/_mapping", url_encode(index)), None).await?;
        fn walk(props: &Value, prefix: &str, out: &mut Vec<FieldInfo>) {
            for (name, f) in props.as_object().into_iter().flatten() {
                let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
                let kind = f["type"].as_str().unwrap_or(if f.get("properties").is_some() { "object" } else { "unknown" });
                if !out.iter().any(|x| x.path == path) {
                    out.push(FieldInfo { path: path.clone(), kind: kind.to_owned() });
                }
                if let Some(p) = f.get("properties") {
                    walk(p, &path, out);
                }
                for (sub, sf) in f["fields"].as_object().into_iter().flatten() {
                    let sp = format!("{path}.{sub}");
                    if !out.iter().any(|x| x.path == sp) {
                        out.push(FieldInfo { path: sp, kind: sf["type"].as_str().unwrap_or("unknown").to_owned() });
                    }
                }
            }
        }
        let mut fields = Vec::new();
        for (_, m) in v.as_object().into_iter().flatten() {
            walk(&m["mappings"]["properties"], "", &mut fields);
        }
        Ok((fields, v))
    }

    /// `_search` with sequence numbers (for optimistic concurrency on save).
    pub async fn search(&self, index: &str, body: &Value) -> Result<Value> {
        self.request("POST", &format!("/{}/_search?seq_no_primary_term=true", url_encode(index)), Some(body)).await
    }

    pub async fn get(&self, index: &str, id: &str) -> Result<Value> {
        self.request("GET", &format!("/{}/_doc/{}", url_encode(index), url_encode(id)), None).await
    }

    /// Index a document: replace `id` (only if unchanged since `if_seq_no`/`if_primary_term`), create
    /// it when `create` (fails if it exists), or add one with a generated id when `id` is empty.
    pub async fn put(&self, index: &str, id: &str, doc: &Value, create: bool, version: Option<(u64, u64)>) -> Result<Value> {
        let idx = url_encode(index);
        let (method, mut path) = if id.is_empty() {
            ("POST", format!("/{idx}/_doc?refresh=wait_for"))
        } else if create {
            ("PUT", format!("/{idx}/_create/{}?refresh=wait_for", url_encode(id)))
        } else {
            ("PUT", format!("/{idx}/_doc/{}?refresh=wait_for", url_encode(id)))
        };
        if let Some((seq, term)) = version {
            path.push_str(&format!("&if_seq_no={seq}&if_primary_term={term}"));
        }
        self.request(method, &path, Some(doc)).await
    }

    /// Delete documents by id in one bulk request; returns how many were deleted.
    pub async fn delete(&self, index: &str, ids: &[String]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let body: String = ids.iter().map(|id| format!("{}\n", json!({ "delete": { "_index": index, "_id": id } }))).collect();
        let (status, v) = self.raw("POST", "/_bulk?refresh=wait_for", Some(&body), true).await?;
        if status >= 400 {
            return Err(error_from(status, &v, b""));
        }
        Ok(v["items"].as_array().into_iter().flatten().filter(|i| i["delete"]["result"].as_str() == Some("deleted")).count() as u64)
    }

    /// Run a console script; stops at the first request that fails.
    pub async fn run(&self, script: &str) -> Result<Vec<SearchOutput>> {
        let mut out = Vec::new();
        for (i, r) in parse_console(script)?.into_iter().enumerate() {
            let started = std::time::Instant::now();
            let label = format!("{} {}", r.method, r.path);
            let (status, response) = self.raw(&r.method, &r.path, r.body.as_deref(), r.ndjson).await.map_err(|mut e| {
                e.statement_index = Some(i);
                e
            })?;
            let error = (status >= 400).then(|| {
                let mut e = error_from(status, &response, b"");
                e.statement_index = Some(i);
                e
            });
            let failed = error.is_some();
            out.push(SearchOutput { request: label, status, response, elapsed_ms: started.elapsed().as_secs_f64() * 1000.0, error });
            if failed {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_scripts() {
        let r = parse_console(
            "# comment\nGET products/_search\n{\n  \"query\": { \"match\": { \"name\": \"lamp\" } }\n}\n\nPOST /_bulk\n{\"index\":{\"_index\":\"p\",\"_id\":\"1\"}}\n{\"name\":\"a\"}\n\nDELETE /p\nPOST /p/_doc\n{\"text\": \"\"\"multi\nline\"\"\"}\n",
        )
        .unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!((r[0].method.as_str(), r[0].path.as_str()), ("GET", "/products/_search"));
        assert_eq!(r[0].body.as_deref(), Some(r#"{"query":{"match":{"name":"lamp"}}}"#));
        assert!(r[1].ndjson);
        assert_eq!(r[1].body.as_deref(), Some("{\"index\":{\"_index\":\"p\",\"_id\":\"1\"}}\n{\"name\":\"a\"}\n"));
        assert_eq!(r[2].body, None);
        assert_eq!(r[3].body.as_deref(), Some(r#"{"text":"multi\nline"}"#));
        assert!(parse_console("GET /x\n{ nope").is_err());
        assert!(parse_console("hello").is_err());
    }

    #[test]
    fn classifies_reads() {
        assert!(is_read_request("GET", "/_cat/indices"));
        assert!(is_read_request("POST", "/products/_search?size=1"));
        assert!(is_read_request("POST", "/_sql?format=txt"));
        assert!(is_read_request("POST", "/a,b/_count"));
        assert!(!is_read_request("POST", "/products/_doc"));
        assert!(!is_read_request("POST", "/products/_update_by_query"));
        assert!(!is_read_request("POST", "/_bulk"));
        assert!(!is_read_request("PUT", "/products"));
        assert!(!is_read_request("DELETE", "/products/_doc/1"));
        assert!(script_writes("GET /a/_search\nPOST /a/_doc\n{}"));
        assert!(!script_writes("GET /a/_search\nPOST /a/_count\n{}"));
    }

    #[test]
    fn errors_use_type_and_root_cause() {
        let body = json!({ "error": { "root_cause": [{ "type": "index_not_found_exception", "reason": "no such index [x]" }], "type": "index_not_found_exception", "reason": "no such index [x]" }, "status": 404 });
        let e = error_from(404, &body, b"");
        assert_eq!(e.message, "no such index [x]");
        assert_eq!(e.code.as_deref(), Some("index_not_found_exception"));
    }
}
