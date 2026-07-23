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
    pub const NAME: i32 = 19;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const BPCHAR: i32 = 1042;
    pub const VARCHAR: i32 = 1043;
    pub const DATE: i32 = 1082;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const TIME: i32 = 1083;
    pub const TIMETZ: i32 = 1266;
    pub const INTERVAL: i32 = 1186;
    pub const JSON: i32 = 114;
    pub const JSONB: i32 = 3802;
    pub const UUID: i32 = 2950;
    pub const NUMERIC: i32 = 1700;
    /// Fixed-length bit string `bit(n)`.
    pub const BIT: i32 = 1560;
    /// Variable-length bit string `bit varying(n)` / `varbit`.
    pub const VARBIT: i32 = 1562;
    pub const BIT_ARRAY: i32 = 1561;
    pub const VARBIT_ARRAY: i32 = 1563;
    // Multirange type OIDs (PostgreSQL 14+).
    pub const INT4MULTIRANGE: i32 = 4451;
    pub const NUMMULTIRANGE: i32 = 4532;
    pub const TSMULTIRANGE: i32 = 4533;
    pub const TSTZMULTIRANGE: i32 = 4534;
    pub const DATEMULTIRANGE: i32 = 4535;
    pub const INT8MULTIRANGE: i32 = 4536;
    /// PostgreSQL's pseudo-type for a string literal / parameter before its
    /// type is resolved from context.
    pub const UNKNOWN: i32 = 705;
    /// Anonymous composite / record type.
    pub const RECORD: i32 = 2249;
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
    /// `name`: PostgreSQL's identifier type (OID 19, typlen 64). Text storage;
    /// input truncates to 63 bytes.
    Name,
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
    /// Time of day carrying its own UTC offset.
    Timetz,
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
    /// A range type (int4range/numrange/…), stored as canonical text.
    Range(RangeKind),
    /// A bit string. `varying` = `false` is `bit(n)` (OID 1560), `true` is
    /// `bit varying` / `varbit` (OID 1562). Length is enforced at cast time,
    /// not tracked here.
    Bit { varying: bool },
    /// A multirange type (int4multirange/…), stored as canonical text.
    Multirange(RangeKind),
}

/// Base storage codes for the parameterized type families. They must stay far
/// enough apart that no two families can produce the same code: `Multirange`
/// once began at 28 and `Array` at 32, which made `bool[]` and `int4[]`
/// (32, 33) indistinguishable from `tsmultirange` and `tstzmultirange`, and
/// [`ColType::from_code`] resolved both to the multirange — so a restart
/// replayed those columns back as the wrong type, losing their values.
///
/// The two moved families are rebased clear of *every* code the old layout
/// could produce (20..=40), so a column written by a build predating this fix
/// decodes to `None` — a loud failure — instead of silently coming back as a
/// different type. Codes outside every assigned span decode to `None` too.
const RANGE_CODE_BASE: u8 = 20;
const MULTIRANGE_CODE_BASE: u8 = 48;
const ARRAY_CODE_BASE: u8 = 64;
/// How many `RangeKind`s there are, i.e. the width of each range family's span.
const RANGE_KINDS: u8 = 6;

