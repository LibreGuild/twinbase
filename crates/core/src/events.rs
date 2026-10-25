//! Events pushed to the UI (and visible to agents through the activity log).

use serde::Serialize;

/// Who caused an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent { name: String },
    Cli,
}

impl Actor {
    pub fn label(&self) -> String {
        match self {
            Actor::User => "You".into(),
            Actor::Agent { name } => name.clone(),
            Actor::Cli => "CLI".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: u64,
    /// Unix milliseconds.
    pub at: i64,
    pub actor: Actor,
    /// API method, e.g. `db.query`.
    pub action: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Activity(Activity),
    MockStatus {
        id: String,
        running: bool,
        port: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    MockTraffic {
        id: String,
        event: twinbase_mock::TrafficEvent,
    },
    ConnectionStatus {
        id: String,
        connected: bool,
    },
    /// Tables or columns changed (DDL) on a connection.
    SchemaChanged {
        connection: String,
    },
    /// Rows changed in a table (or anywhere when `table` is None).
    DataChanged {
        connection: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        table: Option<String>,
    },
    WorkspaceChanged,
    McpClient {
        name: String,
        connected: bool,
    },
}
