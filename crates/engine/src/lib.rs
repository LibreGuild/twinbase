//! TwinLite — the TwinBase mock database engine.
//!
//! Stores data in SQLite files and emulates PostgreSQL, MySQL and SQL Server (T-SQL) semantics on top: dialect
//! translation, type coercion, system catalogs, error codes and transaction behaviour. Protocol
//! servers (see `twinbase-mock`) drive it through [`Session`].

pub mod catalog;
mod ddl_exec;
pub mod engine;
pub mod error;
mod functions;
pub mod session;
pub mod translate;
pub mod tsql;
pub mod types;
pub mod value;

use serde::{Deserialize, Serialize};

pub use catalog::{Catalog, ColumnDef, ForeignKeyDef, IndexDef, KeyDef, TableDef};
pub use engine::{Engine, EngineOptions};
pub use error::{EngineError, Result};
/// A stored cell value (SQLite storage class).
pub use rusqlite::types::Value;
pub use session::{BatchResult, OutColumn, Outcome, Param, Prepared, ResultSet, Session, TxEffect, TxState};
pub use types::SqlType;

/// SQL dialect a mock emulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
    MySql,
    /// SQL Server's T-SQL (one database with schemas, `dbo` by default).
    MsSql,
}

impl Dialect {
    /// Object names match regardless of case (MySQL on case-insensitive file systems, SQL Server's
    /// default collation).
    pub fn case_insensitive_names(self) -> bool {
        matches!(self, Dialect::MySql | Dialect::MsSql)
    }

    /// Schema new objects go to when the session has no other.
    pub fn default_schema(self) -> &'static str {
        if self == Dialect::MsSql { "dbo" } else { "public" }
    }
}
