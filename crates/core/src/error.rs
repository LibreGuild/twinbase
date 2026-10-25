//! Errors returned by the core API (serialized to the UI and to MCP clients).

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct CoreError {
    /// Stable machine-readable kind: `not_found`, `invalid`, `io`, `database`, `forbidden`, `conflict`.
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement_index: Option<usize>,
}

impl CoreError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), code: None, detail: None, statement_index: None }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new("not_found", what)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message)
    }

    pub fn io(path: &Path, e: std::io::Error) -> Self {
        Self::new("io", format!("{}: {e}", path.display()))
    }
}

impl From<twinbase_client::ClientError> for CoreError {
    fn from(e: twinbase_client::ClientError) -> Self {
        CoreError { kind: "database".into(), message: e.message, code: e.code, detail: e.detail, statement_index: e.statement_index }
    }
}

impl From<twinbase_engine::EngineError> for CoreError {
    fn from(e: twinbase_engine::EngineError) -> Self {
        CoreError { kind: "database".into(), message: e.message, code: Some(e.sqlstate), detail: e.detail, statement_index: None }
    }
}

impl From<twinbase_mock::MockError> for CoreError {
    fn from(e: twinbase_mock::MockError) -> Self {
        CoreError::new("mock", e.to_string())
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::invalid(format!("invalid parameters: {e}"))
    }
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
