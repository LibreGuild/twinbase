//! Redis mock server (RESP2/RESP3) over an in-memory store with snapshot persistence.

mod resp;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::BytesMut;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub use resp::Reply;
pub use store::RedisStore;

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};
use store::glob_match;

#[derive(Clone, Debug)]
struct Published {
    channel: Vec<u8>,
    message: Vec<u8>,
}

#[derive(Default)]
struct Subscriptions {
    channels: HashSet<Vec<u8>>,
    patterns: HashSet<Vec<u8>>,
}

struct Server {
    store: Arc<RedisStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    bus: broadcast::Sender<Published>,
    subs: Mutex<HashMap<u64, Subscriptions>>,
    clients: AtomicU64,
    started: Instant,
    port: u16,
}

impl Server {
    fn receivers(&self, channel: &[u8]) -> i64 {
        self.subs
            .lock()
            .values()
            .map(|s| s.channels.contains(channel) as i64 + s.patterns.iter().filter(|p| glob_match(p, channel)).count() as i64)
            .sum()
    }

    fn info(&self, section: Option<&str>) -> String {
        let keys = self.store.key_count();
        let sections = [
            (
                "server",
                format!(
                    "redis_version:7.2.4\r\nredis_mode:standalone\r\nos:TwinBase mock\r\narch_bits:64\r\ntcp_port:{}\r\nuptime_in_seconds:{}\r\nexecutable:twinbase\r\n",
                    self.port,
                    self.started.elapsed().as_secs()
                ),
            ),
            ("clients", format!("connected_clients:{}\r\nblocked_clients:0\r\n", self.clients.load(Ordering::Relaxed))),
            ("memory", "used_memory:1048576\r\nused_memory_human:1.00M\r\nmaxmemory:0\r\nmaxmemory_policy:noeviction\r\n".to_owned()),
            ("persistence", "loading:0\r\nrdb_changes_since_last_save:0\r\naof_enabled:0\r\n".to_owned()),
            ("stats", "total_connections_received:0\r\ntotal_commands_processed:0\r\n".to_owned()),
            ("replication", "role:master\r\nconnected_slaves:0\r\n".to_owned()),
            ("keyspace", if keys > 0 { format!("db0:keys={keys},expires=0,avg_ttl=0\r\n") } else { String::new() }),
        ];
        let mut out = String::new();
        for (name, body) in sections {
            if section.is_none_or(|s| s.eq_ignore_ascii_case(name) || s.eq_ignore_ascii_case("all") || s.eq_ignore_ascii_case("everything"))
            {
                let mut title = name.to_owned();
                title[..1].make_ascii_uppercase();
                out.push_str(&format!("# {title}\r\n{body}\r\n"));
            }
        }
        out
    }
}

struct Conn {
    id: u64,
    db: usize,
    resp3: bool,
    authenticated: bool,
    name: Option<Vec<u8>>,
    multi: Option<Vec<Vec<Vec<u8>>>>,
}

fn upper(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_ascii_uppercase()
}

fn push(kind: &str, channel: &[u8], extra: Reply) -> Reply {
    Reply::Push(vec![Reply::bulk(kind), Reply::bulk(channel), extra])
}

