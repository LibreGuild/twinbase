//! Fault injection: make a mock slow, fail statements with chosen errors, drop connections, or
//! refuse new clients, so applications can be tested against an unreliable database.
//! Rules are changed at runtime through a shared [`Faults`] handle.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FaultConfig {
    /// Master switch; rules and `refuse_connections` apply only when enabled.
    pub enabled: bool,
    /// Accept new connections and close them immediately (the database looks down).
    pub refuse_connections: bool,
    /// Evaluated in order for every statement or command.
    pub rules: Vec<FaultRule>,
}

impl FaultConfig {
    pub fn is_default(&self) -> bool {
        self == &FaultConfig::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultRule {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Case-insensitive substring of the statement, or `/regex/`; empty matches everything.
    #[serde(default)]
    pub matches: String,
    /// Chance from 0 to 1 that the rule fires when it matches.
    #[serde(default = "one")]
    pub probability: f64,
    pub action: FaultAction,
}

fn yes() -> bool {
    true
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultAction {
    /// Wait before running the statement (`ms` plus a random 0..`jitter_ms`).
    Delay {
        ms: u64,
        #[serde(default)]
        jitter_ms: u64,
    },
    /// Fail the statement with this error code (SQLSTATE, MySQL error number or Redis prefix).
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Close the client connection without a reply.
    Drop,
}

/// What a server should do with one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Proceed,
    Fail { code: Option<String>, message: String },
    Drop,
}

/// Live, shared fault configuration of one mock server.
#[derive(Clone, Default)]
pub struct Faults {
    inner: Arc<RwLock<Compiled>>,
}

#[derive(Default)]
struct Compiled {
    config: FaultConfig,
    patterns: Vec<Pattern>,
}

enum Pattern {
    Any,
    Substring(String),
    Regex(regex::Regex),
}

impl Pattern {
    fn new(text: &str) -> Result<Pattern, String> {
        let t = text.trim();
        if t.is_empty() {
            return Ok(Pattern::Any);
        }
        if t.len() >= 2 && t.starts_with('/') && t.ends_with('/') {
            return regex::RegexBuilder::new(&t[1..t.len() - 1])
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
                .map(Pattern::Regex)
                .map_err(|e| format!("invalid pattern {t}: {e}"));
        }
        Ok(Pattern::Substring(t.to_lowercase()))
    }

    fn is_match(&self, statement: &str, lower: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Substring(s) => lower.contains(s.as_str()),
            Pattern::Regex(r) => r.is_match(statement),
        }
    }
}

impl Faults {
    pub fn new(config: FaultConfig) -> Result<Faults, String> {
        let f = Faults::default();
        f.set(config)?;
        Ok(f)
    }

    /// Replace the configuration; invalid patterns are rejected and nothing changes.
    pub fn set(&self, config: FaultConfig) -> Result<(), String> {
        for r in &config.rules {
            if !(0.0..=1.0).contains(&r.probability) || r.probability.is_nan() {
                return Err(format!("probability must be between 0 and 1 (got {})", r.probability));
            }
            if let FaultAction::Delay { ms, jitter_ms } = r.action
                && ms.saturating_add(jitter_ms) > 600_000
            {
                return Err("delays are limited to 10 minutes".into());
            }
        }
        let patterns = config.rules.iter().map(|r| Pattern::new(&r.matches)).collect::<Result<Vec<_>, _>>()?;
        *self.inner.write() = Compiled { config, patterns };
        Ok(())
    }

    pub fn config(&self) -> FaultConfig {
        self.inner.read().config.clone()
    }

    pub fn refuse_connections(&self) -> bool {
        let c = &self.inner.read().config;
        c.enabled && c.refuse_connections
    }

