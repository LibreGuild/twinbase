//! MongoDB mock server: the OP_MSG wire protocol (plus the legacy OP_QUERY handshake) over an
//! in-memory document store with snapshot persistence.

pub mod agg;
pub mod query;
mod scram;
pub mod store;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bson::{Bson, Document, doc};
use bytes::BytesMut;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

pub use query::MongoError;
pub use store::MongoStore;

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};
use query::{MResult, as_f64};
use store::UpdateSpec;

const OP_REPLY: i32 = 1;
const OP_QUERY: i32 = 2004;
const OP_MSG: i32 = 2013;
const MAX_MESSAGE: usize = 48_000_000;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_BATCH: usize = 101;
const CURSOR_IDLE: Duration = Duration::from_secs(600);
const MAX_CURSORS: usize = 10_000;
const VERSION: &str = "7.0.12";

/// Snapshot file used for a MongoDB mock data directory.
pub fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join("mongo.bson")
}

struct Cursor {
    ns: String,
    docs: VecDeque<Document>,
    touched: Instant,
}

struct Server {
    store: Arc<MongoStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cursors: Mutex<HashMap<i64, Cursor>>,
    request_ids: AtomicI32,
    connections: AtomicU64,
    started: Instant,
    port: u16,
}

struct Conn {
    id: u64,
    peer: String,
    user: Option<String>,
    sasl: Option<scram::Conversation>,
}

impl Conn {
    fn authenticated(&self, server: &Server) -> bool {
        matches!(server.auth, AuthMode::Any) || self.user.is_some()
    }
}

/// Serve MongoDB clients until `cancel` fires. Snapshots are written every 2 s when data changed
/// and once more on shutdown.
pub async fn serve(
    listener: TcpListener,
    store: Arc<MongoStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cancel: CancellationToken,
) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let server = Arc::new(Server {
        store: store.clone(),
        auth,
        traffic,
        faults,
        cursors: Mutex::new(HashMap::new()),
        request_ids: AtomicI32::new(1),
        connections: AtomicU64::new(0),
        started: Instant::now(),
        port,
    });
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                server.cursors.lock().retain(|_, c| c.touched.elapsed() < CURSOR_IDLE);
                let st = store.clone();
                let _ = tokio::task::spawn_blocking(move || st.save_if_dirty()).await;
            }
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { continue };
                if server.faults.refuse_connections() {
                    server.traffic.fault(server.traffic.new_connection_id(), "(new connection)", "connection refused");
                    drop(socket);
                    continue;
                }
                let _ = socket.set_nodelay(true);
                tokio::spawn(handle_connection(server.clone(), socket, cancel.clone()));
            }
        }
    }
    let _ = tokio::task::spawn_blocking(move || store.save_if_dirty()).await;
}

// ------------------------------------------------------------------ wire format

struct Request {
    request_id: i32,
    op: i32,
    /// The command, with OP_MSG document sequences folded in and `$db` set.
    command: Document,
    /// OP_MSG `moreToCome`: the client expects no reply.
    no_reply: bool,
}

fn read_i32(b: &[u8], at: usize) -> Option<i32> {
    b.get(at..at + 4).map(|s| i32::from_le_bytes(s.try_into().unwrap()))
}

fn read_cstring(b: &[u8], at: usize) -> Option<(String, usize)> {
    let end = b.get(at..)?.iter().position(|&c| c == 0)?;
    Some((String::from_utf8_lossy(&b[at..at + end]).into_owned(), at + end + 1))
}

fn read_doc(b: &[u8], at: usize) -> Result<(Document, usize), String> {
    let len = read_i32(b, at).filter(|l| *l >= 5).ok_or("truncated document")? as usize;
    let slice = b.get(at..at + len).ok_or("truncated document")?;
    let d = Document::from_reader(slice).map_err(|e| format!("invalid BSON: {e}"))?;
    Ok((d, at + len))
}

fn parse(msg: &[u8]) -> Result<Request, String> {
    let request_id = read_i32(msg, 4).unwrap_or(0);
    let op = read_i32(msg, 12).unwrap_or(0);
    match op {
        OP_MSG => {
            let flags = read_i32(msg, 16).ok_or("truncated OP_MSG")? as u32;
            let end = if flags & 1 != 0 { msg.len().saturating_sub(4) } else { msg.len() };
            let body = &msg[..end];
            let mut at = 20;
            let mut command: Option<Document> = None;
            let mut sequences: Vec<(String, Vec<Bson>)> = Vec::new();
            while at < body.len() {
                match body[at] {
                    0 => {
                        let (d, next) = read_doc(body, at + 1)?;
                        command = Some(d);
                        at = next;
                    }
                    1 => {
                        let size = read_i32(body, at + 1).filter(|s| *s >= 5).ok_or("bad document sequence")? as usize;
                        let section_end = at + 1 + size;
                        if section_end > body.len() {
                            return Err("bad document sequence".into());
                        }
                        let (ident, mut p) = read_cstring(body, at + 5).ok_or("bad document sequence")?;
                        let mut docs = Vec::new();
                        while p < section_end {
                            let (d, next) = read_doc(&body[..section_end], p)?;
                            docs.push(Bson::Document(d));
                            p = next;
                        }
                        sequences.push((ident, docs));
                        at = section_end;
                    }
                    k => return Err(format!("unknown OP_MSG section kind {k}")),
                }
            }
            let mut command = command.ok_or("OP_MSG without a body")?;
            for (ident, docs) in sequences {
                command.insert(ident, docs);
            }
            Ok(Request { request_id, op, command, no_reply: flags & 2 != 0 })
        }
        OP_QUERY => {
            let (ns, at) = read_cstring(msg, 20).ok_or("truncated OP_QUERY")?;
            let (mut q, _) = read_doc(msg, at + 8)?;
            if let Ok(inner) = q.get_document("$query") {
                q = inner.clone();
            }
            let db = ns.strip_suffix(".$cmd").ok_or("only commands are supported over OP_QUERY")?;
            q.insert("$db", db);
            Ok(Request { request_id, op, command: q, no_reply: false })
        }
        2012 => Err("compressed messages are not supported (no compressor was negotiated)".into()),
        other => Err(format!("unsupported opcode {other}")),
    }
}

