//! TwinBase application core shared by the desktop app, the CLI and the MCP server:
//! workspaces, connections, mock servers, the JSON API, change previews and agent access.

pub mod api;
pub mod app;
pub mod bootcamp;
pub mod bundle;
pub mod ddl;
pub mod docs;
pub mod error;
pub mod events;
pub mod explain;
pub mod http;
pub mod mcp;
pub mod model;
pub mod sample;
pub mod scenario;
pub mod schema_diff;
pub mod settings;
pub mod snippets;
pub mod twin;

pub use app::App;
pub use error::{CoreError, Result};
pub use events::{Activity, Actor, Event};

/// Split a Redis command line into raw arguments (used for Redis seed files).
pub fn split_redis_line(line: &str) -> Vec<Vec<u8>> {
    twinbase_client::split_command_line(line).into_iter().map(String::into_bytes).collect()
}
