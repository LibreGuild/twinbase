//! Native PostgreSQL engine for mocks: a real `postgres` server (from an installed PostgreSQL or a
//! configured binary directory) listening only on a private unix socket, fronted by a protocol-aware
//! proxy on the mock's port. The proxy authenticates clients like the TwinLite mock (any user and
//! password, or one user and password), creates missing roles and databases, logs every statement
//! with its row count and error, and injects faults.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::AuthMode;
use crate::faults::{Decision, Faults};
use crate::traffic::{TrafficKind, TrafficLog};

/// An installation of PostgreSQL server binaries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PgBinaries {
    pub bin: PathBuf,
    /// e.g. `17.2`.
    pub version: String,
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() }
}

/// Check one directory for `initdb` and `postgres`.
pub fn probe(dir: &Path) -> Option<PgBinaries> {
    if !dir.join(exe("initdb")).is_file() || !dir.join(exe("postgres")).is_file() {
        return None;
    }
    let out = std::process::Command::new(dir.join(exe("postgres"))).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().last().unwrap_or("").to_owned();
    (!version.is_empty()).then(|| PgBinaries { bin: dir.to_path_buf(), version })
}

/// Find PostgreSQL server binaries: the configured directory, then `TWINBASE_PG_BIN`, `PATH` and
/// the usual install locations (Homebrew, Postgres.app, Debian/Ubuntu, RHEL, Windows installer).
/// The newest version wins.
pub fn detect(configured: Option<&Path>) -> Option<PgBinaries> {
    if let Some(dir) = configured.filter(|d| !d.as_os_str().is_empty()) {
        return probe(dir);
    }
    if let Some(dir) = std::env::var_os("TWINBASE_PG_BIN") {
        return probe(Path::new(&dir));
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let globs = [
        "/opt/homebrew/opt",
        "/usr/local/opt",
        "/Applications/Postgres.app/Contents/Versions",
        "/usr/lib/postgresql",
        "/usr/local/pgsql",
        "C:\\Program Files\\PostgreSQL",
    ];
    for g in globs {
        if let Ok(entries) = std::fs::read_dir(g) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if g.ends_with("opt") && !name.starts_with("postgresql") {
                    continue;
                }
                candidates.push(e.path().join("bin"));
            }
        }
        candidates.push(Path::new(g).join("bin"));
    }
    if let Ok(entries) = std::fs::read_dir("/usr") {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("pgsql-") {
                candidates.push(e.path().join("bin"));
            }
        }
    }
    let mut found: Vec<PgBinaries> = candidates.iter().filter_map(|d| probe(d)).collect();
    let key = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    found.sort_by_key(|b| key(&b.version));
    found.pop()
}

/// A running `postgres` cluster owned by a mock.
pub struct NativeServer {
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    pub bins: PgBinaries,
    pub data: PathBuf,
    /// Directory holding the unix socket (or `127.0.0.1` on Windows).
    pub socket_dir: PathBuf,
    pub port: u16,
    _temp: Option<tempfile::TempDir>,
    _socket_tmp: Option<tempfile::TempDir>,
}

