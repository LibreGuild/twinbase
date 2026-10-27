//! Logical SQL types shared by all dialects, with parsing from declared type names and the
//! metadata each protocol needs (PostgreSQL OIDs, display names, storage classes).

use serde::{Deserialize, Serialize};

use crate::Dialect;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqlType {
    Bool,
    /// MySQL `TINYINT`.
    Int1,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    Text,
    Varchar {
        len: Option<u32>,
    },
    Char {
        len: Option<u32>,
    },
    Bytea,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    Json,
    Jsonb,
    /// MySQL `YEAR`.
    Year,
    /// PostgreSQL named enum (`name` set) or MySQL inline `ENUM(...)` (`name` empty).
    Enum {
        name: String,
        values: Vec<String>,
    },
    Array {
        element: Box<SqlType>,
    },
    /// pgvector `vector(n)`, `halfvec(n)` or `sparsevec(n)`; stored as canonical text.
    Vector {
        family: crate::vector::VectorKind,
        dim: Option<u32>,
    },
    /// Any other type name: stored and returned as text.
    Other {
        name: String,
    },
}

/// How a value of a type is stored in SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Integer,
    Real,
    Text,
    Blob,
}

impl Storage {
    pub fn sqlite_decl(self) -> &'static str {
        match self {
            Storage::Integer => "INTEGER",
            Storage::Real => "REAL",
            Storage::Text => "TEXT",
            Storage::Blob => "BLOB",
        }
    }
}

/// Identity behaviour implied by a type name (PostgreSQL `SERIAL` family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialKind {
    Serial,
}

/// Parsed form of a declared type name.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedType {
    pub ty: SqlType,
    pub serial: Option<SerialKind>,
    pub unsigned: bool,
}

