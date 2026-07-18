//! Runtime values and their PostgreSQL type identities.

use core::fmt;
use core::fmt::Write as _;

use super::numeric::Numeric;

/// OIDs from PostgreSQL's `pg_type` (stable, documented catalog values).
pub mod oid {
    pub const BOOL: i32 = 16;
    pub const BYTEA: i32 = 17;
    pub const INT8: i32 = 20;
    pub const INT2: i32 = 21;
    pub const INT4: i32 = 23;
    pub const TEXT: i32 = 25;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const BPCHAR: i32 = 1042;
    pub const VARCHAR: i32 = 1043;
    pub const DATE: i32 = 1082;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const TIME: i32 = 1083;
    pub const INTERVAL: i32 = 1186;
    pub const JSON: i32 = 114;
    pub const JSONB: i32 = 3802;
    pub const UUID: i32 = 2950;
    pub const NUMERIC: i32 = 1700;
    /// PostgreSQL's pseudo-type for a string literal / parameter before its
    /// type is resolved from context.
    pub const UNKNOWN: i32 = 705;
}

/// Column types the engine stores. A deliberately small, growing set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Bool,
    /// `smallint`/`int2`. Stored as an i32 but range-checked to ±32767. Its
    /// wire OID stays int4 so binary output width matches the 4-byte payload.
    Int2,
    Int4,
    Int8,
    /// `real`/`float4`. Values are rounded to single precision; stored and
    /// sent as float8 (OID/width) since there is no 4-byte float Datum.
    Float4,
    Float8,
    Text,
    /// `varchar`/`character varying`: text storage, but reports OID 1043.
    Varchar,
    /// `char(n)`/`character`/`bpchar`: blank-padded to length, OID 1042.
    Bpchar,
    /// Days since 2000-01-01.
    Date,
    /// Microseconds since 2000-01-01 (no zone).
    Timestamp,
    /// Microseconds since 2000-01-01 UTC.
    Timestamptz,
    /// Microseconds since midnight (time of day, no zone).
    Time,
    /// A duration (months, days, microseconds).
    Interval,
    /// Textual JSON (stored verbatim).
    Json,
    /// Binary/normalized JSON (canonicalized on input).
    Jsonb,
    /// A one-dimensional array of a scalar element type.
    Array(ArrElem),
    Uuid,
    Bytea,
    Numeric,
}

impl ColType {
    /// Maps a SQL type name (already case-folded) to a column type.
    pub fn from_sql_name(name: &str) -> Option<Self> {
        // `elem[]` is a one-dimensional array of a scalar element type.
        if let Some(base) = name.strip_suffix("[]") {
            return ArrElem::from_coltype(ColType::from_sql_name(base)?).map(ColType::Array);
        }
        Some(match name {
            "bool" | "boolean" => Self::Bool,
            "int" | "int4" | "integer" | "serial" | "serial4" => Self::Int4,
            "smallint" | "int2" | "smallserial" | "serial2" => Self::Int2,
            "bigint" | "int8" | "bigserial" | "serial8" => Self::Int8,
            "float8" | "float" | "double precision" => Self::Float8,
            "float4" | "real" => Self::Float4,
            // `name` and the `reg*` object-identifier types render as text for
            // catalog introspection.
            "text" | "name" | "regtype" | "regclass" | "regproc" | "regprocedure"
            | "regrole" | "regnamespace" | "regoper" | "regoperator" => Self::Text,
            "oid" => Self::Int4,
            "varchar" | "character varying" => Self::Varchar,
            "char" | "character" | "bpchar" => Self::Bpchar,
            "date" => Self::Date,
            "timestamp" => Self::Timestamp,
            "timestamptz" => Self::Timestamptz,
            "time" => Self::Time,
            "interval" => Self::Interval,
            "json" => Self::Json,
            "jsonb" => Self::Jsonb,
            "uuid" => Self::Uuid,
            "bytea" => Self::Bytea,
            "numeric" | "decimal" | "dec" => Self::Numeric,
            _ => return None,
        })
    }

