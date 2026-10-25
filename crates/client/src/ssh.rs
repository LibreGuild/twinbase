//! SSH tunnels: a local port forwarded through a bastion host to the database, so connections to
//! databases in private networks work without changing the client protocol.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, ssh_key};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::{ClientError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    #[default]
    Password,
    /// Private key file (OpenSSH or PEM), optionally encrypted with a passphrase.
    Key,
    /// Keys held by the running ssh-agent (`SSH_AUTH_SOCK`).
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshTunnel {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// Password, or the key's passphrase. Never stored with the connection definition.
    #[serde(skip)]
    pub secret: Option<String>,
    /// Where newly trusted host keys are recorded (trust on first use). `~/.ssh/known_hosts` is
    /// always consulted first.
    #[serde(skip)]
    pub known_hosts: Option<PathBuf>,
}

fn default_ssh_port() -> u16 {
    22
}

/// A running tunnel; dropping it closes the listener and the SSH session.
pub struct Tunnel {
    pub local: SocketAddr,
    /// SHA-256 fingerprint of the bastion's host key.
    pub host_key: String,
    /// The host key was unknown and has just been trusted.
    pub newly_trusted: bool,
    task: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the host-key check saw: fingerprint, whether it was newly trusted, and a problem to report.
type SeenKey = Arc<parking_lot::Mutex<Option<(String, bool, Option<String>)>>>;

struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    seen: SeenKey,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let key: &PublicKey = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                *self.seen.lock() = Some((
                    String::new(),
                    false,
                    Some("host certificates are not supported yet; configure the bastion to offer a plain host key".into()),
                ));
                return Ok(false);
            }
        };
        let fingerprint = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        let record = |trusted: bool, new: bool, problem: Option<String>| {
            *self.seen.lock() = Some((fingerprint.clone(), new, problem));
            trusted
        };
        // The user's own known_hosts wins; a changed key there is a hard failure.
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => return Ok(record(true, false, None)),
            Err(russh::keys::Error::KeyChanged { line }) => {
                return Ok(record(
                    false,
                    false,
                    Some(format!("the host key of {} changed (~/.ssh/known_hosts line {line}); refusing to connect", self.host)),
                ));
            }
            _ => {}
        }
        let Some(path) = &self.known_hosts else {
            return Ok(record(true, true, None));
        };
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, path) {
            Ok(true) => Ok(record(true, false, None)),
            Err(russh::keys::Error::KeyChanged { .. }) => Ok(record(
                false,
                false,
                Some(format!(
                    "the host key of {} differs from the one TwinBase trusted before; refusing to connect (possible man-in-the-middle)",
                    self.host
                )),
            )),
            _ => {
                // Trust on first use, like OpenSSH's accept-new.
                let _ = russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, path);
                Ok(record(true, true, None))
            }
        }
    }
}

fn ssh_err(e: impl std::fmt::Display) -> ClientError {
    ClientError::new(format!("SSH: {e}"))
}

async fn authenticate(handle: &mut Handle<HostKeyCheck>, t: &SshTunnel) -> Result<bool> {
    match t.auth {
        SshAuth::Password => {
            let password = t.secret.clone().unwrap_or_default();
            Ok(matches!(handle.authenticate_password(&t.user, password).await.map_err(ssh_err)?, AuthResult::Success))
        }
        SshAuth::Key => {
            let path = t.key_path.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| ClientError::new("SSH: choose a private key file"))?;
            let path = shellexpand_home(path);
            let key = russh::keys::load_secret_key(&path, t.secret.as_deref().filter(|s| !s.is_empty()))
                .map_err(|e| ClientError::new(format!("SSH: cannot read key {}: {e}", path.display())))?;
            let hash = handle.best_supported_rsa_hash().await.map_err(ssh_err)?.flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            Ok(matches!(handle.authenticate_publickey(&t.user, key).await.map_err(ssh_err)?, AuthResult::Success))
        }
        SshAuth::Agent => {
            #[cfg(unix)]
            {
                let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                    .await
                    .map_err(|e| ClientError::new(format!("SSH agent: {e}")))?;
                let identities = agent.request_identities().await.map_err(|e| ClientError::new(format!("SSH agent: {e}")))?;
                let hash = handle.best_supported_rsa_hash().await.map_err(ssh_err)?.flatten();
                for id in identities {
                    let key = id.public_key().into_owned();
                    if let Ok(AuthResult::Success) = handle.authenticate_publickey_with(&t.user, key, hash, &mut agent).await {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            #[cfg(not(unix))]
            {
                Err(ClientError::new("SSH agent authentication is only available on macOS and Linux"))
            }
        }
    }
}

fn shellexpand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs_home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Connect to the bastion, authenticate, and forward a local port to `target_host:target_port`.
pub async fn open(t: &SshTunnel, target_host: &str, target_port: u16, timeout: Duration) -> Result<Tunnel> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 4,
        nodelay: true,
        ..Default::default()
    });
    let seen = Arc::new(parking_lot::Mutex::new(None));
    let handler = HostKeyCheck { host: t.host.clone(), port: t.port, known_hosts: t.known_hosts.clone(), seen: seen.clone() };
    let addr = (t.host.as_str(), if t.port == 0 { 22 } else { t.port });
    let connected = tokio::time::timeout(timeout, client::connect(config, addr, handler)).await;
    let mut handle = match connected {
        Err(_) => return Err(ClientError::new(format!("SSH: connecting to {}:{} timed out", t.host, t.port))),
        Ok(Err(e)) => {
            if let Some((_, _, Some(problem))) = seen.lock().clone() {
                return Err(ClientError::new(format!("SSH: {problem}")));
            }
            return Err(ssh_err(e));
        }
        Ok(Ok(h)) => h,
    };
    if !authenticate(&mut handle, t).await? {
        return Err(ClientError::new(format!("SSH: authentication failed for {}@{}", t.user, t.host)));
    }
    let (host_key, newly_trusted) = seen.lock().clone().map(|(f, n, _)| (f, n)).unwrap_or_default();
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| ClientError::new(format!("SSH: cannot open a local port: {e}")))?;
    let local = listener.local_addr().map_err(ssh_err)?;
    let handle = Arc::new(handle);
    let target_host = target_host.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else { break };
            let handle = handle.clone();
            let target_host = target_host.clone();
            tokio::spawn(async move {
                let channel = match handle
                    .channel_open_direct_tcpip(target_host, u32::from(target_port), peer.ip().to_string(), u32::from(peer.port()))
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("SSH tunnel could not open a channel: {e}");
                        return;
                    }
                };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
    });
    Ok(Tunnel { local, host_key, newly_trusted, task })
}