impl ColType {
    /// Maps a SQL type name (already case-folded) to a column type.
    pub fn from_sql_name(name: &str) -> Option<Self> {
        // `element[]` is a one-dimensional array of a scalar element type.
        if let Some(base) = name.strip_suffix("[]") {
            return ArrElem::from_coltype(ColType::from_sql_name(base)?).map(ColType::Array);
        }
        if let Some(k) = RangeKind::from_name(name) {
            return Some(Self::Range(k));
        }
        if let Some(k) = RangeKind::from_multirange_name(name) {
            return Some(Self::Multirange(k));
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
            "text" | "regtype" | "regclass" | "regproc" | "regprocedure"
            | "regrole" | "regnamespace" | "regoper" | "regoperator" => Self::Text,
            "name" => Self::Name,
            "oid" => Self::Int4,
            "varchar" | "character varying" => Self::Varchar,
            "char" | "character" | "bpchar" => Self::Bpchar,
            "date" => Self::Date,
            "timestamp" => Self::Timestamp,
            "timestamptz" => Self::Timestamptz,
            "time" => Self::Time,
            "timetz" | "time with time zone" => Self::Timetz,
            "interval" => Self::Interval,
            "json" => Self::Json,
            "jsonb" => Self::Jsonb,
            "uuid" => Self::Uuid,
            "bytea" => Self::Bytea,
            "numeric" | "decimal" | "dec" => Self::Numeric,
            "bit" => Self::Bit { varying: false },
            "varbit" | "bit varying" => Self::Bit { varying: true },
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
            Self::Name => oid::NAME,
            Self::Varchar => oid::VARCHAR,
            Self::Bpchar => oid::BPCHAR,
            Self::Date => oid::DATE,
            Self::Timestamp => oid::TIMESTAMP,
            Self::Timestamptz => oid::TIMESTAMPTZ,
            Self::Time => oid::TIME,
            Self::Timetz => oid::TIMETZ,
            Self::Interval => oid::INTERVAL,
            Self::Json => oid::JSON,
            Self::Jsonb => oid::JSONB,
            Self::Array(e) => e.array_oid(),
            Self::Uuid => oid::UUID,
            Self::Bytea => oid::BYTEA,
            Self::Numeric => oid::NUMERIC,
            Self::Range(k) => k.oid(),
            Self::Bit { varying: false } => oid::BIT,
            Self::Bit { varying: true } => oid::VARBIT,
            Self::Multirange(k) => k.multirange_oid(),
        }
    }

    pub fn typlen(self) -> i16 {
        match self {
            Self::Bool => 1,
            Self::Int2 | Self::Int4 | Self::Date => 4,
            Self::Int8 | Self::Float4 | Self::Float8 | Self::Timestamp | Self::Timestamptz | Self::Time => 8,
            Self::Timetz => 12,
            Self::Interval => 16,
            Self::Uuid => 16,
            Self::Name => 64,
            Self::Text | Self::Varchar | Self::Bpchar | Self::Bytea | Self::Numeric | Self::Json | Self::Jsonb => -1,
            Self::Array(_) | Self::Range(_) | Self::Bit { .. } | Self::Multirange(_) => -1,
        }
    }

    /// The underlying storage/Datum type: int2 stores as int4, float4 as
    /// float8, varchar/bpchar as text. Used where behavior is width-driven.
    pub fn storage(self) -> ColType {
        match self {
            Self::Int2 => Self::Int4,
            Self::Float4 => Self::Float8,
            Self::Varchar | Self::Bpchar | Self::Name => Self::Text,
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
            Self::Name => "name",
            Self::Varchar => "varchar",
            Self::Bpchar => "bpchar",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Timestamptz => "timestamptz",
            Self::Time => "time",
            Self::Timetz => "timetz",
            Self::Interval => "interval",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::Array(element) => element.array_name(),
            Self::Uuid => "uuid",
            Self::Bytea => "bytea",
            Self::Numeric => "numeric",
            Self::Range(k) => k.name(),
            Self::Bit { varying: false } => "bit",
            Self::Bit { varying: true } => "varbit",
            Self::Multirange(k) => k.multirange_name(),
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
            Self::Name => "name",
            Self::Varchar => "character varying",
            Self::Bpchar => "character",
            Self::Date => "date",
            Self::Timestamp => "timestamp without time zone",
            Self::Timestamptz => "timestamp with time zone",
            Self::Time => "time without time zone",
            Self::Timetz => "time with time zone",
            Self::Interval => "interval",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::Array(_) => "array",
            Self::Uuid => "uuid",
            Self::Bytea => "bytea",
            Self::Numeric => "numeric",
            Self::Range(k) => k.name(),
            Self::Bit { varying: false } => "bit",
            Self::Bit { varying: true } => "bit varying",
            Self::Multirange(k) => k.multirange_name(),
        }
    }

    /// Stable byte code for the schema-less on-disk encodings — the single
    /// source of truth shared by WAL records and checkpoint SSTs, so the two
    /// can never drift. Composite types fold in their element/kind `code()`.
    pub fn code(self) -> u8 {
        match self {
            Self::Bool => 1,
            Self::Int4 => 2,
            Self::Int8 => 3,
            Self::Float8 => 4,
            Self::Text => 5,
            Self::Date => 6,
            Self::Timestamp => 7,
            Self::Timestamptz => 8,
            Self::Uuid => 9,
            Self::Bytea => 10,
            Self::Numeric => 11,
            Self::Int2 => 12,
            Self::Float4 => 13,
            Self::Varchar => 14,
            Self::Bpchar => 15,
            Self::Time => 16,
            Self::Timetz => 41,
            Self::Interval => 17,
            Self::Json => 18,
            Self::Jsonb => 19,
            Self::Range(k) => RANGE_CODE_BASE + k.code(),
            Self::Bit { varying: false } => 26,
            Self::Bit { varying: true } => 27,
            Self::Name => 42,
            Self::Multirange(k) => MULTIRANGE_CODE_BASE + k.code(),
            Self::Array(e) => ARRAY_CODE_BASE + e.code(),
        }
    }