    /// Decide for one statement without waiting; returns the total delay to apply first.
    pub fn plan(&self, statement: &str) -> (Duration, Decision) {
        let c = self.inner.read();
        if !c.config.enabled || c.config.rules.is_empty() {
            return (Duration::ZERO, Decision::Proceed);
        }
        let lower = statement.to_lowercase();
        let mut delay = Duration::ZERO;
        for (rule, pattern) in c.config.rules.iter().zip(&c.patterns) {
            if !rule.enabled || !pattern.is_match(statement, &lower) {
                continue;
            }
            if rule.probability < 1.0 && rand::random::<f64>() >= rule.probability {
                continue;
            }
            match &rule.action {
                FaultAction::Delay { ms, jitter_ms } => {
                    let jitter = if *jitter_ms > 0 { rand::random_range(0..=*jitter_ms) } else { 0 };
                    delay += Duration::from_millis(ms + jitter);
                }
                FaultAction::Error { code, message } => {
                    let message = message.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "injected fault (TwinBase)".into());
                    return (delay, Decision::Fail { code: code.clone().filter(|c| !c.trim().is_empty()), message });
                }
                FaultAction::Drop => return (delay, Decision::Drop),
            }
        }
        (delay, Decision::Proceed)
    }

    /// Apply delays for one statement and say how to continue.
    pub async fn before(&self, statement: &str) -> Decision {
        let (delay, decision) = self.plan(statement);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(matches: &str, probability: f64, action: FaultAction) -> FaultRule {
        FaultRule { enabled: true, name: String::new(), matches: matches.into(), probability, action }
    }

    #[test]
    fn disabled_or_empty_config_never_interferes() {
        let f =
            Faults::new(FaultConfig { enabled: false, refuse_connections: true, rules: vec![rule("", 1.0, FaultAction::Drop)] }).unwrap();
        assert_eq!(f.plan("select 1"), (Duration::ZERO, Decision::Proceed));
        assert!(!f.refuse_connections());
    }

    #[test]
    fn rules_match_substrings_and_regexes_in_order() {
        let f = Faults::new(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![
                rule("orders", 1.0, FaultAction::Delay { ms: 50, jitter_ms: 0 }),
                rule("/^\\s*update\\b/", 1.0, FaultAction::Error { code: Some("40001".into()), message: None }),
                rule("", 1.0, FaultAction::Delay { ms: 5, jitter_ms: 0 }),
            ],
        })
        .unwrap();
        assert_eq!(f.plan("SELECT * FROM users"), (Duration::from_millis(5), Decision::Proceed));
        assert_eq!(f.plan("select * from ORDERS"), (Duration::from_millis(55), Decision::Proceed));
        let (d, dec) = f.plan("UPDATE orders SET x = 1");
        assert_eq!(d, Duration::from_millis(50));
        assert_eq!(dec, Decision::Fail { code: Some("40001".into()), message: "injected fault (TwinBase)".into() });
    }

    #[test]
    fn probability_controls_how_often_rules_fire() {
        let never =
            Faults::new(FaultConfig { enabled: true, refuse_connections: false, rules: vec![rule("", 0.0, FaultAction::Drop)] }).unwrap();
        assert!((0..200).all(|_| never.plan("x").1 == Decision::Proceed));
        let half =
            Faults::new(FaultConfig { enabled: true, refuse_connections: false, rules: vec![rule("", 0.5, FaultAction::Drop)] }).unwrap();
        let drops = (0..2000).filter(|_| half.plan("x").1 == Decision::Drop).count();
        assert!((800..1200).contains(&drops), "{drops}");
    }

    #[test]
    fn invalid_configs_are_rejected_without_changing_state() {
        let f = Faults::new(FaultConfig { enabled: true, refuse_connections: true, rules: vec![] }).unwrap();
        assert!(f.set(FaultConfig { enabled: true, refuse_connections: false, rules: vec![rule("/(/", 1.0, FaultAction::Drop)] }).is_err());
        assert!(f.set(FaultConfig { enabled: true, refuse_connections: false, rules: vec![rule("", 1.5, FaultAction::Drop)] }).is_err());
        assert!(f.refuse_connections());
    }
}