impl NativeServer {
    /// Initialise the cluster when `data` is empty, then start it on a private socket.
    pub async fn start(bins: PgBinaries, data: Option<PathBuf>) -> Result<NativeServer, String> {
        let (data, temp) = match data {
            Some(d) => (d, None),
            None => {
                let t = tempfile::tempdir().map_err(|e| e.to_string())?;
                (t.path().join("pgdata"), Some(t))
            }
        };
        if !data.join("PG_VERSION").exists() {
            std::fs::create_dir_all(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
            let out = tokio::process::Command::new(bins.bin.join(exe("initdb")))
                .args(["-D"])
                .arg(&data)
                .args(["-U", "postgres", "--auth=trust", "-E", "UTF8", "--locale=C", "--no-instructions"])
                .output()
                .await
                .map_err(|e| format!("cannot run initdb: {e}"))?;
            if !out.status.success() {
                return Err(format!("initdb failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
        }
        // A server left running by a crashed TwinBase still holds the data directory: stop it.
        if data.join("postmaster.pid").exists() {
            let _ = tokio::process::Command::new(bins.bin.join(exe("pg_ctl")))
                .arg("stop")
                .arg("-D")
                .arg(&data)
                .args(["-m", "fast", "-w", "-t", "20"])
                .output()
                .await;
        }
        // Unix sockets need short paths (about 100 bytes): use a private temp directory.
        let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let socket_tmp = tempfile::Builder::new().prefix("tbpg").tempdir_in(base).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(socket_tmp.path(), std::fs::Permissions::from_mode(0o700));
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).map_err(|e| e.to_string())?;
        let mut cmd = tokio::process::Command::new(bins.bin.join(exe("postgres")));
        cmd.arg("-D").arg(&data).arg("-p").arg(port.to_string());
        if cfg!(unix) {
            // No TCP at all: only this OS user can reach the trust-authenticated server.
            cmd.arg("-k").arg(socket_tmp.path()).args(["-c", "listen_addresses="]);
        } else {
            cmd.args(["-c", "listen_addresses=127.0.0.1"]);
        }
        cmd.args(["-c", "fsync=off", "-c", "full_page_writes=off", "-c", "max_connections=200"]);
        cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| format!("cannot start postgres: {e}"))?;
        let server = NativeServer {
            child: tokio::sync::Mutex::new(Some(child)),
            bins,
            data,
            socket_dir: socket_tmp.path().to_path_buf(),
            port,
            _temp: temp,
            _socket_tmp: Some(socket_tmp),
        };
        // Wait until it accepts connections.
        let started = Instant::now();
        loop {
            if server.connect().await.is_ok() {
                return Ok(server);
            }
            if started.elapsed() > Duration::from_secs(30) {
                return Err("postgres did not start within 30 seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A raw connection to the server.
    pub async fn connect(&self) -> std::io::Result<Box<dyn Stream>> {
        #[cfg(unix)]
        {
            let s = tokio::net::UnixStream::connect(self.socket_dir.join(format!(".s.PGSQL.{}", self.port))).await?;
            Ok(Box::new(s))
        }
        #[cfg(not(unix))]
        {
            let s = TcpStream::connect(("127.0.0.1", self.port)).await?;
            Ok(Box::new(s))
        }
    }

    /// An admin session (`postgres` superuser) for setup and seeds.
    pub async fn admin(&self, database: &str) -> Result<tokio_postgres::Client, String> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.user("postgres").dbname(database).port(self.port);
        #[cfg(unix)]
        cfg.host_path(&self.socket_dir);
        #[cfg(not(unix))]
        cfg.host("127.0.0.1");
        let (client, conn) = cfg.connect(tokio_postgres::NoTls).await.map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    }

    /// Create the role and database a client asks for when they do not exist yet.
    pub async fn ensure(&self, user: &str, database: &str) -> Result<(), String> {
        let admin = self.admin("postgres").await?;
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let role = admin.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&user]).await.map_err(|e| e.to_string())?;
        if role.is_none() {
            let _ = admin.batch_execute(&format!("CREATE ROLE {} LOGIN SUPERUSER", quote(user))).await;
        }
        let db = admin.query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&database]).await.map_err(|e| e.to_string())?;
        if db.is_none() {
            let _ = admin.batch_execute(&format!("CREATE DATABASE {} OWNER {}", quote(database), quote(user))).await;
        }
        Ok(())
    }

    /// Graceful stop (`pg_ctl stop -m fast`), killing the process if that fails. Idempotent.
    pub async fn stop(&self) {
        let Some(mut child) = self.child.lock().await.take() else { return };
        let pg_ctl = self.bins.bin.join(exe("pg_ctl"));
        let ok = tokio::process::Command::new(pg_ctl)
            .arg("stop")
            .arg("-D")
            .arg(&self.data)
            .args(["-m", "fast", "-w", "-t", "20"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            let _ = child.start_kill();
        }
        let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    }
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

// ------------------------------------------------------------------ wire helpers

/// Read `n` bytes; memory grows with the data that arrives, not with the length a client claims.
async fn read_exact_vec<R: AsyncRead + Unpin>(r: &mut R, n: usize) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(n.min(64 * 1024));
    (&mut *r).take(n as u64).read_to_end(&mut v).await?;
    if v.len() != n {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed mid-message"));
    }
    Ok(v)
}

/// A typed protocol message (type byte + body).
async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).await?;
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    if !(4..=256 * 1024 * 1024).contains(&len) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid message length"));
    }
    Ok((head[0], read_exact_vec(r, len - 4).await?))
}

