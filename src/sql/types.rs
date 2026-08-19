//! Runtime values and their PostgreSQL type identities.

use core::fmt;
use core::fmt::Write as _;

use super::net::NetAddr;
use super::numeric::Numeric;

/// OIDs from PostgreSQL's `pg_type` (stable, documented catalog values).
pub mod oid {
    pub const BOOL: i32 = 16;
    pub const BYTEA: i32 = 17;
    pub const INT8: i32 = 20;
    pub const INT2: i32 = 21;
    pub const INT2VECTOR: i32 = 22;
    pub const INT4: i32 = 23;
    pub const OID: i32 = 26;
    pub const OID_ARRAY: i32 = 1028;
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
    /// PostgreSQL's pseudo-type for an action-only function result.
    pub const VOID: i32 = 2278;
    pub const BIT_ARRAY: i32 = 1561;
    pub const VARBIT_ARRAY: i32 = 1563;
    // Network address types.
    pub const INET: i32 = 869;
    pub const CIDR: i32 = 650;
    pub const MACADDR: i32 = 829;
    pub const MACADDR8: i32 = 774;
    pub const INET_ARRAY: i32 = 1041;
    pub const CIDR_ARRAY: i32 = 651;
    pub const MACADDR_ARRAY: i32 = 1040;
    pub const MACADDR8_ARRAY: i32 = 775;
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
    pub const REGPROC: i32 = 24;
    pub const REGPROCEDURE: i32 = 2202;
    pub const REGOPER: i32 = 2203;
    pub const REGOPERATOR: i32 = 2204;
    pub const REGCLASS: i32 = 2205;
    pub const REGTYPE: i32 = 2206;
    pub const REGNAMESPACE: i32 = 4089;
    pub const REGROLE: i32 = 4096;
    pub const REGPROC_ARRAY: i32 = 1008;
    pub const REGPROCEDURE_ARRAY: i32 = 2207;
    pub const REGOPER_ARRAY: i32 = 2208;
    pub const REGOPERATOR_ARRAY: i32 = 2209;
    pub const REGCLASS_ARRAY: i32 = 2210;
    pub const REGTYPE_ARRAY: i32 = 2211;
    pub const REGNAMESPACE_ARRAY: i32 = 4090;
    pub const REGROLE_ARRAY: i32 = 4097;
    /// Base OIDs for user-defined domains, enums, composites, and the array types PostgreSQL
    /// creates alongside each of them. Slots are catalog-local identities; the
    /// bands are deliberately disjoint from relation/composite OIDs.
    pub const FIRST_DOMAIN: i32 = 110_000;
    pub const FIRST_ENUM: i32 = 120_000;
    pub const FIRST_COMPOSITE: i32 = 230_000;
    pub const FIRST_DOMAIN_ARRAY: i32 = 150_000;
    pub const FIRST_ENUM_ARRAY: i32 = 160_000;
    pub const FIRST_COMPOSITE_ARRAY: i32 = 240_000;
    pub fn domain_oid(slot: u16) -> i32 {
        FIRST_DOMAIN + slot as i32
    }
    /// The synthesized OID of the enum type in catalog `slot`.
    pub fn enum_oid(slot: u16) -> i32 {
        FIRST_ENUM + slot as i32
    }
    pub fn domain_array_oid(slot: u16) -> i32 {
        FIRST_DOMAIN_ARRAY + slot as i32
    }
    pub fn enum_array_oid(slot: u16) -> i32 {
        FIRST_ENUM_ARRAY + slot as i32
    }
    pub fn composite_oid(slot: u16) -> i32 {
        FIRST_COMPOSITE + slot as i32
    }
    pub fn composite_array_oid(slot: u16) -> i32 {
        FIRST_COMPOSITE_ARRAY + slot as i32
    }
}

/// Column types the engine stores. A deliberately small, growing set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    /// `void` is a routine-result pseudo-type. It is never a stored column or
    /// routine argument; a `void` result is represented by `Datum::Null`.
    Void,
    Bool,
    /// `smallint`/`int2`. A real i16 datum with PostgreSQL's OID 21 and
    /// two-byte binary wire representation.
    Int2,
    /// PostgreSQL's zero-based, space-delimited `int2vector` catalog type.
    /// It is transient catalog data and is never accepted as a stored column.
    Int2Vector,
    Int4,
    /// PostgreSQL object identity, retaining its catalog and wire metadata
    /// while sharing the engine's four-byte integer datum storage.
    Oid,
    /// `regtype`: a catalog type reference with OID storage and catalog-name
    /// text output, not ordinary text.
    Regtype,
    /// PostgreSQL catalog object references. They share the four-byte binary
    /// representation of an OID but retain their distinct type identities and
    /// textual output functions.
    Regproc,
    Regprocedure,
    Regoper,
    Regoperator,
    Regclass,
    Regnamespace,
    Regrole,
    Int8,
    /// `real`/`float4`. Its own [`Datum::Float4`] (f32); reports OID 700 and
    /// typlen 4. On disk it keeps the historical 8-byte float8 layout
    /// (`storage()` is Float8) and narrows to f32 at decode by schema.
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
    /// An array of a scalar element type.
    Array(ArrElem),
    Uuid,
    Bytea,
    Numeric,
    /// A range type (int4range/numrange/…), stored as canonical text.
    Range(RangeKind),
    /// A bit string. `varying` = `false` is `bit(n)` (OID 1560), `true` is
    /// `bit varying` / `varbit` (OID 1562). Length is enforced at cast time,
    /// not tracked here.
    Bit {
        varying: bool,
    },
    /// A multirange type (int4multirange/…), stored as canonical text.
    Multirange(RangeKind),
    /// `inet`: a host or network IPv4/IPv6 address with a mask length. Host
    /// bits are allowed and preserved.
    Inet,
    /// `cidr`: an IPv4/IPv6 network. Like `inet` but rejects bits set to the
    /// right of the mask and always prints the mask length.
    Cidr,
    /// `macaddr`: a six-byte MAC address.
    Macaddr,
    /// `macaddr8`: an eight-byte (EUI-64) MAC address.
    Macaddr8,
    /// An anonymous composite (`ROW(...)`, a whole-row reference, a record
    /// SRF) carried through a derived table's columns. Transient only: a real
    /// table column can never have this type (DDL refuses records), so it has
    /// no on-disk presence.
    Record,
    /// A user-defined enum type (`CREATE TYPE ... AS ENUM`). The `u16` is the
    /// slot of its [`crate::storage::EnumDef`] in the catalog — a runtime
    /// identity only. A column of an enum type persists the enum's *name* (in
    /// [`crate::storage::ColumnMeta::domain`], resolved back to a slot on load),
    /// because slots are not stable across restart. A stored enum *value*
    /// carries its own label and sort key inline, so [`from_code`](Self::from_code)
    /// need not recover the slot to decode a row.
    Enum(u16),
    /// A named composite type (`CREATE TYPE ... AS (...)`). Unlike `Record`,
    /// this is durable and carries a catalog identity.
    Composite(u16),
}

/// Base storage codes for the parameterized type families. They must stay far
/// enough apart that no two families can produce the same code: `Multirange`
/// once began at 28 and `Array` at 32, which made `bool[]` and `int4[]`
/// (32, 33) indistinguishable from `tsmultirange` and `tstzmultirange`, and
/// [`ColType::from_code`] resolved both to the multirange — so a restart
/// replayed those columns back as the wrong type, losing their values.
///
/// The parameter-free catalog object aliases occupy 59..=65, so arrays start
/// above that band. A retired code decodes to `None` rather than becoming a
/// different type.
const RANGE_CODE_BASE: u8 = 20;
const MULTIRANGE_CODE_BASE: u8 = 48;
const ARRAY_CODE_BASE: u8 = 80;
/// How many `RangeKind`s there are, i.e. the width of each range family's span.
const RANGE_KINDS: u8 = 6;

impl ColType {
    /// Whether values of this type carry a PostgreSQL collation.
    ///
    /// Keeping this on the type prevents each catalog, DDL, and recovery path
    /// from making its own incomplete list of collatable types.
    pub const fn is_collatable(self) -> bool {
        matches!(self, Self::Text | Self::Varchar | Self::Bpchar | Self::Name)
    }

    /// Pseudo-types describe executor contracts rather than stored values.
    pub const fn is_pseudo(self) -> bool {
        matches!(self, Self::Void | Self::Record)
    }

    pub const fn is_reg_object(self) -> bool {
        matches!(
            self,
            Self::Regproc
                | Self::Regprocedure
                | Self::Regoper
                | Self::Regoperator
                | Self::Regclass
                | Self::Regnamespace
                | Self::Regrole
        )
    }