fn encode_reply(server: &Server, req: &Request, reply: &Document) -> Vec<u8> {
    let body = reply.to_vec().unwrap_or_else(|_| doc! { "ok": 0.0, "errmsg": "reply could not be encoded" }.to_vec().unwrap());
    let mut out = Vec::with_capacity(body.len() + 40);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&server.request_ids.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    out.extend_from_slice(&req.request_id.to_le_bytes());
    if req.op == OP_QUERY {
        out.extend_from_slice(&OP_REPLY.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes()); // responseFlags
        out.extend_from_slice(&0i64.to_le_bytes()); // cursorID
        out.extend_from_slice(&0i32.to_le_bytes()); // startingFrom
        out.extend_from_slice(&1i32.to_le_bytes()); // numberReturned
    } else {
        out.extend_from_slice(&OP_MSG.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flagBits
        out.push(0);
    }
    out.extend_from_slice(&body);
    let len = out.len() as i32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    out
}

// ------------------------------------------------------------------ connection

async fn handle_connection(server: Arc<Server>, mut socket: TcpStream, cancel: CancellationToken) {
    let id = server.traffic.new_connection_id();
    let peer = socket.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    server.traffic.record(id, TrafficKind::Connect { peer: peer.clone(), user: None, database: None });
    server.connections.fetch_add(1, Ordering::Relaxed);
    let mut conn = Conn { id, peer, user: None, sasl: None };
    let mut buf = BytesMut::with_capacity(16 * 1024);
    'conn: loop {
        // Read one complete message.
        let msg = loop {
            if buf.len() >= 4 {
                let len = i32::from_le_bytes(buf[..4].try_into().unwrap());
                if len < 16 || len as usize > MAX_MESSAGE {
                    break 'conn;
                }
                if buf.len() >= len as usize {
                    break buf.split_to(len as usize);
                }
                buf.reserve(len as usize - buf.len());
            }
            tokio::select! {
                _ = cancel.cancelled() => break 'conn,
                n = socket.read_buf(&mut buf) => match n {
                    Ok(0) | Err(_) => break 'conn,
                    Ok(_) => {}
                },
            }
        };
        let req = match parse(&msg) {
            Ok(r) => r,
            Err(e) => {
                // Answer with an error when the header is usable, then close.
                if msg.len() >= 16 {
                    let req = Request { request_id: read_i32(&msg, 4).unwrap_or(0), op: OP_MSG, command: Document::new(), no_reply: false };
                    let reply = MongoError::new(9, "FailedToParse", e).to_doc();
                    let _ = socket.write_all(&encode_reply(&server, &req, &reply)).await;
                }
                break;
            }
        };
        let (reply, close) = dispatch(&server, &mut conn, req.command.clone()).await;
        if !req.no_reply && socket.write_all(&encode_reply(&server, &req, &reply)).await.is_err() {
            break;
        }
        if close {
            break;
        }
        // Give back memory after a large message.
        if buf.capacity() > 4 * 1024 * 1024 && buf.is_empty() {
            buf = BytesMut::with_capacity(16 * 1024);
        }
    }
    server.connections.fetch_sub(1, Ordering::Relaxed);
    server.traffic.record(id, TrafficKind::Disconnect);
}

/// Commands that never appear in the traffic log (driver handshakes and heartbeats).
fn is_quiet(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "hello"
            | "ismaster"
            | "ping"
            | "saslcontinue"
            | "endsessions"
            | "buildinfo"
            | "getparameter"
            | "getlog"
            | "hostinfo"
            | "getcmdlineopts"
            | "connectionstatus"
            | "atlasversion"
            | "whatsmyuri"
    )
}

/// Commands allowed before authentication.
fn is_preauth(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "hello"
            | "ismaster"
            | "ping"
            | "saslstart"
            | "saslcontinue"
            | "buildinfo"
            | "endsessions"
            | "logout"
            | "connectionstatus"
            | "whatsmyuri"
            | "getnonce"
            | "authenticate"
    )
}

