//! Google Cloud credentials for Firestore, BigQuery and Spanner.
//!
//! - A **service-account key file**: TwinBase signs short-lived JWTs itself (RS256, audience = the
//!   API), so no token exchange is needed and the key never leaves the file.
//! - **Application Default Credentials** from `gcloud auth application-default login` (an
//!   `authorized_user` file): the refresh token is exchanged for access tokens at Google's OAuth
//!   endpoint and cached until shortly before they expire.
//! - A fixed token: emulators (`owner`), TwinBase mocks, or an access token pasted by the user.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{ClientError, Result};

#[derive(Clone)]
pub enum GoogleAuth {
    Fixed(String),
    ServiceAccount {
        email: String,
        key_id: String,
        key: Arc<ring::signature::RsaKeyPair>,
        /// Tokens per audience.
        cached: Arc<Mutex<Vec<(String, String, Instant)>>>,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        cached: Arc<tokio::sync::Mutex<Option<(String, Instant)>>>,
    },
}

/// The credentials file Application Default Credentials would use: `GOOGLE_APPLICATION_CREDENTIALS`,
/// else gcloud's `application_default_credentials.json`.
pub fn default_credentials_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
        return Some(p.into());
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    };
    base.map(|b| b.join("gcloud").join("application_default_credentials.json")).filter(|p| p.is_file())
}