impl Conn {
    /// Handle one command. Returns replies to send and whether to close the connection.
    fn handle(&mut self, server: &Server, args: Vec<Vec<u8>>) -> (Vec<Reply>, bool) {
        let name = upper(&args[0]);
        let in_sub = server.subs.lock().get(&self.id).is_some_and(|s| !s.channels.is_empty() || !s.patterns.is_empty());
        if !self.authenticated && !matches!(name.as_str(), "AUTH" | "HELLO" | "QUIT" | "PING") {
            return (vec![Reply::err("NOAUTH Authentication required.")], false);
        }
        if in_sub
            && !self.resp3
            && !matches!(name.as_str(), "SUBSCRIBE" | "UNSUBSCRIBE" | "PSUBSCRIBE" | "PUNSUBSCRIBE" | "PING" | "QUIT" | "RESET")
        {
            return (
                vec![Reply::err(format!(
                    "ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context",
                    name.to_ascii_lowercase()
                ))],
                false,
            );
        }
        if let Some(queue) = &mut self.multi
            && !matches!(name.as_str(), "EXEC" | "DISCARD" | "MULTI" | "WATCH" | "UNWATCH" | "QUIT")
        {
            queue.push(args);
            return (vec![Reply::Simple("QUEUED".into())], false);
        }
        let reply = match name.as_str() {
            "PING" => {
                if in_sub && !self.resp3 {
                    Reply::Array(vec![Reply::bulk("pong"), Reply::bulk(args.get(1).cloned().unwrap_or_default())])
                } else {
                    match args.get(1) {
                        Some(m) => Reply::Bulk(m.clone()),
                        None => Reply::Simple("PONG".into()),
                    }
                }
            }
            "ECHO" => match args.get(1) {
                Some(m) => Reply::Bulk(m.clone()),
                None => Reply::err("ERR wrong number of arguments for 'echo' command"),
            },
            "QUIT" => return (vec![Reply::ok()], true),
            "AUTH" => {
                let (user, pass) = match args.len() {
                    2 => ("default".to_owned(), String::from_utf8_lossy(&args[1]).into_owned()),
                    3 => (String::from_utf8_lossy(&args[1]).into_owned(), String::from_utf8_lossy(&args[2]).into_owned()),
                    _ => return (vec![Reply::err("ERR wrong number of arguments for 'auth' command")], false),
                };
                match self.check_auth(server, &user, &pass) {
                    Ok(()) => Reply::ok(),
                    Err(e) => e,
                }
            }
            "HELLO" => {
                let mut i = 1;
                let mut proto = if self.resp3 { 3 } else { 2 };
                if let Some(v) = args.get(1).and_then(|a| String::from_utf8_lossy(a).parse::<i64>().ok()) {
                    if !(2..=3).contains(&v) {
                        return (vec![Reply::err("NOPROTO unsupported protocol version")], false);
                    }
                    proto = v;
                    i = 2;
                }
                while i < args.len() {
                    match upper(&args[i]).as_str() {
                        "AUTH" if i + 2 < args.len() => {
                            let user = String::from_utf8_lossy(&args[i + 1]).into_owned();
                            let pass = String::from_utf8_lossy(&args[i + 2]).into_owned();
                            if let Err(e) = self.check_auth(server, &user, &pass) {
                                return (vec![e], false);
                            }
                            i += 3;
                        }
                        "SETNAME" if i + 1 < args.len() => {
                            self.name = Some(args[i + 1].clone());
                            i += 2;
                        }
                        _ => return (vec![Reply::err("ERR syntax error")], false),
                    }
                }
                if !self.authenticated {
                    return (
                        vec![Reply::err(
                            "NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time",
                        )],
                        false,
                    );
                }
                self.resp3 = proto == 3;
                Reply::Map(vec![
                    (Reply::bulk("server"), Reply::bulk("redis")),
                    (Reply::bulk("version"), Reply::bulk("7.2.4")),
                    (Reply::bulk("proto"), Reply::Int(proto)),
                    (Reply::bulk("id"), Reply::Int(self.id as i64)),
                    (Reply::bulk("mode"), Reply::bulk("standalone")),
                    (Reply::bulk("role"), Reply::bulk("master")),
                    (Reply::bulk("modules"), Reply::Array(vec![])),
                ])
            }
            "CLIENT" => match args.get(1).map(|a| upper(a)).as_deref() {
                Some("SETNAME") => {
                    self.name = args.get(2).cloned();
                    Reply::ok()
                }
                Some("GETNAME") => Reply::opt_bulk(self.name.clone()),
                Some("ID") => Reply::Int(self.id as i64),
                Some("INFO") | Some("LIST") => Reply::bulk(format!(
                    "id={} addr=127.0.0.1:0 name={} db={} cmd=client\n",
                    self.id,
                    self.name.as_deref().map(String::from_utf8_lossy).unwrap_or_default(),
                    self.db
                )),
                _ => Reply::ok(),
            },
            "COMMAND" => match args.get(1).map(|a| upper(a)).as_deref() {
                Some("COUNT") => Reply::Int(200),
                Some("DOCS") => Reply::Map(vec![]),
                _ => Reply::Array(vec![]),
            },
            "INFO" => Reply::bulk(server.info(args.get(1).map(|a| String::from_utf8_lossy(a).into_owned()).as_deref())),
            "CONFIG" => match args.get(1).map(|a| upper(a)).as_deref() {
                Some("GET") => {
                    let pattern = args.get(2).cloned().unwrap_or_else(|| b"*".to_vec());
                    let known = [
                        ("maxmemory", "0"),
                        ("maxmemory-policy", "noeviction"),
                        ("save", ""),
                        ("appendonly", "no"),
                        ("databases", "16"),
                        ("timeout", "0"),
                        ("notify-keyspace-events", ""),
                    ];
                    Reply::Map(
                        known
                            .iter()
                            .filter(|(k, _)| glob_match(&pattern, k.as_bytes()))
                            .map(|(k, v)| (Reply::bulk(*k), Reply::bulk(*v)))
                            .collect(),
                    )
                }
                _ => Reply::ok(),
            },
            "TIME" => {
                let now = chrono::Utc::now();
                Reply::Array(vec![Reply::bulk(now.timestamp().to_string()), Reply::bulk(now.timestamp_subsec_micros().to_string())])
            }
            "SAVE" | "BGSAVE" => match server.store.save() {
                Ok(()) => Reply::Simple(if name == "SAVE" { "OK".into() } else { "Background saving started".into() }),
                Err(e) => Reply::err(format!("ERR {e}")),
            },
            "LASTSAVE" => Reply::Int(chrono::Utc::now().timestamp()),
            "MULTI" => {
                if self.multi.is_some() {
                    Reply::err("ERR MULTI calls can not be nested")
                } else {
                    self.multi = Some(Vec::new());
                    Reply::ok()
                }
            }
            "EXEC" => match self.multi.take() {
                Some(queue) => Reply::Array(server.store.execute_many(&mut self.db, &queue)),
                None => Reply::err("ERR EXEC without MULTI"),
            },
            "DISCARD" => match self.multi.take() {
                Some(_) => Reply::ok(),
                None => Reply::err("ERR DISCARD without MULTI"),
            },
            "WATCH" | "UNWATCH" | "READONLY" | "READWRITE" => Reply::ok(),
            "WAIT" => Reply::Int(0),
            "RESET" => {
                self.db = 0;
                self.multi = None;
                self.resp3 = false;
                server.subs.lock().remove(&self.id);
                Reply::Simple("RESET".into())
            }
            "MEMORY" => Reply::Int(64),
            "OBJECT" => Reply::bulk("raw"),
            "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "SCRIPT" | "FUNCTION" | "FCALL" => {
                Reply::err("ERR scripting is not supported by the TwinBase Redis mock")
            }
            "PUBLISH" | "SPUBLISH" => {
                if args.len() != 3 {
                    Reply::err("ERR wrong number of arguments for 'publish' command")
                } else {
                    let n = server.receivers(&args[1]);
                    let _ = server.bus.send(Published { channel: args[1].clone(), message: args[2].clone() });
                    Reply::Int(n)
                }
            }
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" => {
                let pattern = name == "PSUBSCRIBE";
                let mut subs = server.subs.lock();
                let entry = subs.entry(self.id).or_default();
                let mut replies = Vec::new();
                for ch in &args[1..] {
                    if pattern {
                        entry.patterns.insert(ch.clone());
                    } else {
                        entry.channels.insert(ch.clone());
                    }
                    let count = (entry.channels.len() + entry.patterns.len()) as i64;
                    replies.push(push(if pattern { "psubscribe" } else { "subscribe" }, ch, Reply::Int(count)));
                }
                return (replies, false);
            }
            "UNSUBSCRIBE" | "PUNSUBSCRIBE" | "SUNSUBSCRIBE" => {
                let pattern = name == "PUNSUBSCRIBE";
                let mut subs = server.subs.lock();
                let entry = subs.entry(self.id).or_default();
                let targets: Vec<Vec<u8>> = if args.len() > 1 {
                    args[1..].to_vec()
                } else if pattern {
                    entry.patterns.iter().cloned().collect()
                } else {
                    entry.channels.iter().cloned().collect()
                };
                let mut replies = Vec::new();
                for ch in targets {
                    if pattern {
                        entry.patterns.remove(&ch);
                    } else {
                        entry.channels.remove(&ch);
                    }
                    let count = (entry.channels.len() + entry.patterns.len()) as i64;
                    replies.push(push(if pattern { "punsubscribe" } else { "unsubscribe" }, &ch, Reply::Int(count)));
                }
                if replies.is_empty() {
                    replies.push(Reply::Push(vec![
                        Reply::bulk(if pattern { "punsubscribe" } else { "unsubscribe" }),
                        Reply::Null,
                        Reply::Int(0),
                    ]));
                }
                return (replies, false);
            }
            "PUBSUB" => match args.get(1).map(|a| upper(a)).as_deref() {
                Some("CHANNELS") => {
                    let mut all: HashSet<Vec<u8>> = HashSet::new();
                    for s in server.subs.lock().values() {
                        all.extend(s.channels.iter().cloned());
                    }
                    let pattern = args.get(2).cloned();
                    let mut v: Vec<Vec<u8>> = all.into_iter().filter(|c| pattern.as_ref().is_none_or(|p| glob_match(p, c))).collect();
                    v.sort();
                    Reply::bulks(v)
                }
                Some("NUMSUB") => Reply::Array(
                    args[2..]
                        .iter()
                        .flat_map(|c| {
                            let n = server.subs.lock().values().filter(|s| s.channels.contains(c)).count() as i64;
                            [Reply::Bulk(c.clone()), Reply::Int(n)]
                        })
                        .collect(),
                ),
                Some("NUMPAT") => Reply::Int(server.subs.lock().values().map(|s| s.patterns.len() as i64).sum()),
                _ => Reply::err("ERR unknown PUBSUB subcommand"),
            },
            _ => server.store.execute(&mut self.db, &args),
        };
        (vec![reply], false)
    }

    fn check_auth(&mut self, server: &Server, user: &str, pass: &str) -> Result<(), Reply> {
        match &server.auth {
            AuthMode::Any => {
                self.authenticated = true;
                Ok(())
            }
            AuthMode::Password { user: u, password } => {
                if (user == u || user == "default") && pass == password {
                    self.authenticated = true;
                    Ok(())
                } else {
                    Err(Reply::err("WRONGPASS invalid username-password pair or user is disabled."))
                }
            }
        }
    }
}