async fn dispatch(server: &Arc<Server>, conn: &mut Conn, cmd: Document) -> (Document, bool) {
    let Some(name) = cmd.keys().next().cloned() else { return (MongoError::failed_to_parse("empty command").to_doc(), false) };
    let db = cmd.get_str("$db").unwrap_or("admin").to_owned();
    if cmd.contains_key("txnNumber")
        || cmd.contains_key("startTransaction")
        || matches!(name.as_str(), "commitTransaction" | "abortTransaction")
    {
        return (
            MongoError::new(20, "IllegalOperation", "Transaction numbers are only allowed on a replica set member or mongos").to_doc(),
            false,
        );
    }
    // Handshake, authentication and session bookkeeping.
    match name.to_ascii_lowercase().as_str() {
        "hello" | "ismaster" => return (hello(server, conn, &cmd, &name), false),
        "saslstart" => return (sasl_start(server, conn, &cmd, &db), false),
        "saslcontinue" => return (sasl_continue(server, conn, &cmd, &db), false),
        "logout" => {
            conn.user = None;
            return (doc! { "ok": 1.0 }, false);
        }
        _ => {}
    }
    if !conn.authenticated(server) && !is_preauth(&name) {
        return (MongoError::new(13, "Unauthorized", format!("Command {name} requires authentication")).to_doc(), false);
    }
    let started = Instant::now();
    let quiet = is_quiet(&name);
    let text = describe(&db, &name, &cmd);
    if !quiet {
        match server.faults.before(&text).await {
            Decision::Proceed => {}
            Decision::Fail { code, message } => {
                server.traffic.fault(conn.id, &text, &message);
                let code = code.and_then(|c| c.trim().parse().ok()).unwrap_or(1);
                let e = MongoError::new(code, fault_code_name(code), message);
                return (e.to_doc(), false);
            }
            Decision::Drop => {
                server.traffic.fault(conn.id, &text, "connection dropped");
                return (Document::new(), true);
            }
        }
    }
    let s = server.clone();
    let peer = conn.peer.clone();
    let user = conn.user.clone();
    let result = tokio::task::spawn_blocking(move || run(&s, &db, &name, &cmd, &peer, user.as_deref())).await;
    let (reply, rows) = match result {
        Ok(Ok((d, rows))) => (d, rows),
        Ok(Err(e)) => (e.to_doc(), 0),
        Err(_) => (MongoError::new(1, "InternalError", "command panicked").to_doc(), 0),
    };
    if !quiet {
        let error =
            (reply.get("ok").and_then(as_f64) == Some(0.0)).then(|| reply.get_str("errmsg").unwrap_or("error").to_owned()).or_else(|| {
                reply
                    .get_array("writeErrors")
                    .ok()
                    .and_then(|w| w.first())
                    .and_then(|e| e.as_document())
                    .and_then(|e| e.get_str("errmsg").ok())
                    .map(str::to_owned)
            });
        server.traffic.query(conn.id, &text, started, rows, error);
    }
    (reply, false)
}

fn fault_code_name(code: i32) -> &'static str {
    match code {
        6 => "HostUnreachable",
        7 => "HostNotFound",
        11000 => "DuplicateKey",
        13 => "Unauthorized",
        50 => "MaxTimeMSExpired",
        89 => "NetworkTimeout",
        91 => "ShutdownInProgress",
        112 => "WriteConflict",
        189 => "PrimarySteppedDown",
        10107 => "NotWritablePrimary",
        11600 => "InterruptedAtShutdown",
        _ => "TwinBaseFault",
    }
}

fn hello(server: &Server, conn: &Conn, cmd: &Document, name: &str) -> Document {
    let mut d = doc! {
        "helloOk": true,
        "isWritablePrimary": true,
        "maxBsonObjectSize": store::MAX_DOC_SIZE as i32,
        "maxMessageSizeBytes": MAX_MESSAGE as i32,
        "maxWriteBatchSize": 100_000,
        "localTime": bson::DateTime::now(),
        "logicalSessionTimeoutMinutes": 30,
        "connectionId": conn.id as i32,
        "minWireVersion": 0,
        "maxWireVersion": 21,
        "readOnly": false,
        "ok": 1.0,
    };
    if name != "hello" {
        d.insert("ismaster", true);
    }
    if cmd.get_str("saslSupportedMechs").is_ok() && !matches!(server.auth, AuthMode::Any) {
        d.insert("saslSupportedMechs", vec!["SCRAM-SHA-1", "SCRAM-SHA-256"]);
    }
    d
}

fn sasl_payload(cmd: &Document) -> Vec<u8> {
    match cmd.get("payload") {
        Some(Bson::Binary(b)) => b.bytes.clone(),
        Some(Bson::String(s)) => s.clone().into_bytes(),
        _ => Vec::new(),
    }
}

fn sasl_reply(payload: Vec<u8>, done: bool) -> Document {
    doc! { "conversationId": 1, "done": done, "payload": Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Generic, bytes: payload }), "ok": 1.0 }
}

fn auth_failed(server: &Server, conn: &Conn, user: &str, why: &str) -> Document {
    server.traffic.query(conn.id, &format!("db.auth(\"{user}\")"), Instant::now(), 0, Some(why.to_owned()));
    MongoError::new(18, "AuthenticationFailed", "Authentication failed.").to_doc()
}

fn sasl_start(server: &Server, conn: &mut Conn, cmd: &Document, _db: &str) -> Document {
    let mech = cmd.get_str("mechanism").unwrap_or("");
    let Some(m) = scram::Mechanism::parse(mech) else {
        return MongoError::new(334, "MechanismUnavailable", format!("Received authentication for mechanism {mech} which is not enabled"))
            .to_doc();
    };
    if matches!(server.auth, AuthMode::Any) {
        // SCRAM proves the password to both sides, so a mock that accepts anything cannot answer it.
        server.traffic.query(conn.id, "db.auth(…)", Instant::now(), 0, Some("this mock does not use passwords".into()));
        return MongoError::new(
            18,
            "AuthenticationFailed",
            "This TwinBase mock accepts connections without credentials. Remove the user name and password from the connection string, or set a user and password on the mock.",
        )
        .to_doc();
    }
    match scram::start(m, &sasl_payload(cmd)) {
        Ok((conv, server_first)) => {
            conn.sasl = Some(conv);
            sasl_reply(server_first, false)
        }
        Err(e) => MongoError::new(17, "ProtocolError", e).to_doc(),
    }
}

fn sasl_continue(server: &Server, conn: &mut Conn, cmd: &Document, _db: &str) -> Document {
    let Some(conv) = conn.sasl.take() else {
        // The empty final round of clients that do not skip it.
        if conn.user.is_some() {
            return sasl_reply(Vec::new(), true);
        }
        return MongoError::new(17, "ProtocolError", "No SASL session state found").to_doc();
    };
    let AuthMode::Password { user, password } = &server.auth else {
        return MongoError::new(18, "AuthenticationFailed", "Authentication failed.").to_doc();
    };
    if conv.user() != user {
        return auth_failed(server, conn, conv.user(), "unknown user");
    }
    match conv.finish(&sasl_payload(cmd), password) {
        Ok(server_final) => {
            conn.user = Some(user.clone());
            server.traffic.query(conn.id, &format!("db.auth(\"{user}\")"), Instant::now(), 0, None);
            sasl_reply(server_final, true)
        }
        Err(_) => auth_failed(server, conn, conv.user(), "wrong password"),
    }
}