impl GoogleAuth {
    /// Load a service-account key or an authorized-user (ADC) file; also returns the project id it
    /// names, if any.
    pub fn from_file(path: &Path) -> Result<(GoogleAuth, Option<String>)> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ClientError::new(format!("cannot read credentials {}: {e}", path.display())))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ClientError::new(format!("invalid credentials file {}: {e}", path.display())))?;
        let project = v["project_id"].as_str().or(v["quota_project_id"].as_str()).map(str::to_owned);
        match v["type"].as_str() {
            Some("service_account") => {
                let pem = v["private_key"].as_str().unwrap_or_default();
                let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
                let der = B64.decode(body.trim()).map_err(|_| ClientError::new("the service-account private key is not valid PEM"))?;
                let key = ring::signature::RsaKeyPair::from_pkcs8(&der)
                    .map_err(|e| ClientError::new(format!("the service-account private key cannot be used: {e}")))?;
                Ok((
                    GoogleAuth::ServiceAccount {
                        email: v["client_email"].as_str().unwrap_or_default().to_owned(),
                        key_id: v["private_key_id"].as_str().unwrap_or_default().to_owned(),
                        key: Arc::new(key),
                        cached: Arc::new(Mutex::new(Vec::new())),
                    },
                    project,
                ))
            }
            Some("authorized_user") => Ok((
                GoogleAuth::AuthorizedUser {
                    client_id: v["client_id"].as_str().unwrap_or_default().to_owned(),
                    client_secret: v["client_secret"].as_str().unwrap_or_default().to_owned(),
                    refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_owned(),
                    cached: Arc::new(tokio::sync::Mutex::new(None)),
                },
                project,
            )),
            other => Err(ClientError::new(format!(
                "{} is a \"{}\" credentials file; use a service-account key (IAM → Service accounts → Keys → JSON) or run `gcloud auth application-default login`",
                path.display(),
                other.unwrap_or("unknown")
            ))),
        }
    }

    /// A self-signed JWT for `audience` (e.g. `https://bigquery.googleapis.com/`), or the fixed token.
    /// Not available for authorized-user credentials (they need a network exchange: use [`token`]).
    pub fn token_sync(&self, audience: &str) -> Result<String> {
        match self {
            GoogleAuth::Fixed(t) => Ok(t.clone()),
            GoogleAuth::ServiceAccount { email, key_id, key, cached } => {
                let mut c = cached.lock();
                if let Some((_, t, _)) = c.iter().find(|(a, _, at)| a == audience && at.elapsed() < Duration::from_secs(50 * 60)) {
                    return Ok(t.clone());
                }
                let now = chrono::Utc::now().timestamp();
                let header = json!({ "alg": "RS256", "typ": "JWT", "kid": key_id });
                let claims = json!({ "iss": email, "sub": email, "aud": audience, "iat": now, "exp": now + 3600 });
                let input = format!("{}.{}", B64URL.encode(header.to_string()), B64URL.encode(claims.to_string()));
                let mut sig = vec![0u8; key.public().modulus_len()];
                key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig)
                    .map_err(|_| ClientError::new("cannot sign the service-account token"))?;
                let token = format!("{input}.{}", B64URL.encode(sig));
                c.retain(|(a, _, _)| a != audience);
                c.push((audience.to_owned(), token.clone(), Instant::now()));
                Ok(token)
            }
            GoogleAuth::AuthorizedUser { .. } => Err(ClientError::new(
                "user credentials (gcloud application-default login) are not supported for this database; use a service-account key file",
            )),
        }
    }

    /// An access token for `audience`, exchanging an ADC refresh token when needed.
    pub async fn token(&self, audience: &str) -> Result<String> {
        let GoogleAuth::AuthorizedUser { client_id, client_secret, refresh_token, cached } = self else {
            return self.token_sync(audience);
        };
        let mut c = cached.lock().await;
        if let Some((t, until)) = c.as_ref()
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let http = crate::http::HttpConn::new(
            true,
            "oauth2.googleapis.com".into(),
            443,
            None,
            crate::TlsMode::VerifyFull,
            Duration::from_secs(20),
        );
        let body = refresh_body(client_id, client_secret, refresh_token);
        let res = http
            .send("token refresh", Duration::from_secs(30), || {
                hyper::Request::builder()
                    .method("POST")
                    .uri("/token")
                    .header("host", "oauth2.googleapis.com")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(http_body_util::Full::new(bytes::Bytes::from(body.clone())))
                    .map_err(|e| ClientError::new(e.to_string()))
            })
            .await?;
        let v: Value = serde_json::from_slice(&res.body).unwrap_or(Value::Null);
        let token = v["access_token"].as_str().ok_or_else(|| {
            ClientError::new(format!(
                "Google refused the application-default credentials ({}); run `gcloud auth application-default login` again",
                v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no token")
            ))
        })?;
        let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(300);
        *c = Some((token.to_owned(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token.to_owned())
    }
}

fn refresh_body(client_id: &str, client_secret: &str, refresh_token: &str) -> String {
    use crate::http::url_encode as e;
    format!("grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}", e(client_id), e(client_secret), e(refresh_token))
}

/// Credentials for a Google Cloud connection: the key file in the connection (`path`), a token in
/// the password field, or Application Default Credentials. Emulators (`emulator`) need none.
pub fn for_connection(path: Option<&str>, password: Option<&str>, emulator: bool) -> Result<(GoogleAuth, Option<String>)> {
    if let Some(p) = path.filter(|p| !p.trim().is_empty()) {
        return GoogleAuth::from_file(Path::new(p.trim()));
    }
    if let Some(t) = password.filter(|t| !t.is_empty()) {
        return Ok((GoogleAuth::Fixed(t.to_owned()), None));
    }
    if emulator {
        return Ok((GoogleAuth::Fixed("owner".into()), None));
    }
    match default_credentials_path() {
        Some(p) => GoogleAuth::from_file(&p),
        None => Err(ClientError::new(
            "choose a service-account key file, or sign in with `gcloud auth application-default login` (or set GOOGLE_APPLICATION_CREDENTIALS)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_recognised() {
        let dir = std::env::temp_dir().join(format!("tb-gauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("adc.json");
        std::fs::write(
            &user,
            r#"{"type": "authorized_user", "client_id": "id", "client_secret": "s", "refresh_token": "r", "quota_project_id": "demo"}"#,
        )
        .unwrap();
        let (auth, project) = GoogleAuth::from_file(&user).unwrap();
        assert!(matches!(auth, GoogleAuth::AuthorizedUser { .. }));
        assert_eq!(project.as_deref(), Some("demo"));
        assert!(auth.token_sync("https://bigquery.googleapis.com/").is_err());
        let other = dir.join("ext.json");
        std::fs::write(&other, r#"{"type": "external_account"}"#).unwrap();
        assert!(GoogleAuth::from_file(&other).err().unwrap().message.contains("external_account"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(refresh_body("a b", "s&t", "r/1"), "grant_type=refresh_token&client_id=a%20b&client_secret=s%26t&refresh_token=r%2F1");
        let (fixed, _) = for_connection(None, None, true).unwrap();
        assert_eq!(fixed.token_sync("x").unwrap(), "owner");
    }
}