async fn handle_connection(server: Arc<Server>, mut socket: TcpStream, cancel: CancellationToken) {
    let id = server.traffic.new_connection_id();
    server.traffic.record(
        id,
        TrafficKind::Connect { peer: socket.peer_addr().map(|a| a.to_string()).unwrap_or_default(), user: None, database: None },
    );
    server.clients.fetch_add(1, Ordering::Relaxed);
    let mut conn = Conn { id, db: 0, resp3: false, authenticated: matches!(server.auth, AuthMode::Any), name: None, multi: None };
    let mut rx = server.bus.subscribe();
    let mut buf = BytesMut::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    'conn: loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            n = socket.read_buf(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                loop {
                    let cmd = match resp::parse_command(&mut buf) {
                        Ok(Some(c)) => c,
                        Ok(None) => break,
                        Err(e) => {
                            Reply::err(format!("ERR {e}")).encode(&mut out, conn.resp3);
                            let _ = socket.write_all(&out).await;
                            break 'conn;
                        }
                    };
                    if cmd.is_empty() {
                        continue;
                    }
                    let started = Instant::now();
                    let text: String = cmd.iter().map(|a| {
                        let s = String::from_utf8_lossy(a);
                        if s.contains(' ') { format!("\"{s}\"") } else { s.into_owned() }
                    }).collect::<Vec<_>>().join(" ");
                    match server.faults.before(&text).await {
                        Decision::Proceed => {}
                        Decision::Fail { code, message } => {
                            server.traffic.fault(id, &text, &message);
                            Reply::err(format!("{} {message}", code.unwrap_or_else(|| "ERR".into()))).encode(&mut out, conn.resp3);
                            continue;
                        }
                        Decision::Drop => {
                            server.traffic.fault(id, &text, "connection dropped");
                            break 'conn;
                        }
                    }
                    let (replies, close) = conn.handle(&server, cmd);
                    let err = replies.iter().find_map(|r| if let Reply::Error(e) = r { Some(e.clone()) } else { None });
                    server.traffic.query(id, &text, started, 0, err);
                    for r in replies {
                        r.encode(&mut out, conn.resp3);
                    }
                    if close {
                        let _ = socket.write_all(&out).await;
                        break 'conn;
                    }
                }
                if !out.is_empty() {
                    if socket.write_all(&out).await.is_err() {
                        break;
                    }
                    out.clear();
                }
            }
            msg = rx.recv() => {
                let Ok(msg) = msg else { continue };
                let (direct, patterns): (bool, Vec<Vec<u8>>) = {
                    let subs = server.subs.lock();
                    match subs.get(&id) {
                        Some(s) => (s.channels.contains(&msg.channel), s.patterns.iter().filter(|p| glob_match(p, &msg.channel)).cloned().collect()),
                        None => (false, vec![]),
                    }
                };
                if direct {
                    push("message", &msg.channel, Reply::Bulk(msg.message.clone())).encode(&mut out, conn.resp3);
                }
                for p in patterns {
                    Reply::Push(vec![Reply::bulk("pmessage"), Reply::Bulk(p), Reply::Bulk(msg.channel.clone()), Reply::Bulk(msg.message.clone())]).encode(&mut out, conn.resp3);
                }
                if !out.is_empty() {
                    if socket.write_all(&out).await.is_err() {
                        break;
                    }
                    out.clear();
                }
            }
        }
    }
    server.subs.lock().remove(&id);
    server.clients.fetch_sub(1, Ordering::Relaxed);
    server.traffic.record(id, TrafficKind::Disconnect);
}

/// Serve Redis clients until `cancel` fires. Snapshots are written every 2 s when data changed
/// and once more on shutdown.
pub async fn serve(
    listener: TcpListener,
    store: Arc<RedisStore>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cancel: CancellationToken,
) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let (bus, _) = broadcast::channel(1024);
    let server = Arc::new(Server {
        store: store.clone(),
        auth,
        traffic,
        faults,
        bus,
        subs: Mutex::new(HashMap::new()),
        clients: AtomicU64::new(0),
        started: Instant::now(),
        port,
    });
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut ticks = 0u64;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                ticks += 1;
                let st = store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    st.sweep();
                    if ticks.is_multiple_of(2) {
                        let _ = st.save_if_dirty();
                    }
                }).await;
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

/// Snapshot file used for a Redis mock data directory.
pub fn snapshot_path(dir: &std::path::Path) -> PathBuf {
    dir.join("redis.json")
}