    /// Inverse of [`ColType::code`]; `None` for an unknown or corrupt code.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Bool,
            2 => Self::Int4,
            3 => Self::Int8,
            4 => Self::Float8,
            5 => Self::Text,
            6 => Self::Date,
            7 => Self::Timestamp,
            8 => Self::Timestamptz,
            9 => Self::Uuid,
            10 => Self::Bytea,
            11 => Self::Numeric,
            12 => Self::Int2,
            13 => Self::Float4,
            14 => Self::Varchar,
            15 => Self::Bpchar,
            16 => Self::Time,
            41 => Self::Timetz,
            17 => Self::Interval,
            18 => Self::Json,
            19 => Self::Jsonb,
            26 => Self::Bit { varying: false },
            27 => Self::Bit { varying: true },
            42 => Self::Name,
            c if (RANGE_CODE_BASE..RANGE_CODE_BASE + RANGE_KINDS).contains(&c) => {
                Self::Range(RangeKind::from_code(c - RANGE_CODE_BASE))
            }
            c if (MULTIRANGE_CODE_BASE..MULTIRANGE_CODE_BASE + RANGE_KINDS).contains(&c) => {
                Self::Multirange(RangeKind::from_code(c - MULTIRANGE_CODE_BASE))
            }
            c if c >= ARRAY_CODE_BASE => Self::Array(ArrElem::from_code(c - ARRAY_CODE_BASE)?),
            _ => return None,
        })
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
    /// The array type's own name, as PostgreSQL reports it in a message:
    /// `integer[]`, not `array`. The element's name with `[]` appended, but as
    /// a static string, since that is what a type name is here.
    pub fn array_name(self) -> &'static str {
        match self {
            ArrElem::Bool => "boolean[]",
            ArrElem::Int4 => "integer[]",
            ArrElem::Int8 => "bigint[]",
            ArrElem::Float8 => "double precision[]",
            ArrElem::Text => "text[]",
            ArrElem::Numeric => "numeric[]",
            ArrElem::Date => "date[]",
            ArrElem::Timestamp => "timestamp[]",
            ArrElem::Timestamptz => "timestamp with time zone[]",
        }
    }

    /// The array element type matching a scalar datum's runtime type.
    pub fn from_datum(d: &Datum) -> Option<ArrElem> {
        Some(match d {
            Datum::Bool(_) => ArrElem::Bool,
            Datum::Int4(_) => ArrElem::Int4,
            Datum::Int8(_) => ArrElem::Int8,
            Datum::Float8(_) => ArrElem::Float8,
            Datum::Text(_) | Datum::Bpchar(_) => ArrElem::Text,
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

/// The decoded view of a PostgreSQL `atttypmod`.
///
/// On the wire and in the catalog a type modifier is one `i32`, but that
/// integer is three different encodings wearing one type: varchar(n) and
/// numeric(p,s) carry a 4-byte header, the temporal precisions are bare, and
/// interval packs a field-range mask beside its precision. Reading one with the
/// wrong rule was a recurring bug class (a `timestamp(3)` reported as 7, an
/// interval precision read with a header it does not have), because every
/// consumer had to remember which rule applied.
///
/// This enum is the fix: `decode` and `encode` are the only places the integer
/// forms exist, they are adjacent and round-trip-tested, and every consumer
/// pattern-matches on the decoded meaning instead. A site can no longer
/// subtract a header the value does not carry, because there is no integer to
/// subtract from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeMod {
    /// No modifier: `-1` on the wire, or a value meaningless for the type.
    None,
    /// `varchar(n)` / `char(n)` / `bit(n)`: a length in characters or bits.
    Length(usize),
    /// `numeric(p, s)`.
    NumericPS { precision: u16, scale: u16 },
    /// `timestamp(p)` / `timestamptz(p)` / `time(p)` / `timetz(p)`:
    /// fractional-second digits, 0..=6.
    TemporalPrecision(u8),
    /// `interval` with a field-range mask and an optional precision. A plain
    /// `interval(p)` carries [`INTERVAL_FULL_RANGE`]; a range form like
    /// `interval hour to minute` carries its field mask with *no* precision —
    /// which is why the precision is an `Option`: the encoding's `0xFFFF`
    /// low half means "unspecified", and treating it as a number to clamp
    /// would silently round to 6 digits.
    IntervalMod { range: u16, precision: Option<u8> },
}

/// PostgreSQL's INTERVAL_FULL_RANGE: the field-range mask a plain `interval`
/// or `interval(p)` carries in the high half of its modifier.
pub const INTERVAL_FULL_RANGE: u16 = 0x7FFF;

impl TypeMod {
    /// Reads an `atttypmod` under the encoding `ctype` uses. Anything that is
    /// not a valid modifier for the type — negative, or below the header a
    /// headered kind requires — is `None`, never a garbage value.
    pub fn decode(ctype: ColType, atttypmod: i32) -> TypeMod {
        if atttypmod < 0 {
            return TypeMod::None;
        }
        match ctype {
            ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Bit { .. } => {
                if atttypmod >= 4 {
                    TypeMod::Length((atttypmod - 4) as usize)
                } else {
                    TypeMod::None
                }
            }
            ColType::Numeric => {
                if atttypmod >= 4 {
                    let packed = atttypmod - 4;
                    TypeMod::NumericPS {
                        precision: ((packed >> 16) & 0xFFFF) as u16,
                        scale: (packed & 0xFFFF) as u16,
                    }
                } else {
                    TypeMod::None
                }
            }
            ColType::Time | ColType::Timetz | ColType::Timestamp | ColType::Timestamptz => {
                if atttypmod <= 6 {
                    TypeMod::TemporalPrecision(atttypmod as u8)
                } else {
                    TypeMod::None
                }
            }
            ColType::Interval => {
                let precision_raw = atttypmod & 0xFFFF;
                TypeMod::IntervalMod {
                    range: ((atttypmod as u32) >> 16) as u16,
                    // 0xFFFF is "no precision given", not a precision.
                    precision: if precision_raw <= 6 { Some(precision_raw as u8) } else { None },
                }
            }
            _ => TypeMod::None,
        }
    }

    /// The `atttypmod` integer this modifier is written as — the exact value
    /// PostgreSQL stores, byte for byte.
    pub fn encode(&self) -> i32 {
        match *self {
            TypeMod::None => -1,
            TypeMod::Length(n) => n as i32 + 4,
            TypeMod::NumericPS { precision, scale } => {
                (((precision as i32) << 16) | (scale as i32)) + 4
            }
            TypeMod::TemporalPrecision(p) => i32::from(p),
            TypeMod::IntervalMod { range, precision } => {
                ((range as i32) << 16) | precision.map_or(0xFFFF, i32::from)
            }
        }
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

/// The six built-in range types. Discrete kinds (int4/int8/date) canonicalize
/// to `[lower, upper)`; continuous kinds (num/ts/tstz) keep their bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeKind {
    Int4,
    Int8,
    Num,
    Date,
    Ts,
    Tstz,
}

impl RangeKind {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "int4range" => Self::Int4,
            "int8range" => Self::Int8,
            "numrange" => Self::Num,
            "daterange" => Self::Date,
            "tsrange" => Self::Ts,
            "tstzrange" => Self::Tstz,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Int4 => "int4range",
            Self::Int8 => "int8range",
            Self::Num => "numrange",
            Self::Date => "daterange",
            Self::Ts => "tsrange",
            Self::Tstz => "tstzrange",
        }
    }
    pub fn oid(self) -> i32 {
        match self {
            Self::Int4 => 3904,
            Self::Num => 3906,
            Self::Ts => 3908,
            Self::Tstz => 3910,
            Self::Date => 3912,
            Self::Int8 => 3926,
        }
    }
    /// The element (subtype) column type.
    pub fn elem_type(self) -> ColType {
        match self {
            Self::Int4 => ColType::Int4,
            Self::Int8 => ColType::Int8,
            Self::Num => ColType::Numeric,
            Self::Date => ColType::Date,
            Self::Ts => ColType::Timestamp,
            Self::Tstz => ColType::Timestamptz,
        }
    }
    /// Discrete ranges canonicalize to a half-open `[lower, upper)` form.
    pub fn is_discrete(self) -> bool {
        matches!(self, Self::Int4 | Self::Int8 | Self::Date)
    }
    /// A stable byte code for schema-less encodings.
    pub fn code(self) -> u8 {
        match self {
            Self::Int4 => 0,
            Self::Int8 => 1,
            Self::Num => 2,
            Self::Date => 3,
            Self::Ts => 4,
            Self::Tstz => 5,
        }
    }
    pub fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Int8,
            2 => Self::Num,
            3 => Self::Date,
            4 => Self::Ts,
            5 => Self::Tstz,
            _ => Self::Int4,
        }
    }
    /// The multirange type name for this range subtype (`int4range` →
    /// `int4multirange`).
    pub fn multirange_name(self) -> &'static str {
        match self {
            Self::Int4 => "int4multirange",
            Self::Int8 => "int8multirange",
            Self::Num => "nummultirange",
            Self::Date => "datemultirange",
            Self::Ts => "tsmultirange",
            Self::Tstz => "tstzmultirange",
        }
    }
    /// The multirange type OID for this range subtype.
    pub fn multirange_oid(self) -> i32 {
        match self {
            Self::Int4 => oid::INT4MULTIRANGE,
            Self::Int8 => oid::INT8MULTIRANGE,
            Self::Num => oid::NUMMULTIRANGE,
            Self::Date => oid::DATEMULTIRANGE,
            Self::Ts => oid::TSMULTIRANGE,
            Self::Tstz => oid::TSTZMULTIRANGE,
        }
    }
    /// Resolves a multirange type name to its range subtype.
    pub fn from_multirange_name(name: &str) -> Option<Self> {
        Some(match name {
            "int4multirange" => Self::Int4,
            "int8multirange" => Self::Int8,
            "nummultirange" => Self::Num,
            "datemultirange" => Self::Date,
            "tsmultirange" => Self::Ts,
            "tstzmultirange" => Self::Tstz,
            _ => return None,
        })
    }
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
    /// A `char(n)` value, blank-padded to its declared width. The padding is
    /// part of the value (PostgreSQL emits `max(c)` padded even when the
    /// result typmod is -1), but it is *semantically* insignificant: casts to
    /// other string types, comparisons, and functions taking `text` all see
    /// the stripped form, while output functions, `LIKE`/regex matching, and
    /// `octet_length` see the raw padded form.
    Bpchar(&'a str),
    /// Days since 2000-01-01.
    Date(i32),
    /// Microseconds since 2000-01-01 (naive).
    Timestamp(i64),
    /// Microseconds since 2000-01-01 UTC.
    Timestamptz(i64),
    /// Microseconds since midnight (time of day).
    Time(i64),
    /// Time of day with its own UTC offset: microseconds since midnight in
    /// that offset, then the offset itself in seconds **east** of UTC, which
    /// is the sign [`super::datetime::iso_offset_string`] renders and that
    /// `EXTRACT(timezone FROM ...)` reports. PostgreSQL stores and sends the
    /// opposite sign, so the binary wire path negates it.
    Timetz(i64, i32),
    /// A duration.
    Interval(Interval),
    /// JSON text; `jsonb` is true for the binary/normalized form.
    Json { text: &'a str, jsonb: bool },
    /// A one-dimensional array: the element type plus the serialized element
    /// bytes (`u16 count` then `u32 len + element encoding` per element). Kept
    /// as raw bytes so decoding from storage needs no separate allocation.
    Array { element: ArrElem, raw: &'a [u8] },
    Uuid([u8; 16]),
    Bytea(&'a [u8]),
    Numeric(Numeric<'a>),
    /// A range value in its canonical text form (e.g. `[1,5)`, `empty`).
    Range { text: &'a str, kind: RangeKind },
    /// A bit string as a sequence of `'0'`/`'1'` characters. `varying` selects
    /// the reported type: `false` = `bit(n)` (OID 1560), `true` = `varbit`
    /// (OID 1562).
    Bit { bits: &'a str, varying: bool },
    /// A multirange value in canonical text form (e.g. `{[1,3),[5,7)}`, `{}`).
    Multirange { text: &'a str, kind: RangeKind },
    /// A composite/record value: each field's name (for `row_to_json` etc.),
    /// its type OID (for JSON/typed output), and its value. Records are
    /// transient — produced by `t.*`, a bare table reference, or `ROW(...)` —
    /// never stored in a column.
    Record(&'a [RecordField<'a>]),
}

/// One field of a [`Datum::Record`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordField<'a> {
    pub name: &'a str,
    pub type_oid: i32,
    pub value: Datum<'a>,
}

impl<'a> Datum<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    pub fn type_oid(&self) -> i32 {
        match self {
            Datum::Record(_) => oid::RECORD,
            Datum::Null => oid::TEXT,
            Datum::Bool(_) => oid::BOOL,
            Datum::Int4(_) => oid::INT4,
            Datum::Int8(_) => oid::INT8,
            Datum::Float8(_) => oid::FLOAT8,
            Datum::Text(_) => oid::TEXT,
            Datum::Bpchar(_) => oid::BPCHAR,
            Datum::Date(_) => oid::DATE,
            Datum::Timestamp(_) => oid::TIMESTAMP,
            Datum::Timestamptz(_) => oid::TIMESTAMPTZ,
            Datum::Timetz(..) => oid::TIMETZ,
            Datum::Time(_) => oid::TIME,
            Datum::Interval(_) => oid::INTERVAL,
            Datum::Json { jsonb: false, .. } => oid::JSON,
            Datum::Json { jsonb: true, .. } => oid::JSONB,
            Datum::Array { element, .. } => element.array_oid(),
            Datum::Uuid(_) => oid::UUID,
            Datum::Bytea(_) => oid::BYTEA,
            Datum::Numeric(_) => oid::NUMERIC,
            Datum::Range { kind, .. } => kind.oid(),
            Datum::Bit { varying: false, .. } => oid::BIT,
            Datum::Bit { varying: true, .. } => oid::VARBIT,
            Datum::Multirange { kind, .. } => kind.multirange_oid(),
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
            // The output function emits the padding — psql shows `hi   `.
            Datum::Text(s) | Datum::Bpchar(s) => f.write_str(s),
            Datum::Date(d) => f.write_str(super::datetime::format_date(*d).as_str()),
            Datum::Timestamp(t) => {
                f.write_str(super::datetime::format_timestamp(*t, false).as_str())
            }
            Datum::Timestamptz(t) => {
                f.write_str(super::datetime::format_timestamp(*t, true).as_str())
            }
            Datum::Time(t) => f.write_str(super::datetime::format_time(*t).as_str()),
            Datum::Timetz(t, zone) => {
                f.write_str(super::datetime::format_time(*t).as_str())?;
                f.write_str(super::datetime::iso_offset_string(*zone).as_str())
            }
            Datum::Interval(interval) => f.write_str(super::datetime::format_interval(*interval).as_str()),
            Datum::Json { text, .. } => f.write_str(text),
            Datum::Range { text, .. } => f.write_str(text),
            Datum::Bit { bits, .. } => f.write_str(bits),
            Datum::Multirange { text, .. } => f.write_str(text),
            Datum::Array { element, raw } => super::array::write(f, *element, raw),
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
            Datum::Record(fields) => {
                f.write_char('(')?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write_record_field(f, &field.value)?;
                }
                f.write_char(')')
            }
        }
    }
}

/// Renders one record field for PostgreSQL's `record_out` text form: NULL is
/// empty (unquoted); everything else is quoted when the rendered text is
/// empty or contains a delimiter, paren, quote, backslash, or whitespace,
/// with `"` and `\` doubled inside the quotes.
pub(crate) fn write_record_field(f: &mut fmt::Formatter<'_>, v: &Datum) -> fmt::Result {
    if v.is_null() {
        return Ok(());
    }
    let mut buf = crate::util::StackStr::<8192>::default();
    let _ = write!(buf, "{v}");
    let text = buf.as_str();
    let needs_quote = text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace());
    if !needs_quote {
        return f.write_str(text);
    }
    f.write_char('"')?;
    for c in text.chars() {
        if c == '"' || c == '\\' {
            f.write_char(c)?;
        }
        f.write_char(c)?;
    }
    f.write_char('"')
}

/// Renders one array element, quoting text that would otherwise be ambiguous
/// (empty, or containing a delimiter/brace/quote/backslash/whitespace), and
/// spelling NULL unquoted — matching PostgreSQL's array output.
/// Whether an element's rendered text has to be quoted inside an array
/// literal, decided while the value renders so no buffer bounds it.
struct QuoteScan {
    empty: bool,
    special: bool,
    text: [u8; 4],
    len: usize,
}

impl fmt::Write for QuoteScan {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if !s.is_empty() {
            self.empty = false;
        }
        if s.chars().any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace()) {
            self.special = true;
        }
        // Only the first four bytes are kept, enough to recognize `null`.
        for b in s.bytes() {
            if self.len < self.text.len() {
                self.text[self.len] = b;
            }
            self.len += 1;
        }
        Ok(())
    }
}