    pub fn oid(self) -> i32 {
        match self {
            Self::Bool => oid::BOOL,
            // int2/float4 report int4/float8 OIDs so the binary payload width
            // (4/8 bytes, from the i32/f64 storage) matches the declared type.
            Self::Int2 => oid::INT4,
            Self::Int4 => oid::INT4,
            Self::Int8 => oid::INT8,
            Self::Float4 => oid::FLOAT8,
            Self::Float8 => oid::FLOAT8,
            Self::Text => oid::TEXT,
            Self::Varchar => oid::VARCHAR,
            Self::Bpchar => oid::BPCHAR,
            Self::Date => oid::DATE,
            Self::Timestamp => oid::TIMESTAMP,
            Self::Timestamptz => oid::TIMESTAMPTZ,
            Self::Time => oid::TIME,
            Self::Interval => oid::INTERVAL,
            Self::Json => oid::JSON,
            Self::Jsonb => oid::JSONB,
            Self::Array(e) => e.array_oid(),
            Self::Uuid => oid::UUID,
            Self::Bytea => oid::BYTEA,
            Self::Numeric => oid::NUMERIC,
        }
    }

    pub fn typlen(self) -> i16 {
        match self {
            Self::Bool => 1,
            Self::Int2 | Self::Int4 | Self::Date => 4,
            Self::Int8 | Self::Float4 | Self::Float8 | Self::Timestamp | Self::Timestamptz | Self::Time => 8,
            Self::Interval => 16,
            Self::Uuid => 16,
            Self::Text | Self::Varchar | Self::Bpchar | Self::Bytea | Self::Numeric | Self::Json | Self::Jsonb => -1,
            Self::Array(_) => -1,
        }
    }

    /// The underlying storage/Datum type: int2 stores as int4, float4 as
    /// float8, varchar/bpchar as text. Used where behavior is width-driven.
    pub fn storage(self) -> ColType {
        match self {
            Self::Int2 => Self::Int4,
            Self::Float4 => Self::Float8,
            Self::Varchar | Self::Bpchar => Self::Text,
            other => other,
        }
    }

    /// The catalog (internal) name, used to title cast result columns.
    pub fn internal_name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float4 => "float4",
            Self::Float8 => "float8",
            Self::Text => "text",
            Self::Varchar => "varchar",
            Self::Bpchar => "bpchar",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Timestamptz => "timestamptz",
            Self::Time => "time",
            Self::Interval => "interval",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::Array(_) => "array",
            Self::Uuid => "uuid",
            Self::Bytea => "bytea",
            Self::Numeric => "numeric",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "boolean",
            Self::Int2 => "smallint",
            Self::Int4 => "integer",
            Self::Int8 => "bigint",
            Self::Float4 => "real",
            Self::Float8 => "double precision",
            Self::Text => "text",
            Self::Varchar => "character varying",
            Self::Bpchar => "character",
            Self::Date => "date",
            Self::Timestamp => "timestamp without time zone",
            Self::Timestamptz => "timestamp with time zone",
            Self::Time => "time without time zone",
            Self::Interval => "interval",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::Array(_) => "array",
            Self::Uuid => "uuid",
            Self::Bytea => "bytea",
            Self::Numeric => "numeric",
        }
    }
}

/// The element type of a one-dimensional array. A distinct (non-recursive)
/// enum so `ColType`/`Datum` stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrElem {
    Bool,
    Int4,
    Int8,
    Float8,
    Text,
    Numeric,
    Date,
    Timestamp,
    Timestamptz,
}

impl ArrElem {
    /// The array element type matching a scalar datum's runtime type.
    pub fn from_datum(d: &Datum) -> Option<ArrElem> {
        Some(match d {
            Datum::Bool(_) => ArrElem::Bool,
            Datum::Int4(_) => ArrElem::Int4,
            Datum::Int8(_) => ArrElem::Int8,
            Datum::Float8(_) => ArrElem::Float8,
            Datum::Text(_) => ArrElem::Text,
            Datum::Numeric(_) => ArrElem::Numeric,
            Datum::Date(_) => ArrElem::Date,
            Datum::Timestamp(_) => ArrElem::Timestamp,
            Datum::Timestamptz(_) => ArrElem::Timestamptz,
            _ => return None,
        })
    }

    pub fn from_coltype(c: ColType) -> Option<ArrElem> {
        Some(match c.storage() {
            ColType::Bool => ArrElem::Bool,
            ColType::Int4 => ArrElem::Int4,
            ColType::Int8 => ArrElem::Int8,
            ColType::Float8 => ArrElem::Float8,
            ColType::Text => ArrElem::Text,
            ColType::Numeric => ArrElem::Numeric,
            ColType::Date => ArrElem::Date,
            ColType::Timestamp => ArrElem::Timestamp,
            ColType::Timestamptz => ArrElem::Timestamptz,
            _ => return None,
        })
    }

