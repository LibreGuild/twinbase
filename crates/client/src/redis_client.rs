//! Redis driver: key browsing, typed value access and a raw command console.

use serde::{Deserialize, Serialize};

use crate::{ClientError, ConnectionConfig, Result, TlsMode};

#[derive(Clone)]
pub struct RedisClient {
    conn: redis::aio::MultiplexedConnection,
    pub db: i64,
    /// Keeps an SSH tunnel open for as long as the client lives.
    tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RedisKey {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Milliseconds to live; -1 = no expiry.
    pub ttl_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RedisValue {
    String { value: String },
    List { items: Vec<String> },
    Hash { fields: Vec<(String, String)> },
    Set { members: Vec<String> },
    Zset { members: Vec<(String, f64)> },
    None,
}

fn r_err(e: redis::RedisError) -> ClientError {
    let mut err = ClientError::new(e.to_string());
    if let Some(code) = e.code() {
        err.code = Some(code.to_owned());
    }
    err
}

fn to_json(v: redis::Value) -> serde_json::Value {
    use redis::Value as V;
    match v {
        V::Nil => serde_json::Value::Null,
        V::Int(i) => serde_json::Value::from(i),
        V::BulkString(b) => serde_json::Value::String(String::from_utf8_lossy(&b).into_owned()),
        V::SimpleString(s) => serde_json::Value::String(s),
        V::Okay => serde_json::Value::String("OK".into()),
        V::Array(items) | V::Set(items) => serde_json::Value::Array(items.into_iter().map(to_json).collect()),
        V::Map(pairs) => serde_json::Value::Array(pairs.into_iter().flat_map(|(k, v)| [to_json(k), to_json(v)]).collect()),
        V::Double(f) => crate::float_cell(f),
        V::Boolean(b) => serde_json::Value::Bool(b),
        V::VerbatimString { text, .. } => serde_json::Value::String(text),
        V::BigNumber(n) => serde_json::Value::String(String::from_utf8_lossy(&n).into_owned()),
        V::Attribute { data, .. } => to_json(*data),
        V::Push { data, .. } => serde_json::Value::Array(data.into_iter().map(to_json).collect()),
        V::ServerError(e) => serde_json::json!({ "error": e.to_string() }),
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

/// Split a console line into arguments, honouring quotes and backslash escapes.
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl RedisClient {
    pub async fn connect(config: &ConnectionConfig) -> Result<RedisClient> {
        let scheme = if config.tls == TlsMode::Disable { "redis" } else { "rediss" };
        // The Redis driver checks the certificate against the address it connects to, which for a
        // tunnel is the local end; say so instead of failing with a name mismatch.
        if config.via.is_some() && config.tls == TlsMode::VerifyFull {
            return Err(ClientError::new(
                "Redis through an SSH tunnel cannot check the certificate's host name (the driver connects to the tunnel's local port). Choose TLS \"Required\" for this connection, or connect without the tunnel.",
            ));
        }
        let via = config.via.map(|a| (a.ip().to_string(), a.port()));
        let host = match &via {
            Some((h, _)) => h.as_str(),
            None if config.host.is_empty() => "127.0.0.1",
            None => &config.host,
        };
        let port = via.as_ref().map(|v| v.1).unwrap_or(if config.port == 0 { 6379 } else { config.port });
        let db: i64 = config.database.as_deref().and_then(|d| d.parse().ok()).unwrap_or(0);
        let auth = match (&config.user, &config.password) {
            (u, Some(p)) if !u.is_empty() => format!("{}:{}@", enc(u), enc(p)),
            (_, Some(p)) => format!(":{}@", enc(p)),
            _ => String::new(),
        };
        let insecure = if config.tls == TlsMode::Require { "#insecure" } else { "" };
        let url = format!("{scheme}://{auth}{host}:{port}/{db}{insecure}");
        let client = redis::Client::open(url).map_err(r_err)?;
        let conn = client.get_multiplexed_async_connection().await.map_err(r_err)?;
        let mut me = RedisClient { conn, db, tunnel: None };
        // Fail early on bad credentials.
        let _: String = redis::cmd("PING").query_async(&mut me.conn).await.map_err(r_err)?;
        Ok(me)
    }

    pub(crate) fn with_tunnel(mut self, tunnel: std::sync::Arc<crate::ssh::Tunnel>) -> RedisClient {
        self.tunnel = Some(tunnel);
        self
    }

    pub async fn info(&self) -> Result<String> {
        redis::cmd("INFO").arg("server").query_async(&mut self.conn.clone()).await.map_err(r_err)
    }

    pub async fn dbsize(&self) -> Result<i64> {
        redis::cmd("DBSIZE").query_async(&mut self.conn.clone()).await.map_err(r_err)
    }

    /// One SCAN step with type and TTL for each key.
    pub async fn scan(&self, pattern: &str, cursor: u64, count: u64, type_filter: Option<&str>) -> Result<(u64, Vec<RedisKey>)> {
        let mut c = self.conn.clone();
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor).arg("MATCH").arg(if pattern.is_empty() { "*" } else { pattern }).arg("COUNT").arg(count.max(1));
        if let Some(t) = type_filter.filter(|t| !t.is_empty()) {
            cmd.arg("TYPE").arg(t);
        }
        let (next, keys): (String, Vec<String>) = cmd.query_async(&mut c).await.map_err(r_err)?;
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.cmd("TYPE").arg(k).cmd("PTTL").arg(k);
        }
        let meta: Vec<redis::Value> = if keys.is_empty() { vec![] } else { pipe.query_async(&mut c).await.map_err(r_err)? };
        let mut out = Vec::with_capacity(keys.len());
        for (i, k) in keys.into_iter().enumerate() {
            let kind = match meta.get(i * 2) {
                Some(redis::Value::SimpleString(s)) => s.clone(),
                Some(redis::Value::Okay) => "string".into(),
                Some(v) => to_json(v.clone()).as_str().unwrap_or("none").to_owned(),
                None => "none".into(),
            };
            let ttl = match meta.get(i * 2 + 1) {
                Some(redis::Value::Int(t)) => *t,
                _ => -1,
            };
            out.push(RedisKey { key: k, kind, ttl_ms: if ttl < 0 { -1 } else { ttl } });
        }
        Ok((next.parse().unwrap_or(0), out))
    }

    pub async fn get(&self, key: &str) -> Result<RedisValue> {
        let mut c = self.conn.clone();
        let kind: String = redis::cmd("TYPE").arg(key).query_async(&mut c).await.map_err(r_err)?;
        Ok(match kind.as_str() {
            "string" => RedisValue::String { value: redis::cmd("GET").arg(key).query_async(&mut c).await.map_err(r_err)? },
            "list" => RedisValue::List { items: redis::cmd("LRANGE").arg(key).arg(0).arg(9999).query_async(&mut c).await.map_err(r_err)? },
            "hash" => {
                let mut fields: Vec<(String, String)> = redis::cmd("HGETALL").arg(key).query_async(&mut c).await.map_err(r_err)?;
                fields.sort();
                RedisValue::Hash { fields }
            }
            "set" => {
                let mut members: Vec<String> = redis::cmd("SMEMBERS").arg(key).query_async(&mut c).await.map_err(r_err)?;
                members.sort();
                RedisValue::Set { members }
            }
            "zset" => RedisValue::Zset {
                members: redis::cmd("ZRANGE").arg(key).arg(0).arg(9999).arg("WITHSCORES").query_async(&mut c).await.map_err(r_err)?,
            },
            _ => RedisValue::None,
        })
    }

    /// Replace a key's value atomically; `ttl_ms` None keeps no expiry.
    pub async fn set(&self, key: &str, value: &RedisValue, ttl_ms: Option<i64>) -> Result<()> {
        let mut c = self.conn.clone();
        let mut pipe = redis::pipe();
        pipe.atomic().cmd("DEL").arg(key).ignore();
        match value {
            RedisValue::String { value } => {
                pipe.cmd("SET").arg(key).arg(value).ignore();
            }
            RedisValue::List { items } if !items.is_empty() => {
                pipe.cmd("RPUSH").arg(key).arg(items).ignore();
            }
            RedisValue::Hash { fields } if !fields.is_empty() => {
                let mut cmd = redis::cmd("HSET");
                cmd.arg(key);
                for (f, v) in fields {
                    cmd.arg(f).arg(v);
                }
                pipe.add_command(cmd).ignore();
            }
            RedisValue::Set { members } if !members.is_empty() => {
                pipe.cmd("SADD").arg(key).arg(members).ignore();
            }
            RedisValue::Zset { members } if !members.is_empty() => {
                let mut cmd = redis::cmd("ZADD");
                cmd.arg(key);
                for (m, s) in members {
                    cmd.arg(*s).arg(m);
                }
                pipe.add_command(cmd).ignore();
            }
            _ => {}
        }
        if let Some(ttl) = ttl_ms.filter(|t| *t > 0) {
            pipe.cmd("PEXPIRE").arg(key).arg(ttl).ignore();
        }
        pipe.query_async::<()>(&mut c).await.map_err(r_err)
    }

    pub async fn delete(&self, keys: &[String]) -> Result<i64> {
        if keys.is_empty() {
            return Ok(0);
        }
        redis::cmd("DEL").arg(keys).query_async(&mut self.conn.clone()).await.map_err(r_err)
    }

    /// Set a TTL in milliseconds, or remove it with `None`.
    pub async fn expire(&self, key: &str, ttl_ms: Option<i64>) -> Result<()> {
        let mut c = self.conn.clone();
        match ttl_ms {
            Some(ms) => redis::cmd("PEXPIRE").arg(key).arg(ms).query_async::<i64>(&mut c).await.map_err(r_err)?,
            None => redis::cmd("PERSIST").arg(key).query_async::<i64>(&mut c).await.map_err(r_err)?,
        };
        Ok(())
    }

    /// Run a console command line, e.g. `HSET user:1 name "Ann Lee"`.
    pub async fn command(&self, line: &str) -> Result<serde_json::Value> {
        let args = split_command_line(line);
        let Some((name, rest)) = args.split_first() else { return Err(ClientError::new("empty command")) };
        let mut cmd = redis::cmd(name);
        for a in rest {
            cmd.arg(a);
        }
        let v: redis::Value = cmd.query_async(&mut self.conn.clone()).await.map_err(r_err)?;
        Ok(to_json(v))
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_console_lines() {
        assert_eq!(
            split_command_line(r#"HSET user:1 name "Ann Lee" note 'it''s'"#),
            vec!["HSET", "user:1", "name", "Ann Lee", "note", "its"]
        );
        assert_eq!(split_command_line(r#"SET k "a\"b""#), vec!["SET", "k", "a\"b"]);
    }
}