fn message(t: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(t);
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn cstr(b: &[u8]) -> (String, &[u8]) {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    (String::from_utf8_lossy(&b[..end]).into_owned(), b.get(end + 1..).unwrap_or(&[]))
}

fn error_response(code: &str, msg: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (k, v) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', msg)] {
        body.push(k);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    message(b'E', &body)
}

fn fatal(code: &str, msg: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (k, v) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', msg)] {
        body.push(k);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    message(b'E', &body)
}

// ------------------------------------------------------------------ proxy

struct Pending {
    text: String,
    started: Instant,
    rows: u64,
    error: Option<String>,
    /// Simple queries finish at ReadyForQuery; extended ones at their completion message.
    simple: bool,
}

struct Shared {
    pending: VecDeque<Pending>,
    /// An injected error to deliver before the next ReadyForQuery.
    inject: Option<Vec<u8>>,
}

pub struct Proxy {
    pub server: Arc<NativeServer>,
    pub auth: AuthMode,
    pub traffic: TrafficLog,
    pub faults: Faults,
    ensured: Mutex<HashSet<(String, String)>>,
}

/// Serve the mock port until `cancel` fires, forwarding to the native server.
pub async fn serve(
    listener: TcpListener,
    server: Arc<NativeServer>,
    auth: AuthMode,
    traffic: TrafficLog,
    faults: Faults,
    cancel: CancellationToken,
) {
    let proxy = Arc::new(Proxy { server, auth, traffic, faults, ensured: Mutex::new(HashSet::new()) });
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { continue };
                if proxy.faults.refuse_connections() {
                    proxy.traffic.fault(proxy.traffic.new_connection_id(), "(new connection)", "connection refused");
                    continue;
                }
                let _ = socket.set_nodelay(true);
                let p = proxy.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let id = p.traffic.new_connection_id();
                    tokio::select! {
                        _ = p.handle(id, socket, peer) => {}
                        _ = cancel.cancelled() => {}
                    }
                    p.traffic.record(id, TrafficKind::Disconnect);
                });
            }
        }
    }
}