    pub fn to_coltype(self) -> ColType {
        match self {
            ArrElem::Bool => ColType::Bool,
            ArrElem::Int4 => ColType::Int4,
            ArrElem::Int8 => ColType::Int8,
            ArrElem::Float8 => ColType::Float8,
            ArrElem::Text => ColType::Text,
            ArrElem::Numeric => ColType::Numeric,
            ArrElem::Date => ColType::Date,
            ArrElem::Timestamp => ColType::Timestamp,
            ArrElem::Timestamptz => ColType::Timestamptz,
        }
    }

    /// The PostgreSQL array-type OID for this element type.
    pub fn array_oid(self) -> i32 {
        match self {
            ArrElem::Bool => 1000,
            ArrElem::Int4 => 1007,
            ArrElem::Int8 => 1016,
            ArrElem::Float8 => 1022,
            ArrElem::Text => 1009,
            ArrElem::Numeric => 1231,
            ArrElem::Date => 1182,
            ArrElem::Timestamp => 1115,
            ArrElem::Timestamptz => 1185,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            ArrElem::Bool => 0,
            ArrElem::Int4 => 1,
            ArrElem::Int8 => 2,
            ArrElem::Float8 => 3,
            ArrElem::Text => 4,
            ArrElem::Numeric => 5,
            ArrElem::Date => 6,
            ArrElem::Timestamp => 7,
            ArrElem::Timestamptz => 8,
        }
    }

    pub fn from_code(c: u8) -> Option<ArrElem> {
        Some(match c {
            0 => ArrElem::Bool,
            1 => ArrElem::Int4,
            2 => ArrElem::Int8,
            3 => ArrElem::Float8,
            4 => ArrElem::Text,
            5 => ArrElem::Numeric,
            6 => ArrElem::Date,
            7 => ArrElem::Timestamp,
            8 => ArrElem::Timestamptz,
            _ => return None,
        })
    }
}

/// A PostgreSQL `interval`: three independent fields (months, days, and
/// microseconds) that add to a date/timestamp separately — a month is a
/// calendar month, a day is 24 hours only in the absence of a DST shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

