//! Engine errors carry both a PostgreSQL SQLSTATE and a MySQL error number so each
//! protocol front-end can report the code its clients expect.

use std::fmt;

/// Marker used to smuggle a SQLSTATE through SQLite user-function errors.
/// Format: `\u{1}<SQLSTATE>\u{1}<message>`.
pub(crate) const CODE_MARK: char = '\u{1}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    /// PostgreSQL SQLSTATE, e.g. `23505`.
    pub sqlstate: String,
    /// MySQL error number, e.g. `1062`.
    pub mysql_code: u16,
    /// SQL Server error number when it is not derived from the SQLSTATE (THROW, RAISERROR).
    pub mssql_code: Option<u32>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl EngineError {
    pub fn new(sqlstate: &str, message: impl Into<String>) -> Self {
        let mysql_code = mysql_code_for(sqlstate);
        Self { sqlstate: sqlstate.to_owned(), mysql_code, mssql_code: None, message: message.into(), detail: None, hint: None }
    }

    pub fn with_mysql(mut self, code: u16) -> Self {
        self.mysql_code = code;
        self
    }

    pub fn with_mssql(mut self, code: u32) -> Self {
        self.mssql_code = Some(code);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(state::SYNTAX_ERROR, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(state::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(state::INTERNAL_ERROR, message)
    }

    /// MySQL SQLSTATE (5 chars) used in MySQL error packets.
    pub fn mysql_sqlstate(&self) -> &str {
        match self.mysql_code {
            1062 | 1452 | 1451 | 1048 | 3819 => "23000",
            1064 => "42000",
            1146 => "42S02",
            1054 => "42S22",
            1050 => "42S01",
            _ => {
                if self.sqlstate.len() == 5 {
                    &self.sqlstate
                } else {
                    "HY000"
                }
            }
        }
    }

    /// Encode as a SQLite user-function error message.
    pub(crate) fn to_function_error(&self) -> rusqlite::Error {
        rusqlite::Error::UserFunctionError(
            format!("{CODE_MARK}{}{CODE_MARK}{}{CODE_MARK}{}", self.sqlstate, self.mysql_code, self.message).into(),
        )
    }

    /// Decode a message produced by [`Self::to_function_error`].
    pub(crate) fn from_function_message(msg: &str) -> Option<Self> {
        let rest = msg.split_once(CODE_MARK)?.1;
        let (code, rest) = rest.split_once(CODE_MARK)?;
        let (mysql, message) = rest.split_once(CODE_MARK)?;
        let mut e = Self::new(code, message);
        if let Ok(m) = mysql.parse() {
            e.mysql_code = m;
        }
        Some(e)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.sqlstate)
    }
}

impl std::error::Error for EngineError {}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// SQLSTATE codes used by the engine.
pub mod state {
    pub const SYNTAX_ERROR: &str = "42601";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const UNDEFINED_SCHEMA: &str = "3F000";
    pub const INVALID_CATALOG_NAME: &str = "3D000";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const DUPLICATE_DATABASE: &str = "42P04";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const IN_FAILED_TRANSACTION: &str = "25P02";
    pub const NO_ACTIVE_TRANSACTION: &str = "25P01";
    pub const ACTIVE_TRANSACTION: &str = "25001";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const INTERNAL_ERROR: &str = "XX000";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const UNDEFINED_PARAMETER: &str = "42704";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const INVALID_SAVEPOINT: &str = "3B001";
    pub const READ_ONLY_TRANSACTION: &str = "25006";
}

fn mysql_code_for(sqlstate: &str) -> u16 {
    match sqlstate {
        state::SYNTAX_ERROR => 1064,
        state::UNDEFINED_TABLE => 1146,
        state::UNDEFINED_COLUMN => 1054,
        state::DUPLICATE_TABLE => 1050,
        state::DUPLICATE_COLUMN => 1060,
        state::DUPLICATE_DATABASE | state::DUPLICATE_SCHEMA => 1007,
        state::INVALID_CATALOG_NAME | state::UNDEFINED_SCHEMA => 1049,
        state::UNIQUE_VIOLATION => 1062,
        state::NOT_NULL_VIOLATION => 1048,
        state::FOREIGN_KEY_VIOLATION => 1452,
        state::CHECK_VIOLATION => 3819,
        state::INVALID_TEXT_REPRESENTATION => 1366,
        state::INVALID_DATETIME_FORMAT | state::DATETIME_FIELD_OVERFLOW => 1292,
        state::NUMERIC_VALUE_OUT_OF_RANGE => 1264,
        state::STRING_DATA_RIGHT_TRUNCATION => 1406,
        state::DIVISION_BY_ZERO => 1365,
        state::UNDEFINED_FUNCTION => 1305,
        state::FEATURE_NOT_SUPPORTED => 1235,
        state::LOCK_NOT_AVAILABLE => 1205,
        state::DEPENDENT_OBJECTS_STILL_EXIST => 3730,
        state::UNDEFINED_OBJECT => 1091,
        state::AMBIGUOUS_COLUMN => 1052,
        _ => 1105,
    }
}
