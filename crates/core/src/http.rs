//! Local HTTP endpoint on 127.0.0.1: the MCP server (`/mcp`) and, when enabled, a JSON API with
//! server-sent events for running the UI in a normal browser during development.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::Stream;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::app::App;
use crate::events::Actor;
use crate::mcp::{self, Sessions};

#[derive(Clone)]
struct Ctx {
    app: Arc<App>,
    sessions: Arc<Sessions>,
    dev_api: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct HttpOptions {
    pub port: u16,
    /// Expose `/api/*` and `/api/events` for a browser-hosted UI (development only).
    pub dev_api: bool,
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        None => true,
        Some(o) => {
            let host = o.split("://").nth(1).unwrap_or(o).split(['/', ':']).next().unwrap_or("");
            matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "tauri.localhost") || o.starts_with("tauri://")
        }
    }
}

fn token_ok(app: &App, headers: &HeaderMap, query_token: Option<&str>) -> bool {
    let expected = app.settings().mcp.token;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| query_token.map(str::to_owned));
    match given {
        // Constant-time comparison.
        Some(g) => g.len() == expected.len() && g.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0,
        None => false,
    }
}

fn cors(mut resp: Response, headers: &HeaderMap, dev: bool) -> Response {
    if dev && let Some(o) = headers.get(header::ORIGIN) {
        resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, o.clone());
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type, mcp-session-id"));
        resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, DELETE, OPTIONS"));
    }
    resp
}

async fn mcp_post(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    if !ctx.app.settings().mcp.enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, "MCP is disabled in TwinBase settings").into_response();
    }
    if !token_ok(&ctx.app, &headers, None) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid bearer token; copy the MCP setup from TwinBase → Settings → AI agents" })),
        )
            .into_response();
    }
    let session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
            )
                .into_response();
        }
    };
    let (batch, messages) = match parsed {
        Value::Array(items) => (true, items),
        other => (false, vec![other]),
    };
    let mut replies = Vec::new();
    let mut new_session = None;
    for m in messages {
        let (reply, sid) = mcp::handle(&ctx.app, &ctx.sessions, session.as_deref().or(new_session.as_deref()), m).await;
        if sid.is_some() {
            new_session = sid;
        }
        if let Some(r) = reply {
            replies.push(r);
        }
    }
    let mut resp = if replies.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if batch {
        Json(Value::Array(replies)).into_response()
    } else {
        Json(replies.into_iter().next().unwrap()).into_response()
    };
    if let Some(sid) = new_session
        && let Ok(v) = HeaderValue::from_str(&sid)
    {
        resp.headers_mut().insert("mcp-session-id", v);
    }
    resp
}

/// End an MCP session (Streamable HTTP clients send DELETE when they disconnect).
async fn mcp_delete(State(ctx): State<Ctx>, headers: HeaderMap) -> StatusCode {
    if !origin_allowed(&headers) || !token_ok(&ctx.app, &headers, None) {
        return StatusCode::FORBIDDEN;
    }
    match headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        Some(sid) if ctx.sessions.end(sid) => StatusCode::OK,
        _ => StatusCode::NOT_FOUND,
    }
}

struct Json(Value);

impl IntoResponse for Json {
    fn into_response(self) -> Response {
        ([(header::CONTENT_TYPE, "application/json")], self.0.to_string()).into_response()
    }
}

async fn api_call(
    State(ctx): State<Ctx>,
    Path(method): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !ctx.dev_api || !origin_allowed(&headers) || !token_ok(&ctx.app, &headers, q.get("token").map(String::as_str)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let params: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return cors(
                    Json(json!({ "ok": false, "error": { "kind": "invalid", "message": format!("invalid JSON: {e}") } })).into_response(),
                    &headers,
                    true,
                );
            }
        }
    };
    let resp = match crate::api::call(&ctx.app, &method, params, &Actor::User).await {
        Ok(v) => Json(json!({ "ok": true, "result": v })).into_response(),
        Err(e) => Json(json!({ "ok": false, "error": e })).into_response(),
    };
    cors(resp, &headers, true)
}

async fn api_events(State(ctx): State<Ctx>, Query(q): Query<std::collections::HashMap<String, String>>, headers: HeaderMap) -> Response {
    if !ctx.dev_api || !origin_allowed(&headers) || !token_ok(&ctx.app, &headers, q.get("token").map(String::as_str)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> = Box::pin(
        BroadcastStream::new(ctx.app.subscribe())
            .filter_map(|e| e.ok())
            .map(|e| Ok(SseEvent::default().data(serde_json::to_string(&e).unwrap_or_default()))),
    );
    cors(Sse::new(stream).keep_alive(KeepAlive::default()).into_response(), &headers, true)
}

async fn preflight(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    cors(StatusCode::NO_CONTENT.into_response(), &headers, ctx.dev_api)
}

/// Start the local endpoint. Returns the bound address and the server task.
pub async fn serve(app: Arc<App>, opts: HttpOptions) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let ctx = Ctx { app, sessions: Arc::new(Sessions::default()), dev_api: opts.dev_api };
    let app_for_exit = ctx.app.clone();
    let router = Router::new()
        .route("/mcp", post(mcp_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }).delete(mcp_delete))
        .route("/health", get(|| async { Json(json!({ "ok": true, "app": "twinbase" })) }))
        .route("/api/events", get(api_events))
        .route("/api/{method}", post(api_call).options(preflight))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port)).await?;
    let addr = listener.local_addr()?;
    app_for_exit.set_mcp_listening(Some(addr.port()));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
        app_for_exit.set_mcp_listening(None);
    });
    Ok((addr, task))
}