/// A runtime value. Text borrows from the statement arena or storage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Datum<'a> {
    Null,
    Bool(bool),
    Int4(i32),
    Int8(i64),
    Float8(f64),
    Text(&'a str),
    /// Days since 2000-01-01.
    Date(i32),
    /// Microseconds since 2000-01-01 (naive).
    Timestamp(i64),
    /// Microseconds since 2000-01-01 UTC.
    Timestamptz(i64),
    /// Microseconds since midnight (time of day).
    Time(i64),
    /// A duration.
    Interval(Interval),
    /// JSON text; `jsonb` is true for the binary/normalized form.
    Json { text: &'a str, jsonb: bool },
    /// A one-dimensional array: the element type plus the serialized element
    /// bytes (`u16 count` then `u32 len + element encoding` per element). Kept
    /// as raw bytes so decoding from storage needs no separate allocation.
    Array { elem: ArrElem, raw: &'a [u8] },
    Uuid([u8; 16]),
    Bytea(&'a [u8]),
    Numeric(Numeric<'a>),
}

impl<'a> Datum<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    pub fn type_oid(&self) -> i32 {
        match self {
            Datum::Null => oid::TEXT,
            Datum::Bool(_) => oid::BOOL,
            Datum::Int4(_) => oid::INT4,
            Datum::Int8(_) => oid::INT8,
            Datum::Float8(_) => oid::FLOAT8,
            Datum::Text(_) => oid::TEXT,
            Datum::Date(_) => oid::DATE,
            Datum::Timestamp(_) => oid::TIMESTAMP,
            Datum::Timestamptz(_) => oid::TIMESTAMPTZ,
            Datum::Time(_) => oid::TIME,
            Datum::Interval(_) => oid::INTERVAL,
            Datum::Json { jsonb: false, .. } => oid::JSON,
            Datum::Json { jsonb: true, .. } => oid::JSONB,
            Datum::Array { elem, .. } => elem.array_oid(),
            Datum::Uuid(_) => oid::UUID,
            Datum::Bytea(_) => oid::BYTEA,
            Datum::Numeric(_) => oid::NUMERIC,
        }
    }
}

/// Text-format rendering per PostgreSQL output conventions: booleans as
/// `t`/`f`, floats via Rust's shortest-roundtrip formatting.
impl fmt::Display for Datum<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Datum::Null => Ok(()), // never rendered; NULL is a column-length of -1
            Datum::Bool(true) => f.write_str("t"),
            Datum::Bool(false) => f.write_str("f"),
            Datum::Int4(v) => write!(f, "{v}"),
            Datum::Int8(v) => write!(f, "{v}"),
            Datum::Float8(v) => {
                if v.is_infinite() {
                    f.write_str(if *v > 0.0 { "Infinity" } else { "-Infinity" })
                } else if v.is_nan() {
                    f.write_str("NaN")
                } else {
                    write!(f, "{v}")
                }
            }
            Datum::Text(s) => f.write_str(s),
            Datum::Date(d) => f.write_str(super::datetime::format_date(*d).as_str()),
            Datum::Timestamp(t) => {
                f.write_str(super::datetime::format_timestamp(*t, false).as_str())
            }
            Datum::Timestamptz(t) => {
                f.write_str(super::datetime::format_timestamp(*t, true).as_str())
            }
            Datum::Time(t) => f.write_str(super::datetime::format_time(*t).as_str()),
            Datum::Interval(iv) => f.write_str(super::datetime::format_interval(*iv).as_str()),
            Datum::Json { text, .. } => f.write_str(text),
            Datum::Array { elem, raw } => super::array::write(f, *elem, raw),
            Datum::Uuid(b) => {
                for (i, byte) in b.iter().enumerate() {
                    if matches!(i, 4 | 6 | 8 | 10) {
                        f.write_str("-")?;
                    }
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Datum::Bytea(b) => {
                f.write_str("\\x")?;
                for byte in *b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Datum::Numeric(n) => write!(f, "{n}"),
        }
    }
}

/// Renders one array element, quoting text that would otherwise be ambiguous
/// (empty, or containing a delimiter/brace/quote/backslash/whitespace), and
/// spelling NULL unquoted — matching PostgreSQL's array output.
pub(crate) fn write_array_elem(f: &mut fmt::Formatter<'_>, v: &Datum) -> fmt::Result {
    match v {
        Datum::Null => f.write_str("NULL"),
        Datum::Text(s) => {
            let needs_quote = s.is_empty()
                || s.eq_ignore_ascii_case("null")
                || s.chars().any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
            if needs_quote {
                f.write_str("\"")?;
                for c in s.chars() {
                    if c == '"' || c == '\\' {
                        f.write_char('\\')?;
                    }
                    f.write_char(c)?;
                }
                f.write_str("\"")
            } else {
                f.write_str(s)
            }
        }
        other => write!(f, "{other}"),
    }
}

/// Description of one result column.
#[derive(Debug, Clone, Copy)]
pub struct ColDesc<'a> {
    pub name: &'a str,
    pub type_oid: i32,
    pub typlen: i16,
}

impl<'a> ColDesc<'a> {
    pub fn new(name: &'a str, type_oid: i32, typlen: i16) -> Self {
        Self { name, type_oid, typlen }
    }

    pub fn of_type(name: &'a str, t: ColType) -> Self {
        Self::new(name, t.oid(), t.typlen())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rendering_matches_postgres_conventions() {
        assert_eq!(Datum::Bool(true).to_string(), "t");
        assert_eq!(Datum::Bool(false).to_string(), "f");
        assert_eq!(Datum::Int8(-42).to_string(), "-42");
        assert_eq!(Datum::Float8(2.5).to_string(), "2.5");
        assert_eq!(Datum::Float8(f64::INFINITY).to_string(), "Infinity");
        assert_eq!(Datum::Text("hi").to_string(), "hi");
    }

    #[test]
    fn type_names_map() {
        assert_eq!(ColType::from_sql_name("integer"), Some(ColType::Int4));
        assert_eq!(ColType::from_sql_name("float8"), Some(ColType::Float8));
        assert_eq!(ColType::from_sql_name("geometry"), None);
    }
}