    /// Maps a SQL type name (already case-folded) to a column type.
    pub fn from_sql_name(name: &str) -> Option<Self> {
        // `element[]` is an array of a scalar element type.
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
            "void" => Self::Void,
            "bool" | "boolean" => Self::Bool,
            "int" | "int4" | "integer" | "serial" | "serial4" => Self::Int4,
            "smallint" | "int2" | "smallserial" | "serial2" => Self::Int2,
            "bigint" | "int8" | "bigserial" | "serial8" => Self::Int8,
            "float8" | "float" | "double precision" => Self::Float8,
            "float4" | "real" => Self::Float4,
            "text" => Self::Text,
            "regtype" => Self::Regtype,
            "regproc" => Self::Regproc,
            "regprocedure" => Self::Regprocedure,
            "regoper" => Self::Regoper,
            "regoperator" => Self::Regoperator,
            "regclass" => Self::Regclass,
            "regnamespace" => Self::Regnamespace,
            "regrole" => Self::Regrole,
            "name" => Self::Name,
            "oid" => Self::Oid,
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
            "inet" => Self::Inet,
            "cidr" => Self::Cidr,
            "macaddr" => Self::Macaddr,
            "macaddr8" => Self::Macaddr8,
            "record" => Self::Record,
            _ => return None,
        })
    }

    pub fn oid(self) -> i32 {
        match self {
            Self::Void => oid::VOID,
            Self::Bool => oid::BOOL,
            Self::Int2 => oid::INT2,
            Self::Int2Vector => oid::INT2VECTOR,
            Self::Int4 => oid::INT4,
            Self::Oid => oid::OID,
            Self::Regtype => oid::REGTYPE,
            Self::Regproc => oid::REGPROC,
            Self::Regprocedure => oid::REGPROCEDURE,
            Self::Regoper => oid::REGOPER,
            Self::Regoperator => oid::REGOPERATOR,
            Self::Regclass => oid::REGCLASS,
            Self::Regnamespace => oid::REGNAMESPACE,
            Self::Regrole => oid::REGROLE,
            Self::Int8 => oid::INT8,
            Self::Float4 => oid::FLOAT4,
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
            Self::Inet => oid::INET,
            Self::Cidr => oid::CIDR,
            Self::Macaddr => oid::MACADDR,
            Self::Macaddr8 => oid::MACADDR8,
            Self::Record => oid::RECORD,
            Self::Enum(slot) => oid::enum_oid(slot),
            Self::Composite(slot) => oid::composite_oid(slot),
        }
    }
    /// Inverse of [`ColType::oid`]: the column type a PostgreSQL type OID names,
    /// or `None` for an OID this engine does not model. Used to decode a
    /// binary-format parameter whose type the client declared by OID.
    pub fn from_oid(type_oid: i32) -> Option<ColType> {
        // Scalars.
        let scalar = match type_oid {
            oid::VOID => Some(Self::Void),
            oid::BOOL => Some(Self::Bool),
            oid::INT2 => Some(Self::Int2),
            oid::INT2VECTOR => Some(Self::Int2Vector),
            oid::INT4 => Some(Self::Int4),
            oid::OID => Some(Self::Oid),
            oid::REGPROC => Some(Self::Regproc),
            oid::REGPROCEDURE => Some(Self::Regprocedure),
            oid::REGOPER => Some(Self::Regoper),
            oid::REGOPERATOR => Some(Self::Regoperator),
            oid::REGCLASS => Some(Self::Regclass),
            oid::REGNAMESPACE => Some(Self::Regnamespace),
            oid::REGROLE => Some(Self::Regrole),
            oid::REGTYPE => Some(Self::Regtype),
            oid::INT8 => Some(Self::Int8),
            oid::FLOAT4 => Some(Self::Float4),
            oid::FLOAT8 => Some(Self::Float8),
            oid::TEXT => Some(Self::Text),
            oid::NAME => Some(Self::Name),
            oid::VARCHAR => Some(Self::Varchar),
            oid::BPCHAR => Some(Self::Bpchar),
            oid::DATE => Some(Self::Date),
            oid::TIMESTAMP => Some(Self::Timestamp),
            oid::TIMESTAMPTZ => Some(Self::Timestamptz),
            oid::TIME => Some(Self::Time),
            oid::TIMETZ => Some(Self::Timetz),
            oid::INTERVAL => Some(Self::Interval),
            oid::JSON => Some(Self::Json),
            oid::JSONB => Some(Self::Jsonb),
            oid::UUID => Some(Self::Uuid),
            oid::BYTEA => Some(Self::Bytea),
            oid::NUMERIC => Some(Self::Numeric),
            oid::BIT => Some(Self::Bit { varying: false }),
            oid::VARBIT => Some(Self::Bit { varying: true }),
            oid::INET => Some(Self::Inet),
            oid::CIDR => Some(Self::Cidr),
            oid::MACADDR => Some(Self::Macaddr),
            oid::MACADDR8 => Some(Self::Macaddr8),
            oid::RECORD => Some(Self::Record),
            _ => None,
        };
        if scalar.is_some() {
            return scalar;
        }
        // Ranges and multiranges.
        for kind in [
            RangeKind::Int4,
            RangeKind::Int8,
            RangeKind::Num,
            RangeKind::Date,
            RangeKind::Ts,
            RangeKind::Tstz,
        ] {
            if type_oid == kind.oid() {
                return Some(Self::Range(kind));
            }
            if type_oid == kind.multirange_oid() {
                return Some(Self::Multirange(kind));
            }
        }
        // The same inventory drives catalog synthesis, so every advertised
        // built-in array OID is decodable at the wire boundary.
        for element in ArrElem::BUILTIN {
            if type_oid == element.array_oid() {
                return Some(Self::Array(element));
            }
        }
        // User-defined enum types occupy a synthesized OID band.
        if type_oid >= oid::FIRST_ENUM
            && type_oid < oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32
        {
            return Some(Self::Enum((type_oid - oid::FIRST_ENUM) as u16));
        }
        if type_oid >= oid::FIRST_ENUM_ARRAY
            && type_oid < oid::FIRST_ENUM_ARRAY + crate::storage::MAX_ENUMS as i32
        {
            return Some(Self::Array(ArrElem::Enum(
                (type_oid - oid::FIRST_ENUM_ARRAY) as u16,
            )));
        }
        if type_oid >= oid::FIRST_COMPOSITE
            && type_oid < oid::FIRST_COMPOSITE + crate::storage::MAX_COMPOSITES as i32
        {
            return Some(Self::Composite((type_oid - oid::FIRST_COMPOSITE) as u16));
        }
        if type_oid >= oid::FIRST_COMPOSITE_ARRAY
            && type_oid < oid::FIRST_COMPOSITE_ARRAY + crate::storage::MAX_COMPOSITES as i32
        {
            return Some(Self::Array(ArrElem::Composite(
                (type_oid - oid::FIRST_COMPOSITE_ARRAY) as u16,
            )));
        }
        // Bit-string arrays have no array-element type here, so they (and any
        // other unmodeled OID) fall through unsupported.
        None
    }

    pub fn typlen(self) -> i16 {
        match self {
            Self::Void => 4,
            Self::Bool => 1,
            Self::Int2 => 2,
            Self::Int2Vector => -1,
            Self::Int4
            | Self::Oid
            | Self::Regtype
            | Self::Regproc
            | Self::Regprocedure
            | Self::Regoper
            | Self::Regoperator
            | Self::Regclass
            | Self::Regnamespace
            | Self::Regrole
            | Self::Date
            | Self::Float4 => 4,
            Self::Int8 | Self::Float8 | Self::Timestamp | Self::Timestamptz | Self::Time => 8,
            Self::Timetz => 12,
            Self::Interval => 16,
            Self::Uuid => 16,
            Self::Macaddr => 6,
            Self::Macaddr8 => 8,
            Self::Name => 64,
            Self::Text
            | Self::Varchar
            | Self::Bpchar
            | Self::Bytea
            | Self::Numeric
            | Self::Json
            | Self::Jsonb => -1,
            Self::Array(_) | Self::Range(_) | Self::Bit { .. } | Self::Multirange(_) => -1,
            Self::Inet | Self::Cidr => -1,
            Self::Record => -1,
            // PostgreSQL enums are a fixed 4-byte OID on the wire.
            Self::Enum(_) => 4,
            Self::Composite(_) => -1,
        }
    }

    /// The underlying storage/Datum type: int2 stores as int4, float4 as
    /// float8, varchar/bpchar as text. Used where behavior is width-driven.
    pub fn storage(self) -> ColType {
        match self {
            Self::Float4 => Self::Float8,
            Self::Varchar | Self::Bpchar | Self::Name => Self::Text,
            Self::Oid => Self::Int4,
            Self::Regtype
            | Self::Regproc
            | Self::Regprocedure
            | Self::Regoper
            | Self::Regoperator
            | Self::Regclass
            | Self::Regnamespace
            | Self::Regrole => self,
            other => other,
        }
    }

    /// The catalog (internal) name, used to title cast result columns.
    pub fn internal_name(self) -> &'static str {
        match self {
            Self::Void => "void",
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int2Vector => "int2vector",
            Self::Int4 => "int4",
            Self::Oid => "oid",
            Self::Regtype => "regtype",
            Self::Regproc => "regproc",
            Self::Regprocedure => "regprocedure",
            Self::Regoper => "regoper",
            Self::Regoperator => "regoperator",
            Self::Regclass => "regclass",
            Self::Regnamespace => "regnamespace",
            Self::Regrole => "regrole",
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
            Self::Inet => "inet",
            Self::Cidr => "cidr",
            Self::Macaddr => "macaddr",
            Self::Macaddr8 => "macaddr8",
            Self::Record => "record",
            // The real enum name is dynamic (per catalog slot); callers that
            // must title a column after the enum resolve it via the catalog.
            Self::Enum(_) => "enum",
            Self::Composite(_) => "record",
        }
    }

    /// The actual `pg_type.typname`, which differs from SQL's display spelling
    /// for arrays (`_int4`, not `integer[]`).
    pub fn catalog_name(self) -> &'static str {
        match self {
            Self::Array(element) => element.catalog_name(),
            other => other.internal_name(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Void => "void",
            Self::Bool => "boolean",
            Self::Int2 => "smallint",
            Self::Int2Vector => "int2vector",
            Self::Int4 => "integer",
            Self::Oid => "oid",
            Self::Regtype => "regtype",
            Self::Regproc => "regproc",
            Self::Regprocedure => "regprocedure",
            Self::Regoper => "regoper",
            Self::Regoperator => "regoperator",
            Self::Regclass => "regclass",
            Self::Regnamespace => "regnamespace",
            Self::Regrole => "regrole",
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
            Self::Inet => "inet",
            Self::Cidr => "cidr",
            Self::Macaddr => "macaddr",
            Self::Macaddr8 => "macaddr8",
            Self::Record => "record",
            Self::Enum(_) => "enum",
            Self::Composite(_) => "record",
        }
    }

    /// Stable byte code for the schema-less on-disk encodings — the single
    /// source of truth shared by WAL records and checkpoint SSTs, so the two
    /// can never drift. Composite types fold in their element/kind `code()`.
    pub fn code(self) -> u8 {
        match self {
            // Routine definitions persist their result type, including `void`.
            // Row encoding separately rejects pseudo-types.
            Self::Void => 57,
            Self::Bool => 1,
            Self::Int4 => 2,
            Self::Oid => 56,
            Self::Regtype => 58,
            Self::Regproc => 59,
            Self::Regprocedure => 60,
            Self::Regoper => 61,
            Self::Regoperator => 62,
            Self::Regclass => 63,
            Self::Regnamespace => 64,
            Self::Regrole => 65,
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
            Self::Int2Vector => 55,
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
            Self::Inet => 43,
            Self::Cidr => 44,
            Self::Macaddr => 45,
            Self::Macaddr8 => 47,
            Self::Multirange(k) => MULTIRANGE_CODE_BASE + k.code(),
            Self::Array(e) => ARRAY_CODE_BASE + e.code(),
            // Records are transient (never a stored column); the code is a
            // loud sentinel with no `from_code` inverse, so any leak into the
            // persistence layer fails visibly at reload.
            Self::Record => 46,
            // The code marks "an enum column"; *which* enum is carried
            // alongside as the type name (slots are not stable across restart),
            // and a stored enum value carries its own label + sort inline.
            Self::Enum(_) => 54,
            Self::Composite(_) => 66,
        }
    }

    /// The catalog slot placed in [`ColType::Enum`] by [`from_code`](Self::from_code)
    /// before the real slot is resolved from the persisted type name.
    pub const ENUM_SLOT_UNRESOLVED: u16 = u16::MAX;
    pub const COMPOSITE_SLOT_UNRESOLVED: u16 = u16::MAX;

    /// Inverse of [`ColType::code`]; `None` for an unknown or corrupt code.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Bool,
            57 => Self::Void,
            2 => Self::Int4,
            56 => Self::Oid,
            58 => Self::Regtype,
            59 => Self::Regproc,
            60 => Self::Regprocedure,
            61 => Self::Regoper,
            62 => Self::Regoperator,
            63 => Self::Regclass,
            64 => Self::Regnamespace,
            65 => Self::Regrole,
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
            55 => Self::Int2Vector,
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
            43 => Self::Inet,
            44 => Self::Cidr,
            45 => Self::Macaddr,
            47 => Self::Macaddr8,
            // An enum column: the concrete slot is resolved from the persisted
            // type name after decode (see the column codec's name handling).
            54 => Self::Enum(Self::ENUM_SLOT_UNRESOLVED),
            66 => Self::Composite(Self::COMPOSITE_SLOT_UNRESOLVED),
            c if (RANGE_CODE_BASE..RANGE_CODE_BASE + RANGE_KINDS).contains(&c) => {
                Self::Range(RangeKind::from_code(c - RANGE_CODE_BASE)?)
            }
            c if (MULTIRANGE_CODE_BASE..MULTIRANGE_CODE_BASE + RANGE_KINDS).contains(&c) => {
                Self::Multirange(RangeKind::from_code(c - MULTIRANGE_CODE_BASE)?)
            }
            c if c >= ARRAY_CODE_BASE => Self::Array(ArrElem::from_code(c - ARRAY_CODE_BASE)?),
            _ => return None,
        })
    }
}