// ------------------------------------------------------------------ traffic text

fn ext(v: &Bson) -> String {
    let s = v.clone().into_relaxed_extjson().to_string();
    if s.len() > 2000 { format!("{}…", s.chars().take(2000).collect::<String>()) } else { s }
}

fn describe(db: &str, name: &str, cmd: &Document) -> String {
    let coll = cmd.get_str(name).unwrap_or("");
    let g = |k: &str| cmd.get(k).cloned().unwrap_or(Bson::Document(Document::new()));
    match name {
        "find" => {
            let mut s = format!("db.{coll}.find({}", ext(&g("filter")));
            if let Ok(p) = cmd.get_document("projection") {
                s.push_str(&format!(", {}", ext(&Bson::Document(p.clone()))));
            }
            s.push(')');
            if let Ok(sort) = cmd.get_document("sort") {
                s.push_str(&format!(".sort({})", ext(&Bson::Document(sort.clone()))));
            }
            if let Some(n) = cmd.get("skip").and_then(as_f64).filter(|n| *n > 0.0) {
                s.push_str(&format!(".skip({n})"));
            }
            if let Some(n) = cmd.get("limit").and_then(as_f64).filter(|n| *n != 0.0) {
                s.push_str(&format!(".limit({n})"));
            }
            s
        }
        "insert" => {
            let docs = cmd.get_array("documents").map(|a| a.as_slice()).unwrap_or(&[]);
            match docs {
                [one] => format!("db.{coll}.insertOne({})", ext(one)),
                [first, rest @ ..] => format!("db.{coll}.insertMany([{}, … {} more])", ext(first), rest.len()),
                [] => format!("db.{coll}.insertMany([])"),
            }
        }
        "update" => cmd
            .get_array("updates")
            .map(|a| {
                a.iter()
                    .filter_map(|u| u.as_document())
                    .map(|u| {
                        let replace = matches!(u.get("u"), Some(Bson::Document(d)) if !query::is_update_operators(d));
                        let m = if replace {
                            "replaceOne"
                        } else if u.get_bool("multi").unwrap_or(false) {
                            "updateMany"
                        } else {
                            "updateOne"
                        };
                        let upsert = if u.get_bool("upsert").unwrap_or(false) { ", {\"upsert\":true}" } else { "" };
                        format!(
                            "db.{coll}.{m}({}, {}{upsert})",
                            ext(&u.get("q").cloned().unwrap_or(Bson::Null)),
                            ext(&u.get("u").cloned().unwrap_or(Bson::Null))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default(),
        "delete" => cmd
            .get_array("deletes")
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_document())
                    .map(|d| {
                        let m = if d.get("limit").and_then(as_f64) == Some(1.0) { "deleteOne" } else { "deleteMany" };
                        format!("db.{coll}.{m}({})", ext(&d.get("q").cloned().unwrap_or(Bson::Null)))
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default(),
        "aggregate" => {
            let target = if coll.is_empty() { "db".to_owned() } else { format!("db.{coll}") };
            format!("{target}.aggregate({})", ext(&g("pipeline")))
        }
        "count" => format!("db.{coll}.countDocuments({})", ext(&g("query"))),
        "distinct" => format!("db.{coll}.distinct(\"{}\", {})", cmd.get_str("key").unwrap_or(""), ext(&g("query"))),
        "findAndModify" | "findandmodify" => {
            if cmd.get_bool("remove").unwrap_or(false) {
                format!("db.{coll}.findOneAndDelete({})", ext(&g("query")))
            } else {
                format!("db.{coll}.findOneAndUpdate({}, {})", ext(&g("query")), ext(&g("update")))
            }
        }
        "getMore" => format!("db.{}.getMore()", cmd.get_str("collection").unwrap_or("")),
        "createIndexes" => format!("db.{coll}.createIndexes({})", ext(&g("indexes"))),
        "drop" => format!("db.{coll}.drop()"),
        "dropDatabase" => format!("use {db}; db.dropDatabase()"),
        _ => {
            let mut c = cmd.clone();
            for k in ["$db", "lsid", "$clusterTime", "$readPreference", "apiVersion", "apiStrict", "apiDeprecationErrors"] {
                c.remove(k);
            }
            format!("db.runCommand({})", ext(&Bson::Document(c)))
        }
    }
}

// ------------------------------------------------------------------ commands

/// An integer argument; 64-bit values (cursor ids) stay exact.
fn int(cmd: &Document, key: &str) -> Option<i64> {
    match cmd.get(key)? {
        Bson::Int32(i) => Some(*i as i64),
        Bson::Int64(i) => Some(*i),
        other => as_f64(other).map(|f| f as i64),
    }
}

fn opt_doc<'a>(cmd: &'a Document, key: &str) -> MResult<Option<&'a Document>> {
    match cmd.get(key) {
        None | Some(Bson::Null) => Ok(None),
        Some(Bson::Document(d)) => Ok(Some(d)),
        Some(other) => Err(MongoError::new(
            14,
            "TypeMismatch",
            format!("BSON field '{key}' is the wrong type '{}', expected type 'object'", query::bson_type_name(other)),
        )),
    }
}

fn coll_of<'a>(cmd: &'a Document, name: &str) -> MResult<&'a str> {
    match cmd.get(name) {
        Some(Bson::String(s)) if !s.is_empty() => Ok(s),
        _ => Err(MongoError::new(
            73,
            "InvalidNamespace",
            format!("collection name has invalid type {}", cmd.get(name).map(query::bson_type_name).unwrap_or("missing")),
        )),
    }
}

fn docs_array(cmd: &Document, key: &str) -> MResult<Vec<Document>> {
    match cmd.get(key) {
        Some(Bson::Array(a)) => a
            .iter()
            .map(|v| v.as_document().cloned().ok_or_else(|| MongoError::new(14, "TypeMismatch", format!("{key} entries must be objects"))))
            .collect(),
        None => Ok(vec![]),
        _ => Err(MongoError::new(14, "TypeMismatch", format!("{key} must be an array"))),
    }
}

/// Take one batch from the front of `docs`, bounded by count and bytes.
fn take_batch(docs: &mut VecDeque<Document>, size: usize) -> Vec<Bson> {
    let mut out = Vec::new();
    let mut bytes = 0;
    while out.len() < size {
        let Some(d) = docs.front() else { break };
        let n = d.to_vec().map(|v| v.len()).unwrap_or(0);
        if !out.is_empty() && bytes + n > MAX_BATCH_BYTES {
            break;
        }
        bytes += n;
        out.push(Bson::Document(docs.pop_front().unwrap()));
    }
    out
}

impl Server {
    /// Reply with a cursor: the first batch now, the rest kept for `getMore`.
    fn cursor_reply(&self, ns: String, docs: Vec<Document>, batch: Option<i64>, single: bool) -> (Document, u64) {
        let total = docs.len() as u64;
        let mut docs: VecDeque<Document> = docs.into();
        let size = batch.filter(|b| *b >= 0).map(|b| b as usize).unwrap_or(DEFAULT_BATCH);
        let first = take_batch(&mut docs, if batch == Some(0) { 0 } else { size });
        let id = if docs.is_empty() || single {
            0
        } else {
            let mut cursors = self.cursors.lock();
            if cursors.len() >= MAX_CURSORS
                && let Some(oldest) = cursors.iter().min_by_key(|(_, c)| c.touched).map(|(k, _)| *k)
            {
                cursors.remove(&oldest);
            }
            let mut id: i64 = rand::random::<i64>().abs();
            while id == 0 || cursors.contains_key(&id) {
                id = rand::random::<i64>().abs();
            }
            cursors.insert(id, Cursor { ns: ns.clone(), docs, touched: Instant::now() });
            id
        };
        (doc! { "cursor": { "firstBatch": first, "id": id, "ns": ns }, "ok": 1.0 }, total)
    }
}

fn run(server: &Server, db: &str, name: &str, cmd: &Document, peer: &str, user: Option<&str>) -> MResult<(Document, u64)> {
    let store = &server.store;
    let ok = |d: Document| -> MResult<(Document, u64)> {
        let mut d = d;
        d.insert("ok", 1.0);
        Ok((d, 0))
    };
    match name {
        "find" => {
            let coll = coll_of(cmd, name)?;
            let filter = opt_doc(cmd, "filter")?.cloned().unwrap_or_default();
            let limit = int(cmd, "limit").unwrap_or(0);
            let docs = store.find(
                db,
                coll,
                &filter,
                opt_doc(cmd, "sort")?,
                int(cmd, "skip").unwrap_or(0).max(0) as usize,
                (limit != 0).then_some(limit.unsigned_abs() as usize),
                opt_doc(cmd, "projection")?,
            )?;
            let single = cmd.get_bool("singleBatch").unwrap_or(false) || limit < 0;
            Ok(server.cursor_reply(format!("{db}.{coll}"), docs, int(cmd, "batchSize"), single))
        }
        "getMore" => {
            let id = int(cmd, "getMore").unwrap_or(0);
            let size = int(cmd, "batchSize").filter(|b| *b > 0).map(|b| b as usize).unwrap_or(usize::MAX);
            let mut cursors = server.cursors.lock();
            let c = cursors.get_mut(&id).ok_or_else(|| MongoError::new(43, "CursorNotFound", format!("cursor id {id} not found")))?;
            c.touched = Instant::now();
            let batch = take_batch(&mut c.docs, size);
            let ns = c.ns.clone();
            let rows = batch.len() as u64;
            let next_id = if c.docs.is_empty() {
                cursors.remove(&id);
                0
            } else {
                id
            };
            Ok((doc! { "cursor": { "nextBatch": batch, "id": next_id, "ns": ns }, "ok": 1.0 }, rows))
        }
        "killCursors" => {
            let ids: Vec<i64> =
                cmd.get_array("cursors").map(|a| a.iter().filter_map(|v| int(&doc! { "v": v.clone() }, "v")).collect()).unwrap_or_default();
            let mut cursors = server.cursors.lock();
            let (mut killed, mut missing) = (Vec::new(), Vec::new());
            for id in ids {
                if cursors.remove(&id).is_some() { killed.push(id) } else { missing.push(id) }
            }
            ok(doc! { "cursorsKilled": killed, "cursorsNotFound": missing, "cursorsAlive": [], "cursorsUnknown": [] })
        }
        "insert" => {
            let coll = coll_of(cmd, name)?;
            let docs = docs_array(cmd, "documents")?;
            let (n, errors) = store.insert(db, coll, docs, cmd.get_bool("ordered").unwrap_or(true))?;
            let mut d = doc! { "n": n as i32 };
            if !errors.is_empty() {
                d.insert("writeErrors", errors.iter().map(|(i, e)| Bson::Document(e.write_error(*i))).collect::<Vec<_>>());
            }
            d.insert("ok", 1.0);
            Ok((d, n))
        }
        "update" => {
            let coll = coll_of(cmd, name)?;
            let ordered = cmd.get_bool("ordered").unwrap_or(true);
            let (mut n, mut modified) = (0u64, 0u64);
            let mut upserted = Vec::new();
            let mut errors = Vec::new();
            for (i, u) in docs_array(cmd, "updates")?.iter().enumerate() {
                let filter = opt_doc(u, "q")?.cloned().unwrap_or_default();
                let update = u.get("u").cloned().ok_or_else(|| MongoError::failed_to_parse("update entry needs 'u'"))?;
                let filters = docs_array(u, "arrayFilters")?;
                let spec = UpdateSpec {
                    filter: &filter,
                    update: &update,
                    upsert: u.get_bool("upsert").unwrap_or(false),
                    multi: u.get_bool("multi").unwrap_or(false),
                    array_filters: &filters,
                    sort: opt_doc(u, "sort")?,
                };
                if spec.multi && matches!(&update, Bson::Document(d) if !query::is_update_operators(d)) {
                    errors.push((i, MongoError::failed_to_parse("multi update is not supported for replacement-style update")));
                    if ordered {
                        break;
                    }
                    continue;
                }
                match store.update(db, coll, &spec) {
                    Ok(o) => {
                        n += o.matched;
                        modified += o.modified;
                        if let Some(id) = o.upserted {
                            n += 1;
                            upserted.push(Bson::Document(doc! { "index": i as i32, "_id": id }));
                        }
                    }
                    Err(e) => {
                        errors.push((i, e));
                        if ordered {
                            break;
                        }
                    }
                }
            }
            let mut d = doc! { "n": n as i32, "nModified": modified as i32 };
            if !upserted.is_empty() {
                d.insert("upserted", upserted);
            }
            if !errors.is_empty() {
                d.insert("writeErrors", errors.iter().map(|(i, e)| Bson::Document(e.write_error(*i))).collect::<Vec<_>>());
            }
            d.insert("ok", 1.0);
            Ok((d, n))
        }
        "delete" => {
            let coll = coll_of(cmd, name)?;
            let mut n = 0;
            for d in docs_array(cmd, "deletes")? {
                let filter = opt_doc(&d, "q")?.cloned().unwrap_or_default();
                n += store.delete(db, coll, &filter, int(&d, "limit") == Some(1))?;
            }
            Ok((doc! { "n": n as i32, "ok": 1.0 }, n))
        }
        "findAndModify" | "findandmodify" => {
            let coll = coll_of(cmd, name)?;
            let filter = opt_doc(cmd, "query")?.cloned().unwrap_or_default();
            let filters = docs_array(cmd, "arrayFilters")?;
            let remove = cmd.get_bool("remove").unwrap_or(false);
            let (value, outcome) = store.find_and_modify(
                db,
                coll,
                &filter,
                opt_doc(cmd, "sort")?,
                remove,
                cmd.get("update"),
                cmd.get_bool("upsert").unwrap_or(false),
                cmd.get_bool("new").unwrap_or(false),
                opt_doc(cmd, "fields")?,
                &filters,
            )?;
            let mut last = doc! { "n": (outcome.matched + outcome.upserted.is_some() as u64) as i32 };
            if !remove {
                last.insert("updatedExisting", outcome.matched > 0);
            }
            if let Some(id) = &outcome.upserted {
                last.insert("upserted", id.clone());
            }
            let rows = value.is_some() as u64;
            Ok((doc! { "lastErrorObject": last, "value": value.map(Bson::Document).unwrap_or(Bson::Null), "ok": 1.0 }, rows))
        }
        "aggregate" => {
            let coll = match cmd.get("aggregate") {
                Some(Bson::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let pipeline = docs_array(cmd, "pipeline")?;
            if cmd.get_bool("explain").unwrap_or(false) {
                return ok(
                    doc! { "stages": pipeline.iter().map(|s| Bson::Document(s.clone())).collect::<Vec<_>>(), "serverInfo": { "host": "twinbase", "version": VERSION } },
                );
            }
            if let Some(first) = pipeline.first().and_then(|s| s.keys().next())
                && matches!(
                    first.as_str(),
                    "$collStats" | "$indexStats" | "$currentOp" | "$listSessions" | "$listLocalSessions" | "$changeStream"
                )
            {
                if first == "$collStats" {
                    let n = coll.map(|c| store.count(db, c, &Document::new())).transpose()?.unwrap_or(0);
                    return Ok(server.cursor_reply(
                        format!("{db}.{}", coll.unwrap_or("$cmd.aggregate")),
                        vec![
                            doc! { "ns": format!("{db}.{}", coll.unwrap_or("")), "count": n as i64, "storageStats": { "count": n as i64 } },
                        ],
                        None,
                        false,
                    ));
                }
                return Err(MongoError::new(40324, "Location40324", format!("{first} is not supported by the TwinBase mock")));
            }
            let docs = store.aggregate(db, coll, &pipeline)?;
            let batch = cmd.get_document("cursor").ok().and_then(|c| int(c, "batchSize"));
            Ok(server.cursor_reply(format!("{db}.{}", coll.unwrap_or("$cmd.aggregate")), docs, batch, false))
        }
        "count" => {
            let coll = coll_of(cmd, name)?;
            let filter = opt_doc(cmd, "query")?.cloned().unwrap_or_default();
            let mut n = store.count(db, coll, &filter)?;
            n = n.saturating_sub(int(cmd, "skip").unwrap_or(0).max(0) as u64);
            if let Some(l) = int(cmd, "limit").filter(|l| *l != 0) {
                n = n.min(l.unsigned_abs());
            }
            Ok((doc! { "n": n as i64, "ok": 1.0 }, n))
        }
        "distinct" => {
            let coll = coll_of(cmd, name)?;
            let key = cmd.get_str("key").map_err(|_| MongoError::failed_to_parse("distinct needs a 'key'"))?;
            let filter = opt_doc(cmd, "query")?.cloned().unwrap_or_default();
            let values = store.distinct(db, coll, key, &filter)?;
            let rows = values.len() as u64;
            Ok((doc! { "values": values, "ok": 1.0 }, rows))
        }
        "create" => {
            let coll = coll_of(cmd, name)?;
            let mut options = cmd.clone();
            for k in [
                "create",
                "$db",
                "lsid",
                "$clusterTime",
                "$readPreference",
                "writeConcern",
                "apiVersion",
                "apiStrict",
                "apiDeprecationErrors",
                "comment",
            ] {
                options.remove(k);
            }
            store.create_collection(db, coll, options)?;
            ok(Document::new())
        }
        "drop" => {
            let coll = coll_of(cmd, name)?;
            let n = store.list_indexes(db, coll).map(|i| i.len()).unwrap_or(0);
            store.drop_collection(db, coll);
            ok(doc! { "nIndexesWas": n as i32, "ns": format!("{db}.{coll}") })
        }
        "dropDatabase" => {
            store.drop_database(db);
            ok(doc! { "dropped": db })
        }
        "renameCollection" => {
            let from = cmd.get_str("renameCollection").map_err(|_| MongoError::failed_to_parse("renameCollection needs a namespace"))?;
            let to = cmd.get_str("to").map_err(|_| MongoError::failed_to_parse("renameCollection needs 'to'"))?;
            let split = |ns: &str| {
                ns.split_once('.')
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                    .ok_or_else(|| MongoError::new(73, "InvalidNamespace", format!("Invalid namespace: {ns}")))
            };
            let (f, t) = (split(from)?, split(to)?);
            store.rename_collection((&f.0, &f.1), (&t.0, &t.1), cmd.get_bool("dropTarget").unwrap_or(false))?;
            ok(Document::new())
        }
        "listCollections" => {
            let filter = opt_doc(cmd, "filter")?.cloned().unwrap_or_default();
            let name_only = cmd.get_bool("nameOnly").unwrap_or(false);
            let mut out = Vec::new();
            for (n, options) in store.list_collections(db) {
                let full = doc! { "name": n.clone(), "type": "collection", "options": options, "info": { "readOnly": false }, "idIndex": { "v": 2, "key": { "_id": 1 }, "name": "_id_" } };
                if query::matches(&full, &filter)? {
                    out.push(if name_only {
                        doc! { "name": n, "type": "collection" }
                    } else {
                        full
                    });
                }
            }
            let batch = cmd.get_document("cursor").ok().and_then(|c| int(c, "batchSize"));
            Ok(server.cursor_reply(format!("{db}.$cmd.listCollections"), out, batch, false))
        }
        "listDatabases" => {
            let filter = opt_doc(cmd, "filter")?.cloned().unwrap_or_default();
            let name_only = cmd.get_bool("nameOnly").unwrap_or(false);
            let mut total = 0i64;
            let mut dbs = Vec::new();
            for (n, size, empty) in store.list_databases() {
                let d = if name_only {
                    doc! { "name": n }
                } else {
                    doc! { "name": n, "sizeOnDisk": size as i64, "empty": empty }
                };
                if query::matches(&d, &filter)? {
                    total += size as i64;
                    dbs.push(Bson::Document(d));
                }
            }
            ok(doc! { "databases": dbs, "totalSize": total, "totalSizeMb": total / (1024 * 1024) })
        }
        "createIndexes" => {
            let coll = coll_of(cmd, name)?;
            let existed = store.list_indexes(db, coll);
            let before = existed.as_ref().map(|i| i.len()).unwrap_or(1);
            for spec in docs_array(cmd, "indexes")? {
                store.create_index(db, coll, &spec)?;
            }
            let after = store.list_indexes(db, coll).map(|i| i.len()).unwrap_or(1);
            ok(
                doc! { "createdCollectionAutomatically": existed.is_none(), "numIndexesBefore": before as i32, "numIndexesAfter": after as i32 },
            )
        }
        "listIndexes" => {
            let coll = coll_of(cmd, name)?;
            let indexes = store
                .list_indexes(db, coll)
                .ok_or_else(|| MongoError::new(26, "NamespaceNotFound", format!("ns does not exist: {db}.{coll}")))?;
            Ok(server.cursor_reply(format!("{db}.{coll}"), indexes, None, false))
        }
        "dropIndexes" | "deleteIndexes" => {
            let coll = coll_of(cmd, name)?;
            let which = cmd.get("index").cloned().unwrap_or(Bson::String("*".into()));
            let was = store.drop_index(db, coll, &which)?;
            ok(doc! { "nIndexesWas": was as i32 })
        }
        "collStats" | "dataSize" => {
            let coll = coll_of(cmd, name)?;
            let data = store.read();
            let c = data.collection(db, coll);
            let count = c.map(|c| c.docs.len()).unwrap_or(0);
            let size: usize = c.map(|c| c.docs.values().map(|d| d.to_vec().map(|v| v.len()).unwrap_or(0)).sum()).unwrap_or(0);
            let nindexes = c.map(|c| c.indexes.len()).unwrap_or(0);
            ok(
                doc! { "ns": format!("{db}.{coll}"), "count": count as i64, "size": size as i64, "avgObjSize": size.checked_div(count).unwrap_or(0) as i64, "storageSize": size as i64, "nindexes": nindexes as i32, "totalIndexSize": 0, "indexSizes": {} },
            )
        }
        "dbStats" => {
            let data = store.read();
            let colls = data.databases.get(db).map(|d| d.collections.len()).unwrap_or(0);
            let (objects, size, indexes) = data
                .databases
                .get(db)
                .map(|d| {
                    d.collections.values().fold((0usize, 0usize, 0usize), |(o, s, i), c| {
                        (
                            o + c.docs.len(),
                            s + c.docs.values().map(|d| d.to_vec().map(|v| v.len()).unwrap_or(0)).sum::<usize>(),
                            i + c.indexes.len(),
                        )
                    })
                })
                .unwrap_or_default();
            ok(
                doc! { "db": db, "collections": colls as i32, "views": 0, "objects": objects as i64, "avgObjSize": size.checked_div(objects).unwrap_or(0) as f64, "dataSize": size as f64, "storageSize": size as f64, "indexes": indexes as i32, "indexSize": 0.0, "totalSize": size as f64, "scaleFactor": 1 },
            )
        }
        "explain" => {
            let inner = cmd.get_document("explain").map_err(|_| MongoError::failed_to_parse("explain needs a command object"))?;
            let inner_name = inner.keys().next().cloned().unwrap_or_default();
            let coll = inner.get_str(&inner_name).unwrap_or("");
            let filter = inner.get_document("filter").or_else(|_| inner.get_document("query")).cloned().unwrap_or_default();
            let started = Instant::now();
            let total = store.read().collection(db, coll).map(|c| c.docs.len()).unwrap_or(0);
            let matched = store.count(db, coll, &filter)?;
            let id_lookup = filter.get("_id").is_some_and(|v| !matches!(v, Bson::Document(_) | Bson::RegularExpression(_)));
            let stage = if id_lookup { "IDHACK" } else { "COLLSCAN" };
            ok(doc! {
                "queryPlanner": { "namespace": format!("{db}.{coll}"), "parsedQuery": filter.clone(), "winningPlan": { "stage": stage, "filter": filter, "direction": "forward" }, "rejectedPlans": [] },
                "executionStats": { "executionSuccess": true, "nReturned": matched as i64, "executionTimeMillis": started.elapsed().as_millis() as i64, "totalKeysExamined": if id_lookup { 1 } else { 0 }, "totalDocsExamined": if id_lookup { matched.min(1) as i64 } else { total as i64 }, "executionStages": { "stage": stage, "nReturned": matched as i64, "docsExamined": if id_lookup { matched.min(1) as i64 } else { total as i64 } } },
                "command": inner.clone(),
                "serverInfo": { "host": "twinbase", "port": server.port as i32, "version": VERSION },
            })
        }
        "ping" => ok(Document::new()),
        "buildInfo" | "buildinfo" => ok(doc! {
            "version": VERSION, "gitVersion": "twinbase-mock", "versionArray": [7, 0, 12, 0], "modules": [], "allocator": "system",
            "javascriptEngine": "none", "sysInfo": "deprecated", "bits": 64, "debug": false, "maxBsonObjectSize": store::MAX_DOC_SIZE as i32,
            "storageEngines": ["twinbase"], "openssl": { "running": "disabled" },
        }),
        "getParameter" => ok(doc! { "featureCompatibilityVersion": { "version": "7.0" } }),
        "getLog" => ok(doc! { "totalLinesWritten": 0, "log": [] }),
        "hostInfo" => ok(
            doc! { "system": { "hostname": format!("twinbase:{}", server.port), "numCores": 1, "memSizeMB": 1024 }, "os": { "type": std::env::consts::OS, "name": "TwinBase mock" }, "extra": {} },
        ),
        "getCmdLineOpts" => ok(doc! { "argv": ["twinbase"], "parsed": {} }),
        "connectionStatus" => {
            let users: Vec<Bson> = user.map(|u| Bson::Document(doc! { "user": u, "db": "admin" })).into_iter().collect();
            ok(doc! { "authInfo": { "authenticatedUsers": users, "authenticatedUserRoles": [] } })
        }
        "whatsmyuri" => ok(doc! { "you": peer }),
        "serverStatus" => ok(doc! {
            "host": format!("twinbase:{}", server.port), "version": VERSION, "process": "mongod", "pid": std::process::id() as i64,
            "uptime": server.started.elapsed().as_secs() as f64, "localTime": bson::DateTime::now(),
            "connections": { "current": server.connections.load(Ordering::Relaxed) as i32, "available": 1000 },
        }),
        "endSessions" | "killSessions" | "refreshSessions" | "killAllSessions" => ok(Document::new()),
        "startSession" => ok(
            doc! { "id": { "id": bson::Binary { subtype: bson::spec::BinarySubtype::Uuid, bytes: uuid::Uuid::new_v4().as_bytes().to_vec() } }, "timeoutMinutes": 30 },
        ),
        "collMod" | "validate" | "compact" | "fsync" | "reIndex" | "setFeatureCompatibilityVersion" => ok(Document::new()),
        other => Err(MongoError::new(59, "CommandNotFound", format!("no such command: '{other}'"))),
    }
}

/// Apply a seed document (`{collections: {...}}`, Extended JSON) to a database.
pub fn apply_seed(store: &MongoStore, db: &str, seed: &str) -> Result<usize, String> {
    let value: serde_json::Value = serde_json::from_str(seed).map_err(|e| format!("seed is not valid JSON: {e}"))?;
    let bson = Bson::try_from(value).map_err(|e| format!("seed is not valid Extended JSON: {e}"))?;
    let Bson::Document(d) = bson else { return Err("seed must be a JSON object".into()) };
    store.seed(db, &d).map_err(|e| e.message)
}

/// A database as a seed (canonical Extended JSON, pretty printed).
pub fn export_seed(store: &MongoStore, db: &str) -> String {
    let v = Bson::Document(store.export(db)).into_canonical_extjson();
    serde_json::to_string_pretty(&v).unwrap_or_default()
}
