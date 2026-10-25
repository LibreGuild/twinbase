//! A kept-alive HTTP/1.1 connection to one endpoint (TLS for `https`), shared by the drivers that
//! speak HTTP APIs (DynamoDB, ClickHouse, Elasticsearch/OpenSearch).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::{ClientError, Result, TlsMode};

pub struct Response {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Bytes,
}

pub struct HttpConn {
    pub https: bool,
    /// `Host` header and TLS name.
    pub host: String,
    pub port: u16,
    /// Where to open TCP connections (the endpoint, or an SSH tunnel's local port).
    connect_to: (String, u16),
    tls: Option<Arc<rustls::ClientConfig>>,
    pub timeout: Duration,
    /// The kept-alive connection and when it was last used.
    conn: tokio::sync::Mutex<Option<(SendRequest<Full<Bytes>>, std::time::Instant)>>,
}

/// Non-idempotent requests (POST) only reuse a connection used this recently: servers close idle
/// keep-alive connections, and a request written as that happens may or may not have run, so it
/// cannot be retried safely.
const FRESH: Duration = Duration::from_secs(4);

impl HttpConn {
    /// `tls` decides certificate checks for `https`: `Require` skips them, anything else verifies.
    pub fn new(https: bool, host: String, port: u16, via: Option<std::net::SocketAddr>, tls: TlsMode, timeout: Duration) -> HttpConn {
        let tls = https
            .then(|| Arc::new(crate::tls::client_config(if tls == TlsMode::Require { TlsMode::Require } else { TlsMode::VerifyFull })));
        let connect_to = match via {
            Some(v) => (v.ip().to_string(), v.port()),
            None => (host.clone(), port),
        };
        HttpConn { https, host, port, connect_to, tls, timeout, conn: tokio::sync::Mutex::new(None) }
    }

    /// `host` or `host:port` when the port is not the scheme's default.
    pub fn host_header(&self) -> String {
        if (self.https && self.port == 443) || (!self.https && self.port == 80) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn base_url(&self) -> String {
        format!("{}://{}", if self.https { "https" } else { "http" }, self.host_header())
    }

    async fn open(&self) -> Result<SendRequest<Full<Bytes>>> {
        let tcp = tokio::time::timeout(self.timeout, TcpStream::connect((self.connect_to.0.as_str(), self.connect_to.1)))
            .await
            .map_err(|_| ClientError::new(format!("connecting to {}:{} timed out", self.host, self.port)))?
            .map_err(|e| ClientError::new(format!("cannot connect to {}:{}: {e}", self.host, self.port)))?;
        let _ = tcp.set_nodelay(true);
        macro_rules! handshake {
            ($io:expr) => {{
                let (send, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new($io)).await.map_err(|e| ClientError::new(format!("HTTP: {e}")))?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                send
            }};
        }
        Ok(match &self.tls {
            Some(cfg) => {
                let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| ClientError::new(format!("TLS: {e}")))?;
                let stream = tokio_rustls::TlsConnector::from(cfg.clone())
                    .connect(name, tcp)
                    .await
                    .map_err(|e| ClientError::new(format!("TLS: {e}")))?;
                handshake!(stream)
            }
            None => handshake!(tcp),
        })
    }

    /// Send a request built by `build` (called again for a retry). Requests are serialized on the
    /// connection. A request is retried on a new connection only when it certainly did not run: it
    /// was never sent, or it is idempotent (GET, HEAD, PUT, DELETE).
    pub async fn send(&self, what: &str, deadline: Duration, build: impl Fn() -> Result<hyper::Request<Full<Bytes>>>) -> Result<Response> {
        for attempt in 0..2 {
            let req = build()?;
            let idempotent = matches!(
                *req.method(),
                hyper::Method::GET | hyper::Method::HEAD | hyper::Method::PUT | hyper::Method::DELETE | hyper::Method::OPTIONS
            );
            let mut guard = self.conn.lock().await;
            let mut send = match guard.take() {
                Some((s, used)) if !s.is_closed() && (idempotent || used.elapsed() < FRESH) => s,
                _ => self.open().await?,
            };
            let res = match tokio::time::timeout(deadline, send.send_request(req)).await {
                Err(_) => return Err(ClientError::new(format!("{what} timed out after {}s", deadline.as_secs()))),
                Ok(Err(e)) if attempt == 0 && (e.is_canceled() || (idempotent && (e.is_closed() || e.is_incomplete_message()))) => continue,
                Ok(Err(e)) => return Err(ClientError::new(format!("HTTP: {e}"))),
                Ok(Ok(r)) => r,
            };
            let status = res.status().as_u16();
            let keep = !res.headers().get("connection").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
            let headers = res.headers().clone();
            let body = match tokio::time::timeout(deadline, res.into_body().collect()).await {
                Err(_) => return Err(ClientError::new(format!("{what} timed out after {}s", deadline.as_secs()))),
                Ok(r) => r.map_err(|e| ClientError::new(format!("HTTP: {e}")))?.to_bytes(),
            };
            if keep {
                *guard = Some((send, std::time::Instant::now()));
            }
            return Ok(Response { status, headers, body });
        }
        Err(ClientError::new(format!("{what}: the connection was closed")))
    }
}

/// `application/x-www-form-urlencoded` / query-string escaping.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