/// The scalar element type of an array. A distinct (non-recursive)
/// enum so `ColType`/`Datum` stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrElem {
    Bool,
    Int4,
    Oid,
    Int8,
    Float8,
    Text,
    Numeric,
    Date,
    Timestamp,
    Timestamptz,
    Int2,
    Float4,
    Time,
    Timetz,
    Interval,
    Uuid,
    Bytea,
    Json,
    Jsonb,
    Varchar,
    Bpchar,
    Name,
    Inet,
    Cidr,
    Macaddr,
    Macaddr8,
    /// A fixed-length `bit` array element. The element typmod supplies the
    /// declared width; this tag preserves the `_bit` catalog identity.
    Bit,
    /// A `bit varying` / `varbit` array element, with `_varbit` identity.
    Varbit,
    Regtype,
    Regproc,
    Regprocedure,
    Regoper,
    Regoperator,
    Regclass,
    Regnamespace,
    Regrole,
    /// One of PostgreSQL's six built-in range types. Keeping the subtype in
    /// the element identity makes `int4range[]` and `numrange[]` distinct
    /// durable and wire types rather than text arrays.
    Range(RangeKind),
    /// One of PostgreSQL's six built-in multirange types.
    Multirange(RangeKind),
    /// An enum array keeps the catalog slot as its runtime identity. Table
    /// metadata also persists the type name and rebinds the slot on startup.
    Enum(u16),
    /// An array of one named composite type.
    Composite(u16),
    /// An array whose elements are values of a domain. Domain values use their
    /// ultimate base representation; `base_code` identifies that runtime
    /// representation and `base_user_slot` completes it for an enum or named
    /// composite base. Keeping both facts inline preserves allocation-free
    /// array decoding without reducing a user-defined base to an untyped tag.
    Domain {
        slot: u16,
        base_code: u8,
        base_user_slot: u16,
    },
}

impl ArrElem {
    const ENUM_CODE_BASE: u8 = 32;
    const DOMAIN_CODE_BASE: u8 = 64;
    const COMPOSITE_CODE_BASE: u8 = 96;

    /// Every catalog-defined built-in element type that pos3ql stores and
    /// transmits as an array. This is the single inventory for OID decoding
    /// and catalog synthesis, so adding an accepted array cannot leave its
    /// `pg_type` identity behind.
    pub const BUILTIN: [Self; 48] = [
        Self::Bool,
        Self::Int2,
        Self::Int4,
        Self::Oid,
        Self::Int8,
        Self::Float4,
        Self::Float8,
        Self::Text,
        Self::Name,
        Self::Varchar,
        Self::Bpchar,
        Self::Date,
        Self::Timestamp,
        Self::Timestamptz,
        Self::Time,
        Self::Timetz,
        Self::Interval,
        Self::Json,
        Self::Jsonb,
        Self::Uuid,
        Self::Bytea,
        Self::Numeric,
        Self::Inet,
        Self::Cidr,
        Self::Macaddr,
        Self::Macaddr8,
        Self::Bit,
        Self::Varbit,
        Self::Regtype,
        Self::Regproc,
        Self::Regprocedure,
        Self::Regoper,
        Self::Regoperator,
        Self::Regclass,
        Self::Regnamespace,
        Self::Regrole,
        Self::Range(RangeKind::Int4),
        Self::Range(RangeKind::Int8),
        Self::Range(RangeKind::Num),
        Self::Range(RangeKind::Date),
        Self::Range(RangeKind::Ts),
        Self::Range(RangeKind::Tstz),
        Self::Multirange(RangeKind::Int4),
        Self::Multirange(RangeKind::Int8),
        Self::Multirange(RangeKind::Num),
        Self::Multirange(RangeKind::Date),
        Self::Multirange(RangeKind::Ts),
        Self::Multirange(RangeKind::Tstz),
    ];

    /// Whether text input for this element needs catalog identity resolution.
    pub const fn is_catalog_reference(self) -> bool {
        matches!(
            self,
            Self::Regtype
                | Self::Regproc
                | Self::Regprocedure
                | Self::Regoper
                | Self::Regoperator
                | Self::Regclass
                | Self::Regnamespace
                | Self::Regrole
        )
    }