impl SqlType {
    /// Parse a declared type name such as `VARCHAR(255)`, `timestamp with time zone`,
    /// `INTEGER[]`, `serial`, `tinyint(1)` or `enum('a','b')`.
    ///
    /// `lookup_enum` resolves user-defined (PostgreSQL) enum type names.
    pub fn parse(decl: &str, dialect: Dialect, lookup_enum: &dyn Fn(&str) -> Option<Vec<String>>) -> ParsedType {
        let raw = decl.trim();
        let mut upper = raw.to_ascii_uppercase();

        // Arrays: `type[]`, `type[3]`, `type ARRAY`.
        let mut array_depth = 0;
        loop {
            let trimmed = upper.trim_end();
            if trimmed.ends_with(']')
                && let Some(open) = trimmed.rfind('[')
            {
                upper = trimmed[..open].to_owned();
                array_depth += 1;
                continue;
            }
            if let Some(stripped) = trimmed.strip_suffix(" ARRAY") {
                upper = stripped.to_owned();
                array_depth += 1;
                continue;
            }
            break;
        }
        if array_depth > 0 {
            let base_len = raw.len().min(upper.len());
            let inner = Self::parse(&raw[..base_len], dialect, lookup_enum);
            let mut ty = inner.ty;
            for _ in 0..array_depth {
                ty = SqlType::Array { element: Box::new(ty) };
            }
            return ParsedType { ty, serial: None, unsigned: false };
        }

        let mut unsigned = false;
        for suffix in [" ZEROFILL", " UNSIGNED", " SIGNED"] {
            if let Some(s) = upper.strip_suffix(suffix) {
                if suffix == " UNSIGNED" {
                    unsigned = true;
                }
                upper = s.trim_end().to_owned();
            }
        }
        if let Some(s) = upper.strip_suffix(" ZEROFILL") {
            upper = s.trim_end().to_owned();
        }

        // Split `NAME(args) rest`.
        let (name, args, rest) = match upper.find('(') {
            Some(open) => {
                let close = upper.rfind(')').unwrap_or(upper.len());
                let args = upper[open + 1..close.max(open + 1)].to_owned();
                let rest = if close < upper.len() { upper[close + 1..].trim().to_owned() } else { String::new() };
                (upper[..open].trim().to_owned(), Some(args), rest)
            }
            None => (upper.clone(), None, String::new()),
        };
        let full_name = if rest.is_empty() { name.clone() } else { format!("{name} {rest}") };
        // Oracle length semantics (`VARCHAR2(100 CHAR)`) do not change the length.
        let nums: Vec<u32> = args
            .as_deref()
            .map(|a| {
                a.split(',').filter_map(|p| p.trim().trim_end_matches(" CHAR").trim_end_matches(" BYTE").trim().parse().ok()).collect()
            })
            .unwrap_or_default();
        let n0 = nums.first().copied();
        let n1 = nums.get(1).copied();

        let mut serial = None;
        let ty = match full_name.as_str() {
            "BOOL" | "BOOLEAN" => SqlType::Bool,
            "BIT" if n0.unwrap_or(1) == 1 => SqlType::Bool,
            "TINYINT" if dialect == Dialect::MySql && n0 == Some(1) => SqlType::Bool,
            "TINYINT" | "INT1" => SqlType::Int1,
            "SMALLINT" | "INT2" => SqlType::Int2,
            "MEDIUMINT" | "INT" | "INTEGER" | "INT4" | "INT3" => {
                if dialect == Dialect::MySql && unsigned && full_name != "MEDIUMINT" {
                    SqlType::Int8
                } else {
                    SqlType::Int4
                }
            }
            "BIGINT" | "INT8" => SqlType::Int8,
            "SMALLSERIAL" | "SERIAL2" => {
                serial = Some(SerialKind::Serial);
                SqlType::Int2
            }
            "SERIAL" | "SERIAL4" => {
                serial = Some(SerialKind::Serial);
                // MySQL `SERIAL` is BIGINT UNSIGNED NOT NULL AUTO_INCREMENT UNIQUE.
                if dialect == Dialect::MySql { SqlType::Int8 } else { SqlType::Int4 }
            }
            "BIGSERIAL" | "SERIAL8" => {
                serial = Some(SerialKind::Serial);
                SqlType::Int8
            }
            "REAL" | "FLOAT4" => SqlType::Float4,
            "FLOAT" => {
                // FLOAT(p) is single precision up to 24 bits; MySQL's bare FLOAT is too.
                let single = n0.map(|p| p <= 24).unwrap_or(dialect == Dialect::MySql);
                if single { SqlType::Float4 } else { SqlType::Float8 }
            }
            "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => SqlType::Float8,
            "NUMERIC" | "DECIMAL" | "DEC" | "FIXED" => SqlType::Numeric { precision: n0, scale: n1.or(n0.map(|_| 0)) },
            "MONEY" => SqlType::Numeric { precision: None, scale: Some(2) },
            // Oracle.
            "NUMBER" if n0.is_some_and(|p| p <= 18) && n1.unwrap_or(0) == 0 => {
                if n0.unwrap_or(19) <= 9 {
                    SqlType::Int4
                } else {
                    SqlType::Int8
                }
            }
            "NUMBER" => SqlType::Numeric { precision: n0, scale: n1.or(n0.map(|_| 0)) },
            "BINARY_FLOAT" => SqlType::Float4,
            "BINARY_DOUBLE" => SqlType::Float8,
            "RAW" | "LONG RAW" => SqlType::Bytea,
            "NCLOB" => SqlType::Text,
            "SMALLMONEY" => SqlType::Numeric { precision: Some(10), scale: Some(4) },
            "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "CITEXT" | "STRING" | "CLOB" | "NAME" | "NTEXT" => SqlType::Text,
            "VARCHAR" | "CHARACTER VARYING" | "CHAR VARYING" | "NVARCHAR" | "VARCHAR2" => SqlType::Varchar { len: n0 },
            "CHAR" | "CHARACTER" | "BPCHAR" | "NCHAR" => SqlType::Char { len: n0.or(Some(1)) },
            "BYTEA" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "BYTES" | "IMAGE" => SqlType::Bytea,
            "DATE" => SqlType::Date,
            "TIME" | "TIME WITHOUT TIME ZONE" | "TIME WITH TIME ZONE" | "TIMETZ" => SqlType::Time,
            "TIMESTAMP WITHOUT TIME ZONE" | "DATETIME" | "SMALLDATETIME" | "DATETIME2" => SqlType::Timestamp,
            "TIMESTAMP" => {
                // MySQL TIMESTAMP is stored in UTC but shown in session time zone (UTC here).
                SqlType::Timestamp
            }
            "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" | "DATETIMEOFFSET" => SqlType::TimestampTz,
            "INTERVAL" => SqlType::Interval,
            "UUID" | "UNIQUEIDENTIFIER" => SqlType::Uuid,
            "JSON" => SqlType::Json,
            "JSONB" => SqlType::Jsonb,
            "YEAR" => SqlType::Year,
            "ENUM" => {
                let values = args.as_deref().map(|a| parse_quoted_list(raw_args(raw).unwrap_or(a))).unwrap_or_default();
                SqlType::Enum { name: String::new(), values }
            }
            "SET" => SqlType::Text,
            "VECTOR" | "HALFVEC" | "SPARSEVEC" => {
                SqlType::Vector { family: crate::vector::VectorKind::from_name(&full_name).unwrap(), dim: n0 }
            }
            _ => {
                let lower = raw.to_ascii_lowercase();
                if let Some(values) = lookup_enum(&lower) {
                    SqlType::Enum { name: lower, values }
                } else if full_name.starts_with("TIMESTAMP") && full_name.ends_with("WITH TIME ZONE") {
                    SqlType::TimestampTz
                } else if full_name.starts_with("TIMESTAMP") {
                    SqlType::Timestamp
                } else if full_name.starts_with("TIME") {
                    SqlType::Time
                } else if full_name.starts_with("INTERVAL") {
                    SqlType::Interval
                } else {
                    SqlType::Other { name: lower }
                }
            }
        };
        ParsedType { ty, serial, unsigned }
    }