/// Escapes `"` and `\` as it forwards, for an element being quoted.
struct EscapeTo<'x, 'y>(&'x mut fmt::Formatter<'y>);

impl fmt::Write for EscapeTo<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if c == '"' || c == '\\' {
                self.0.write_char('\\')?;
            }
            self.0.write_char(c)?;
        }
        Ok(())
    }
}

/// One element of an array literal. PostgreSQL quotes an element whose text is
/// empty, spells `null`, or carries a comma, brace, quote, backslash or space —
/// which is why a timestamp, a range and a json value all come out quoted, not
/// only a string. The value is rendered twice rather than buffered, so nothing
/// caps how long an element may be.
pub(crate) fn write_array_elem(f: &mut fmt::Formatter<'_>, v: &Datum) -> fmt::Result {
    if matches!(v, Datum::Null) {
        return f.write_str("NULL");
    }
    let mut scan = QuoteScan { empty: true, special: false, text: [0; 4], len: 0 };
    write!(scan, "{v}")?;
    let is_null_word = scan.len == 4 && scan.text.eq_ignore_ascii_case(b"null");
    if scan.empty || scan.special || is_null_word {
        f.write_str("\"")?;
        write!(EscapeTo(f), "{v}")?;
        f.write_str("\"")
    } else {
        write!(f, "{v}")
    }
}