impl Proxy {
    async fn handle(&self, id: u64, mut client: TcpStream, peer: std::net::SocketAddr) -> std::io::Result<()> {
        // Startup: answer SSL/GSS requests with "no", forward cancel requests.
        let params = loop {
            let len = client.read_u32().await? as usize;
            if !(8..=10_000).contains(&len) {
                return Ok(());
            }
            let body = read_exact_vec(&mut client, len - 4).await?;
            let code = u32::from_be_bytes(body[..4].try_into().unwrap());
            match code {
                80877103 | 80877104 => client.write_all(b"N").await?,
                80877102 => {
                    let mut s = self.server.connect().await?;
                    let mut m = Vec::new();
                    m.extend_from_slice(&(len as u32).to_be_bytes());
                    m.extend_from_slice(&body);
                    s.write_all(&m).await?;
                    return Ok(());
                }
                196608 => break body[4..].to_vec(),
                _ => {
                    client.write_all(&fatal("08P01", "unsupported protocol version")).await?;
                    return Ok(());
                }
            }
        };
        let mut pairs = Vec::new();
        let mut rest = params.as_slice();
        while !rest.is_empty() && rest[0] != 0 {
            let (k, r) = cstr(rest);
            let (v, r2) = cstr(r);
            pairs.push((k, v));
            rest = r2;
        }
        let get = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let user = get("user").unwrap_or_else(|| "postgres".into());
        let database = get("database").unwrap_or_else(|| user.clone());
        self.traffic
            .record(id, TrafficKind::Connect { peer: peer.to_string(), user: Some(user.clone()), database: Some(database.clone()) });
        if let AuthMode::Password { user: want_user, password } = &self.auth {
            client.write_all(&message(b'R', &3u32.to_be_bytes())).await?;
            let (t, body) = read_message(&mut client).await?;
            let (given, _) = cstr(&body);
            if t != b'p' || &user != want_user || &given != password {
                client.write_all(&fatal("28P01", &format!("password authentication failed for user \"{user}\""))).await?;
                return Ok(());
            }
        }
        let key = (user.clone(), database.clone());
        if !self.ensured.lock().contains(&key) {
            if let Err(e) = self.server.ensure(&user, &database).await {
                client.write_all(&fatal("57P03", &format!("the PostgreSQL engine is not ready: {e}"))).await?;
                return Ok(());
            }
            self.ensured.lock().insert(key);
        }
        // Forward the startup message unchanged; the server trusts the private socket.
        let mut server = self.server.connect().await?;
        let mut m = Vec::new();
        m.extend_from_slice(&((params.len() + 8) as u32).to_be_bytes());
        m.extend_from_slice(&196608u32.to_be_bytes());
        m.extend_from_slice(&params);
        server.write_all(&m).await?;
        let shared = Arc::new(Mutex::new(Shared { pending: VecDeque::new(), inject: None }));
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = tokio::io::split(server);
        let traffic = self.traffic.clone();
        let sh = shared.clone();
        // Server → client: count rows, record errors and finish statements, deliver injected errors.
        let down = async move {
            let mut out = BytesMut::with_capacity(16 * 1024);
            loop {
                let (t, body) = read_message(&mut sr).await?;
                {
                    let mut s = sh.lock();
                    match t {
                        b'C' => {
                            let (tag, _) = cstr(&body);
                            let n = tag.rsplit(' ').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                            if let Some(p) = s.pending.front_mut() {
                                p.rows += n;
                                if !p.simple {
                                    let p = s.pending.pop_front().unwrap();
                                    traffic.query(id, &p.text, p.started, p.rows, p.error);
                                }
                            }
                        }
                        b'E' => {
                            let mut f = body.as_slice();
                            let mut msg = String::new();
                            let mut code = String::new();
                            while !f.is_empty() && f[0] != 0 {
                                let k = f[0];
                                let (v, r) = cstr(&f[1..]);
                                if k == b'M' {
                                    msg = v;
                                } else if k == b'C' {
                                    code = v;
                                }
                                f = r;
                            }
                            if let Some(p) = s.pending.front_mut() {
                                p.error = Some(format!("{msg} ({code})"));
                                if !p.simple {
                                    let p = s.pending.pop_front().unwrap();
                                    traffic.query(id, &p.text, p.started, p.rows, p.error);
                                }
                            }
                        }
                        b'I' | b's' => {
                            if let Some(p) = s.pending.pop_front_if(|p| !p.simple) {
                                traffic.query(id, &p.text, p.started, p.rows, p.error);
                            }
                        }
                        b'Z' => {
                            while let Some(p) = s.pending.pop_front() {
                                traffic.query(id, &p.text, p.started, p.rows, p.error);
                            }
                            if let Some(err) = s.inject.take() {
                                out.put_slice(&err);
                            }
                        }
                        _ => {}
                    }
                }
                out.put_slice(&message(t, &body));
                if out.len() > 8 * 1024
                    || matches!(
                        t,
                        b'Z' | b'R'
                            | b'E'
                            | b'G'
                            | b'H'
                            | b'W'
                            | b'A'
                            | b'N'
                            | b'1'
                            | b'2'
                            | b'3'
                            | b'n'
                            | b's'
                            | b't'
                            | b'T'
                            | b'C'
                            | b'I'
                    )
                {
                    cw.write_all(&out).await?;
                    out.clear();
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        };
        // Client → server: log statements, apply faults.
        let faults = self.faults.clone();
        let traffic2 = self.traffic.clone();
        let up = async move {
            let mut statements: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            let mut portals: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            let mut swallow = false;
            loop {
                let (t, body) = read_message(&mut cr).await?;
                match t {
                    b'Q' => {
                        let (sql, _) = cstr(&body);
                        match faults.before(&sql).await {
                            Decision::Proceed => {}
                            Decision::Fail { code, message: msg } => {
                                traffic2.fault(id, &sql, &msg);
                                // The statement is not sent. A Sync makes the server answer with
                                // ReadyForQuery, and the error is delivered just before it, so the
                                // reply order and transaction status stay correct.
                                shared.lock().inject = Some(error_response(code.as_deref().unwrap_or("XX000"), &msg));
                                sw.write_all(&message(b'S', &[])).await?;
                                continue;
                            }
                            Decision::Drop => {
                                traffic2.fault(id, &sql, "connection dropped");
                                return Ok(());
                            }
                        }
                        shared.lock().pending.push_back(Pending { text: sql, started: Instant::now(), rows: 0, error: None, simple: true });
                    }
                    b'P' => {
                        let (name, r) = cstr(&body);
                        let (sql, _) = cstr(r);
                        statements.insert(name, sql);
                    }
                    b'B' => {
                        let (portal, r) = cstr(&body);
                        let (stmt, _) = cstr(r);
                        portals.insert(portal, statements.get(&stmt).cloned().unwrap_or_default());
                    }
                    b'E' => {
                        if swallow {
                            continue;
                        }
                        let (portal, _) = cstr(&body);
                        let sql = portals.get(&portal).cloned().unwrap_or_default();
                        match faults.before(&sql).await {
                            Decision::Proceed => {}
                            Decision::Fail { code, message: msg } => {
                                traffic2.fault(id, &sql, &msg);
                                // Skip this Execute; the error is delivered before the next ReadyForQuery.
                                shared.lock().inject = Some(error_response(code.as_deref().unwrap_or("XX000"), &msg));
                                swallow = true;
                                continue;
                            }
                            Decision::Drop => {
                                traffic2.fault(id, &sql, "connection dropped");
                                return Ok(());
                            }
                        }
                        shared.lock().pending.push_back(Pending {
                            text: sql,
                            started: Instant::now(),
                            rows: 0,
                            error: None,
                            simple: false,
                        });
                    }
                    b'S' => swallow = false,
                    b'X' => {
                        sw.write_all(&message(t, &body)).await?;
                        return Ok(());
                    }
                    _ => {}
                }
                if swallow && t != b'S' {
                    continue;
                }
                sw.write_all(&message(t, &body)).await?;
            }
        };
        tokio::select! {
            r = down => r,
            r = up => r,
        }
    }
}

/// Run a seed script in a database (created when missing).
pub async fn seed(server: &NativeServer, database: &str, sql: &str) -> Result<(), String> {
    server.ensure("postgres", database).await?;
    let admin = server.admin(database).await?;
    admin.batch_execute(sql).await.map_err(|e| match e.as_db_error() {
        Some(d) => format!("{} ({})", d.message(), d.code().code()),
        None => e.to_string(),
    })
}