    /// Parse with no enum lookup (convenience for tests and built-in names).
    pub fn parse_simple(decl: &str, dialect: Dialect) -> SqlType {
        Self::parse(decl, dialect, &|_| None).ty
    }

    pub fn storage(&self) -> Storage {
        match self {
            SqlType::Bool | SqlType::Int1 | SqlType::Int2 | SqlType::Int4 | SqlType::Int8 | SqlType::Year => Storage::Integer,
            SqlType::Float4 | SqlType::Float8 | SqlType::Numeric { .. } => Storage::Real,
            SqlType::Bytea => Storage::Blob,
            _ => Storage::Text,
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, SqlType::Int1 | SqlType::Int2 | SqlType::Int4 | SqlType::Int8 | SqlType::Year)
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || matches!(self, SqlType::Float4 | SqlType::Float8 | SqlType::Numeric { .. })
    }

    pub fn is_textual(&self) -> bool {
        matches!(self, SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. } | SqlType::Other { .. } | SqlType::Enum { .. })
    }

    pub fn is_temporal(&self) -> bool {
        matches!(self, SqlType::Date | SqlType::Time | SqlType::Timestamp | SqlType::TimestampTz)
    }

    /// Whether literals compared with or assigned to this type must be normalized
    /// through `tb_cast` to compare correctly against stored values.
    pub fn needs_coercion(&self) -> bool {
        !matches!(self, SqlType::Text | SqlType::Other { .. } | SqlType::Varchar { len: None })
    }

    /// PostgreSQL type OID. Enums use `enum_oid` (from the catalog).
    pub fn pg_oid(&self) -> u32 {
        match self {
            SqlType::Bool => 16,
            SqlType::Bytea => 17,
            SqlType::Int8 => 20,
            SqlType::Int1 | SqlType::Int2 => 21,
            SqlType::Int4 | SqlType::Year => 23,
            SqlType::Text => 25,
            SqlType::Other { name } => match name.as_str() {
                "oid" => 26,
                "regproc" => 24,
                "regclass" => 2205,
                "regtype" => 2206,
                "int2vector" => 22,
                "oidvector" => 30,
                "xid" => 28,
                _ => 25,
            },
            SqlType::Json => 114,
            SqlType::Float4 => 700,
            SqlType::Float8 => 701,
            SqlType::Char { .. } => 1042,
            SqlType::Varchar { .. } => 1043,
            SqlType::Date => 1082,
            SqlType::Time => 1083,
            SqlType::Timestamp => 1114,
            SqlType::TimestampTz => 1184,
            SqlType::Interval => 1186,
            SqlType::Numeric { .. } => 1700,
            SqlType::Uuid => 2950,
            SqlType::Jsonb => 3802,
            SqlType::Vector { family: kind, .. } => kind.oid(),
            // Enums are reported as text on the wire so generic clients can decode them.
            SqlType::Enum { .. } => 25,
            SqlType::Array { element } => match element.as_ref() {
                SqlType::Bool => 1000,
                SqlType::Bytea => 1001,
                SqlType::Int1 | SqlType::Int2 => 1005,
                SqlType::Int4 | SqlType::Year => 1007,
                SqlType::Int8 => 1016,
                SqlType::Float4 => 1021,
                SqlType::Float8 => 1022,
                SqlType::Char { .. } => 1014,
                SqlType::Varchar { .. } => 1015,
                SqlType::Date => 1182,
                SqlType::Time => 1183,
                SqlType::Timestamp => 1115,
                SqlType::TimestampTz => 1185,
                SqlType::Interval => 1187,
                SqlType::Numeric { .. } => 1231,
                SqlType::Uuid => 2951,
                SqlType::Json => 199,
                SqlType::Jsonb => 3807,
                SqlType::Vector { family: kind, .. } => kind.array_oid(),
                _ => 1009,
            },
        }
    }

    /// PostgreSQL internal type name (`pg_type.typname`, `information_schema.columns.udt_name`).
    pub fn pg_udt_name(&self) -> String {
        match self {
            SqlType::Bool => "bool".into(),
            SqlType::Int1 | SqlType::Int2 => "int2".into(),
            SqlType::Int4 | SqlType::Year => "int4".into(),
            SqlType::Int8 => "int8".into(),
            SqlType::Float4 => "float4".into(),
            SqlType::Float8 => "float8".into(),
            SqlType::Numeric { .. } => "numeric".into(),
            SqlType::Text => "text".into(),
            SqlType::Varchar { .. } => "varchar".into(),
            SqlType::Char { .. } => "bpchar".into(),
            SqlType::Bytea => "bytea".into(),
            SqlType::Date => "date".into(),
            SqlType::Time => "time".into(),
            SqlType::Timestamp => "timestamp".into(),
            SqlType::TimestampTz => "timestamptz".into(),
            SqlType::Interval => "interval".into(),
            SqlType::Uuid => "uuid".into(),
            SqlType::Json => "json".into(),
            SqlType::Jsonb => "jsonb".into(),
            SqlType::Enum { name, .. } => name.clone(),
            SqlType::Array { element } => format!("_{}", element.pg_udt_name()),
            SqlType::Vector { family: kind, .. } => kind.name().into(),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// PostgreSQL `format_type()` output, e.g. `character varying(255)`.
    pub fn pg_format_type(&self) -> String {
        match self {
            SqlType::Bool => "boolean".into(),
            SqlType::Int1 | SqlType::Int2 => "smallint".into(),
            SqlType::Int4 | SqlType::Year => "integer".into(),
            SqlType::Int8 => "bigint".into(),
            SqlType::Float4 => "real".into(),
            SqlType::Float8 => "double precision".into(),
            SqlType::Numeric { precision: Some(p), scale } => format!("numeric({},{})", p, scale.unwrap_or(0)),
            SqlType::Numeric { .. } => "numeric".into(),
            SqlType::Text => "text".into(),
            SqlType::Varchar { len: Some(n) } => format!("character varying({n})"),
            SqlType::Varchar { len: None } => "character varying".into(),
            SqlType::Char { len } => format!("character({})", len.unwrap_or(1)),
            SqlType::Bytea => "bytea".into(),
            SqlType::Date => "date".into(),
            SqlType::Time => "time without time zone".into(),
            SqlType::Timestamp => "timestamp without time zone".into(),
            SqlType::TimestampTz => "timestamp with time zone".into(),
            SqlType::Interval => "interval".into(),
            SqlType::Uuid => "uuid".into(),
            SqlType::Json => "json".into(),
            SqlType::Jsonb => "jsonb".into(),
            SqlType::Enum { name, .. } => name.clone(),
            SqlType::Array { element } => format!("{}[]", element.pg_format_type()),
            SqlType::Vector { family: kind, dim: Some(n) } => format!("{}({n})", kind.name()),
            SqlType::Vector { family: kind, dim: None } => kind.name().into(),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// `information_schema.columns.data_type` for PostgreSQL.
    pub fn pg_info_data_type(&self) -> String {
        match self {
            SqlType::Array { .. } => "ARRAY".into(),
            SqlType::Enum { .. } | SqlType::Vector { .. } => "USER-DEFINED".into(),
            SqlType::Numeric { .. } => "numeric".into(),
            SqlType::Varchar { .. } => "character varying".into(),
            SqlType::Char { .. } => "character".into(),
            other => other.pg_format_type(),
        }
    }

    /// MySQL `DATA_TYPE` (information_schema) name.
    pub fn mysql_data_type(&self) -> String {
        match self {
            SqlType::Vector { .. } => "vector".into(),
            SqlType::Bool | SqlType::Int1 => "tinyint".into(),
            SqlType::Int2 => "smallint".into(),
            SqlType::Int4 => "int".into(),
            SqlType::Int8 => "bigint".into(),
            SqlType::Float4 => "float".into(),
            SqlType::Float8 => "double".into(),
            SqlType::Numeric { .. } => "decimal".into(),
            SqlType::Text | SqlType::Other { .. } | SqlType::Interval | SqlType::Array { .. } => "text".into(),
            SqlType::Varchar { .. } => "varchar".into(),
            SqlType::Char { .. } => "char".into(),
            SqlType::Bytea => "blob".into(),
            SqlType::Date => "date".into(),
            SqlType::Time => "time".into(),
            SqlType::Timestamp => "datetime".into(),
            SqlType::TimestampTz => "timestamp".into(),
            SqlType::Uuid => "char".into(),
            SqlType::Json | SqlType::Jsonb => "json".into(),
            SqlType::Year => "year".into(),
            SqlType::Enum { .. } => "enum".into(),
        }
    }

    /// MySQL `COLUMN_TYPE`, e.g. `varchar(255)`, `tinyint(1)`, `enum('a','b')`.
    pub fn mysql_column_type(&self) -> String {
        match self {
            SqlType::Vector { dim: Some(n), .. } => format!("vector({n})"),
            SqlType::Bool => "tinyint(1)".into(),
            SqlType::Numeric { precision, scale } => {
                format!("decimal({},{})", precision.unwrap_or(10), scale.unwrap_or(0))
            }
            SqlType::Varchar { len } => format!("varchar({})", len.unwrap_or(255)),
            SqlType::Char { len } => format!("char({})", len.unwrap_or(1)),
            SqlType::Uuid => "char(36)".into(),
            SqlType::Enum { values, .. } => {
                format!("enum({})", values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect::<Vec<_>>().join(","))
            }
            other => other.mysql_data_type(),
        }
    }

    /// Parse a ClickHouse type (`Nullable(Int32)`, `LowCardinality(String)`, `DateTime64(3)`,
    /// `Array(UInt8)`, `Decimal(10, 2)`). Its names clash with PostgreSQL's (`Int8` is 8 bits here).
    pub fn parse_clickhouse(decl: &str) -> SqlType {
        let t = decl.trim();
        let inner = |prefix: &str| t.strip_prefix(prefix).and_then(|r| r.strip_suffix(')'));
        if let Some(i) = inner("Nullable(").or_else(|| inner("LowCardinality(")) {
            return Self::parse_clickhouse(i);
        }
        if let Some(i) = inner("Array(") {
            return SqlType::Array { element: Box::new(Self::parse_clickhouse(i)) };
        }
        let (name, args) = match t.find('(') {
            Some(p) => (&t[..p], t[p + 1..].trim_end_matches(')')),
            None => (t, ""),
        };
        let nums: Vec<u32> = args.split(',').filter_map(|a| a.trim().parse().ok()).collect();
        match name {
            "Bool" | "Boolean" => SqlType::Bool,
            "Int8" | "UInt8" => SqlType::Int2,
            "Int16" => SqlType::Int2,
            "UInt16" | "Int32" => SqlType::Int4,
            "UInt32" | "Int64" => SqlType::Int8,
            "UInt64" | "Int128" | "UInt128" | "Int256" | "UInt256" => SqlType::Numeric { precision: Some(39), scale: Some(0) },
            "Float32" => SqlType::Float4,
            "Float64" => SqlType::Float8,
            "Decimal" => SqlType::Numeric { precision: nums.first().copied(), scale: nums.get(1).copied().or(Some(0)) },
            "Decimal32" => SqlType::Numeric { precision: Some(9), scale: nums.first().copied() },
            "Decimal64" => SqlType::Numeric { precision: Some(18), scale: nums.first().copied() },
            "Decimal128" => SqlType::Numeric { precision: Some(38), scale: nums.first().copied() },
            "String" => SqlType::Text,
            "FixedString" => SqlType::Char { len: nums.first().copied() },
            "Date" | "Date32" => SqlType::Date,
            "DateTime" | "DateTime64" if args.contains('\'') => SqlType::TimestampTz,
            "DateTime" | "DateTime64" => SqlType::Timestamp,
            "UUID" => SqlType::Uuid,
            "JSON" | "Object" => SqlType::Json,
            "IPv4" | "IPv6" | "Enum8" | "Enum16" => SqlType::Varchar { len: None },
            _ => SqlType::Other { name: t.to_owned() },
        }
    }

    /// Oracle column type, e.g. `NUMBER(10)`, `VARCHAR2(255 CHAR)`, `TIMESTAMP WITH TIME ZONE`.
    pub fn oracle_column_type(&self) -> String {
        match self {
            SqlType::Vector { family: crate::vector::VectorKind::Sparsevec, dim } => {
                format!("VECTOR({}, FLOAT32, SPARSE)", dim.map(|n| n.to_string()).unwrap_or_else(|| "*".into()))
            }
            SqlType::Vector { dim, .. } => format!("VECTOR({}, FLOAT32)", dim.map(|n| n.to_string()).unwrap_or_else(|| "*".into())),
            SqlType::Bool | SqlType::Int1 => "NUMBER(3)".into(),
            SqlType::Int2 => "NUMBER(5)".into(),
            SqlType::Int4 | SqlType::Year => "NUMBER(10)".into(),
            SqlType::Int8 => "NUMBER(19)".into(),
            SqlType::Float4 => "BINARY_FLOAT".into(),
            SqlType::Float8 => "BINARY_DOUBLE".into(),
            SqlType::Numeric { precision: Some(p), scale } => format!("NUMBER({},{})", p.min(&38), scale.unwrap_or(0)),
            SqlType::Numeric { .. } => "NUMBER".into(),
            SqlType::Varchar { len: Some(n) } if *n <= 4000 => format!("VARCHAR2({n} CHAR)"),
            SqlType::Char { len } if len.unwrap_or(1) <= 2000 => format!("CHAR({} CHAR)", len.unwrap_or(1)),
            SqlType::Enum { .. } => "VARCHAR2(255 CHAR)".into(),
            SqlType::Uuid => "VARCHAR2(36 CHAR)".into(),
            SqlType::Time => "VARCHAR2(32 CHAR)".into(),
            SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. } | SqlType::Json | SqlType::Jsonb | SqlType::Array { .. } => {
                "CLOB".into()
            }
            SqlType::Bytea => "BLOB".into(),
            SqlType::Date => "DATE".into(),
            SqlType::Timestamp => "TIMESTAMP".into(),
            SqlType::TimestampTz => "TIMESTAMP WITH TIME ZONE".into(),
            SqlType::Interval => "INTERVAL DAY TO SECOND".into(),
            SqlType::Other { name } => name.to_ascii_uppercase(),
        }
    }

    /// Parse a GoogleSQL type (Spanner or BigQuery): `INT64`, `STRING(MAX)`, `NUMERIC`,
    /// `ARRAY<STRING>`, `STRUCT<…>`.
    pub fn parse_googlesql(decl: &str) -> SqlType {
        let t = decl.trim().to_ascii_uppercase();
        if let Some(e) = t.strip_prefix("ARRAY<").and_then(|r| r.strip_suffix('>')) {
            return SqlType::Array { element: Box::new(Self::parse_googlesql(e)) };
        }
        let (name, arg) = match t.find('(') {
            Some(p) => (&t[..p], t[p + 1..].trim_end_matches(')').trim()),
            None => (t.as_str(), ""),
        };
        match name {
            "INT64" | "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" | "BYTEINT" => SqlType::Int8,
            "FLOAT64" | "FLOAT" => SqlType::Float8,
            "FLOAT32" => SqlType::Float4,
            "NUMERIC" | "DECIMAL" => SqlType::Numeric { precision: Some(38), scale: Some(9) },
            "BIGNUMERIC" | "BIGDECIMAL" => SqlType::Numeric { precision: None, scale: None },
            "BOOL" | "BOOLEAN" => SqlType::Bool,
            "STRING" => SqlType::Varchar { len: arg.parse().ok() },
            "BYTES" => SqlType::Bytea,
            "DATE" => SqlType::Date,
            "TIMESTAMP" => SqlType::TimestampTz,
            "DATETIME" => SqlType::Timestamp,
            "TIME" => SqlType::Time,
            "JSON" => SqlType::Json,
            "INTERVAL" => SqlType::Interval,
            _ if name.starts_with("STRUCT") => SqlType::Json,
            _ => SqlType::Other { name: decl.trim().to_owned() },
        }
    }

    /// GoogleSQL column type; `spanner` adds the lengths Spanner requires (`STRING(MAX)`).
    pub fn googlesql_column_type(&self, spanner: bool) -> String {
        let max = |n: Option<u32>| {
            if spanner { format!("({})", n.map(|n| n.to_string()).unwrap_or_else(|| "MAX".into())) } else { String::new() }
        };
        match self {
            SqlType::Vector { dim, .. } if spanner => match dim {
                Some(n) => format!("ARRAY<FLOAT32>(vector_length=>{n})"),
                None => "ARRAY<FLOAT32>".into(),
            },
            SqlType::Vector { .. } => "ARRAY<FLOAT64>".into(),
            SqlType::Bool => "BOOL".into(),
            SqlType::Int1 | SqlType::Int2 | SqlType::Int4 | SqlType::Int8 | SqlType::Year => "INT64".into(),
            SqlType::Float4 => {
                if spanner {
                    "FLOAT32".into()
                } else {
                    "FLOAT64".into()
                }
            }
            SqlType::Float8 => "FLOAT64".into(),
            SqlType::Numeric { precision: Some(p), scale } if *p <= 29 && scale.unwrap_or(0) <= 9 => "NUMERIC".into(),
            SqlType::Numeric { .. } => {
                if spanner {
                    "NUMERIC".into()
                } else {
                    "BIGNUMERIC".into()
                }
            }
            SqlType::Varchar { len } | SqlType::Char { len } => format!("STRING{}", max(*len)),
            SqlType::Text | SqlType::Enum { .. } => format!("STRING{}", max(None)),
            SqlType::Bytea => format!("BYTES{}", max(None)),
            SqlType::Date => "DATE".into(),
            SqlType::Time => {
                if spanner {
                    format!("STRING{}", max(Some(32)))
                } else {
                    "TIME".into()
                }
            }
            SqlType::Timestamp => {
                if spanner {
                    "TIMESTAMP".into()
                } else {
                    "DATETIME".into()
                }
            }
            SqlType::TimestampTz => "TIMESTAMP".into(),
            SqlType::Interval => {
                if spanner {
                    format!("STRING{}", max(Some(64)))
                } else {
                    "INTERVAL".into()
                }
            }
            SqlType::Uuid => format!("STRING{}", max(Some(36))),
            SqlType::Json | SqlType::Jsonb => "JSON".into(),
            SqlType::Array { element } => format!("ARRAY<{}>", element.googlesql_column_type(spanner)),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// Parse a CQL type (`text`, `bigint`, `timeuuid`, `set<text>`, `frozen<map<text, int>>`).
    pub fn parse_cql(decl: &str) -> SqlType {
        let t = decl.trim().to_ascii_lowercase();
        if let Some(i) = t.strip_prefix("frozen<").and_then(|r| r.strip_suffix('>')) {
            return Self::parse_cql(i);
        }
        if let Some(e) = t.strip_prefix("list<").or_else(|| t.strip_prefix("set<")).and_then(|r| r.strip_suffix('>')) {
            return SqlType::Array { element: Box::new(Self::parse_cql(e)) };
        }
        match t.as_str() {
            "boolean" => SqlType::Bool,
            "tinyint" => SqlType::Int1,
            "smallint" => SqlType::Int2,
            "int" => SqlType::Int4,
            "bigint" | "counter" => SqlType::Int8,
            "varint" => SqlType::Numeric { precision: None, scale: Some(0) },
            "decimal" => SqlType::Numeric { precision: None, scale: None },
            "float" => SqlType::Float4,
            "double" => SqlType::Float8,
            "text" | "varchar" | "ascii" | "inet" => SqlType::Text,
            "blob" => SqlType::Bytea,
            "date" => SqlType::Date,
            "time" => SqlType::Time,
            "timestamp" => SqlType::TimestampTz,
            "uuid" | "timeuuid" => SqlType::Uuid,
            "duration" => SqlType::Interval,
            _ if t.starts_with("map<") || t.starts_with("tuple<") => SqlType::Json,
            _ => SqlType::Other { name: t },
        }
    }

    /// CQL column type, e.g. `bigint`, `decimal`, `list<text>`.
    pub fn cql_column_type(&self) -> String {
        match self {
            SqlType::Vector { dim: Some(n), .. } => format!("vector<float, {n}>"),
            SqlType::Vector { .. } => "list<float>".into(),
            SqlType::Bool => "boolean".into(),
            SqlType::Int1 => "tinyint".into(),
            SqlType::Int2 => "smallint".into(),
            SqlType::Int4 | SqlType::Year => "int".into(),
            SqlType::Int8 => "bigint".into(),
            SqlType::Float4 => "float".into(),
            SqlType::Float8 => "double".into(),
            SqlType::Numeric { scale: Some(0), precision: None } => "varint".into(),
            SqlType::Numeric { .. } => "decimal".into(),
            SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. } | SqlType::Enum { .. } | SqlType::Json | SqlType::Jsonb => {
                "text".into()
            }
            SqlType::Interval => "duration".into(),
            SqlType::Bytea => "blob".into(),
            SqlType::Date => "date".into(),
            SqlType::Time => "time".into(),
            SqlType::Timestamp | SqlType::TimestampTz => "timestamp".into(),
            SqlType::Uuid => "uuid".into(),
            SqlType::Array { element } => format!("list<{}>", element.cql_column_type()),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// ClickHouse column type (without `Nullable`), e.g. `Int64`, `Decimal(10, 2)`, `DateTime64(6)`.
    pub fn clickhouse_column_type(&self) -> String {
        match self {
            SqlType::Vector { .. } => "Array(Float32)".into(),
            SqlType::Bool => "Bool".into(),
            SqlType::Int1 => "Int8".into(),
            SqlType::Int2 => "Int16".into(),
            SqlType::Int4 | SqlType::Year => "Int32".into(),
            SqlType::Int8 => "Int64".into(),
            SqlType::Float4 => "Float32".into(),
            SqlType::Float8 => "Float64".into(),
            SqlType::Numeric { precision: Some(p), scale } => format!("Decimal({}, {})", p.min(&76), scale.unwrap_or(0)),
            SqlType::Numeric { .. } => "Decimal(38, 10)".into(),
            SqlType::Char { len: Some(n) } => format!("FixedString({n})"),
            SqlType::Text
            | SqlType::Varchar { .. }
            | SqlType::Char { .. }
            | SqlType::Enum { .. }
            | SqlType::Interval
            | SqlType::Time
            | SqlType::Bytea => "String".into(),
            SqlType::Json | SqlType::Jsonb => "String".into(),
            SqlType::Date => "Date".into(),
            SqlType::Timestamp => "DateTime64(6)".into(),
            SqlType::TimestampTz => "DateTime64(6, 'UTC')".into(),
            SqlType::Uuid => "UUID".into(),
            SqlType::Array { element } => format!("Array({})", element.clickhouse_column_type()),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// SQL Server column type, e.g. `nvarchar(255)`, `decimal(10,2)`, `datetime2`.
    pub fn mssql_column_type(&self) -> String {
        match self {
            SqlType::Vector { dim: Some(n), .. } => format!("vector({n})"),
            SqlType::Vector { .. } => "nvarchar(max)".into(),
            SqlType::Bool => "bit".into(),
            SqlType::Int1 | SqlType::Int2 => "smallint".into(),
            SqlType::Int4 | SqlType::Year => "int".into(),
            SqlType::Int8 => "bigint".into(),
            SqlType::Float4 => "real".into(),
            SqlType::Float8 => "float".into(),
            SqlType::Numeric { precision: Some(p), scale } => format!("decimal({},{})", p.min(&38), scale.unwrap_or(0)),
            SqlType::Numeric { .. } => "decimal(38,10)".into(),
            SqlType::Varchar { len: Some(n) } if *n <= 4000 => format!("nvarchar({n})"),
            SqlType::Char { len } if len.unwrap_or(1) <= 4000 => format!("nchar({})", len.unwrap_or(1)),
            SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. } | SqlType::Json | SqlType::Jsonb | SqlType::Array { .. } => {
                "nvarchar(max)".into()
            }
            SqlType::Enum { .. } => "nvarchar(255)".into(),
            SqlType::Interval => "nvarchar(64)".into(),
            SqlType::Bytea => "varbinary(max)".into(),
            SqlType::Date => "date".into(),
            SqlType::Time => "time".into(),
            SqlType::Timestamp => "datetime2".into(),
            SqlType::TimestampTz => "datetimeoffset".into(),
            SqlType::Uuid => "uniqueidentifier".into(),
            SqlType::Other { name } => name.clone(),
        }
    }

    /// Character length limit, if any.
    pub fn char_len(&self) -> Option<u32> {
        match self {
            SqlType::Varchar { len } | SqlType::Char { len } => *len,
            _ => None,
        }
    }

    /// Human-facing type name in the given dialect (used by the UI and `SHOW CREATE`).
    pub fn display(&self, dialect: Dialect) -> String {
        match dialect {
            Dialect::MySql => self.mysql_column_type(),
            _ => self.pg_format_type(),
        }
    }
}

/// Extract the raw (case-preserving) text between the outermost parentheses.
fn raw_args(raw: &str) -> Option<&str> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    (close > open).then(|| &raw[open + 1..close])
}

/// Parse `'a', 'b''c'` into `["a", "b'c"]`.
pub(crate) fn parse_quoted_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            let quote = c;
            let mut value = String::new();
            while let Some(ch) = chars.next() {
                if ch == quote {
                    if chars.peek() == Some(&quote) {
                        value.push(quote);
                        chars.next();
                    } else {
                        break;
                    }
                } else {
                    value.push(ch);
                }
            }
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(s: &str) -> SqlType {
        SqlType::parse_simple(s, Dialect::Postgres)
    }

    #[test]
    fn parses_common_postgres_types() {
        assert_eq!(pg("integer"), SqlType::Int4);
        assert_eq!(pg("VARCHAR(255)"), SqlType::Varchar { len: Some(255) });
        assert_eq!(pg("character varying(20)"), SqlType::Varchar { len: Some(20) });
        assert_eq!(pg("timestamp with time zone"), SqlType::TimestampTz);
        assert_eq!(pg("TIMESTAMP(3) WITH TIME ZONE"), SqlType::TimestampTz);
        assert_eq!(pg("timestamptz"), SqlType::TimestampTz);
        assert_eq!(pg("numeric(10,2)"), SqlType::Numeric { precision: Some(10), scale: Some(2) });
        assert_eq!(pg("text[]"), SqlType::Array { element: Box::new(SqlType::Text) });
        assert_eq!(pg("INTEGER[]").pg_oid(), 1007);
        let serial = SqlType::parse("serial", Dialect::Postgres, &|_| None);
        assert_eq!(serial.ty, SqlType::Int4);
        assert_eq!(serial.serial, Some(SerialKind::Serial));
    }

    #[test]
    fn parses_mysql_types() {
        let my = |s| SqlType::parse_simple(s, Dialect::MySql);
        assert_eq!(my("tinyint(1)"), SqlType::Bool);
        assert_eq!(my("INT(11)"), SqlType::Int4);
        assert_eq!(my("int unsigned"), SqlType::Int8);
        assert_eq!(my("DATETIME(6)"), SqlType::Timestamp);
        assert_eq!(my("enum('small','Large')"), SqlType::Enum { name: String::new(), values: vec!["small".into(), "Large".into()] });
        assert_eq!(my("decimal(8,3)").mysql_column_type(), "decimal(8,3)");
    }

    #[test]
    fn resolves_named_enums() {
        let t = SqlType::parse("mood", Dialect::Postgres, &|n| (n == "mood").then(|| vec!["happy".into()]));
        assert_eq!(t.ty, SqlType::Enum { name: "mood".into(), values: vec!["happy".into()] });
    }
}