    /// The array type's internal `pg_type.typname`.
    pub fn catalog_name(self) -> &'static str {
        match self {
            ArrElem::Bool => "_bool",
            ArrElem::Int4 => "_int4",
            ArrElem::Oid => "_oid",
            ArrElem::Int8 => "_int8",
            ArrElem::Float8 => "_float8",
            ArrElem::Text => "_text",
            ArrElem::Numeric => "_numeric",
            ArrElem::Date => "_date",
            ArrElem::Timestamp => "_timestamp",
            ArrElem::Timestamptz => "_timestamptz",
            ArrElem::Int2 => "_int2",
            ArrElem::Float4 => "_float4",
            ArrElem::Time => "_time",
            ArrElem::Timetz => "_timetz",
            ArrElem::Interval => "_interval",
            ArrElem::Uuid => "_uuid",
            ArrElem::Bytea => "_bytea",
            ArrElem::Json => "_json",
            ArrElem::Jsonb => "_jsonb",
            ArrElem::Varchar => "_varchar",
            ArrElem::Bpchar => "_bpchar",
            ArrElem::Name => "_name",
            ArrElem::Inet => "_inet",
            ArrElem::Cidr => "_cidr",
            ArrElem::Macaddr => "_macaddr",
            ArrElem::Macaddr8 => "_macaddr8",
            ArrElem::Bit => "_bit",
            ArrElem::Varbit => "_varbit",
            ArrElem::Regtype => "_regtype",
            ArrElem::Regproc => "_regproc",
            ArrElem::Regprocedure => "_regprocedure",
            ArrElem::Regoper => "_regoper",
            ArrElem::Regoperator => "_regoperator",
            ArrElem::Regclass => "_regclass",
            ArrElem::Regnamespace => "_regnamespace",
            ArrElem::Regrole => "_regrole",
            ArrElem::Range(kind) => match kind {
                RangeKind::Int4 => "_int4range",
                RangeKind::Int8 => "_int8range",
                RangeKind::Num => "_numrange",
                RangeKind::Date => "_daterange",
                RangeKind::Ts => "_tsrange",
                RangeKind::Tstz => "_tstzrange",
            },
            ArrElem::Multirange(kind) => match kind {
                RangeKind::Int4 => "_int4multirange",
                RangeKind::Int8 => "_int8multirange",
                RangeKind::Num => "_nummultirange",
                RangeKind::Date => "_datemultirange",
                RangeKind::Ts => "_tsmultirange",
                RangeKind::Tstz => "_tstzmultirange",
            },
            ArrElem::Enum(_) => "_enum",
            ArrElem::Composite(_) => "_record",
            ArrElem::Domain { .. } => "_domain",
        }
    }

    /// The array type's own name, as PostgreSQL reports it in a message:
    /// `integer[]`, not `array`. The element's name with `[]` appended, but as
    /// a static string, since that is what a type name is here.
    pub fn array_name(self) -> &'static str {
        match self {
            ArrElem::Bool => "boolean[]",
            ArrElem::Int4 => "integer[]",
            ArrElem::Oid => "oid[]",
            ArrElem::Int8 => "bigint[]",
            ArrElem::Float8 => "double precision[]",
            ArrElem::Text => "text[]",
            ArrElem::Numeric => "numeric[]",
            ArrElem::Date => "date[]",
            ArrElem::Timestamp => "timestamp[]",
            ArrElem::Timestamptz => "timestamp with time zone[]",
            ArrElem::Int2 => "smallint[]",
            ArrElem::Float4 => "real[]",
            ArrElem::Time => "time without time zone[]",
            ArrElem::Timetz => "time with time zone[]",
            ArrElem::Interval => "interval[]",
            ArrElem::Uuid => "uuid[]",
            ArrElem::Bytea => "bytea[]",
            ArrElem::Json => "json[]",
            ArrElem::Jsonb => "jsonb[]",
            ArrElem::Varchar => "character varying[]",
            ArrElem::Bpchar => "character[]",
            ArrElem::Name => "name[]",
            ArrElem::Inet => "inet[]",
            ArrElem::Cidr => "cidr[]",
            ArrElem::Macaddr => "macaddr[]",
            ArrElem::Macaddr8 => "macaddr8[]",
            ArrElem::Bit => "bit[]",
            ArrElem::Varbit => "bit varying[]",
            ArrElem::Regtype => "regtype[]",
            ArrElem::Regproc => "regproc[]",
            ArrElem::Regprocedure => "regprocedure[]",
            ArrElem::Regoper => "regoper[]",
            ArrElem::Regoperator => "regoperator[]",
            ArrElem::Regclass => "regclass[]",
            ArrElem::Regnamespace => "regnamespace[]",
            ArrElem::Regrole => "regrole[]",
            ArrElem::Range(kind) => match kind {
                RangeKind::Int4 => "int4range[]",
                RangeKind::Int8 => "int8range[]",
                RangeKind::Num => "numrange[]",
                RangeKind::Date => "daterange[]",
                RangeKind::Ts => "tsrange[]",
                RangeKind::Tstz => "tstzrange[]",
            },
            ArrElem::Multirange(kind) => match kind {
                RangeKind::Int4 => "int4multirange[]",
                RangeKind::Int8 => "int8multirange[]",
                RangeKind::Num => "nummultirange[]",
                RangeKind::Date => "datemultirange[]",
                RangeKind::Ts => "tsmultirange[]",
                RangeKind::Tstz => "tstzmultirange[]",
            },
            ArrElem::Enum(_) => "enum[]",
            ArrElem::Composite(_) => "record[]",
            ArrElem::Domain { .. } => "domain[]",
        }
    }

    /// The array type's name as `pg_typeof` spells it — the temporal names
    /// written out in full, unlike the message form [`ArrElem::array_name`].
    pub fn typeof_name(self) -> &'static str {
        match self {
            ArrElem::Timestamp => "timestamp without time zone[]",
            other => other.array_name(),
        }
    }

    /// The array element type matching a scalar datum's runtime type.
    pub fn from_datum(d: &Datum) -> Option<ArrElem> {
        Some(match d {
            Datum::Bool(_) => ArrElem::Bool,
            Datum::Int2(_) => ArrElem::Int2,
            Datum::Int4(_) => ArrElem::Int4,
            Datum::Oid(_) => ArrElem::Oid,
            Datum::Int8(_) => ArrElem::Int8,
            Datum::Float4(_) => ArrElem::Float4,
            Datum::Float8(_) => ArrElem::Float8,
            Datum::Text(_) => ArrElem::Text,
            Datum::Bpchar(_) => ArrElem::Bpchar,
            Datum::Numeric(_) => ArrElem::Numeric,
            Datum::Date(_) => ArrElem::Date,
            Datum::Timestamp(_) => ArrElem::Timestamp,
            Datum::Timestamptz(_) => ArrElem::Timestamptz,
            Datum::Time(_) => ArrElem::Time,
            Datum::Timetz(..) => ArrElem::Timetz,
            Datum::Interval(_) => ArrElem::Interval,
            Datum::Uuid(_) => ArrElem::Uuid,
            Datum::Bytea(_) => ArrElem::Bytea,
            Datum::Json { jsonb: false, .. } => ArrElem::Json,
            Datum::Json { jsonb: true, .. } => ArrElem::Jsonb,
            Datum::Inet(_) => ArrElem::Inet,
            Datum::Cidr(_) => ArrElem::Cidr,
            Datum::Macaddr(_) => ArrElem::Macaddr,
            Datum::Macaddr8(_) => ArrElem::Macaddr8,
            Datum::Bit { varying: false, .. } => ArrElem::Bit,
            Datum::Bit { varying: true, .. } => ArrElem::Varbit,
            Datum::Regtype { .. } => ArrElem::Regtype,
            Datum::RegObject { type_oid, .. } => match *type_oid {
                oid::REGPROC => ArrElem::Regproc,
                oid::REGPROCEDURE => ArrElem::Regprocedure,
                oid::REGOPER => ArrElem::Regoper,
                oid::REGOPERATOR => ArrElem::Regoperator,
                oid::REGCLASS => ArrElem::Regclass,
                oid::REGNAMESPACE => ArrElem::Regnamespace,
                oid::REGROLE => ArrElem::Regrole,
                _ => return None,
            },
            Datum::Range { kind, .. } => ArrElem::Range(*kind),
            Datum::Multirange { kind, .. } => ArrElem::Multirange(*kind),
            Datum::Enum { slot, .. } => ArrElem::Enum(*slot),
            Datum::Composite { slot, .. } | Datum::CompositeText { slot, .. } => {
                ArrElem::Composite(*slot)
            }
            _ => return None,
        })
    }

    pub fn from_coltype(c: ColType) -> Option<ArrElem> {
        // The string types keep their identity as elements (varchar[] and
        // bpchar[] are their own array types), so match them before the
        // storage fold collapses them into text.
        match c {
            ColType::Varchar => return Some(ArrElem::Varchar),
            ColType::Bpchar => return Some(ArrElem::Bpchar),
            ColType::Name => return Some(ArrElem::Name),
            ColType::Oid => return Some(ArrElem::Oid),
            // real keeps its identity — storage() would fold it to float8.
            ColType::Float4 => return Some(ArrElem::Float4),
            ColType::Bit { varying: false } => return Some(ArrElem::Bit),
            ColType::Bit { varying: true } => return Some(ArrElem::Varbit),
            ColType::Enum(slot) => return Some(ArrElem::Enum(slot)),
            ColType::Composite(slot) => return Some(ArrElem::Composite(slot)),
            ColType::Regtype => return Some(ArrElem::Regtype),
            ColType::Regproc => return Some(ArrElem::Regproc),
            ColType::Regprocedure => return Some(ArrElem::Regprocedure),
            ColType::Regoper => return Some(ArrElem::Regoper),
            ColType::Regoperator => return Some(ArrElem::Regoperator),
            ColType::Regclass => return Some(ArrElem::Regclass),
            ColType::Regnamespace => return Some(ArrElem::Regnamespace),
            ColType::Regrole => return Some(ArrElem::Regrole),
            ColType::Range(kind) => return Some(ArrElem::Range(kind)),
            ColType::Multirange(kind) => return Some(ArrElem::Multirange(kind)),
            _ => {}
        }
        Some(match c.storage() {
            ColType::Bool => ArrElem::Bool,
            ColType::Int2 => ArrElem::Int2,
            ColType::Int4 => ArrElem::Int4,
            ColType::Int8 => ArrElem::Int8,
            ColType::Float8 => ArrElem::Float8,
            ColType::Text => ArrElem::Text,
            ColType::Numeric => ArrElem::Numeric,
            ColType::Date => ArrElem::Date,
            ColType::Timestamp => ArrElem::Timestamp,
            ColType::Timestamptz => ArrElem::Timestamptz,
            ColType::Time => ArrElem::Time,
            ColType::Timetz => ArrElem::Timetz,
            ColType::Interval => ArrElem::Interval,
            ColType::Uuid => ArrElem::Uuid,
            ColType::Bytea => ArrElem::Bytea,
            ColType::Json => ArrElem::Json,
            ColType::Jsonb => ArrElem::Jsonb,
            ColType::Inet => ArrElem::Inet,
            ColType::Cidr => ArrElem::Cidr,
            ColType::Macaddr => ArrElem::Macaddr,
            ColType::Macaddr8 => ArrElem::Macaddr8,
            _ => return None,
        })
    }

    pub fn to_coltype(self) -> ColType {
        match self {
            ArrElem::Bool => ColType::Bool,
            ArrElem::Int4 => ColType::Int4,
            ArrElem::Oid => ColType::Oid,
            ArrElem::Int8 => ColType::Int8,
            ArrElem::Float8 => ColType::Float8,
            ArrElem::Text => ColType::Text,
            ArrElem::Numeric => ColType::Numeric,
            ArrElem::Date => ColType::Date,
            ArrElem::Timestamp => ColType::Timestamp,
            ArrElem::Timestamptz => ColType::Timestamptz,
            ArrElem::Int2 => ColType::Int2,
            ArrElem::Float4 => ColType::Float4,
            ArrElem::Time => ColType::Time,
            ArrElem::Timetz => ColType::Timetz,
            ArrElem::Interval => ColType::Interval,
            ArrElem::Uuid => ColType::Uuid,
            ArrElem::Bytea => ColType::Bytea,
            ArrElem::Json => ColType::Json,
            ArrElem::Jsonb => ColType::Jsonb,
            ArrElem::Varchar => ColType::Varchar,
            ArrElem::Bpchar => ColType::Bpchar,
            ArrElem::Name => ColType::Name,
            ArrElem::Inet => ColType::Inet,
            ArrElem::Cidr => ColType::Cidr,
            ArrElem::Macaddr => ColType::Macaddr,
            ArrElem::Macaddr8 => ColType::Macaddr8,
            ArrElem::Bit => ColType::Bit { varying: false },
            ArrElem::Varbit => ColType::Bit { varying: true },
            ArrElem::Regtype => ColType::Regtype,
            ArrElem::Regproc => ColType::Regproc,
            ArrElem::Regprocedure => ColType::Regprocedure,
            ArrElem::Regoper => ColType::Regoper,
            ArrElem::Regoperator => ColType::Regoperator,
            ArrElem::Regclass => ColType::Regclass,
            ArrElem::Regnamespace => ColType::Regnamespace,
            ArrElem::Regrole => ColType::Regrole,
            ArrElem::Range(kind) => ColType::Range(kind),
            ArrElem::Multirange(kind) => ColType::Multirange(kind),
            ArrElem::Enum(slot) => ColType::Enum(slot),
            ArrElem::Composite(slot) => ColType::Composite(slot),
            ArrElem::Domain {
                base_code,
                base_user_slot,
                ..
            } => {
                match ColType::from_code(base_code)
                    .expect("domain array carries a valid scalar base code")
                {
                    ColType::Enum(_) => ColType::Enum(base_user_slot),
                    ColType::Composite(_) => ColType::Composite(base_user_slot),
                    base => base,
                }
            }
        }
    }

    /// The PostgreSQL array-type OID for this element type.
    pub fn array_oid(self) -> i32 {
        match self {
            ArrElem::Bool => 1000,
            ArrElem::Int4 => 1007,
            ArrElem::Oid => oid::OID_ARRAY,
            ArrElem::Int8 => 1016,
            ArrElem::Float8 => 1022,
            ArrElem::Text => 1009,
            ArrElem::Numeric => 1231,
            ArrElem::Date => 1182,
            ArrElem::Timestamp => 1115,
            ArrElem::Timestamptz => 1185,
            ArrElem::Int2 => 1005,
            ArrElem::Float4 => 1021,
            ArrElem::Time => 1183,
            ArrElem::Timetz => 1270,
            ArrElem::Interval => 1187,
            ArrElem::Uuid => 2951,
            ArrElem::Bytea => 1001,
            ArrElem::Json => 199,
            ArrElem::Jsonb => 3807,
            ArrElem::Varchar => 1015,
            ArrElem::Bpchar => 1014,
            ArrElem::Name => 1003,
            ArrElem::Inet => oid::INET_ARRAY,
            ArrElem::Cidr => oid::CIDR_ARRAY,
            ArrElem::Macaddr => oid::MACADDR_ARRAY,
            ArrElem::Macaddr8 => oid::MACADDR8_ARRAY,
            ArrElem::Bit => oid::BIT_ARRAY,
            ArrElem::Varbit => oid::VARBIT_ARRAY,
            ArrElem::Regtype => oid::REGTYPE_ARRAY,
            ArrElem::Regproc => oid::REGPROC_ARRAY,
            ArrElem::Regprocedure => oid::REGPROCEDURE_ARRAY,
            ArrElem::Regoper => oid::REGOPER_ARRAY,
            ArrElem::Regoperator => oid::REGOPERATOR_ARRAY,
            ArrElem::Regclass => oid::REGCLASS_ARRAY,
            ArrElem::Regnamespace => oid::REGNAMESPACE_ARRAY,
            ArrElem::Regrole => oid::REGROLE_ARRAY,
            ArrElem::Range(kind) => kind.array_oid(),
            ArrElem::Multirange(kind) => kind.multirange_array_oid(),
            ArrElem::Enum(slot) => oid::enum_array_oid(slot),
            ArrElem::Composite(slot) => oid::composite_array_oid(slot),
            ArrElem::Domain { slot, .. } => oid::domain_array_oid(slot),
        }
    }

    /// The PostgreSQL element-type OID carried inside a binary array value.
    /// Domain arrays retain the domain identity here even though their stored
    /// values use the domain's base representation.
    pub fn element_oid(self) -> i32 {
        match self {
            ArrElem::Domain { slot, .. } => oid::domain_oid(slot),
            _ => self.to_coltype().oid(),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            ArrElem::Bool => 0,
            ArrElem::Int4 => 1,
            ArrElem::Oid => 63,
            ArrElem::Int8 => 2,
            ArrElem::Float8 => 3,
            ArrElem::Text => 4,
            ArrElem::Numeric => 5,
            ArrElem::Date => 6,
            ArrElem::Timestamp => 7,
            ArrElem::Timestamptz => 8,
            ArrElem::Int2 => 9,
            ArrElem::Time => 10,
            ArrElem::Timetz => 11,
            ArrElem::Interval => 12,
            ArrElem::Uuid => 13,
            ArrElem::Bytea => 14,
            ArrElem::Json => 15,
            ArrElem::Jsonb => 16,
            ArrElem::Varchar => 17,
            ArrElem::Bpchar => 18,
            ArrElem::Name => 19,
            ArrElem::Float4 => 20,
            ArrElem::Inet => 21,
            ArrElem::Cidr => 22,
            ArrElem::Macaddr => 23,
            ArrElem::Macaddr8 => 24,
            ArrElem::Bit => 25,
            ArrElem::Varbit => 26,
            ArrElem::Regtype => 27,
            ArrElem::Regproc => 28,
            ArrElem::Regprocedure => 29,
            ArrElem::Regoper => 30,
            ArrElem::Regoperator => 31,
            ArrElem::Regclass => 48,
            ArrElem::Regnamespace => 49,
            ArrElem::Regrole => 50,
            ArrElem::Range(kind) => 51 + kind.code(),
            ArrElem::Multirange(kind) => 57 + kind.code(),
            ArrElem::Enum(slot) => Self::ENUM_CODE_BASE + slot as u8,
            ArrElem::Domain { slot, .. } => Self::DOMAIN_CODE_BASE + slot as u8,
            ArrElem::Composite(slot) => Self::COMPOSITE_CODE_BASE + slot as u8,
        }
    }

    pub fn from_code(c: u8) -> Option<ArrElem> {
        Some(match c {
            0 => ArrElem::Bool,
            1 => ArrElem::Int4,
            63 => ArrElem::Oid,
            2 => ArrElem::Int8,
            3 => ArrElem::Float8,
            4 => ArrElem::Text,
            5 => ArrElem::Numeric,
            6 => ArrElem::Date,
            7 => ArrElem::Timestamp,
            8 => ArrElem::Timestamptz,
            9 => ArrElem::Int2,
            10 => ArrElem::Time,
            11 => ArrElem::Timetz,
            12 => ArrElem::Interval,
            13 => ArrElem::Uuid,
            14 => ArrElem::Bytea,
            15 => ArrElem::Json,
            16 => ArrElem::Jsonb,
            17 => ArrElem::Varchar,
            18 => ArrElem::Bpchar,
            19 => ArrElem::Name,
            20 => ArrElem::Float4,
            21 => ArrElem::Inet,
            22 => ArrElem::Cidr,
            23 => ArrElem::Macaddr,
            24 => ArrElem::Macaddr8,
            25 => ArrElem::Bit,
            26 => ArrElem::Varbit,
            27 => ArrElem::Regtype,
            28 => ArrElem::Regproc,
            29 => ArrElem::Regprocedure,
            30 => ArrElem::Regoper,
            31 => ArrElem::Regoperator,
            48 => ArrElem::Regclass,
            49 => ArrElem::Regnamespace,
            50 => ArrElem::Regrole,
            51..=56 => ArrElem::Range(RangeKind::from_code(c - 51)?),
            57..=62 => ArrElem::Multirange(RangeKind::from_code(c - 57)?),
            c if (Self::ENUM_CODE_BASE..Self::ENUM_CODE_BASE + crate::storage::MAX_ENUMS as u8)
                .contains(&c) =>
            {
                ArrElem::Enum((c - Self::ENUM_CODE_BASE) as u16)
            }
            c if (Self::DOMAIN_CODE_BASE
                ..Self::DOMAIN_CODE_BASE + crate::storage::MAX_DOMAINS as u8)
                .contains(&c) =>
            {
                ArrElem::Domain {
                    slot: (c - Self::DOMAIN_CODE_BASE) as u16,
                    base_code: ColType::Text.code(),
                    base_user_slot: ColType::ENUM_SLOT_UNRESOLVED,
                }
            }
            c if (Self::COMPOSITE_CODE_BASE
                ..Self::COMPOSITE_CODE_BASE + crate::storage::MAX_COMPOSITES as u8)
                .contains(&c) =>
            {
                ArrElem::Composite((c - Self::COMPOSITE_CODE_BASE) as u16)
            }
            _ => return None,
        })
    }

    /// Rebuilds a domain-array element identity from the domain catalog.
    pub fn domain(slot: u16, base: ColType) -> Option<Self> {
        if matches!(base, ColType::Record) {
            return None;
        }
        let base_user_slot = match base {
            ColType::Enum(slot) | ColType::Composite(slot) => slot,
            _ => ColType::ENUM_SLOT_UNRESOLVED,
        };
        Some(Self::Domain {
            slot,
            base_code: base.code(),
            base_user_slot,
        })
    }

    pub fn user_type_slot(self) -> Option<u16> {
        match self {
            Self::Enum(slot) | Self::Composite(slot) | Self::Domain { slot, .. } => Some(slot),
            _ => None,
        }
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
            ColType::Array(element) => Self::decode(element.to_coltype(), atttypmod),
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
                    precision: if precision_raw <= 6 {
                        Some(precision_raw as u8)
                    } else {
                        None
                    },
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
    /// PostgreSQL's array type OID for this range type.
    pub fn array_oid(self) -> i32 {
        match self {
            Self::Int4 => 3905,
            Self::Num => 3907,
            Self::Ts => 3909,
            Self::Tstz => 3911,
            Self::Date => 3913,
            Self::Int8 => 3927,
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
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Self::Int4,
            1 => Self::Int8,
            2 => Self::Num,
            3 => Self::Date,
            4 => Self::Ts,
            5 => Self::Tstz,
            _ => return None,
        })
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
    /// PostgreSQL's array type OID for this multirange type.
    pub fn multirange_array_oid(self) -> i32 {
        match self {
            Self::Int4 => 6150,
            Self::Num => 6151,
            Self::Ts => 6152,
            Self::Tstz => 6153,
            Self::Date => 6155,
            Self::Int8 => 6157,
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
    /// `smallint`. The width is the type: an i16 cannot hold what PostgreSQL's
    /// smallint cannot, so out-of-range states are unrepresentable. Stored
    /// rows keep the historical 4-byte layout; decode narrows by schema.
    Int2(i16),
    Int4(i32),
    /// PostgreSQL's unsigned object identifier. This is distinct from int4:
    /// its upper half is valid and must not be rendered or ordered as negative.
    Oid(u32),
    Int8(i64),
    /// `real`/`float4`. The width is the type: an f32 holds exactly what
    /// PostgreSQL's real does, so casts round through it and arithmetic between
    /// two reals stays single precision. Stored rows keep the historical
    /// 8-byte float8 layout; decode narrows by schema.
    Float4(f32),
    Float8(f64),
    Text(&'a str),
    /// A `char(n)` value, blank-padded to its declared width. The padding is
    /// part of the value (PostgreSQL emits `max(c)` padded even when the
    /// result typmod is -1), but it is *semantically* insignificant: casts to
    /// other string types, comparisons, and functions taking `text` all see
    /// the stripped form, while output functions, `LIKE`/regex matching, and
    /// `octet_length` see the raw padded form.
    Bpchar(&'a str),
    /// A `regtype` value: the referenced type OID is its binary representation
    /// while the catalog-resolved name is its text representation.
    Regtype {
        referenced_oid: i32,
        name: &'a str,
    },
    /// A non-type catalog object reference. `type_oid` distinguishes the
    /// reg* aliases; its four-byte binary form is `referenced_oid`.
    RegObject {
        type_oid: i32,
        referenced_oid: i32,
        name: &'a str,
    },
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
    Json {
        text: &'a str,
        jsonb: bool,
    },
    /// An array's element type and canonical shaped row encoding.
    Array {
        element: ArrElem,
        raw: &'a [u8],
    },
    /// The fixed-width integer vector used by PostgreSQL system catalogs.
    Int2Vector(&'a [u8]),
    Uuid([u8; 16]),
    Bytea(&'a [u8]),
    Numeric(Numeric<'a>),
    /// A range value in its canonical text form (e.g. `[1,5)`, `empty`).
    Range {
        text: &'a str,
        kind: RangeKind,
    },
    /// A bit string as a sequence of `'0'`/`'1'` characters. `varying` selects
    /// the reported type: `false` = `bit(n)` (OID 1560), `true` = `varbit`
    /// (OID 1562).
    Bit {
        bits: &'a str,
        varying: bool,
    },
    /// A multirange value in canonical text form (e.g. `{[1,3),[5,7)}`, `{}`).
    Multirange {
        text: &'a str,
        kind: RangeKind,
    },
    /// An `inet` address (host bits preserved).
    Inet(NetAddr),
    /// A `cidr` network (always prints its mask length).
    Cidr(NetAddr),
    /// A six-byte `macaddr`.
    Macaddr([u8; 6]),
    /// An eight-byte `macaddr8`.
    Macaddr8([u8; 8]),
    /// A composite/record value: each field's name (for `row_to_json` etc.),
    /// its type OID (for JSON/typed output), and its value. Records are
    /// transient — produced by `t.*`, a bare table reference, or `ROW(...)` —
    /// never stored in a column.
    Record(&'a [RecordField<'a>]),
    /// A value of one named composite catalog type. Keeping this distinct from
    /// `Record` preserves the PostgreSQL type OID across expressions, Bind,
    /// Result and persistence; an anonymous row can never masquerade as it.
    Composite {
        slot: u16,
        fields: &'a [RecordField<'a>],
    },
    /// The durable row form of a named composite. It carries its immutable
    /// catalog slot and PostgreSQL text input spelling; only catalog-aware
    /// evaluation may materialize fields from it.
    CompositeText {
        slot: u16,
        /// Physical attribute count when the text was persisted. This is a
        /// layout version, not a display arity: dropped attributes retain
        /// their position and added attributes cannot be mistaken for old
        /// trailing values.
        physical_fields: u8,
        text: &'a str,
    },
    /// A user-defined enum value. `slot` identifies the enum type (for OID /
    /// `pg_typeof`); `sort` is the member's sort key, by which enum values
    /// order (PostgreSQL orders by `enumsortorder`, not label text); `label`
    /// is the member's text, used for output and equality. All three are
    /// carried inline so a value is self-describing — decode needs no catalog
    /// and [`compare_datums`](super::eval::operators::compare_datums) stays pure.
    Enum {
        slot: u16,
        sort: f64,
        label: &'a str,
    },
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
            Datum::Composite { slot, .. } => oid::composite_oid(*slot),
            Datum::CompositeText { slot, .. } => oid::composite_oid(*slot),
            Datum::Null => oid::TEXT,
            Datum::Bool(_) => oid::BOOL,
            Datum::Int2(_) => oid::INT2,
            Datum::Int4(_) => oid::INT4,
            Datum::Oid(_) => oid::OID,
            Datum::Int8(_) => oid::INT8,
            Datum::Float4(_) => oid::FLOAT4,
            Datum::Float8(_) => oid::FLOAT8,
            Datum::Text(_) => oid::TEXT,
            Datum::Bpchar(_) => oid::BPCHAR,
            Datum::Regtype { .. } => oid::REGTYPE,
            Datum::RegObject { type_oid, .. } => *type_oid,
            Datum::Date(_) => oid::DATE,
            Datum::Timestamp(_) => oid::TIMESTAMP,
            Datum::Timestamptz(_) => oid::TIMESTAMPTZ,
            Datum::Timetz(..) => oid::TIMETZ,
            Datum::Time(_) => oid::TIME,
            Datum::Interval(_) => oid::INTERVAL,
            Datum::Json { jsonb: false, .. } => oid::JSON,
            Datum::Json { jsonb: true, .. } => oid::JSONB,
            Datum::Array { element, .. } => element.array_oid(),
            Datum::Int2Vector(_) => oid::INT2VECTOR,
            Datum::Uuid(_) => oid::UUID,
            Datum::Bytea(_) => oid::BYTEA,
            Datum::Numeric(_) => oid::NUMERIC,
            Datum::Range { kind, .. } => kind.oid(),
            Datum::Bit { varying: false, .. } => oid::BIT,
            Datum::Bit { varying: true, .. } => oid::VARBIT,
            Datum::Multirange { kind, .. } => kind.multirange_oid(),
            Datum::Inet(_) => oid::INET,
            Datum::Cidr(_) => oid::CIDR,
            Datum::Macaddr(_) => oid::MACADDR,
            Datum::Macaddr8(_) => oid::MACADDR8,
            Datum::Enum { slot, .. } => oid::enum_oid(*slot),
        }
    }
}

/// Text-format rendering per PostgreSQL output conventions: booleans as
/// `t`/`f`, floats via Rust's shortest-roundtrip formatting.
/// PostgreSQL's `float8out`: shortest round-trip digits, fixed notation
/// only while the decimal exponent lies in [-4, 15), scientific otherwise
/// with a signed, at-least-two-digit exponent (`1e+15`, `1e-05`). Rust's
/// `{}` never chooses scientific notation, so `1e300` printed as 301
/// digits until a COPY-of-every-type corpus caught it.
fn write_pg_float8(f: &mut fmt::Formatter<'_>, v: f64) -> fmt::Result {
    if v.is_infinite() {
        return f.write_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    }
    if v.is_nan() {
        return f.write_str("NaN");
    }
    if v == 0.0 {
        return f.write_str(if v.is_sign_negative() { "-0" } else { "0" });
    }
    // Shortest digits from PostgreSQL's own Ryū (its non-STRICTLY_SHORTEST
    // boundary handling, which Rust's `{:e}` does not reproduce). float8out
    // uses fixed notation for decimal exponents in [-4, 15).
    let (digits, exp10) = crate::sql::ryu::f64_shortest(v);
    let mut buf = crate::util::StackStr::<24>::new();
    let _ = write!(buf, "{digits}");
    let digits = buf.as_str();
    let (head, tail) = digits.split_at(1);
    let exp = exp10 + (digits.len() as i32 - 1);
    let sign = if v.is_sign_negative() { "-" } else { "" };
    write_pg_float_notation(f, sign, head, tail, exp, 15)
}

/// `real`/float4 output, byte-for-byte with PostgreSQL. Its shortest digits
/// come from PostgreSQL's own Ryū (see [`crate::sql::ryu`]) — Rust's
/// `{:e}` resolves boundary cases differently — and its notation window is
/// narrower: fixed notation for decimal exponents in [-4, 6), scientific
/// otherwise.
fn write_pg_float4(f: &mut fmt::Formatter<'_>, v: f32) -> fmt::Result {
    if v.is_infinite() {
        return f.write_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    }
    if v.is_nan() {
        return f.write_str("NaN");
    }
    if v == 0.0 {
        return f.write_str(if v.is_sign_negative() { "-0" } else { "0" });
    }
    let (digits, exp10) = crate::sql::ryu::f32_shortest(v);
    let mut buf = crate::util::StackStr::<16>::new();
    let _ = write!(buf, "{digits}");
    let digits = buf.as_str();
    let (head, tail) = digits.split_at(1);
    // The exponent of the first significant digit.
    let exp = exp10 + (digits.len() as i32 - 1);
    let sign = if v.is_sign_negative() { "-" } else { "" };
    write_pg_float_notation(f, sign, head, tail, exp, 6)
}

/// Renders shortest-decimal digits under PostgreSQL's float output rule: fixed
/// notation when the first significant digit's decimal exponent is in
/// `[-4, upper)`, otherwise `d.ddde±XX` with a signed, ≥2-digit exponent. The
/// `upper` bound is the only thing that differs between float8 (15) and float4
/// (6). `head`+`tail` are the significant digits (one leading digit, then the
/// rest); `exp` is the exponent of `head`. Sign, infinities, zero and NaN are
/// handled by the callers.
fn write_pg_float_notation(
    f: &mut fmt::Formatter<'_>,
    sign: &str,
    head: &str,
    tail: &str,
    exp: i32,
    upper: i32,
) -> fmt::Result {
    debug_assert_eq!(head.len(), 1, "one leading significant digit");
    if (-4..upper).contains(&exp) {
        f.write_str(sign)?;
        if exp < 0 {
            // 0.000ddd…
            f.write_str("0.")?;
            for _ in 0..(-exp - 1) {
                f.write_str("0")?;
            }
            f.write_str(head)?;
            f.write_str(tail)?;
        } else {
            let exp = exp as usize;
            let digits_after_head = tail.len();
            f.write_str(head)?;
            if digits_after_head <= exp {
                // All digits precede the point; pad with zeros.
                f.write_str(tail)?;
                for _ in 0..(exp - digits_after_head) {
                    f.write_str("0")?;
                }
            } else {
                f.write_str(&tail[..exp])?;
                f.write_str(".")?;
                f.write_str(&tail[exp..])?;
            }
        }
        Ok(())
    } else {
        f.write_str(sign)?;
        f.write_str(head)?;
        if !tail.is_empty() {
            f.write_str(".")?;
            f.write_str(tail)?;
        }
        if exp < 0 {
            write!(f, "e-{:02}", -exp)
        } else {
            write!(f, "e+{exp:02}")
        }
    }
}

impl fmt::Display for Datum<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Datum::Null => Ok(()), // never rendered; NULL is a column-length of -1
            Datum::Bool(true) => f.write_str("t"),
            Datum::Bool(false) => f.write_str("f"),
            Datum::Int2(v) => write!(f, "{v}"),
            Datum::Int4(v) => write!(f, "{v}"),
            Datum::Oid(v) => write!(f, "{v}"),
            Datum::Int8(v) => write!(f, "{v}"),
            Datum::Float4(v) => write_pg_float4(f, *v),
            Datum::Float8(v) => write_pg_float8(f, *v),
            // The output function emits the padding — psql shows `hi   `.
            Datum::Text(s)
            | Datum::Bpchar(s)
            | Datum::Regtype { name: s, .. }
            | Datum::RegObject { name: s, .. } => f.write_str(s),
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
            Datum::Interval(interval) => {
                f.write_str(super::datetime::format_interval(*interval).as_str())
            }
            Datum::Json { text, .. } => f.write_str(text),
            Datum::Range { text, .. } => f.write_str(text),
            Datum::Bit { bits, .. } => f.write_str(bits),
            Datum::Multirange { text, .. } => f.write_str(text),
            Datum::Array { element, raw } => super::array::write(f, *element, raw),
            Datum::Int2Vector(raw) => {
                for (index, bytes) in raw.chunks_exact(2).enumerate() {
                    if index > 0 {
                        f.write_str(" ")?;
                    }
                    let value = i16::from_le_bytes([bytes[0], bytes[1]]);
                    write!(f, "{value}")?;
                }
                Ok(())
            }
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
            Datum::Enum { label, .. } => f.write_str(label),
            Datum::Inet(net) => super::net::format_addr(net, false, f),
            Datum::Cidr(net) => super::net::format_addr(net, true, f),
            Datum::Macaddr(bytes) => super::net::format_mac(bytes, f),
            Datum::Macaddr8(bytes) => super::net::format_mac(bytes, f),
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
            Datum::Composite { fields, .. } => {
                f.write_char('(')?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write_record_field(f, &field.value)?;
                }
                f.write_char(')')
            }
            Datum::CompositeText { text, .. } => f.write_str(text),
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
        if s.chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace())
        {
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
    let mut scan = QuoteScan {
        empty: true,
        special: false,
        text: [0; 4],
        len: 0,
    };
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
    /// The resolved collation identity of a collatable result.  It is internal
    /// planning metadata, not a RowDescription wire field.
    pub collation: crate::sql::ast::Collation,
}

impl<'a> ColDesc<'a> {
    pub fn new(name: &'a str, type_oid: i32, typlen: i16) -> Self {
        Self {
            name,
            type_oid,
            typlen,
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
        }
    }

    pub fn of_type(name: &'a str, t: ColType) -> Self {
        Self {
            collation: if t.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
            ..Self::new(name, t.oid(), t.typlen())
        }
    }

    /// The same description carrying the column's declared type modifier.
    pub fn with_type_mod(mut self, type_mod: i32) -> Self {
        self.type_mod = type_mod;
        self
    }

    /// The collation selected by the source expression or stored column.
    pub fn with_collation(mut self, collation: crate::sql::ast::Collation) -> Self {
        self.collation = collation;
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
        assert_eq!(
            TypeMod::NumericPS {
                precision: 6,
                scale: 2
            }
            .encode(),
            393222
        );
        assert_eq!(TypeMod::TemporalPrecision(3).encode(), 3); // timestamp(3)
        assert_eq!(TypeMod::TemporalPrecision(0).encode(), 0); // timestamp(0)
        assert_eq!(
            TypeMod::IntervalMod {
                range: INTERVAL_FULL_RANGE,
                precision: Some(1)
            }
            .encode(),
            2147418113 // interval(1)
        );
        assert_eq!(
            TypeMod::IntervalMod {
                range: 0x0C00,
                precision: None
            }
            .encode(),
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
            (
                ColType::Numeric,
                TypeMod::NumericPS {
                    precision: 6,
                    scale: 2,
                },
            ),
            (ColType::Timestamp, TypeMod::TemporalPrecision(3)),
            (ColType::Timestamptz, TypeMod::TemporalPrecision(0)),
            (ColType::Time, TypeMod::TemporalPrecision(6)),
            (ColType::Timetz, TypeMod::TemporalPrecision(2)),
            (
                ColType::Interval,
                TypeMod::IntervalMod {
                    range: INTERVAL_FULL_RANGE,
                    precision: Some(4),
                },
            ),
            (
                ColType::Interval,
                TypeMod::IntervalMod {
                    range: 0x0C00,
                    precision: None,
                },
            ),
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
            TypeMod::IntervalMod {
                range: 0x0C00,
                precision: None
            }
        );
    }

    #[test]
    fn text_rendering_matches_postgres_conventions() {
        assert_eq!(Datum::Bool(true).to_string(), "t");
        assert_eq!(Datum::Bool(false).to_string(), "f");
        assert_eq!(Datum::Int8(-42).to_string(), "-42");
        assert_eq!(Datum::Oid(u32::MAX).to_string(), "4294967295");
        assert_eq!(Datum::Float8(2.5).to_string(), "2.5");
        assert_eq!(Datum::Float8(f64::INFINITY).to_string(), "Infinity");
        assert_eq!(Datum::Text("hi").to_string(), "hi");
        // real/float4 output: shortest f32 digits, PostgreSQL's float4out
        // notation window ([-4, 6) fixed, scientific otherwise). The values
        // that render differently from float8 are the point.
        assert_eq!(Datum::Float4(12345678.0).to_string(), "1.2345678e+07");
        assert_eq!(Datum::Float4(1234567.0).to_string(), "1.234567e+06");
        assert_eq!(Datum::Float4(123456.0).to_string(), "123456");
        assert_eq!(Datum::Float4(1000000.0).to_string(), "1e+06");
        assert_eq!(Datum::Float4(0.1).to_string(), "0.1");
        assert_eq!(Datum::Float4(0.0001).to_string(), "0.0001");
        assert_eq!(Datum::Float4(1e-5).to_string(), "1e-05");
        assert_eq!(Datum::Float4(-0.0).to_string(), "-0");
        assert_eq!(Datum::Float4(f32::INFINITY).to_string(), "Infinity");
        assert_eq!(Datum::Float4(f32::NAN).to_string(), "NaN");
    }

    #[test]
    fn float4_survives_the_row_encoding() {
        // real keeps the historical 8-byte float8 layout and narrows back to
        // f32 at decode by schema — the value round-trips exactly.
        use crate::storage::rowenc;
        let values = [Datum::Float4(0.1_f32), Datum::Float4(16777216.0)];
        let len = rowenc::encoded_len(&values);
        let mut buf = [0u8; 48];
        rowenc::encode(&values, &mut buf[..len]);
        let mut out = [Datum::Null; 2];
        rowenc::decode(&buf[..len], &[ColType::Float4, ColType::Float4], &mut out).unwrap();
        assert_eq!(out[0], Datum::Float4(0.1_f32));
        assert_eq!(out[1], Datum::Float4(16777216.0));
    }

    #[test]
    fn type_names_map() {
        assert_eq!(ColType::from_sql_name("integer"), Some(ColType::Int4));
        assert_eq!(ColType::from_sql_name("oid"), Some(ColType::Oid));
        assert_eq!(ColType::from_sql_name("float8"), Some(ColType::Float8));
        assert_eq!(ColType::from_sql_name("record"), Some(ColType::Record));
        assert_eq!(ColType::from_sql_name("geometry"), None);
    }

    #[test]
    fn from_oid_inverts_oid() {
        // Every type the binary-parameter path can name by OID round-trips.
        let mut types = vec![
            ColType::Void,
            ColType::Bool,
            ColType::Int2,
            ColType::Int2Vector,
            ColType::Int4,
            ColType::Oid,
            ColType::Regtype,
            ColType::Regproc,
            ColType::Regprocedure,
            ColType::Regoper,
            ColType::Regoperator,
            ColType::Regclass,
            ColType::Regnamespace,
            ColType::Regrole,
            ColType::Int8,
            ColType::Float4,
            ColType::Float8,
            ColType::Text,
            ColType::Name,
            ColType::Varchar,
            ColType::Bpchar,
            ColType::Date,
            ColType::Timestamp,
            ColType::Timestamptz,
            ColType::Time,
            ColType::Timetz,
            ColType::Interval,
            ColType::Json,
            ColType::Jsonb,
            ColType::Uuid,
            ColType::Bytea,
            ColType::Numeric,
            ColType::Bit { varying: false },
            ColType::Bit { varying: true },
            ColType::Inet,
            ColType::Cidr,
            ColType::Macaddr,
            ColType::Macaddr8,
            ColType::Record,
        ];
        for k in [
            RangeKind::Int4,
            RangeKind::Int8,
            RangeKind::Num,
            RangeKind::Date,
            RangeKind::Ts,
            RangeKind::Tstz,
        ] {
            types.push(ColType::Range(k));
            types.push(ColType::Multirange(k));
        }
        for e in ArrElem::BUILTIN {
            types.push(ColType::Array(e));
        }
        for t in types {
            assert_eq!(
                ColType::from_oid(t.oid()),
                Some(t),
                "{t:?} did not round-trip through its OID"
            );
        }
        // An OID this engine does not model is None, never a wrong type.
        assert_eq!(ColType::from_oid(0), None);
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
            ColType::Void,
            ColType::Bool,
            ColType::Int2,
            ColType::Int2Vector,
            ColType::Int4,
            ColType::Oid,
            ColType::Regtype,
            ColType::Regproc,
            ColType::Regprocedure,
            ColType::Regoper,
            ColType::Regoperator,
            ColType::Regclass,
            ColType::Regnamespace,
            ColType::Regrole,
            ColType::Int8,
            ColType::Float4,
            ColType::Float8,
            ColType::Text,
            ColType::Name,
            ColType::Varchar,
            ColType::Bpchar,
            ColType::Date,
            ColType::Timestamp,
            ColType::Timestamptz,
            ColType::Time,
            ColType::Timetz,
            ColType::Interval,
            ColType::Json,
            ColType::Jsonb,
            ColType::Uuid,
            ColType::Bytea,
            ColType::Numeric,
            ColType::Bit { varying: false },
            ColType::Bit { varying: true },
            ColType::Inet,
            ColType::Cidr,
            ColType::Macaddr,
            ColType::Macaddr8,
        ];
        for k in [
            RangeKind::Int4,
            RangeKind::Int8,
            RangeKind::Num,
            RangeKind::Date,
            RangeKind::Ts,
            RangeKind::Tstz,
        ] {
            types.push(ColType::Range(k));
            types.push(ColType::Multirange(k));
        }
        for e in ArrElem::BUILTIN {
            types.push(ColType::Array(e));
        }
        // The layout this replaced could emit any code in 20..=40; a moved
        // family must not reuse one, or old data decodes as the wrong type
        // instead of failing.
        // 20..=40 is what the previous layout could emit. Only the families
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
            assert_eq!(
                ColType::from_code(c),
                Some(t),
                "code {c} does not round-trip for {t:?}"
            );
            if let Some((_, other)) = seen.iter().find(|(code, _)| *code == c) {
                panic!("code {c} is produced by both {other:?} and {t:?}");
            }
            seen.push((c, t));
        }
        assert_eq!(ColType::from_code(ColType::Record.code()), None);
        assert_eq!(
            ColType::from_code(ColType::Enum(0).code()),
            Some(ColType::Enum(ColType::ENUM_SLOT_UNRESOLVED))
        );
    }
}