/// Description of one result column.
#[derive(Debug, Clone, Copy)]
pub struct ColDesc<'a> {
    pub name: &'a str,
    pub type_oid: i32,
    pub typlen: i16,
    /// The column's atttypmod, as RowDescription reports it: a table column's
    /// declared modifier, a cast's target modifier, `-1` for every computed
    /// expression — matching what PostgreSQL sends.
    pub type_mod: i32,
}

impl<'a> ColDesc<'a> {
    pub fn new(name: &'a str, type_oid: i32, typlen: i16) -> Self {
        Self { name, type_oid, typlen, type_mod: -1 }
    }

    pub fn of_type(name: &'a str, t: ColType) -> Self {
        Self::new(name, t.oid(), t.typlen())
    }

    /// The same description carrying the column's declared type modifier.
    pub fn with_type_mod(mut self, type_mod: i32) -> Self {
        self.type_mod = type_mod;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typemod_encodes_postgres_exact_values() {
        // The values PostgreSQL 18.4 stores in pg_attribute, byte for byte.
        assert_eq!(TypeMod::Length(5).encode(), 9); // varchar(5)
        assert_eq!(TypeMod::Length(3).encode(), 7); // char(3)
        assert_eq!(TypeMod::NumericPS { precision: 6, scale: 2 }.encode(), 393222);
        assert_eq!(TypeMod::TemporalPrecision(3).encode(), 3); // timestamp(3)
        assert_eq!(TypeMod::TemporalPrecision(0).encode(), 0); // timestamp(0)
        assert_eq!(
            TypeMod::IntervalMod { range: INTERVAL_FULL_RANGE, precision: Some(1) }.encode(),
            2147418113 // interval(1)
        );
        assert_eq!(
            TypeMod::IntervalMod { range: 0x0C00, precision: None }.encode(),
            201392127 // interval hour to minute — precision unspecified
        );
        assert_eq!(TypeMod::None.encode(), -1);
    }

    #[test]
    fn typemod_round_trips_through_every_encoding() {
        let cases: &[(ColType, TypeMod)] = &[
            (ColType::Varchar, TypeMod::Length(5)),
            (ColType::Bpchar, TypeMod::Length(3)),
            (ColType::Bit { varying: false }, TypeMod::Length(8)),
            (ColType::Numeric, TypeMod::NumericPS { precision: 6, scale: 2 }),
            (ColType::Timestamp, TypeMod::TemporalPrecision(3)),
            (ColType::Timestamptz, TypeMod::TemporalPrecision(0)),
            (ColType::Time, TypeMod::TemporalPrecision(6)),
            (ColType::Timetz, TypeMod::TemporalPrecision(2)),
            (
                ColType::Interval,
                TypeMod::IntervalMod { range: INTERVAL_FULL_RANGE, precision: Some(4) },
            ),
            (ColType::Interval, TypeMod::IntervalMod { range: 0x0C00, precision: None }),
        ];
        for &(ctype, modifier) in cases {
            assert_eq!(
                TypeMod::decode(ctype, modifier.encode()),
                modifier,
                "{ctype:?} did not round-trip"
            );
        }
    }

    #[test]
    fn typemod_decode_rejects_what_is_not_a_modifier() {
        // -1 is "none" for every type; a headered kind refuses a value below
        // its header; a bare precision refuses one past 6. Garbage decodes to
        // None, never to a wrong number.
        for ctype in [
            ColType::Varchar,
            ColType::Numeric,
            ColType::Timestamp,
            ColType::Interval,
            ColType::Int4,
        ] {
            assert_eq!(TypeMod::decode(ctype, -1), TypeMod::None, "{ctype:?}");
        }
        assert_eq!(TypeMod::decode(ColType::Varchar, 3), TypeMod::None);
        assert_eq!(TypeMod::decode(ColType::Numeric, 2), TypeMod::None);
        assert_eq!(TypeMod::decode(ColType::Timestamp, 7), TypeMod::None);
        // A type with no modifier concept ignores any value.
        assert_eq!(TypeMod::decode(ColType::Int4, 9), TypeMod::None);
        // The interval 0xFFFF low half is "no precision", not precision 65535.
        assert_eq!(
            TypeMod::decode(ColType::Interval, 201392127),
            TypeMod::IntervalMod { range: 0x0C00, precision: None }
        );
    }

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

#[cfg(test)]
mod code_roundtrip_tests {
    use super::*;

    /// Every code any `ColType` can produce must decode back to that same type.
    /// A family whose span overlaps another's silently becomes it — `bool[]`
    /// once decoded as `tsmultirange` — and these codes are what the WAL and
    /// the checkpoint store, so the confusion outlives the process.
    #[test]
    fn every_coltype_code_roundtrips() {
        let mut types = vec![
            ColType::Bool, ColType::Int2, ColType::Int4, ColType::Int8, ColType::Float4,
            ColType::Float8, ColType::Text, ColType::Varchar, ColType::Bpchar, ColType::Date,
            ColType::Timestamp, ColType::Timestamptz, ColType::Time, ColType::Timetz, ColType::Interval,
            ColType::Json, ColType::Jsonb, ColType::Uuid, ColType::Bytea, ColType::Numeric,
            ColType::Bit { varying: false }, ColType::Bit { varying: true },
        ];
        for k in [RangeKind::Int4, RangeKind::Int8, RangeKind::Num, RangeKind::Date, RangeKind::Ts, RangeKind::Tstz] {
            types.push(ColType::Range(k));
            types.push(ColType::Multirange(k));
        }
        for e in [ArrElem::Bool, ArrElem::Int4, ArrElem::Int8, ArrElem::Float8, ArrElem::Text,
                  ArrElem::Numeric, ArrElem::Date, ArrElem::Timestamp, ArrElem::Timestamptz] {
            types.push(ColType::Array(e));
        }
        // The layout this replaced could emit any code in 20..=40; a moved
        // family must not reuse one, or old data decodes as the wrong type
        // instead of failing.
        // 20..=40 is what the pre-B-095 layout could emit. Only the families
        // that legitimately held codes there then may hold them now; anything
        // else would decode old data as itself instead of failing.
        for t in &types {
            let c = t.code();
            let held_them_before = matches!(t, ColType::Range(_) | ColType::Bit { .. });
            assert!(
                held_them_before || !(20..=40).contains(&c),
                "{t:?} takes retired code {c}, which old data may still carry"
            );
        }
        // No two types may share a code, and each must decode back to itself.
        let mut seen: Vec<(u8, ColType)> = Vec::new();
        for t in types {
            let c = t.code();
            assert_eq!(ColType::from_code(c), Some(t), "code {c} does not round-trip for {t:?}");
            if let Some((_, other)) = seen.iter().find(|(code, _)| *code == c) {
                panic!("code {c} is produced by both {other:?} and {t:?}");
            }
            seen.push((c, t));
        }
    }
}
