//! Table storage: the in-memory write path of the LSM.
//!
//! Row bytes live in one fixed heap (the memtable); each table maps rowid →
//! location. Updates write a new copy and repoint the map — superseded
//! bytes are reclaimed when the memtable flushes to object storage (later
//! phase). All capacities are fixed at startup.

pub(crate) mod rowenc;

use core::cell::Cell;
use core::hash::{Hash, Hasher};

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::mem::value_index::ValueIndexPool;
use crate::pg::replication_client::{ConnectionInfo, ConnectionInfoError};
use crate::sql::ast::{Collation, TablespaceCost};
use crate::sql::eval::{SqlError, SqlState, hash_key, hash_key_collated, sqlstate};
use crate::sql::types::{ArrElem, ColType, Datum};
use crate::sql_err;
use crate::store::BlockStore;
use crate::util::StackStr;

pub(crate) use rowenc::MAX_COLUMNS;

/// Maximum explicit relation membership entries in one publication.  WAL
/// encodes this count in one byte, so the capacity derives from that boundary
/// instead of permitting an unencodable 256th member.
pub(crate) const MAX_PUBLICATION_TABLES: usize = u8::MAX as usize;
/// One publication's filters live inline in its fixed catalog record.  The
/// bound keeps catalog and WAL decode frames comfortably below the server's
/// bounded-stack contract while still accepting the simple predicates that
/// PostgreSQL permits here.
pub(crate) const PUBLICATION_FILTER_SQL_MAX: usize = 64;
/// Total filter-source storage for one publication. Individual filters retain
/// their own SQL boundary above; this shared storage avoids reserving that
/// boundary for every unused relation member.
pub(crate) const PUBLICATION_FILTER_STORAGE_BYTES: usize = 512;

#[derive(Clone, Copy, Debug)]
pub struct PublicationFilters {
    sql: StackStr<PUBLICATION_FILTER_STORAGE_BYTES>,
    ends: [u16; MAX_PUBLICATION_TABLES],
}

impl PublicationFilters {
    pub(crate) const EMPTY: Self = Self {
        sql: StackStr::new(),
        ends: [0; MAX_PUBLICATION_TABLES],
    };

    pub(crate) fn from_sql(
        filters: &[StackStr<PUBLICATION_FILTER_SQL_MAX>],
    ) -> Result<Self, SqlError> {
        let mut out = Self::EMPTY;
        for (index, filter) in filters.iter().enumerate() {
            let before = out.sql.as_str().len();
            use core::fmt::Write;
            let _ = out.sql.write_str(filter.as_str());
            if out.sql.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "publication row filters exceed {} bytes",
                    PUBLICATION_FILTER_STORAGE_BYTES
                ));
            }
            out.ends[index] = (before + filter.as_str().len()) as u16;
        }
        Ok(out)
    }

    pub(crate) fn get(&self, index: usize) -> &str {
        let start = if index == 0 {
            0
        } else {
            self.ends[index - 1] as usize
        };
        &self.sql.as_str()[start..self.ends[index] as usize]
    }

    pub(crate) fn materialize_sql(
        &self,
        count: usize,
    ) -> [StackStr<PUBLICATION_FILTER_SQL_MAX>; MAX_PUBLICATION_TABLES] {
        let mut filters = [StackStr::new(); MAX_PUBLICATION_TABLES];
        for (index, filter) in filters[..count].iter_mut().enumerate() {
            *filter = StackStr::from_str(self.get(index));
        }
        filters
    }
}

/// A subscription's publication list is part of its startup-bounded durable
/// state, rather than an unbounded connection-side string.
pub(crate) const MAX_SUBSCRIPTION_PUBLICATIONS: usize = 16;
pub(crate) const SUBSCRIPTION_CONNINFO_BYTES: usize = 512;
pub(crate) const MAX_TRIGGER_ARGUMENTS: usize = 16;
pub(crate) const TRIGGER_ARGUMENT_BYTES: usize = u8::MAX as usize;

/// A bounded trigger invocation argument vector.  SQL parses the literals once
/// before catalog mutation; WAL and checkpoints carry the same typed state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TriggerArguments {
    values: [StackStr<TRIGGER_ARGUMENT_BYTES>; MAX_TRIGGER_ARGUMENTS],
    count: u8,
}

impl TriggerArguments {
    pub(crate) const EMPTY: Self = Self {
        values: [StackStr::new(); MAX_TRIGGER_ARGUMENTS],
        count: 0,
    };

    pub(crate) fn parse(values: &[&str]) -> Result<Self, SqlError> {
        if values.len() > MAX_TRIGGER_ARGUMENTS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many trigger arguments (limit {})",
                MAX_TRIGGER_ARGUMENTS
            ));
        }
        let mut out = Self::EMPTY;
        for (index, value) in values.iter().enumerate() {
            if value.as_bytes().contains(&0) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "trigger argument contains NUL byte"
                ));
            }
            let parsed = StackStr::from_str(value);
            if parsed.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "trigger argument exceeds {} bytes",
                    TRIGGER_ARGUMENT_BYTES
                ));
            }
            out.values[index] = parsed;
        }
        out.count = values.len() as u8;
        Ok(out)
    }

    pub(crate) fn values(&self) -> &[StackStr<TRIGGER_ARGUMENT_BYTES>] {
        &self.values[..usize::from(self.count)]
    }
}

/// A bounded, nonempty connection-info value. SQL and WAL must construct this
/// before durable catalog state is changed, so truncation and NUL bytes cannot
/// become a deferred runtime connection failure.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionConnInfo {
    text: StackStr<SUBSCRIPTION_CONNINFO_BYTES>,
    endpoint: Option<ConnectionInfo>,
}

impl SubscriptionConnInfo {
    pub(crate) fn parse(value: &str) -> Result<Self, SqlError> {
        if value.is_empty() || value.as_bytes().contains(&0) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "subscription connection string must be nonempty and contain no NUL byte"
            ));
        }
        let value = StackStr::from_str(value);
        if value.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription connection string exceeds {} bytes",
                SUBSCRIPTION_CONNINFO_BYTES
            ));
        }
        crate::pg::replication_client::validate_connection_syntax(value.as_str())
            .map_err(subscription_conninfo_error)?;
        // A disabled PostgreSQL subscription may contain connection settings
        // that this server never opens. Keep its validated catalog spelling;
        // an enabled subscription must prove a usable bounded endpoint below.
        let endpoint = ConnectionInfo::parse(value.as_str()).ok();
        Ok(Self {
            text: value,
            endpoint,
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }

    /// The typed publisher endpoint, when this conninfo is usable by the
    /// bounded replication client.
    pub(crate) fn endpoint(&self) -> Option<ConnectionInfo> {
        self.endpoint
    }

    pub(crate) fn require_endpoint(&self) -> Result<ConnectionInfo, SqlError> {
        self.endpoint.ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "enabled subscription connection string requires a numeric host, port, user, dbname, and sslmode"
            )
        })
    }
}

fn subscription_conninfo_error(error: ConnectionInfoError) -> SqlError {
    let message = match error {
        ConnectionInfoError::Missing(field) => {
            return sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "subscription connection string requires {}",
                field
            );
        }
        ConnectionInfoError::Duplicate => "subscription connection string repeats an option",
        ConnectionInfoError::InvalidValue => {
            "subscription connection string contains an invalid value"
        }
        ConnectionInfoError::InvalidPort => "subscription connection string has an invalid port",
        ConnectionInfoError::NonNumericHost => {
            "subscription connection string requires a numeric host address"
        }
        ConnectionInfoError::UnsupportedOption => {
            "subscription connection string contains an unsupported option"
        }
        ConnectionInfoError::UnsupportedSslMode => {
            "subscription connection string has an unsupported sslmode"
        }
        ConnectionInfoError::Limit => "subscription connection string value exceeds its limit",
        ConnectionInfoError::Syntax => "subscription connection string has invalid syntax",
    };
    sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "{}", message)
}

/// Rows handed from the durable merge cursor to the executor. The fixed batch
/// boundary lets one resident SST block feed a bounded scan step rather than
/// crossing the storage/executor seam once per row.
#[derive(Clone, Copy)]
pub(crate) struct SpilledRow<'a> {
    pub(crate) rowid: u64,
    pub(crate) representation: SpilledRowRepresentation<'a>,
}

/// The physical representation a merged spill cursor hands to the executor.
/// Canonical entries retain the historical path; PAX entries arrive as
/// statement-owned decoded values after the resident block has been released.
#[derive(Clone, Copy)]
pub(crate) enum SpilledRowRepresentation<'a> {
    Encoded(&'a [u8]),
    Values(&'a [Datum<'a>]),
}

/// A scan batch is deliberately small enough to leave statement-arena space
/// for joins and expression results, while amortizing the cold cursor seam.
pub(crate) const SPILL_SCAN_BATCH_ROWS: usize = 128;

type SpilledRowBatchVisitor<'a, 'callback> =
    dyn FnMut(&[SpilledRow<'a>]) -> Result<core::ops::ControlFlow<()>, SqlError> + 'callback;

fn spill_read_error(error: crate::store::SstError) -> SqlError {
    match error {
        crate::store::SstError::Store(crate::store::StoreError::NotReady) => {
            sql_err!(sqlstate::INTERNAL_IO_WAIT, "durable block read in progress")
        }
        other => sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", other),
    }
}

/// An SQL identifier, owned inline. PostgreSQL caps names at 63 bytes
/// (NAMEDATALEN - 1); so does this.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SqlName {
    len: u8,
    bytes: [u8; 63],
}

impl SqlName {
    /// A zero-length name, for statically initializing arrays of names.
    pub const EMPTY: Self = SqlName {
        len: 0,
        bytes: [0u8; 63],
    };

    pub fn parse(s: &str) -> Result<Self, SqlError> {
        if s.len() > 63 {
            // PostgreSQL truncates with a notice; failing loudly is safer.
            return Err(sql_err!(
                crate::sql::eval::sqlstate::NAME_TOO_LONG,
                "name \"{}\" is longer than 63 bytes",
                s
            ));
        }
        let mut bytes = [0u8; 63];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Ok(Self {
            len: s.len() as u8,
            bytes,
        })
    }

    pub fn as_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len as usize]) }
    }
}

/// A replication-slot name already proven to satisfy PostgreSQL's portable
/// `[a-z0-9_]{1,63}` boundary. Keeping it distinct from an SQL identifier
/// prevents quoted or catalog-recovered names from bypassing that contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplicationSlotName(SqlName);

impl ReplicationSlotName {
    pub(crate) fn parse(value: &str) -> Result<Self, SqlError> {
        if value.is_empty()
            || value.len() > 63
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(sql_err!(
                sqlstate::INVALID_NAME,
                "invalid replication slot name \"{}\"",
                value
            ));
        }
        Ok(Self(SqlName::parse(value)?))
    }

    pub(crate) const fn sql_name(self) -> SqlName {
        self.0
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Hash for SqlName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl core::fmt::Debug for SqlName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A small owned constant, storable in the catalog (column defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OwnedDatum {
    Null,
    Bool(bool),
    Int4(i32),
    Oid(u32),
    Int8(i64),
    Regtype {
        referenced_oid: i32,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    RegObject {
        type_oid: i32,
        referenced_oid: i32,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Float8(f64),
    Text {
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Numeric {
        sign: u8,
        weight: i16,
        dscale: u16,
        nbytes: u8,
        digits: [u8; MAX_DEFAULT_TEXT],
    },
    Date(i32),
    Timestamp(i64),
    Timestamptz(i64),
    Time(i64),
    Timetz(i64, i32),
    Interval(crate::sql::types::Interval),
    Json {
        jsonb: bool,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Array {
        element: crate::sql::types::ArrElem,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Range {
        kind: crate::sql::types::RangeKind,
        multirange: bool,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Bit {
        varying: bool,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Uuid([u8; 16]),
    Bytea {
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
    Inet(crate::sql::net::NetAddr),
    Cidr(crate::sql::net::NetAddr),
    Macaddr([u8; 6]),
    Macaddr8([u8; 8]),
    Enum {
        slot: u16,
        sort: f64,
        len: u8,
        bytes: [u8; MAX_DEFAULT_TEXT],
    },
}

/// Fixed catalog capacity for a parsed default value. It matches the bounded
/// SQL source capacity, so a supported literal cannot become unrepresentable
/// merely because its typed form takes a few more bytes.
pub(crate) const MAX_DEFAULT_TEXT: usize = DEFAULT_EXPR_MAX;

/// The complete default state of one column.
///
/// A constant retains its parsed value as an execution cache and its source as
/// catalog identity. Keeping the alternatives together prevents a column from
/// being both generated and defaulted, or from carrying a value without the
/// expression that describes it.
#[derive(Debug, Clone, Copy)]
pub enum ColumnDefault {
    None,
    Constant {
        value: OwnedDatum,
        expression: StackStr<DEFAULT_EXPR_MAX>,
    },
    Expression(StackStr<DEFAULT_EXPR_MAX>),
    Generated(StackStr<DEFAULT_EXPR_MAX>),
}

impl ColumnDefault {
    pub const NONE: Self = Self::None;

    pub const fn expression(&self) -> Option<&StackStr<DEFAULT_EXPR_MAX>> {
        match self {
            Self::None => None,
            Self::Constant { expression, .. }
            | Self::Expression(expression)
            | Self::Generated(expression) => Some(expression),
        }
    }

    pub const fn constant(&self) -> Option<&OwnedDatum> {
        match self {
            Self::Constant { value, .. } => Some(value),
            Self::None | Self::Expression(_) | Self::Generated(_) => None,
        }
    }

    pub const fn is_generated(self) -> bool {
        matches!(self, Self::Generated(_))
    }

    pub const fn from_parts(
        value: Option<OwnedDatum>,
        expression: Option<StackStr<DEFAULT_EXPR_MAX>>,
        generated: bool,
    ) -> Option<Self> {
        match (value, expression, generated) {
            (None, None, false) => Some(Self::None),
            (Some(value), Some(expression), false) => Some(Self::Constant { value, expression }),
            (Some(_), None, false) => None,
            (None, Some(expression), false) => Some(Self::Expression(expression)),
            (None, Some(expression), true) => Some(Self::Generated(expression)),
            _ => None,
        }
    }
}

impl OwnedDatum {
    pub fn from_datum(d: &crate::sql::types::Datum) -> Result<Self, SqlError> {
        use crate::sql::types::Datum;
        Ok(match d {
            Datum::Record(_) | Datum::Composite { .. } | Datum::CompositeText { .. } => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot store a composite (record) value in a column"
                ));
            }
            Datum::Int2Vector(_) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot store an int2vector value in a column"
                ));
            }
            Datum::OidVector(_) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot store an oidvector value in a column"
                ));
            }
            Datum::Regtype {
                referenced_oid,
                name,
            } => {
                let (len, bytes) = Self::bytes(name.as_bytes(), "regtype")?;
                Self::Regtype {
                    referenced_oid: *referenced_oid,
                    len,
                    bytes,
                }
            }
            Datum::RegObject {
                type_oid,
                referenced_oid,
                name,
            } => {
                let (len, bytes) = Self::bytes(name.as_bytes(), "catalog object")?;
                Self::RegObject {
                    type_oid: *type_oid,
                    referenced_oid: *referenced_oid,
                    len,
                    bytes,
                }
            }
            Datum::Null => Self::Null,
            Datum::Bool(b) => Self::Bool(*b),
            Datum::Int4(v) => Self::Int4(*v),
            Datum::Oid(v) => Self::Oid(*v),
            Datum::Int2(v) => Self::Int4(*v as i32),
            Datum::Int8(v) => Self::Int8(*v),
            // Widened like int2→int4; the column re-coerces the default back to
            // real (f64→f32 is lossless for a value that was already f32).
            Datum::Float4(v) => Self::Float8(f64::from(*v)),
            Datum::Float8(v) => Self::Float8(*v),
            Datum::Date(value) => Self::Date(*value),
            Datum::Timestamp(value) => Self::Timestamp(*value),
            Datum::Timestamptz(value) => Self::Timestamptz(*value),
            Datum::Time(value) => Self::Time(*value),
            Datum::Timetz(time, zone) => Self::Timetz(*time, *zone),
            Datum::Interval(value) => Self::Interval(*value),
            Datum::Uuid(value) => Self::Uuid(*value),
            Datum::Json { text, jsonb } => Self::json(*jsonb, text)?,
            Datum::Array { element, raw } => Self::array(*element, raw)?,
            Datum::Range { text, kind } => Self::range(*kind, false, text)?,
            Datum::Multirange { text, kind } => Self::range(*kind, true, text)?,
            Datum::Bit { bits, varying } => Self::bit(*varying, bits)?,
            Datum::Bytea(bytes) => Self::bytea(bytes)?,
            Datum::Inet(n) => Self::Inet(*n),
            Datum::Cidr(n) => Self::Cidr(*n),
            Datum::Macaddr(b) => Self::Macaddr(*b),
            Datum::Macaddr8(b) => Self::Macaddr8(*b),
            Datum::Enum { slot, sort, label } => {
                if label.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "enum defaults are limited to {} bytes",
                        MAX_DEFAULT_TEXT
                    ));
                }
                let mut bytes = [0u8; MAX_DEFAULT_TEXT];
                bytes[..label.len()].copy_from_slice(label.as_bytes());
                Self::Enum {
                    slot: *slot,
                    sort: *sort,
                    len: label.len() as u8,
                    bytes,
                }
            }
            Datum::Numeric(n) => {
                if n.digits.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "numeric default too large"
                    ));
                }
                let mut digits = [0u8; MAX_DEFAULT_TEXT];
                digits[..n.digits.len()].copy_from_slice(n.digits);
                Self::Numeric {
                    sign: match n.sign {
                        crate::sql::numeric::Sign::Pos => 0,
                        crate::sql::numeric::Sign::Neg => 1,
                        crate::sql::numeric::Sign::NaN => 2,
                    },
                    weight: n.weight,
                    dscale: n.dscale,
                    nbytes: n.digits.len() as u8,
                    digits,
                }
            }
            Datum::Text(s) | Datum::Bpchar(s) => {
                if s.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "text defaults are limited to {} bytes",
                        MAX_DEFAULT_TEXT
                    ));
                }
                let mut bytes = [0u8; MAX_DEFAULT_TEXT];
                bytes[..s.len()].copy_from_slice(s.as_bytes());
                Self::Text {
                    len: s.len() as u8,
                    bytes,
                }
            }
        })
    }

    pub fn as_datum(&self) -> crate::sql::types::Datum<'_> {
        use crate::sql::types::Datum;
        match self {
            Self::Null => Datum::Null,
            Self::Bool(b) => Datum::Bool(*b),
            Self::Int4(v) => Datum::Int4(*v),
            Self::Oid(v) => Datum::Oid(*v),
            Self::Int8(v) => Datum::Int8(*v),
            Self::Regtype {
                referenced_oid,
                len,
                bytes,
            } => Datum::Regtype {
                referenced_oid: *referenced_oid,
                name: core::str::from_utf8(&bytes[..*len as usize])
                    .expect("stored from valid UTF-8"),
            },
            Self::RegObject {
                type_oid,
                referenced_oid,
                len,
                bytes,
            } => Datum::RegObject {
                type_oid: *type_oid,
                referenced_oid: *referenced_oid,
                name: core::str::from_utf8(&bytes[..*len as usize])
                    .expect("stored from valid UTF-8"),
            },
            Self::Float8(v) => Datum::Float8(*v),
            Self::Date(value) => Datum::Date(*value),
            Self::Timestamp(value) => Datum::Timestamp(*value),
            Self::Timestamptz(value) => Datum::Timestamptz(*value),
            Self::Time(value) => Datum::Time(*value),
            Self::Timetz(time, zone) => Datum::Timetz(*time, *zone),
            Self::Interval(value) => Datum::Interval(*value),
            Self::Uuid(value) => Datum::Uuid(*value),
            Self::Text { len, bytes } => Datum::Text(
                core::str::from_utf8(&bytes[..*len as usize]).expect("stored from valid UTF-8"),
            ),
            Self::Numeric {
                sign,
                weight,
                dscale,
                nbytes,
                digits,
            } => Datum::Numeric(crate::sql::numeric::Numeric {
                sign: match sign {
                    0 => crate::sql::numeric::Sign::Pos,
                    1 => crate::sql::numeric::Sign::Neg,
                    _ => crate::sql::numeric::Sign::NaN,
                },
                weight: *weight,
                dscale: *dscale,
                digits: &digits[..*nbytes as usize],
            }),
            Self::Json { jsonb, len, bytes } => Datum::Json {
                text: core::str::from_utf8(&bytes[..*len as usize])
                    .expect("stored from valid UTF-8"),
                jsonb: *jsonb,
            },
            Self::Array {
                element,
                len,
                bytes,
            } => Datum::Array {
                element: *element,
                raw: &bytes[..*len as usize],
            },
            Self::Range {
                kind,
                multirange,
                len,
                bytes,
            } => {
                let text =
                    core::str::from_utf8(&bytes[..*len as usize]).expect("stored from valid UTF-8");
                if *multirange {
                    Datum::Multirange { text, kind: *kind }
                } else {
                    Datum::Range { text, kind: *kind }
                }
            }
            Self::Bit {
                varying,
                len,
                bytes,
            } => Datum::Bit {
                bits: core::str::from_utf8(&bytes[..*len as usize])
                    .expect("stored from valid UTF-8"),
                varying: *varying,
            },
            Self::Bytea { len, bytes } => Datum::Bytea(&bytes[..*len as usize]),
            Self::Inet(n) => Datum::Inet(*n),
            Self::Cidr(n) => Datum::Cidr(*n),
            Self::Macaddr(b) => Datum::Macaddr(*b),
            Self::Macaddr8(b) => Datum::Macaddr8(*b),
            Self::Enum {
                slot,
                sort,
                len,
                bytes,
            } => Datum::Enum {
                slot: *slot,
                sort: *sort,
                label: core::str::from_utf8(&bytes[..*len as usize])
                    .expect("stored from valid UTF-8"),
            },
        }
    }

    fn bytes(value: &[u8], what: &str) -> Result<(u8, [u8; MAX_DEFAULT_TEXT]), SqlError> {
        if value.len() > MAX_DEFAULT_TEXT {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "{} default exceeds the fixed {} byte catalog limit",
                what,
                MAX_DEFAULT_TEXT
            ));
        }
        let mut bytes = [0; MAX_DEFAULT_TEXT];
        bytes[..value.len()].copy_from_slice(value);
        Ok((value.len() as u8, bytes))
    }

    fn json(jsonb: bool, text: &str) -> Result<Self, SqlError> {
        let (len, bytes) = Self::bytes(text.as_bytes(), "JSON")?;
        Ok(Self::Json { jsonb, len, bytes })
    }

    fn array(element: crate::sql::types::ArrElem, raw: &[u8]) -> Result<Self, SqlError> {
        let (len, bytes) = Self::bytes(raw, "array")?;
        Ok(Self::Array {
            element,
            len,
            bytes,
        })
    }

    fn range(
        kind: crate::sql::types::RangeKind,
        multirange: bool,
        text: &str,
    ) -> Result<Self, SqlError> {
        let (len, bytes) = Self::bytes(text.as_bytes(), "range")?;
        Ok(Self::Range {
            kind,
            multirange,
            len,
            bytes,
        })
    }

    fn bit(varying: bool, bits: &str) -> Result<Self, SqlError> {
        let (len, bytes) = Self::bytes(bits.as_bytes(), "bit string")?;
        Ok(Self::Bit {
            varying,
            len,
            bytes,
        })
    }

    fn bytea(value: &[u8]) -> Result<Self, SqlError> {
        let (len, bytes) = Self::bytes(value, "bytea")?;
        Ok(Self::Bytea { len, bytes })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnMeta {
    pub name: SqlName,
    pub ctype: ColType,
    /// PostgreSQL atttypmod: -1 = none. varchar(n)/char(n) encode `n + 4`;
    /// numeric(p,s) encodes `((p<<16)|s) + 4`. Enforced during coercion.
    pub type_mod: i32,
    pub collation: Collation,
    pub not_null: NotNullOrigin,
    pub unique: bool,
    pub primary: bool,
    /// `serial`/`bigserial`/`smallserial` or GENERATED AS IDENTITY: when the
    /// column is omitted (or DEFAULT) on INSERT, it is assigned one past the
    /// column's current maximum.
    pub auto_increment: bool,
    /// DEFAULT or `GENERATED ALWAYS AS (...) STORED`, including its source for
    /// catalog rendering and replay.
    pub default: ColumnDefault,
    /// A `GENERATED ... AS IDENTITY` column (also `auto_increment`): distinguishes
    /// it from a bare `serial` for `pg_attribute.attidentity`.
    pub is_identity: bool,
    /// `GENERATED ALWAYS AS IDENTITY` (reject explicit inserts) vs `BY DEFAULT`.
    pub identity_always: bool,
    /// The auto-increment step: 1 for `serial`, or the identity `INCREMENT BY`.
    pub auto_increment_step: i64,
    /// When the column was declared with a user-defined type, its stable
    /// schema-qualified identity. Runtime enum/domain slots are rebound from
    /// this identity after restart.
    pub user_type: Option<UserTypeName>,
}

/// Durable provenance of a table column's effective `NOT NULL` constraint.
/// Partitions can retain a local constraint while also inheriting one, so a
/// boolean cannot represent parent ALTER and DETACH semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NotNullOrigin {
    Nullable = 0,
    Local = 1,
    Inherited = 2,
    LocalAndInherited = 3,
}

impl NotNullOrigin {
    pub const fn local(required: bool) -> Self {
        if required {
            Self::Local
        } else {
            Self::Nullable
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Nullable,
            1 => Self::Local,
            2 => Self::Inherited,
            3 => Self::LocalAndInherited,
            _ => return None,
        })
    }

    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn is_required(self) -> bool {
        !matches!(self, Self::Nullable)
    }

    pub const fn is_local(self) -> bool {
        matches!(self, Self::Local | Self::LocalAndInherited)
    }

    pub const fn is_inherited(self) -> bool {
        matches!(self, Self::Inherited | Self::LocalAndInherited)
    }

    pub const fn attach_inherited(self) -> Self {
        Self::Inherited
    }

    pub const fn add_inherited(self) -> Self {
        if self.is_local() {
            Self::LocalAndInherited
        } else {
            Self::Inherited
        }
    }

    pub const fn drop_inherited(self) -> Self {
        if self.is_local() {
            Self::Local
        } else {
            Self::Nullable
        }
    }

    pub const fn add_local(self) -> Self {
        if self.is_inherited() {
            Self::LocalAndInherited
        } else {
            Self::Local
        }
    }

    pub const fn drop_local(self) -> Self {
        if self.is_inherited() {
            Self::Inherited
        } else {
            Self::Nullable
        }
    }

    pub const fn localize(self) -> Self {
        Self::local(self.is_required())
    }
}

/// A durable schema-qualified user-type identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserTypeName {
    pub schema: SqlName,
    pub name: SqlName,
}

/// The durable identity of a table column's declared type.
///
/// `ColType` deliberately describes the representation used by the executor;
/// that is not sufficient for a domain, whose values use its base
/// representation while clients must still see the domain OID. Keeping this
/// distinction at one catalog boundary prevents callers from accidentally
/// reporting the storage type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredColumnType {
    Builtin {
        oid: i32,
    },
    UserDefined {
        oid: i32,
        schema: SqlName,
        name: SqlName,
    },
}

/// An OID accepted only by prepared-statement parameter inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParameterTypeOid(i32);

impl ParameterTypeOid {
    pub(crate) const fn raw(self) -> i32 {
        self.0
    }
}

impl DeclaredColumnType {
    pub(crate) const fn parameter_oid(self) -> ParameterTypeOid {
        ParameterTypeOid(self.schema_oid())
    }

    pub(crate) const fn catalog_oid(self) -> i32 {
        self.schema_oid()
    }

    pub(crate) const fn replication_oid(self) -> i32 {
        self.schema_oid()
    }

    const fn schema_oid(self) -> i32 {
        match self {
            Self::Builtin { oid } | Self::UserDefined { oid, .. } => oid,
        }
    }

    pub(crate) const fn replication_user_type(self) -> Option<(SqlName, SqlName)> {
        match self {
            Self::Builtin { .. } => None,
            Self::UserDefined { schema, name, .. } => Some((schema, name)),
        }
    }
}

/// Maximum stored length of a non-constant DEFAULT expression's source text.
/// Ample for real defaults (`nextval('schema.seq')`, `now()`,
/// `gen_random_uuid()`, …); a longer one is a loud error, never silent growth.
pub(crate) const DEFAULT_EXPR_MAX: usize = 128;

impl ColumnMeta {
    pub const EMPTY: Self = ColumnMeta {
        name: SqlName::EMPTY,
        ctype: ColType::Bool,
        type_mod: -1,
        collation: Collation::None,
        not_null: NotNullOrigin::Nullable,
        unique: false,
        primary: false,
        auto_increment: false,
        default: ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        user_type: None,
    };
}

/// Maximum number of multi-column UNIQUE/PRIMARY KEY constraints per table.
pub(crate) const MAX_UNIQUES: usize = 8;
/// Maximum number of CHECK constraints per table.
pub(crate) const MAX_CHECKS: usize = 8;
/// Maximum stored length of a CHECK predicate's source text.
pub(crate) const CHECK_SQL_MAX: usize = 512;
/// Maximum number of FOREIGN KEY constraints per table.
pub(crate) const MAX_FKEYS: usize = 8;
/// Maximum number of exclusion constraints per table.
pub(crate) const MAX_EXCLUSIONS: usize = 8;
/// Exclusion predicates share the bounded table-definition footprint with
/// CHECK expressions. Exhaustion is reported while parsing DDL.
pub(crate) const EXCLUSION_PREDICATE_MAX: usize = 128;

/// A referential action for ON DELETE / ON UPDATE. Mirrors the parser's
/// `FkAction` so the storage/WAL/checkpoint layers do not depend on the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FkAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub fn code(self) -> u8 {
        match self {
            FkAction::NoAction => 0,
            FkAction::Restrict => 1,
            FkAction::Cascade => 2,
            FkAction::SetNull => 3,
            FkAction::SetDefault => 4,
        }
    }

    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => FkAction::NoAction,
            1 => FkAction::Restrict,
            2 => FkAction::Cascade,
            3 => FkAction::SetNull,
            4 => FkAction::SetDefault,
            _ => return None,
        })
    }
}

/// Durable constraint check timing. A deferred initial mode can only exist on
/// a deferrable constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintTiming {
    NotDeferrable,
    DeferrableImmediate,
    DeferrableDeferred,
}

impl ConstraintTiming {
    pub const fn code(self) -> u8 {
        match self {
            Self::NotDeferrable => 0,
            Self::DeferrableImmediate => 1,
            Self::DeferrableDeferred => 2,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::NotDeferrable),
            1 => Some(Self::DeferrableImmediate),
            2 => Some(Self::DeferrableDeferred),
            _ => None,
        }
    }

    pub const fn is_deferrable(self) -> bool {
        !matches!(self, Self::NotDeferrable)
    }

    pub const fn initially_deferred(self) -> bool {
        matches!(self, Self::DeferrableDeferred)
    }
}

/// Durable validation/enforcement state for CHECK and FOREIGN KEY
/// constraints. The enum excludes a validated-but-unenforced state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintValidation {
    EnforcedValidated,
    EnforcedNotValid,
    NotEnforced,
}

impl ConstraintValidation {
    pub const fn code(self) -> u8 {
        match self {
            Self::EnforcedValidated => 0,
            Self::EnforcedNotValid => 1,
            Self::NotEnforced => 2,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::EnforcedValidated),
            1 => Some(Self::EnforcedNotValid),
            2 => Some(Self::NotEnforced),
            _ => None,
        }
    }

    pub const fn enforced(self) -> bool {
        !matches!(self, Self::NotEnforced)
    }

    pub const fn validated(self) -> bool {
        matches!(self, Self::EnforcedValidated)
    }
}

/// A multi-column UNIQUE or PRIMARY KEY constraint. Single-column PK/UNIQUE
/// declared inline on a column stays on that column's flags; this covers the
/// multi-column table-level form.
#[derive(Debug, Clone, Copy)]
pub struct UniqueKey {
    pub name: SqlName,
    pub columns: [u16; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub is_primary: bool,
    pub timing: ConstraintTiming,
}

impl UniqueKey {
    pub const EMPTY: Self = UniqueKey {
        name: SqlName::EMPTY,
        columns: [0u16; MAX_INDEX_COLS],
        n_cols: 0,
        is_primary: false,
        timing: ConstraintTiming::NotDeferrable,
    };

    pub fn columns(&self) -> &[u16] {
        &self.columns[..self.n_cols]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionOperator {
    Equal,
    Overlaps,
    Adjacent,
}

impl ExclusionOperator {
    pub const fn code(self) -> u8 {
        match self {
            Self::Equal => 0,
            Self::Overlaps => 1,
            Self::Adjacent => 2,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Equal),
            1 => Some(Self::Overlaps),
            2 => Some(Self::Adjacent),
            _ => None,
        }
    }

    pub const fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::Overlaps => "&&",
            Self::Adjacent => "-|-",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExclusionConstraint {
    pub name: SqlName,
    pub columns: [u16; MAX_INDEX_COLS],
    pub operators: [ExclusionOperator; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub predicate: Option<StackStr<EXCLUSION_PREDICATE_MAX>>,
    pub timing: ConstraintTiming,
}

impl ExclusionConstraint {
    pub const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        columns: [0; MAX_INDEX_COLS],
        operators: [ExclusionOperator::Equal; MAX_INDEX_COLS],
        n_cols: 0,
        predicate: None,
        timing: ConstraintTiming::NotDeferrable,
    };

    pub fn columns(&self) -> &[u16] {
        &self.columns[..self.n_cols]
    }
}

/// A CHECK constraint: its source predicate text, re-parsed and evaluated per
/// candidate row at INSERT/UPDATE time.
#[derive(Debug, Clone, Copy)]
pub struct CheckConstraint {
    pub name: SqlName,
    pub expression: StackStr<CHECK_SQL_MAX>,
    pub validation: ConstraintValidation,
}

impl CheckConstraint {
    pub const EMPTY: Self = CheckConstraint {
        name: SqlName::EMPTY,
        expression: StackStr::new(),
        validation: ConstraintValidation::EnforcedValidated,
    };
}

/// A FOREIGN KEY constraint on a child table's column tuple referencing a
/// parent table's column tuple.
#[derive(Debug, Clone, Copy)]
pub struct ForeignKey {
    pub name: SqlName,
    pub columns: [u16; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub parent_schema: SqlName,
    pub parent: SqlName,
    pub parent_cols: [u16; MAX_INDEX_COLS],
    pub n_parent_cols: usize,
    pub on_delete: FkAction,
    pub on_update: FkAction,
    pub timing: ConstraintTiming,
    pub validation: ConstraintValidation,
}

impl ForeignKey {
    pub const EMPTY: Self = ForeignKey {
        name: SqlName::EMPTY,
        parent_schema: SqlName::EMPTY,
        columns: [0u16; MAX_INDEX_COLS],
        n_cols: 0,
        parent: SqlName::EMPTY,
        parent_cols: [0u16; MAX_INDEX_COLS],
        n_parent_cols: 0,
        on_delete: FkAction::NoAction,
        on_update: FkAction::NoAction,
        timing: ConstraintTiming::NotDeferrable,
        validation: ConstraintValidation::EnforcedValidated,
    };

    pub fn columns(&self) -> &[u16] {
        &self.columns[..self.n_cols]
    }

    pub fn parent_cols(&self) -> &[u16] {
        &self.parent_cols[..self.n_parent_cols]
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TableDef {
    /// The schema the table lives in ("public" unless created qualified or
    /// under a search_path naming another schema).
    pub schema: SqlName,
    pub name: SqlName,
    pub columns: [ColumnMeta; MAX_COLUMNS],
    pub n_columns: usize,
    pub uniques: [UniqueKey; MAX_UNIQUES],
    pub n_uniques: usize,
    pub checks: [CheckConstraint; MAX_CHECKS],
    pub n_checks: usize,
    pub fkeys: [ForeignKey; MAX_FKEYS],
    pub n_fkeys: usize,
    pub exclusions: [ExclusionConstraint; MAX_EXCLUSIONS],
    pub n_exclusions: usize,
    pub row_level_security: RowLevelSecurityState,
    pub partition: PartitionDef,
}

/// The two independent pg_class row-security flags. A policy may exist while
/// enforcement is disabled, and FORCE affects only the ordinary owner bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLevelSecurityState {
    pub enabled: bool,
    pub forced: bool,
}

impl RowLevelSecurityState {
    pub const DISABLED: Self = Self {
        enabled: false,
        forced: false,
    };
}

/// A table's independent partitioning and attachment roles. Parent links are
/// storage slots, so renames cannot stale routing metadata. Keeping the roles
/// orthogonal permits a partition to be partitioned without an ambiguous enum
/// state.
#[derive(Debug, Clone, Copy)]
pub struct PartitionDef {
    pub scheme: Option<PartitionScheme>,
    pub attachment: Option<PartitionAttachment>,
}

#[derive(Debug, Clone, Copy)]
pub struct PartitionScheme {
    pub strategy: PartitionStrategy,
    pub keys: [u16; MAX_PARTITION_KEYS],
    pub n_keys: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct PartitionAttachment {
    pub parent: u16,
    pub bound: PartitionBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

#[derive(Debug, Clone, Copy)]
pub enum PartitionBoundValue {
    MinValue,
    Value(OwnedDatum),
    MaxValue,
}

#[derive(Debug, Clone, Copy)]
pub enum PartitionBound {
    Default,
    Range {
        lower: [PartitionBoundValue; MAX_PARTITION_KEYS],
        upper: [PartitionBoundValue; MAX_PARTITION_KEYS],
        n_keys: u8,
    },
    /// List bounds have one key; tuple-list grammar is rejected at DDL time
    /// rather than stored ambiguously.
    List {
        values: [OwnedDatum; MAX_PARTITION_LIST_VALUES],
        n_values: u8,
    },
    Hash {
        modulus: u32,
        remainder: u32,
    },
}

impl PartitionDef {
    pub const NONE: Self = Self {
        scheme: None,
        attachment: None,
    };

    pub const fn parent(
        strategy: PartitionStrategy,
        keys: [u16; MAX_PARTITION_KEYS],
        n_keys: u8,
    ) -> Self {
        Self {
            scheme: Some(PartitionScheme {
                strategy,
                keys,
                n_keys,
            }),
            attachment: None,
        }
    }

    pub const fn child(parent: u16, bound: PartitionBound) -> Self {
        Self {
            scheme: None,
            attachment: Some(PartitionAttachment { parent, bound }),
        }
    }

    pub const fn is_partitioned(self) -> bool {
        self.scheme.is_some()
    }

    pub const fn is_attached(self) -> bool {
        self.attachment.is_some()
    }
}

/// Inline catalog bounds have an explicit, startup-bounded capacity.
pub(crate) const MAX_PARTITION_KEYS: usize = 4;
pub(crate) const MAX_PARTITION_LIST_VALUES: usize = 8;

impl TableDef {
    /// A table with a name and no columns or constraints, for spread-init of
    /// the constraint arrays at construction sites.
    pub const fn empty() -> Self {
        TableDef {
            schema: SqlName::EMPTY,
            name: SqlName::EMPTY,
            columns: [ColumnMeta::EMPTY; MAX_COLUMNS],
            n_columns: 0,
            uniques: [UniqueKey::EMPTY; MAX_UNIQUES],
            n_uniques: 0,
            checks: [CheckConstraint::EMPTY; MAX_CHECKS],
            n_checks: 0,
            fkeys: [ForeignKey::EMPTY; MAX_FKEYS],
            n_fkeys: 0,
            exclusions: [ExclusionConstraint::EMPTY; MAX_EXCLUSIONS],
            n_exclusions: 0,
            row_level_security: RowLevelSecurityState::DISABLED,
            partition: PartitionDef::NONE,
        }
    }

    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns[..self.n_columns]
    }

    pub fn uniques(&self) -> &[UniqueKey] {
        &self.uniques[..self.n_uniques]
    }

    pub fn checks(&self) -> &[CheckConstraint] {
        &self.checks[..self.n_checks]
    }

    pub fn fkeys(&self) -> &[ForeignKey] {
        &self.fkeys[..self.n_fkeys]
    }

    pub fn exclusions(&self) -> &[ExclusionConstraint] {
        &self.exclusions[..self.n_exclusions]
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns().iter().position(|c| c.name.as_str() == name)
    }

    /// Column types into a caller-provided array, for the row codec.
    pub fn schema(&self, out: &mut [ColType; MAX_COLUMNS]) -> usize {
        for (i, c) in self.columns().iter().enumerate() {
            out[i] = c.ctype;
        }
        self.n_columns
    }
}

pub(crate) fn partition_bound_matches(
    strategy: PartitionStrategy,
    keys: [u16; MAX_PARTITION_KEYS],
    n_keys: u8,
    bound: PartitionBound,
    values: &[crate::sql::types::Datum],
) -> Result<bool, SqlError> {
    use crate::sql::eval::compare_datums;
    use core::cmp::Ordering;
    let key = |i: usize| {
        values.get(usize::from(keys[i])).copied().ok_or_else(|| {
            sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "partition key is outside row width"
            )
        })
    };
    match (strategy, bound) {
        (PartitionStrategy::Range, PartitionBound::Range { lower, upper, .. }) => {
            let compare = |tuple: &[PartitionBoundValue; MAX_PARTITION_KEYS]| {
                for (index, bound) in tuple.iter().copied().enumerate().take(usize::from(n_keys)) {
                    let value = key(index)?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    let ordering = match bound {
                        PartitionBoundValue::MinValue => Ordering::Greater,
                        PartitionBoundValue::MaxValue => Ordering::Less,
                        PartitionBoundValue::Value(bound) => {
                            compare_datums(&value, &bound.as_datum())?
                        }
                    };
                    if !ordering.is_eq() {
                        return Ok(Some(ordering));
                    }
                }
                Ok(Some(Ordering::Equal))
            };
            let Some(lower_ordering) = compare(&lower)? else {
                return Ok(false);
            };
            let Some(upper_ordering) = compare(&upper)? else {
                return Ok(false);
            };
            Ok(lower_ordering != Ordering::Less && upper_ordering == Ordering::Less)
        }
        (
            PartitionStrategy::List,
            PartitionBound::List {
                values: list,
                n_values,
            },
        ) => {
            let value = key(0)?;
            Ok((0..usize::from(n_values))
                .any(|i| compare_datums(&value, &list[i].as_datum()).is_ok_and(Ordering::is_eq)))
        }
        (PartitionStrategy::Hash, PartitionBound::Hash { modulus, remainder }) => {
            let mut hash = 0u64;
            for index in 0..usize::from(n_keys) {
                let value = key(index)?;
                let part = match value {
                    crate::sql::types::Datum::Null => 0,
                    crate::sql::types::Datum::Int2(value) => {
                        postgres_hash_uint32_extended(value as i32 as u32)
                    }
                    crate::sql::types::Datum::Int4(value) => {
                        postgres_hash_uint32_extended(value as u32)
                    }
                    crate::sql::types::Datum::Int8(value) => {
                        let low = value as u32;
                        let high = (value >> 32) as u32;
                        postgres_hash_uint32_extended(low ^ if value >= 0 { high } else { !high })
                    }
                    _ => {
                        return Err(sql_err!(
                            crate::sql::eval::sqlstate::INTERNAL_ERROR,
                            "non-integer HASH partition key reached routing"
                        ));
                    }
                };
                hash ^= part
                    .wrapping_add(0x49a0_f4dd_15e5_a8e3)
                    .wrapping_add(hash << 54)
                    .wrapping_add(hash >> 7);
            }
            Ok(hash % u64::from(modulus) == u64::from(remainder))
        }
        _ => Ok(false),
    }
}

fn postgres_hash_uint32_extended(value: u32) -> u64 {
    fn mix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(4);
        b = b.wrapping_add(a);
        (a, b, c)
    }
    let initial = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let seed = 0x7a5b_2236_7996_dcfd_u64;
    let (mut a, mut b, mut c) = mix(
        initial.wrapping_add((seed >> 32) as u32),
        initial.wrapping_add(seed as u32),
        initial,
    );
    a = a.wrapping_add(value);
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    (u64::from(b) << 32) | u64::from(c)
}

/// Where a row's bytes live in the heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLoc {
    pub offset: u32,
    pub len: u32,
}

/// A row's visibility state: the committed image plus a bounded chain of
/// uncommitted command versions owned by one transaction. Keeping each
/// command's image is what lets a statement-level snapshot look past a later
/// write to the image produced by an earlier command in the same transaction.
/// A second transaction still fails fast instead of blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowState {
    pub committed: Option<RowHome>,
    /// Commit LSN of `committed`, or of the deletion when `committed` is
    /// absent but the entry shadows an older SST row. Zero means no committed
    /// image has been installed.
    pub committed_lsn: u64,
    /// Older committed images retained while a repeatable-read snapshot can
    /// still see them. Their bytes remain in the heap or in immutable SST
    /// generations; WAL/SST objects, not this metadata, are the durable copy.
    pub history: CommittedHistory,
    pub pending: PendingVersions,
}

/// Where a committed row's bytes live: the RAM heap, or spilled to the
/// table's checkpoint SST in the block store (fetched back through the cache
/// tiers on read). The resident row map is a bounded overlay: cold committed
/// rows remain indexed by immutable SST objects and are synthesized into the
/// overlay on demand, so RAM and local disk are caches rather than authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowHome {
    Heap(RowLoc),
    Spilled {
        len: u32,
        sst: u8,
        /// Exact immutable version to fetch.
        commit_lsn: u64,
    },
}

impl RowHome {
    pub fn heap_loc(self) -> Option<RowLoc> {
        match self {
            RowHome::Heap(loc) => Some(loc),
            RowHome::Spilled { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedVersion {
    pub home: Option<RowHome>,
    pub lsn: u64,
}

/// The per-row committed history needed by active snapshots. Static memory
/// discipline makes the bound explicit; exhaustion is rejected before WAL
/// durability rather than losing a version after commit.
pub(crate) const MAX_COMMITTED_ROW_VERSIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedHistory {
    entries: [CommittedVersion; MAX_COMMITTED_ROW_VERSIONS],
    len: u8,
}

impl CommittedHistory {
    pub const fn empty() -> Self {
        Self {
            entries: [CommittedVersion { home: None, lsn: 0 }; MAX_COMMITTED_ROW_VERSIONS],
            len: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn get(&self, index: usize) -> Option<CommittedVersion> {
        (index < self.len()).then_some(self.entries[index])
    }

    fn push_newest(&mut self, version: CommittedVersion) -> Result<(), SqlError> {
        if self.len() == MAX_COMMITTED_ROW_VERSIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "active snapshots retain more than {} committed versions of one row",
                MAX_COMMITTED_ROW_VERSIONS
            ));
        }
        let len = self.len();
        self.entries.copy_within(0..len, 1);
        self.entries[0] = version;
        self.len += 1;
        Ok(())
    }

    /// Keeps every version newer than the oldest snapshot and the first
    /// version at or before it. That is the minimal chain that can answer all
    /// active snapshots.
    fn prune(&mut self, oldest_snapshot: Option<u64>) {
        let Some(oldest) = oldest_snapshot else {
            self.len = 0;
            return;
        };
        let mut keep = self.len();
        for (index, version) in self.entries[..self.len()].iter().enumerate() {
            if version.lsn <= oldest {
                keep = index + 1;
                break;
            }
        }
        self.len = keep as u8;
    }

    fn visible_at(&self, commit_snapshot: u64) -> Option<Option<RowHome>> {
        self.entries[..self.len()]
            .iter()
            .find(|version| version.lsn <= commit_snapshot)
            .map(|version| version.home)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingChange {
    pub txid: u32,
    /// The command (statement) within the transaction that made this change —
    /// PostgreSQL's command-id. A reader with an earlier command's snapshot does
    /// not see it, which is what lets a data-modifying CTE's changes stay
    /// invisible to the same statement's main query (they share one snapshot).
    pub cid: u32,
    /// `None` = pending delete.
    pub loc: Option<RowLoc>,
}

/// The most distinct command versions one transaction may retain for one row.
/// Multiple writes by the same command replace its last image, so this bounds
/// cross-command history rather than expression-level work. Exhaustion is a
/// loud static-capacity error.
pub(crate) const MAX_PENDING_ROW_VERSIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingVersions {
    entries: [PendingChange; MAX_PENDING_ROW_VERSIONS],
    len: u8,
}

impl PendingVersions {
    pub const fn empty() -> Self {
        Self {
            entries: [PendingChange {
                txid: 0,
                cid: 0,
                loc: None,
            }; MAX_PENDING_ROW_VERSIONS],
            len: 0,
        }
    }

    pub fn is_none(&self) -> bool {
        self.len == 0
    }

    pub fn is_some(&self) -> bool {
        self.len != 0
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn last(&self) -> Option<PendingChange> {
        self.len
            .checked_sub(1)
            .map(|index| self.entries[index as usize])
    }

    fn last_mut(&mut self) -> Option<&mut PendingChange> {
        let index = self.len.checked_sub(1)? as usize;
        Some(&mut self.entries[index])
    }

    fn push(&mut self, change: PendingChange) -> Result<(), SqlError> {
        if self.len() == MAX_PENDING_ROW_VERSIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one transaction creates more than {} command versions of one row",
                MAX_PENDING_ROW_VERSIONS
            ));
        }
        self.entries[self.len()] = change;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<PendingChange> {
        let index = self.len.checked_sub(1)? as usize;
        self.len -= 1;
        Some(self.entries[index])
    }

    fn get(&self, index: usize) -> Option<PendingChange> {
        (index < self.len()).then_some(self.entries[index])
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut PendingChange> {
        (index < self.len()).then_some(&mut self.entries[index])
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn visible_at(&self, txid: u32, snapshot: u32) -> Option<Option<RowLoc>> {
        self.entries[..self.len()]
            .iter()
            .rev()
            .find(|change| change.txid == txid && change.cid < snapshot)
            .map(|change| change.loc)
    }
}

/// The command-id a read that should see *all* of its own transaction's
/// uncommitted changes uses (the ordinary case: every write so far is visible).
pub(crate) const SNAPSHOT_ALL: u32 = u32::MAX;

impl RowState {
    pub fn committed_only(loc: RowLoc) -> Self {
        Self::committed_only_at(loc, 0)
    }

    pub fn committed_only_at(loc: RowLoc, commit_lsn: u64) -> Self {
        Self {
            committed: Some(RowHome::Heap(loc)),
            committed_lsn: commit_lsn,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        }
    }

    /// What transaction `txid` sees with all its own changes visible (the
    /// ordinary snapshot). `None` = row invisible.
    pub fn visible_to(&self, txid: u32) -> Option<RowHome> {
        self.visible_at(txid, SNAPSHOT_ALL)
    }

    /// What `txid` sees under a command snapshot: its own pending change is
    /// visible only if that change was made by a command *earlier* than
    /// `snapshot` (`cid < snapshot`); a later/same-command change is not, so the
    /// committed image shows through. `snapshot == SNAPSHOT_ALL` sees everything.
    pub fn visible_at(&self, txid: u32, snapshot: u32) -> Option<RowHome> {
        self.visible_at_lsn(txid, snapshot, u64::MAX)
    }

    /// Resident visibility under both the transaction's command snapshot and
    /// a durable commit-LSN snapshot. `Storage::visible_row_home_at` owns the
    /// separate object-resident tier at the engine-wide visibility boundary.
    pub fn visible_at_lsn(
        &self,
        txid: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Option<RowHome> {
        match self.pending.visible_at(txid, command_snapshot) {
            Some(loc) => loc.map(RowHome::Heap),
            None if self.committed_lsn <= commit_snapshot => self.committed,
            None => self.history.visible_at(commit_snapshot).flatten(),
        }
    }

    /// Whether another transaction has an uncommitted change here.
    pub fn locked_by_other(&self, txid: u32) -> Option<u32> {
        match self.pending.last() {
            Some(p) if p.txid != txid => Some(p.txid),
            _ => None,
        }
    }
}

/// Fixed byte heap for encoded rows.
pub struct RowHeap {
    buffer: Box<[u8]>,
    used: usize,
}

impl RowHeap {
    fn new(budget: &mut Budget, bytes: usize) -> Result<Self, BudgetError> {
        budget.draw(bytes, "memtable")?;
        Ok(Self {
            buffer: vec![0; bytes].into_boxed_slice(),
            used: 0,
        })
    }

    pub fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        if self.buffer.len() - self.used < len {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "memtable is full ({} bytes); with object storage on, rows spill at the next checkpoint — retry, raise memtable_bytes, or enable object storage",
                self.buffer.len()
            ));
        }
        let loc = RowLoc {
            offset: self.used as u32,
            len: len as u32,
        };
        let slice = &mut self.buffer[self.used..self.used + len];
        self.used += len;
        Ok((loc, slice))
    }

    pub fn get(&self, loc: RowLoc) -> &[u8] {
        &self.buffer[loc.offset as usize..(loc.offset + loc.len) as usize]
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }
}

pub struct Table {
    pub def: TableDef,
    pub ownership: Ownership,
    /// Transaction-owned table-definition versions. Other transactions keep
    /// resolving and decoding against `def`; the owner resolves against the
    /// latest pending definition. RowState's command versions carry the
    /// matching transformed row encodings.
    pending_def_slots: [u32; MAX_PENDING_TABLE_DEFS],
    n_pending_defs: u8,
    pending_def_txid: Option<u32>,
    /// Monotonic creation stamp (catalog sequence), giving dependency
    /// reports PostgreSQL's OID ordering.
    pub created_at: u64,
    pub rows: FixedMap<u64, RowState>,
    /// Committed existence: whether the table is part of the last-committed
    /// catalog image. `pending_ddl` overlays an uncommitted CREATE/DROP.
    pub live: bool,
    /// An uncommitted CREATE or DROP owned by a single transaction. Mirrors
    /// `RowState`: other transactions see `live`; the owner sees the pending
    /// existence. `None` once committed or rolled back.
    pub pending_ddl: Option<PendingDdl>,
    /// Changed since the last checkpoint (drives delta checkpoints).
    pub dirty: bool,
    /// Bumped on every committed change ([`Table::mark_dirty`], the one
    /// place `dirty` may be set). The sliced checkpoint compares it against
    /// the generation it captured when it wrote the table's SSTs, so a
    /// table that changed after its slice is re-sliced before the manifest
    /// publishes — the bug class this kills is a snapshot quietly missing
    /// writes that landed between beats.
    pub generation: u64,
    /// Planner statistics produced by ANALYZE. They are derived metadata:
    /// query correctness never depends on them, and the authoritative rows
    /// remain in immutable object-store generations.
    pub(crate) statistics: TableStatistics,
    pub(crate) statistics_dirty: bool,
    /// The in-place relation-statistics half of ANALYZE has not yet been
    /// included in a durable WAL commit. Unlike pg_statistic column rows,
    /// PostgreSQL's pg_class reltuples/relpages update survives rollback.
    statistics_wal_dirty: bool,
    /// Transaction-private pg_statistic versions. The large images live in a
    /// startup-sized storage slab; the table retains only slot handles so SQL
    /// transaction/savepoint undo records stay compact.
    pending_statistics_slots: [u32; MAX_PENDING_TABLE_DEFS],
    n_pending_statistics: u8,
    pending_statistics_txid: Option<u32>,
    /// Per-column sequence state for serial/identity columns: the last value
    /// a *default* assignment handed out. PostgreSQL's sequence, not a max
    /// scan — explicit inserts do not advance it, deletes and TRUNCATE
    /// (without RESTART IDENTITY) do not rewind it, and a rolled-back insert
    /// still consumes its number.
    pub serial_last: [i64; MAX_COLUMNS],
    /// Whether `serial_last` changed since it was last written to the WAL.
    pub serial_dirty: bool,
    /// The SSTs holding this table's spilled rows, in flush order: a full
    /// checkpoint writes one, each delta checkpoint appends one, and a merge
    /// (list full) collapses back to one. A row's map entry names which list
    /// slot its bytes live in.
    pub(crate) spill_ssts: [Option<crate::store::SstHandle>; MAX_SPILL_SSTS],
    pub(crate) n_spill_ssts: usize,
    /// Rowids removed since the last checkpoint while this table had spilled
    /// SSTs — each becomes a tombstone entry in the next delta, so a cold
    /// start does not resurrect an older SST's version. Overflow forces the
    /// next checkpoint to a full rewrite instead of a delta (never dropping a
    /// tombstone).
    pub(crate) tombstones: [u64; MAX_TOMBSTONES],
    pub(crate) n_tombstones: usize,
    pub(crate) tombstones_overflow: bool,
    /// Value caches accelerating this table's uniqueness and equality probes,
    /// one per distinct constrained or named-index tuple, rebuilt whenever the
    /// definition/index set changes and maintained per committed row otherwise.
    pub(crate) enforcers: [Option<Enforcer>; MAX_VALUE_ENFORCERS],
    pub(crate) n_enforcers: usize,
}

/// Statistics for one stored column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnStatistics {
    /// Whether this column was included in the latest applicable ANALYZE.
    /// Table-level statistics can be valid while a targeted ANALYZE leaves
    /// other columns without a pg_statistic row.
    pub(crate) valid: bool,
    /// Fraction of rows that are NULL, in millionths.
    pub(crate) null_fraction_ppm: u32,
    /// HyperLogLog estimate over non-NULL values.
    pub(crate) distinct_values: u64,
    /// PostgreSQL's negative `n_distinct` ratio, in millionths, when the
    /// estimate exceeded its ratio threshold at collection time. Keeping the
    /// sampled ratio prevents targeted ANALYZE from recomputing nonsense
    /// against a newer table row estimate.
    pub(crate) distinct_fraction_ppm: u32,
    /// Average encoded bytes for a non-NULL value.
    pub(crate) average_width: u32,
}

impl ColumnStatistics {
    pub(crate) const EMPTY: Self = Self {
        valid: false,
        null_fraction_ppm: 0,
        distinct_values: 0,
        distinct_fraction_ppm: 0,
        average_width: 0,
    };
}

/// Table cardinality and width statistics used by the storage-aware planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableStatistics {
    pub(crate) valid: bool,
    pub(crate) rows: u64,
    pub(crate) average_row_width: u32,
    pub(crate) analyzed_generation: u64,
    pub(crate) columns: [ColumnStatistics; MAX_COLUMNS],
}

#[derive(Debug, Clone, Copy)]
struct PendingTableStatisticsSlot {
    used: bool,
    statistics: TableStatistics,
}

impl TableStatistics {
    pub(crate) const EMPTY: Self = Self {
        valid: false,
        rows: 0,
        average_row_width: 0,
        analyzed_generation: 0,
        columns: [ColumnStatistics::EMPTY; MAX_COLUMNS],
    };
}

/// The most delta SSTs a table accumulates before a checkpoint merges them
/// back into one — the write-amplification / read-fan-out tradeoff.
pub(crate) const MAX_SPILL_SSTS: usize = 8;

/// Deletes remembered between checkpoints; past this the next checkpoint
/// rewrites the table fully rather than lose one.
pub(crate) const MAX_TOMBSTONES: usize = 1024;

/// The most value indexes one table can carry: one per distinct indexed
/// column tuple, whether introduced by a constraint or a named index.
/// Exceeding it at DDL is a loud error.
pub(crate) const MAX_VALUE_ENFORCERS: usize = 16;

/// Extended-statistics objects and their computed values are startup-bounded.
/// PostgreSQL accepts more objects/keys/MCV entries; crossing one of these
/// envelopes is an explicit capacity error rather than an incomplete object.
pub(crate) const MAX_EXTENDED_STATISTICS_PER_TABLE: usize = 8;
pub(crate) const MAX_EXTENDED_STATISTICS_KEYS: usize = 8;
pub(crate) const MAX_EXTENDED_STATISTICS_MCV: usize = 100;
pub(crate) const EXTENDED_STATISTICS_EXPRESSION_MAX: usize = CHECK_SQL_MAX;
pub(crate) const EXTENDED_STATISTICS_MCV_TEXT_MAX: usize = 128;

pub(crate) fn extended_statistics_expression(
    source: &str,
) -> Result<StackStr<EXTENDED_STATISTICS_EXPRESSION_MAX>, SqlError> {
    let expression = StackStr::from_str(source);
    if expression.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "statistics expression exceeds {} bytes",
            EXTENDED_STATISTICS_EXPRESSION_MAX
        ));
    }
    Ok(expression)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Expressions stay inline because runtime catalog storage cannot allocate.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ExtendedStatisticsKey {
    Column(SqlName),
    Expression(StackStr<EXTENDED_STATISTICS_EXPRESSION_MAX>),
}

impl ExtendedStatisticsKey {
    const EMPTY: Self = Self::Column(SqlName::EMPTY);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtendedStatisticsMcv {
    pub(crate) valid: bool,
    pub(crate) hash: u64,
    pub(crate) count: u64,
    pub(crate) values: StackStr<EXTENDED_STATISTICS_MCV_TEXT_MAX>,
}

impl ExtendedStatisticsMcv {
    pub(crate) const EMPTY: Self = Self {
        valid: false,
        hash: 0,
        count: 0,
        values: StackStr::new(),
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtendedStatisticsData {
    pub(crate) valid: bool,
    pub(crate) inherited: bool,
    pub(crate) analyzed_generation: u64,
    pub(crate) rows: u64,
    pub(crate) non_null_rows: u64,
    pub(crate) distinct_values: u64,
    /// Pairwise functional-dependency strengths in millionths. Entry i,j is
    /// the degree to which key i determines key j; diagonal entries are zero.
    pub(crate) dependencies_ppm: [u32; MAX_EXTENDED_STATISTICS_KEYS * MAX_EXTENDED_STATISTICS_KEYS],
    pub(crate) expression_statistics: [ColumnStatistics; MAX_EXTENDED_STATISTICS_KEYS],
    pub(crate) mcv: [ExtendedStatisticsMcv; MAX_EXTENDED_STATISTICS_MCV],
    pub(crate) n_mcv: u16,
}

impl ExtendedStatisticsData {
    pub(crate) const EMPTY: Self = Self {
        valid: false,
        inherited: false,
        analyzed_generation: 0,
        rows: 0,
        non_null_rows: 0,
        distinct_values: 0,
        dependencies_ppm: [0; MAX_EXTENDED_STATISTICS_KEYS * MAX_EXTENDED_STATISTICS_KEYS],
        expression_statistics: [ColumnStatistics::EMPTY; MAX_EXTENDED_STATISTICS_KEYS],
        mcv: [ExtendedStatisticsMcv::EMPTY; MAX_EXTENDED_STATISTICS_MCV],
        n_mcv: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtendedStatisticsMutableDefinition {
    pub(crate) schema: SqlName,
    pub(crate) name: SqlName,
    pub(crate) target: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtendedStatisticsDefinition {
    pub(crate) txid: u32,
    pub(crate) definition: ExtendedStatisticsMutableDefinition,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtendedStatisticsKeys {
    pub(crate) txid: u32,
    pub(crate) keys: [ExtendedStatisticsKey; MAX_EXTENDED_STATISTICS_KEYS],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtendedStatisticsDataSlot {
    used: bool,
    data: ExtendedStatisticsData,
}

#[derive(Clone, Copy)]
pub(crate) struct ExtendedStatisticsDef {
    pub(crate) created_at: u64,
    pub(crate) table: u16,
    pub(crate) mutable: ExtendedStatisticsMutableDefinition,
    pub(crate) pending_definition: Option<PendingExtendedStatisticsDefinition>,
    pub(crate) pending_keys: Option<PendingExtendedStatisticsKeys>,
    pub(crate) ownership: Ownership,
    pub(crate) keys: [ExtendedStatisticsKey; MAX_EXTENDED_STATISTICS_KEYS],
    pub(crate) n_keys: u8,
    pub(crate) kinds: crate::sql::ast::StatisticsKinds,
    pub(crate) expression_only: bool,
    pub(crate) data: ExtendedStatisticsData,
    pending_data_slots: [u32; MAX_PENDING_TABLE_DEFS],
    n_pending_data: u8,
    pending_data_txid: Option<u32>,
    pub(crate) ddl_state: CatalogDdlState,
}

#[derive(Clone, Copy)]
pub(crate) struct ExtendedStatisticsSpec {
    pub(crate) created_at: u64,
    pub(crate) schema: SqlName,
    pub(crate) name: SqlName,
    pub(crate) table: u16,
    pub(crate) target: Option<u16>,
    pub(crate) keys: [ExtendedStatisticsKey; MAX_EXTENDED_STATISTICS_KEYS],
    pub(crate) n_keys: u8,
    pub(crate) kinds: crate::sql::ast::StatisticsKinds,
    pub(crate) expression_only: bool,
}

impl ExtendedStatisticsDef {
    const EMPTY: Self = Self {
        created_at: 0,
        table: u16::MAX,
        mutable: ExtendedStatisticsMutableDefinition {
            schema: SqlName::EMPTY,
            name: SqlName::EMPTY,
            target: None,
        },
        pending_definition: None,
        pending_keys: None,
        ownership: Ownership::BOOTSTRAP,
        keys: [ExtendedStatisticsKey::EMPTY; MAX_EXTENDED_STATISTICS_KEYS],
        n_keys: 0,
        kinds: crate::sql::ast::StatisticsKinds::EXPRESSION,
        expression_only: false,
        data: ExtendedStatisticsData::EMPTY,
        pending_data_slots: [u32::MAX; MAX_PENDING_TABLE_DEFS],
        n_pending_data: 0,
        pending_data_txid: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> ExtendedStatisticsMutableDefinition {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(self.mutable, |pending| pending.definition)
    }

    pub(crate) fn keys_for(&self, txid: u32) -> &[ExtendedStatisticsKey] {
        let keys = match &self.pending_keys {
            Some(pending) if pending.txid == txid => &pending.keys,
            _ => &self.keys,
        };
        &keys[..usize::from(self.n_keys)]
    }
}

/// A table's binding of one indexed tuple to its value cache: the key columns
/// it covers and the pool slot holding the `value_hash → rowid` map. Constraint
/// enforcement and eligible query scans share this lookup.
#[derive(Clone, Copy)]
pub(crate) struct Enforcer {
    slot: u32,
    columns: [u16; MAX_INDEX_COLS],
    n_cols: usize,
    durable: Option<crate::store::ValueIndexHandle>,
}

impl Enforcer {
    fn columns(&self) -> &[u16] {
        &self.columns[..self.n_cols]
    }
}

/// An uncommitted catalog change to one table, owned by one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingDdl {
    pub txid: u32,
    /// `true` = pending CREATE (committed baseline: absent), `false` = pending
    /// DROP (committed baseline: present).
    pub creating: bool,
}

/// A catalog object's committed existence and transaction-local DDL. The
/// variants include a create followed by drop, which savepoint rollback must
/// restore as a pending create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogDdlState {
    Absent,
    Present,
    PendingCreate { txid: u32 },
    PendingDrop { txid: u32 },
    PendingCreateDrop { txid: u32 },
}

impl CatalogDdlState {
    pub const fn visible_to(self, txid: u32) -> bool {
        match self {
            Self::Absent => false,
            Self::Present => true,
            Self::PendingCreate { txid: owner } => owner == txid,
            Self::PendingDrop { txid: owner } => owner != txid,
            Self::PendingCreateDrop { .. } => false,
        }
    }

    pub const fn pending_txid(self) -> Option<u32> {
        match self {
            Self::PendingCreate { txid }
            | Self::PendingDrop { txid }
            | Self::PendingCreateDrop { txid } => Some(txid),
            Self::Absent | Self::Present => None,
        }
    }

    fn drop_by(self, txid: u32) -> Self {
        match self {
            Self::PendingCreate { txid: owner } if owner == txid => {
                Self::PendingCreateDrop { txid }
            }
            Self::Present => Self::PendingDrop { txid },
            _ => panic!("catalog DDL drop does not match object state"),
        }
    }

    fn commit_create(self) -> Self {
        match self {
            Self::PendingCreate { .. } => Self::Present,
            Self::PendingCreateDrop { txid } => Self::PendingDrop { txid },
            _ => panic!("catalog CREATE commit does not match object state"),
        }
    }

    fn commit_drop(self) -> Self {
        assert!(matches!(self, Self::PendingDrop { .. }));
        Self::Absent
    }

    fn rollback_create(self) -> Self {
        assert!(matches!(self, Self::PendingCreate { .. }));
        Self::Absent
    }

    fn rollback_drop(self, txid: u32) -> Self {
        match self {
            Self::PendingDrop { txid: owner } if owner == txid => Self::Present,
            Self::PendingCreateDrop { txid: owner } if owner == txid => {
                Self::PendingCreate { txid }
            }
            _ => panic!("catalog DROP rollback does not match object state"),
        }
    }
}

/// The maximum number of ALTER TABLE commands one transaction may apply to a
/// single table. This is a static-memory bound, not an accept-and-ignore limit.
pub(crate) const MAX_PENDING_TABLE_DEFS: usize = 8;
pub(crate) const MAX_PENDING_STATISTICS_PER_TXN: usize = 64;

fn pending_extended_statistics_capacity(config: &Config) -> usize {
    let object_bound = config
        .max_tables
        .saturating_mul(MAX_EXTENDED_STATISTICS_PER_TABLE)
        .saturating_mul(MAX_PENDING_TABLE_DEFS);
    let transaction_bound =
        (config.max_connections as usize).saturating_mul(MAX_PENDING_STATISTICS_PER_TXN);
    object_bound.min(transaction_bound)
}

#[derive(Debug, Clone, Copy)]
struct PendingTableDef {
    pub txid: u32,
    pub def: TableDef,
    /// Committed-definition column index → latest column name. `None` means
    /// the committed column was dropped. This composes across ALTER commands
    /// and lets sequence ownership rebind once, atomically, at commit.
    pub column_mapping: [Option<SqlName>; MAX_COLUMNS],
    /// Every visible row was re-homed into the heap under this definition, so
    /// the committed spill list becomes obsolete when this version commits.
    pub rewrites_rows: bool,
}

#[derive(Debug, Clone, Copy)]
struct PendingTableDefSlot {
    used: bool,
    version: PendingTableDef,
}

impl Table {
    /// The one place `dirty` is set: every committed change advances the
    /// generation with it, so the sliced checkpoint can tell "dirty since
    /// the last publish" from "dirty since my slice of this sweep".
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.generation += 1;
    }

    /// Whether `txid` sees this table exist: its own pending CREATE/DROP,
    /// else the committed `live` baseline (another transaction's uncommitted
    /// DDL is invisible).
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending_ddl {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }

    /// The txid of an uncommitted CREATE/DROP held by another transaction, if
    /// any — or of a pending definition version. That transaction has the
    /// catalog identity locked.
    pub fn ddl_locked_by_other(&self, txid: u32) -> Option<u32> {
        match self.pending_ddl {
            Some(p) if p.txid != txid => Some(p.txid),
            _ => self.pending_def_txid.filter(|&owner| owner != txid),
        }
    }

    /// Whether the slot is free for a fresh CREATE: no committed table, no
    /// pending DDL, and no retained rows.
    fn is_free(&self) -> bool {
        !self.live && self.pending_ddl.is_none() && self.n_pending_defs == 0 && self.rows.is_empty()
    }
}

/// Maximum length of a stored view definition (the SELECT text).
pub(crate) const VIEW_SQL_MAX: usize = 2048;

pub(crate) const MAX_STORED_QUERY_DEPENDENCIES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DependencyClass {
    Table = 1,
    View = 2,
    Domain = 3,
    Enum = 4,
    Sequence = 5,
    Composite = 6,
    Routine = 7,
}

impl DependencyClass {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Table),
            2 => Some(Self::View),
            3 => Some(Self::Domain),
            4 => Some(Self::Enum),
            5 => Some(Self::Sequence),
            6 => Some(Self::Composite),
            7 => Some(Self::Routine),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredDependencyIdentity {
    Name,
    RoutineOid(i32),
}

impl StoredDependencyIdentity {
    pub(crate) const fn encoded(self) -> i32 {
        match self {
            Self::Name => 0,
            Self::RoutineOid(oid) => oid,
        }
    }

    pub(crate) fn decode(class: DependencyClass, encoded: i32) -> Option<Self> {
        match (class, encoded) {
            (DependencyClass::Routine, oid)
                if (ROUTINE_OID_BASE..TRIGGER_OID_BASE).contains(&oid) =>
            {
                Some(Self::RoutineOid(oid))
            }
            (DependencyClass::Routine, _) => None,
            (_, 0) => Some(Self::Name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredQueryDependency {
    pub class: DependencyClass,
    pub slot: u16,
    pub identity: StoredDependencyIdentity,
    /// One bit per referenced attribute (PostgreSQL attribute numbers start
    /// at one, so bit zero represents attnum 1). Relation-only dependencies
    /// retain zero; this avoids treating an unknown column set as every
    /// column when reconstructing catalog dependencies.
    pub referenced_columns: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub referenced_schema: SqlName,
    pub referenced_name: SqlName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SerializedStoredQueryDependency {
    pub class: DependencyClass,
    pub identity: StoredDependencyIdentity,
    pub referenced_columns: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub referenced_schema: SqlName,
    pub referenced_name: SqlName,
}

impl StoredQueryDependency {
    pub(crate) const EMPTY: Self = Self {
        class: DependencyClass::Table,
        slot: 0,
        identity: StoredDependencyIdentity::Name,
        referenced_columns: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        referenced_schema: SqlName::EMPTY,
        referenced_name: SqlName::EMPTY,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredQueryDependencies {
    entries: [StoredQueryDependency; MAX_STORED_QUERY_DEPENDENCIES],
    len: u8,
}

#[derive(Debug, Clone, Copy)]
struct PendingRoutineDependencies {
    used: bool,
    txid: u32,
    routine: u16,
    dependencies: StoredQueryDependencies,
}

impl PendingRoutineDependencies {
    const EMPTY: Self = Self {
        used: false,
        txid: 0,
        routine: u16::MAX,
        dependencies: StoredQueryDependencies::EMPTY,
    };
}

impl StoredQueryDependencies {
    pub(crate) const EMPTY: Self = Self {
        entries: [StoredQueryDependency::EMPTY; MAX_STORED_QUERY_DEPENDENCIES],
        len: 0,
    };

    pub fn entries(&self) -> &[StoredQueryDependency] {
        &self.entries[..self.len as usize]
    }

    pub fn push(&mut self, dependency: StoredQueryDependency) -> Result<(), SqlError> {
        if StoredDependencyIdentity::decode(dependency.class, dependency.identity.encoded())
            != Some(dependency.identity)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored-query dependency has an invalid catalog identity"
            ));
        }
        if self.entries().iter().any(|entry| {
            entry.class == dependency.class
                && entry.slot == dependency.slot
                && entry.referenced_schema == dependency.referenced_schema
                && entry.referenced_name == dependency.referenced_name
        }) {
            return Ok(());
        }
        if self.len as usize == MAX_STORED_QUERY_DEPENDENCIES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query depends on more than {} catalog objects",
                MAX_STORED_QUERY_DEPENDENCIES
            ));
        }
        self.entries[self.len as usize] = dependency;
        self.len += 1;
        Ok(())
    }

    pub fn depends_on(&self, class: DependencyClass, slot: usize) -> bool {
        self.entries()
            .iter()
            .any(|entry| entry.class == class && entry.slot as usize == slot)
    }

    pub fn mark_referenced_column(
        &mut self,
        class: DependencyClass,
        slot: usize,
        column: usize,
    ) -> Result<(), SqlError> {
        let bit = 1u64.checked_shl(column as u32).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query references a column beyond the static attribute bound"
            )
        })?;
        let dependency = self
            .entries
            .iter_mut()
            .take(self.len as usize)
            .find(|entry| entry.class == class && entry.slot as usize == slot)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "column dependency has no owning relation dependency"
                )
            })?;
        dependency.referenced_columns |= bit;
        Ok(())
    }

    pub(crate) fn serialized_push(
        &mut self,
        dependency: SerializedStoredQueryDependency,
    ) -> Result<(), SqlError> {
        if StoredDependencyIdentity::decode(dependency.class, dependency.identity.encoded())
            != Some(dependency.identity)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored-query dependency has an invalid catalog identity"
            ));
        }
        if self.len as usize == MAX_STORED_QUERY_DEPENDENCIES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query depends on more than {} catalog objects",
                MAX_STORED_QUERY_DEPENDENCIES
            ));
        }
        self.entries[self.len as usize] = StoredQueryDependency {
            class: dependency.class,
            slot: u16::MAX,
            identity: dependency.identity,
            referenced_columns: dependency.referenced_columns,
            schema: dependency.schema,
            name: dependency.name,
            referenced_schema: dependency.referenced_schema,
            referenced_name: dependency.referenced_name,
        };
        self.len += 1;
        Ok(())
    }

    pub fn replace_slot(
        &mut self,
        class: DependencyClass,
        old_slot: usize,
        new_slot: usize,
        schema: SqlName,
        name: SqlName,
    ) {
        for entry in &mut self.entries[..self.len as usize] {
            if entry.class == class && entry.slot as usize == old_slot {
                entry.slot = new_slot as u16;
                entry.schema = schema;
                entry.name = name;
            }
        }
    }

    pub fn rename(&mut self, class: DependencyClass, slot: usize, schema: SqlName, name: SqlName) {
        for entry in &mut self.entries[..self.len as usize] {
            if entry.class == class && entry.slot as usize == slot {
                entry.schema = schema;
                entry.name = name;
            }
        }
    }
}

/// The durable, creation-time portion shared by views and materialized views.
/// Keeping these fields together makes it impossible for a catalog creation
/// path to pass SQL text without its binding context.
#[derive(Clone)]
pub struct StoredQueryDefinition {
    pub sql: StackStr<VIEW_SQL_MAX>,
    pub creation_path: StackStr<128>,
    pub dependencies: StoredQueryDependencies,
}

/// A named view: its output is its stored SELECT text, expanded as a derived
/// table at query time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewSecurity {
    Definer,
    Invoker,
}

#[derive(Clone)]
pub struct ViewDef {
    /// Monotonic creation stamp, shared with tables (see `Table::created_at`).
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub sql: StackStr<VIEW_SQL_MAX>,
    /// The session search_path when the view was created. PostgreSQL binds a
    /// view body by OID at creation; this engine re-resolves the stored text,
    /// so it must re-resolve under the creator's path, not the reader's.
    pub creation_path: StackStr<128>,
    pub security: ViewSecurity,
    pub ownership: Ownership,
    pending_schema: Option<PendingObjectSchema>,
    ddl_state: CatalogDdlState,
}

impl ViewDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn schema_for(&self, txid: u32) -> SqlName {
        self.pending_schema
            .filter(|pending| pending.txid == txid)
            .map_or(self.schema, |pending| pending.schema)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingObjectSchema {
    pub txid: u32,
    pub schema: SqlName,
}

/// A logical publication.  Publications are database-scoped catalog objects;
/// relation membership is stored as stable table slots and is resolved again
/// when the replication stream is opened.
#[derive(Clone, Copy)]
pub struct PublicationDef {
    pub created_at: u64,
    pub name: SqlName,
    pending_name: Option<PendingPublicationName>,
    pub all_tables: bool,
    pub tables: [u16; MAX_PUBLICATION_TABLES],
    /// A zero mask means the member publishes all columns; otherwise bit n
    /// selects PostgreSQL attribute n + 1.
    pub table_column_masks: [u64; MAX_PUBLICATION_TABLES],
    pub table_filters: PublicationFilters,
    pub table_count: usize,
    pub schemas: [u8; MAX_SCHEMAS],
    pub schema_count: usize,
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
    pub publish_via_partition_root: bool,
    pub publish_generated_columns: PublishGeneratedColumns,
    pending_definition: Option<PendingPublicationDefinition>,
    pub ownership: Ownership,
    pub ddl_state: CatalogDdlState,
}

/// The mutable portion of a publication definition.  It is separate from the
/// catalog identity so an ALTER can remain private to its transaction until
/// the same commit boundary that makes its WAL record durable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicationDefinition {
    pub all_tables: bool,
    pub tables: [u16; MAX_PUBLICATION_TABLES],
    pub table_column_masks: [u64; MAX_PUBLICATION_TABLES],
    pub table_filters: PublicationFilters,
    pub table_count: usize,
    pub schemas: [u8; MAX_SCHEMAS],
    pub schema_count: usize,
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
    pub publish_via_partition_root: bool,
    pub publish_generated_columns: PublishGeneratedColumns,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingPublicationDefinition {
    pub txid: u32,
    pub definition: PublicationDefinition,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingPublicationName {
    pub txid: u32,
    pub name: SqlName,
}

/// The exact state an ALTER replaced.  Undo restores this value on both full
/// and savepoint rollback, which keeps nested ALTERs transactionally ordered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PublicationAlteration {
    Committed(Option<PendingPublicationDefinition>),
    Created(PublicationDefinition),
}

/// The immutable definition supplied when a publication enters the catalog.
/// Grouping the options keeps every durable creation path (SQL, WAL replay,
/// and manifests) on the same semantic boundary.
#[derive(Clone, Copy)]
pub struct PublicationSpec<'a> {
    pub name: SqlName,
    pub all_tables: bool,
    pub tables: &'a [u16],
    pub table_column_masks: &'a [u64],
    pub table_filter_sql: &'a [StackStr<PUBLICATION_FILTER_SQL_MAX>],
    pub schemas: &'a [u8],
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
    pub publish_via_partition_root: bool,
    pub publish_generated_columns: PublishGeneratedColumns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishGeneratedColumns {
    None,
    Stored,
}

impl PublishGeneratedColumns {
    pub(crate) const fn pg_code(self) -> &'static str {
        match self {
            Self::None => "n",
            Self::Stored => "s",
        }
    }
}

impl From<crate::sql::ast::PublishGeneratedColumns> for PublishGeneratedColumns {
    fn from(value: crate::sql::ast::PublishGeneratedColumns) -> Self {
        match value {
            crate::sql::ast::PublishGeneratedColumns::None => Self::None,
            crate::sql::ast::PublishGeneratedColumns::Stored => Self::Stored,
        }
    }
}

/// Durable state required to resume a logical replication consumer. A slot is
/// database-scoped and deliberately carries only the pgoutput-compatible
/// fields; physical XLOG slots are outside pos3ql's object-native design.
#[derive(Clone, Copy)]
pub(crate) struct ReplicationSlotDef {
    pub name: SqlName,
    pub restart_lsn: u64,
    pub confirmed_flush_lsn: u64,
    pub behavior: ReplicationSlotBehavior,
    pub active: bool,
    pub live: bool,
}

/// PostgreSQL logical-slot properties that affect what the publisher retains
/// and where the slot can resume. They are one durable state, not command text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplicationSlotBehavior {
    pub two_phase: bool,
    pub failover: bool,
}

impl ReplicationSlotBehavior {
    pub(crate) const DEFAULT: Self = Self {
        two_phase: false,
        failover: false,
    };

    pub(crate) const fn code(self) -> u8 {
        self.two_phase as u8 | ((self.failover as u8) << 1)
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        if code & !3 != 0 {
            return None;
        }
        Some(Self {
            two_phase: code & 1 != 0,
            failover: code & 2 != 0,
        })
    }
}

/// A logical-replication subscription. Connection information and publication
/// names are retained as separate typed fields so the later apply client never
/// has to reparse SQL source or infer an omitted publication list.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionDef {
    pub created_at: u64,
    /// Increments only when a committed publisher stream definition changes.
    /// An acknowledgement is valid for exactly one such definition.
    pub definition_generation: u64,
    pub name: SqlName,
    pending_name: Option<PendingSubscriptionName>,
    pub connection: SubscriptionConnInfo,
    pub publications: [SqlName; MAX_SUBSCRIPTION_PUBLICATIONS],
    pub publication_count: usize,
    pending_definition: Option<PendingSubscriptionDefinition>,
    pub enabled: bool,
    pending_enabled: Option<PendingSubscriptionEnabled>,
    pub slot: SubscriptionSlot,
    pub behavior: SubscriptionBehavior,
    pub bootstrap: SubscriptionBootstrap,
    pending_bootstrap: Option<PendingSubscriptionBootstrap>,
    pub(crate) cleanup: SubscriptionCleanup,
    pub(crate) failure: Option<SubscriptionFailure>,
    /// The latest publisher transaction durably applied locally.  This is
    /// advanced only after the same local commit has reached the WAL/object
    /// store durability boundary.
    pub confirmed_lsn: u64,
    pub ownership: Ownership,
    pub ddl_state: CatalogDdlState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionRelationState {
    Initializing,
    DataCopy,
    Ready,
}

impl SubscriptionRelationState {
    pub(crate) const fn pg_code(self) -> &'static str {
        match self {
            Self::Initializing => "i",
            Self::DataCopy => "d",
            Self::Ready => "r",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Initializing => 0,
            Self::DataCopy => 1,
            Self::Ready => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Initializing),
            1 => Some(Self::DataCopy),
            2 => Some(Self::Ready),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SubscriptionRelation {
    subscription_created_at: u64,
    definition_generation: u64,
    table_slot: u16,
    state: SubscriptionRelationState,
    synchronization_lsn: u64,
    ddl_state: CatalogDdlState,
}

impl SubscriptionRelation {
    pub(crate) fn table_slot(self) -> usize {
        usize::from(self.table_slot)
    }

    pub(crate) fn state(self) -> SubscriptionRelationState {
        self.state
    }

    pub(crate) fn synchronization_lsn(self) -> u64 {
        self.synchronization_lsn
    }
}

/// The complete durable identity of a publisher stream.  This is constructed
/// from a committed catalog entry and is required to advance its frontier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubscriptionStream {
    slot: usize,
    created_at: u64,
    definition_generation: u64,
    name: SqlName,
}

impl SubscriptionStream {
    pub(crate) const EMPTY: Self = Self {
        slot: usize::MAX,
        created_at: 0,
        definition_generation: 0,
        name: SqlName::EMPTY,
    };

    pub(crate) fn created_at(self) -> u64 {
        self.created_at
    }

    pub(crate) fn slot(self) -> usize {
        self.slot
    }

    pub(crate) fn name(self) -> SqlName {
        self.name
    }

    pub(crate) fn definition_generation(self) -> u64 {
        self.definition_generation
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: SqlName, definition_generation: u64) -> Self {
        Self {
            slot: 0,
            created_at: 1,
            definition_generation,
            name,
        }
    }
}

/// One transaction-private subscription lifecycle change.  Keeping the owner
/// and intended state together prevents a pending catalog value from being
/// mistaken for a committed setting by another transaction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingSubscriptionEnabled {
    txid: u32,
    enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingSubscriptionBootstrap {
    txid: u32,
    bootstrap: SubscriptionBootstrap,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingSubscriptionName {
    pub txid: u32,
    pub name: SqlName,
}

/// One transaction-private replacement for a subscription's stream identity.
/// Keeping connection and publication names together prevents a worker from
/// combining one committed half with one staged half.
#[derive(Clone, Copy)]
pub(crate) struct PendingSubscriptionDefinition {
    txid: u32,
    connection: SubscriptionConnInfo,
    publications: [SqlName; MAX_SUBSCRIPTION_PUBLICATIONS],
    publication_count: usize,
    slot: SubscriptionSlot,
    behavior: SubscriptionBehavior,
}

impl core::fmt::Debug for PendingSubscriptionDefinition {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PendingSubscriptionDefinition")
            .field("txid", &self.txid)
            .field("publication_count", &self.publication_count)
            .finish()
    }
}

/// Result of staging a subscription stream definition. The fixed-size undo
/// image stays a field, rather than inflating a control-flow enum variant.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionDefinitionChange {
    pub(crate) changed: bool,
    pub(crate) prior: Option<PendingSubscriptionDefinition>,
}

/// Result of requesting a transactional subscription lifecycle state.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SubscriptionEnabledChange {
    Unchanged,
    Changed {
        prior: Option<PendingSubscriptionEnabled>,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SubscriptionBootstrapChange {
    Unchanged,
    Changed {
        prior: Option<PendingSubscriptionBootstrap>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionBehavior {
    pub binary: bool,
    pub streaming: SubscriptionStreaming,
    pub synchronous_commit: SubscriptionSynchronousCommit,
    pub two_phase: bool,
    pub disable_on_error: bool,
    pub password_required: bool,
    pub run_as_owner: bool,
    pub origin: SubscriptionOrigin,
    pub failover: bool,
    pub skip_lsn: Option<u64>,
}

impl SubscriptionBehavior {
    pub(crate) const POSTGRESQL_18_DEFAULT: Self = Self {
        binary: false,
        streaming: SubscriptionStreaming::Parallel,
        synchronous_commit: SubscriptionSynchronousCommit::Off,
        two_phase: false,
        disable_on_error: false,
        password_required: true,
        run_as_owner: false,
        origin: SubscriptionOrigin::Any,
        failover: false,
        skip_lsn: None,
    };
}

impl From<crate::sql::ast::SubscriptionBehavior> for SubscriptionBehavior {
    fn from(value: crate::sql::ast::SubscriptionBehavior) -> Self {
        Self {
            binary: value.binary,
            streaming: value.streaming.into(),
            synchronous_commit: value.synchronous_commit.into(),
            two_phase: value.two_phase,
            disable_on_error: value.disable_on_error,
            password_required: value.password_required,
            run_as_owner: value.run_as_owner,
            origin: value.origin.into(),
            failover: value.failover,
            skip_lsn: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionStreaming {
    Off,
    On,
    Parallel,
}

impl SubscriptionStreaming {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Parallel => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::Parallel),
            _ => None,
        }
    }

    pub(crate) const fn pg_code(self) -> &'static str {
        match self {
            Self::Off => "f",
            Self::On => "t",
            Self::Parallel => "p",
        }
    }
}

impl From<crate::sql::ast::SubscriptionStreaming> for SubscriptionStreaming {
    fn from(value: crate::sql::ast::SubscriptionStreaming) -> Self {
        match value {
            crate::sql::ast::SubscriptionStreaming::Off => Self::Off,
            crate::sql::ast::SubscriptionStreaming::On => Self::On,
            crate::sql::ast::SubscriptionStreaming::Parallel => Self::Parallel,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionSynchronousCommit {
    Off,
    Local,
    RemoteWrite,
    On,
    RemoteApply,
}

impl SubscriptionSynchronousCommit {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Local => 1,
            Self::RemoteWrite => 2,
            Self::On => 3,
            Self::RemoteApply => 4,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Off),
            1 => Some(Self::Local),
            2 => Some(Self::RemoteWrite),
            3 => Some(Self::On),
            4 => Some(Self::RemoteApply),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
            Self::RemoteWrite => "remote_write",
            Self::On => "on",
            Self::RemoteApply => "remote_apply",
        }
    }
}

impl From<crate::sql::ast::SubscriptionSynchronousCommit> for SubscriptionSynchronousCommit {
    fn from(value: crate::sql::ast::SubscriptionSynchronousCommit) -> Self {
        match value {
            crate::sql::ast::SubscriptionSynchronousCommit::Off => Self::Off,
            crate::sql::ast::SubscriptionSynchronousCommit::Local => Self::Local,
            crate::sql::ast::SubscriptionSynchronousCommit::RemoteWrite => Self::RemoteWrite,
            crate::sql::ast::SubscriptionSynchronousCommit::On => Self::On,
            crate::sql::ast::SubscriptionSynchronousCommit::RemoteApply => Self::RemoteApply,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionOrigin {
    None,
    Any,
}

impl SubscriptionOrigin {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Any => 1,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::None),
            1 => Some(Self::Any),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Any => "any",
        }
    }
}

impl From<crate::sql::ast::SubscriptionOrigin> for SubscriptionOrigin {
    fn from(value: crate::sql::ast::SubscriptionOrigin) -> Self {
        match value {
            crate::sql::ast::SubscriptionOrigin::None => Self::None,
            crate::sql::ast::SubscriptionOrigin::Any => Self::Any,
        }
    }
}

impl SubscriptionBehavior {
    fn same_publisher_stream(self, other: Self) -> bool {
        self.binary == other.binary
            && self.streaming == other.streaming
            && self.two_phase == other.two_phase
            && self.origin == other.origin
            && self.failover == other.failover
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SubscriptionSpec<'a> {
    pub name: SqlName,
    pub connection: SubscriptionConnInfo,
    pub publications: &'a [SqlName],
    pub enabled: bool,
    pub slot: SubscriptionSlot,
    pub behavior: SubscriptionBehavior,
    pub bootstrap: SubscriptionBootstrap,
}

/// The publisher slot association and its ownership are one durable value.
/// Only a managed slot may be removed as a consequence of DROP SUBSCRIPTION.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionSlot {
    Absent,
    External(ReplicationSlotName),
    Managed(ReplicationSlotName),
}

impl SubscriptionSlot {
    pub(crate) const fn name(self) -> Option<ReplicationSlotName> {
        match self {
            Self::Absent => None,
            Self::External(name) | Self::Managed(name) => Some(name),
        }
    }
}

/// Durable work required before steady pgoutput apply may begin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionBootstrap {
    Deferred,
    CreateManagedSlot { copy_data: bool },
    CopyExternalSlot,
    CopyWithoutSlot,
    Refresh { copy_data: bool },
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionCleanup {
    None,
    DropManagedSlot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionFailure {
    pub sqlstate: SqlState,
    pub message: StackStr<192>,
}

impl SubscriptionBootstrap {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Deferred => 0,
            Self::CreateManagedSlot { copy_data: false } => 1,
            Self::CreateManagedSlot { copy_data: true } => 2,
            Self::CopyExternalSlot => 3,
            Self::CopyWithoutSlot => 4,
            Self::Ready => 5,
            Self::Refresh { copy_data: false } => 6,
            Self::Refresh { copy_data: true } => 7,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Deferred),
            1 => Some(Self::CreateManagedSlot { copy_data: false }),
            2 => Some(Self::CreateManagedSlot { copy_data: true }),
            3 => Some(Self::CopyExternalSlot),
            4 => Some(Self::CopyWithoutSlot),
            5 => Some(Self::Ready),
            6 => Some(Self::Refresh { copy_data: false }),
            7 => Some(Self::Refresh { copy_data: true }),
            _ => None,
        }
    }
}

impl SubscriptionDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn name_for(&self, txid: u32) -> SqlName {
        self.pending_name
            .filter(|pending| pending.txid == txid)
            .map_or(self.name, |pending| pending.name)
    }

    pub(crate) fn enabled_to(&self, txid: u32) -> bool {
        self.pending_enabled
            .filter(|pending| pending.txid == txid)
            .map_or(self.enabled, |pending| pending.enabled)
    }

    pub(crate) fn bootstrap_to(&self, txid: u32) -> SubscriptionBootstrap {
        self.pending_bootstrap
            .filter(|pending| pending.txid == txid)
            .map_or(self.bootstrap, |pending| pending.bootstrap)
    }

    pub(crate) fn definition_to(
        &self,
        txid: u32,
    ) -> (
        SubscriptionConnInfo,
        [SqlName; MAX_SUBSCRIPTION_PUBLICATIONS],
        usize,
        SubscriptionSlot,
        SubscriptionBehavior,
    ) {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(
                (
                    self.connection,
                    self.publications,
                    self.publication_count,
                    self.slot,
                    self.behavior,
                ),
                |pending| {
                    (
                        pending.connection,
                        pending.publications,
                        pending.publication_count,
                        pending.slot,
                        pending.behavior,
                    )
                },
            )
    }
}

/// A validated subscription acknowledgement ready to be included in the
/// local transaction that applied its publisher changes.  The creation stamp
/// prevents a dropped-and-reused catalog slot from advancing the wrong
/// subscription after a delayed worker event.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionAdvance {
    stream: SubscriptionStream,
    confirmed_lsn: u64,
}

impl SubscriptionAdvance {
    pub(crate) fn name(&self) -> &str {
        self.stream.name.as_str()
    }

    pub(crate) fn stream(&self) -> SubscriptionStream {
        self.stream
    }

    pub(crate) fn confirmed_lsn(&self) -> u64 {
        self.confirmed_lsn
    }
}

/// A validated slot acknowledgement, ready to become durable.
///
/// Constructing this proof checks the live slot and its monotonic cursor before
/// WAL is written; only this module can then apply it to the recorded slot.
pub(crate) struct ReplicationSlotAdvance {
    slot: usize,
    name: SqlName,
    confirmed_flush_lsn: u64,
}

impl ReplicationSlotAdvance {
    pub(crate) fn name(&self) -> &str {
        self.name.as_str()
    }
}

impl PublicationDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition(&self) -> PublicationDefinition {
        PublicationDefinition {
            all_tables: self.all_tables,
            tables: self.tables,
            table_column_masks: self.table_column_masks,
            table_filters: self.table_filters,
            table_count: self.table_count,
            schemas: self.schemas,
            schema_count: self.schema_count,
            publish_insert: self.publish_insert,
            publish_update: self.publish_update,
            publish_delete: self.publish_delete,
            publish_truncate: self.publish_truncate,
            publish_via_partition_root: self.publish_via_partition_root,
            publish_generated_columns: self.publish_generated_columns,
        }
    }

    pub(crate) fn name_for(&self, txid: u32) -> SqlName {
        self.pending_name
            .filter(|pending| pending.txid == txid)
            .map_or(self.name, |pending| pending.name)
    }

    fn set_definition(&mut self, definition: PublicationDefinition) {
        self.all_tables = definition.all_tables;
        self.tables = definition.tables;
        self.table_column_masks = definition.table_column_masks;
        self.table_filters = definition.table_filters;
        self.table_count = definition.table_count;
        self.schemas = definition.schemas;
        self.schema_count = definition.schema_count;
        self.publish_insert = definition.publish_insert;
        self.publish_update = definition.publish_update;
        self.publish_delete = definition.publish_delete;
        self.publish_truncate = definition.publish_truncate;
        self.publish_via_partition_root = definition.publish_via_partition_root;
        self.publish_generated_columns = definition.publish_generated_columns;
    }

    pub(crate) fn definition_for(&self, txid: u32) -> PublicationDefinition {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or_else(|| self.definition(), |pending| pending.definition)
    }
}

/// A materialized view's catalog entry: like a [`ViewDef`], but its rows live in
/// a same-named backing table (an ordinary [`Table`]). This entry stores only
/// the defining query (re-run by REFRESH) and whether it has been populated.
#[derive(Clone)]
pub struct MatviewDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub sql: StackStr<VIEW_SQL_MAX>,
    pub creation_path: StackStr<128>,
    pub ownership: Ownership,
    /// False after `WITH NO DATA` until the first REFRESH.
    pub populated: bool,
    pub ddl_state: CatalogDdlState,
}

impl MatviewDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }
}

/// The most sequences the catalog holds. A compile-time cap (not config-driven)
/// so a session's `currval` bag ([`crate::sql::guc::GucState`]) can be a fixed
/// inline array keyed by slot; exhausting it is a loud error, never growth.
pub(crate) const MAX_SEQUENCES: usize = 64;

/// How many domain types may exist at once, and how many CHECK constraints a
/// single domain may carry. Bounded conservatively: a `DomainDef` inlines its
/// CHECK predicate text, so the catalog's static footprint is
/// `MAX_DOMAINS * MAX_DOMAIN_CHECKS * CHECK_SQL_MAX`.
pub(crate) const MAX_DOMAINS: usize = 32;

/// Stored SQL routines share the table-sized catalog budget.  They are not
/// executable closures: every durable definition is a bounded, replayable SQL
/// identity and body.
pub(crate) const MAX_ROUTINE_ARGUMENTS: usize = 16;
pub(crate) const ROUTINE_SQL_MAX: usize = VIEW_SQL_MAX;
pub(crate) const ROUTINE_DEFAULT_MAX: usize = DEFAULT_EXPR_MAX;
pub(crate) const AGGREGATE_INIT_MAX: usize = 256;
/// User-defined routine OIDs occupy a stable, disjoint catalog range.
pub(crate) const ROUTINE_OID_BASE: i32 = 100_000;

/// Trigger definitions share the table-sized catalog budget.  A trigger has no
/// runtime allocation: its target and function are stable catalog slots.
pub(crate) const TRIGGER_OID_BASE: i32 = 140_000;
pub(crate) const POLICY_OID_BASE: i32 = 180_000;
pub(crate) const MAX_POLICIES_PER_TABLE: usize = 8;
pub(crate) const MAX_POLICY_ROLES: usize = 8;
pub(crate) const POLICY_EXPRESSION_MAX: usize = CHECK_SQL_MAX;

pub(crate) fn trigger_oid(trigger: &TriggerDef) -> i32 {
    TRIGGER_OID_BASE
        .checked_add(i32::try_from(trigger.created_at).expect("trigger OID range exhausted"))
        .expect("trigger OID range exhausted")
}

pub(crate) fn policy_oid(policy: &PolicyDef) -> i32 {
    POLICY_OID_BASE
        .checked_add(i32::try_from(policy.created_at).expect("policy OID range exhausted"))
        .expect("policy OID range exhausted")
}

pub(crate) fn routine_oid(routine: &RoutineDef) -> i32 {
    ROUTINE_OID_BASE
        .checked_add(i32::try_from(routine.created_at).expect("routine OID range exhausted"))
        .expect("routine OID range exhausted")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineArgumentDef {
    pub name: SqlName,
    pub ctype: ColType,
    /// The declared catalog type identity when this is not a built-in type.
    /// Slots are executor-local; this name is the durable routine contract.
    pub user_type: Option<UserTypeName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineParameterMode {
    In {
        default: Option<StackStr<ROUTINE_DEFAULT_MAX>>,
    },
    Out,
    InOut {
        default: Option<StackStr<ROUTINE_DEFAULT_MAX>>,
    },
    Variadic {
        default: Option<StackStr<ROUTINE_DEFAULT_MAX>>,
    },
}

impl RoutineParameterMode {
    pub(crate) const fn is_input(self) -> bool {
        !matches!(self, Self::Out)
    }

    pub(crate) const fn is_output(self) -> bool {
        matches!(self, Self::Out | Self::InOut { .. })
    }

    pub(crate) const fn default(self) -> Option<StackStr<ROUTINE_DEFAULT_MAX>> {
        match self {
            Self::In { default } | Self::InOut { default } | Self::Variadic { default } => default,
            Self::Out => None,
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::In { .. } => 0,
            Self::Out => 1,
            Self::InOut { .. } => 2,
            Self::Variadic { .. } => 3,
        }
    }

    pub(crate) const fn from_code(
        code: u8,
        default: Option<StackStr<ROUTINE_DEFAULT_MAX>>,
    ) -> Option<Self> {
        match code {
            0 => Some(Self::In { default }),
            1 if default.is_none() => Some(Self::Out),
            2 => Some(Self::InOut { default }),
            3 => Some(Self::Variadic { default }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineParameterDef {
    pub name: SqlName,
    pub ctype: ColType,
    pub user_type: Option<UserTypeName>,
    pub mode: RoutineParameterMode,
}

impl RoutineParameterDef {
    pub(crate) const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        ctype: ColType::Text,
        user_type: None,
        mode: RoutineParameterMode::In { default: None },
    };
}

/// PostgreSQL polymorphic pseudo-types are durable routine contracts, not
/// executor value types. They use the ordinary routine type-name field so WAL
/// and checkpoints retain the declaration while runtime call binding produces
/// a concrete [`RoutineResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolymorphicType {
    Element,
    Array,
    NonArray,
    Enum,
    Range,
    Multirange,
    Compatible,
    CompatibleArray,
    CompatibleNonArray,
    CompatibleRange,
    CompatibleMultirange,
}

impl PolymorphicType {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Element => "anyelement",
            Self::Array => "anyarray",
            Self::NonArray => "anynonarray",
            Self::Enum => "anyenum",
            Self::Range => "anyrange",
            Self::Multirange => "anymultirange",
            Self::Compatible => "anycompatible",
            Self::CompatibleArray => "anycompatiblearray",
            Self::CompatibleNonArray => "anycompatiblenonarray",
            Self::CompatibleRange => "anycompatiblerange",
            Self::CompatibleMultirange => "anycompatiblemultirange",
        }
    }

    pub(crate) const fn oid(self) -> i32 {
        use crate::sql::types::oid;
        match self {
            Self::Element => oid::ANYELEMENT,
            Self::Array => oid::ANYARRAY,
            Self::NonArray => oid::ANYNONARRAY,
            Self::Enum => oid::ANYENUM,
            Self::Range => oid::ANYRANGE,
            Self::Multirange => oid::ANYMULTIRANGE,
            Self::Compatible => oid::ANYCOMPATIBLE,
            Self::CompatibleArray => oid::ANYCOMPATIBLEARRAY,
            Self::CompatibleNonArray => oid::ANYCOMPATIBLENONARRAY,
            Self::CompatibleRange => oid::ANYCOMPATIBLERANGE,
            Self::CompatibleMultirange => oid::ANYCOMPATIBLEMULTIRANGE,
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "anyelement" => Self::Element,
            "anyarray" => Self::Array,
            "anynonarray" => Self::NonArray,
            "anyenum" => Self::Enum,
            "anyrange" => Self::Range,
            "anymultirange" => Self::Multirange,
            "anycompatible" => Self::Compatible,
            "anycompatiblearray" => Self::CompatibleArray,
            "anycompatiblenonarray" => Self::CompatibleNonArray,
            "anycompatiblerange" => Self::CompatibleRange,
            "anycompatiblemultirange" => Self::CompatibleMultirange,
            _ => return None,
        })
    }

    pub(crate) const fn compatible_family(self) -> bool {
        matches!(
            self,
            Self::Compatible
                | Self::CompatibleArray
                | Self::CompatibleNonArray
                | Self::CompatibleRange
                | Self::CompatibleMultirange
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct PolymorphicBinding {
    simple: Option<i32>,
    compatible: Option<i32>,
}

impl PolymorphicBinding {
    const EMPTY: Self = Self {
        simple: None,
        compatible: None,
    };

    fn bind(&mut self, kind: PolymorphicType, actual_oid: i32) -> Option<()> {
        let key = polymorphic_element_oid(kind, actual_oid)?;
        let compatible = matches!(
            kind,
            PolymorphicType::Compatible
                | PolymorphicType::CompatibleArray
                | PolymorphicType::CompatibleNonArray
                | PolymorphicType::CompatibleRange
                | PolymorphicType::CompatibleMultirange
        );
        let target = if compatible {
            &mut self.compatible
        } else {
            &mut self.simple
        };
        *target = Some(match *target {
            None => key,
            Some(bound) if bound == key => bound,
            Some(bound) if compatible => compatible_type_oid(bound, key)?,
            Some(_) => return None,
        });
        Some(())
    }

    fn concrete_oid(self, kind: PolymorphicType) -> Option<i32> {
        let key = if matches!(
            kind,
            PolymorphicType::Compatible
                | PolymorphicType::CompatibleArray
                | PolymorphicType::CompatibleNonArray
                | PolymorphicType::CompatibleRange
                | PolymorphicType::CompatibleMultirange
        ) {
            self.compatible?
        } else {
            self.simple?
        };
        Some(match kind {
            PolymorphicType::Element
            | PolymorphicType::NonArray
            | PolymorphicType::Enum
            | PolymorphicType::Compatible
            | PolymorphicType::CompatibleNonArray => key,
            PolymorphicType::Array | PolymorphicType::CompatibleArray => {
                array_oid_for_element(key)?
            }
            PolymorphicType::Range | PolymorphicType::CompatibleRange => {
                range_oid_for_element(key)?.0
            }
            PolymorphicType::Multirange | PolymorphicType::CompatibleMultirange => {
                range_oid_for_element(key)?.1
            }
        })
    }
}

fn compatible_type_oid(left: i32, right: i32) -> Option<i32> {
    let left = ColType::from_oid(left)?;
    let right = ColType::from_oid(right)?;
    let rank = |value: ColType| match value {
        ColType::Int2 => 1,
        ColType::Int4 => 2,
        ColType::Int8 => 3,
        ColType::Numeric => 4,
        ColType::Float4 => 5,
        ColType::Float8 => 6,
        _ => 0,
    };
    let (left_rank, right_rank) = (rank(left), rank(right));
    (left_rank > 0 && right_rank > 0).then(|| {
        if left_rank >= right_rank {
            left.oid()
        } else {
            right.oid()
        }
    })
}

fn array_oid_for_element(element_oid: i32) -> Option<i32> {
    use crate::sql::types::{ArrElem, oid};
    if (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + MAX_DOMAINS as i32).contains(&element_oid) {
        return Some(oid::FIRST_DOMAIN_ARRAY + element_oid - oid::FIRST_DOMAIN);
    }
    if (oid::FIRST_ENUM..oid::FIRST_ENUM + MAX_ENUMS as i32).contains(&element_oid) {
        return Some(oid::FIRST_ENUM_ARRAY + element_oid - oid::FIRST_ENUM);
    }
    if (oid::FIRST_COMPOSITE..oid::FIRST_COMPOSITE + MAX_COMPOSITES as i32).contains(&element_oid) {
        return Some(oid::FIRST_COMPOSITE_ARRAY + element_oid - oid::FIRST_COMPOSITE);
    }
    ArrElem::from_coltype(ColType::from_oid(element_oid)?).map(ArrElem::array_oid)
}

fn range_oid_for_element(element_oid: i32) -> Option<(i32, i32)> {
    use crate::sql::types::RangeKind;
    [
        RangeKind::Int4,
        RangeKind::Int8,
        RangeKind::Num,
        RangeKind::Date,
        RangeKind::Ts,
        RangeKind::Tstz,
    ]
    .into_iter()
    .find(|kind| kind.elem_type().oid() == element_oid)
    .map(|kind| (kind.oid(), kind.multirange_oid()))
}

fn polymorphic_element_oid(kind: PolymorphicType, actual_oid: i32) -> Option<i32> {
    use crate::sql::types::{ColType, oid};
    let actual = ColType::from_oid(actual_oid);
    match kind {
        PolymorphicType::Element | PolymorphicType::Compatible => (!matches!(
            actual,
            Some(ColType::Void | ColType::Internal | ColType::Record)
        ) && actual_oid != oid::UNKNOWN)
            .then_some(actual_oid),
        PolymorphicType::NonArray | PolymorphicType::CompatibleNonArray => {
            (!matches!(actual, Some(ColType::Array(_)))
                && !matches!(
                    actual,
                    Some(ColType::Void | ColType::Internal | ColType::Record)
                )
                && actual_oid != oid::UNKNOWN)
                .then_some(actual_oid)
        }
        PolymorphicType::Array | PolymorphicType::CompatibleArray => {
            if (oid::FIRST_DOMAIN_ARRAY..oid::FIRST_DOMAIN_ARRAY + MAX_DOMAINS as i32)
                .contains(&actual_oid)
            {
                Some(oid::FIRST_DOMAIN + actual_oid - oid::FIRST_DOMAIN_ARRAY)
            } else {
                match actual? {
                    ColType::Array(element) => Some(element.element_oid()),
                    _ => None,
                }
            }
        }
        PolymorphicType::Enum => matches!(actual, Some(ColType::Enum(_))).then_some(actual_oid),
        PolymorphicType::Range | PolymorphicType::CompatibleRange => match actual? {
            ColType::Range(kind) => Some(kind.elem_type().oid()),
            _ => None,
        },
        PolymorphicType::Multirange | PolymorphicType::CompatibleMultirange => match actual? {
            ColType::Multirange(kind) => Some(kind.elem_type().oid()),
            _ => None,
        },
    }
}

fn polymorphic_type(ctype: ColType, user_type: Option<UserTypeName>) -> Option<PolymorphicType> {
    let _ = ctype;
    user_type
        .filter(|identity| identity.schema.as_str() == "pg_catalog")
        .and_then(|identity| PolymorphicType::from_name(identity.name.as_str()))
}

impl RoutineArgumentDef {
    pub(crate) fn polymorphic_type(self) -> Option<PolymorphicType> {
        polymorphic_type(self.ctype, self.user_type)
    }
}

/// A scalar routine result keeps its executor representation and its durable
/// catalog identity together.  A bare `ColType::Enum(slot)` cannot survive a
/// catalog rebuild because slots are allocation details, not identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineResult {
    pub ctype: ColType,
    pub user_type: Option<UserTypeName>,
}

impl RoutineResult {
    pub(crate) const TEXT: Self = Self {
        ctype: ColType::Text,
        user_type: None,
    };

    pub(crate) const fn builtin(ctype: ColType) -> Self {
        Self {
            ctype,
            user_type: None,
        }
    }

    pub(crate) fn polymorphic_type(self) -> Option<PolymorphicType> {
        polymorphic_type(self.ctype, self.user_type)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineSpec {
    pub identity: RoutineIdentity,
    pub schema: SqlName,
    pub name: SqlName,
    pub arguments: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub argument_count: usize,
    pub parameters: [RoutineParameterDef; MAX_ROUTINE_ARGUMENTS],
    pub parameter_count: usize,
    pub kind: RoutineKind,
    pub result_columns: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub result_column_count: usize,
    pub language: RoutineLanguage,
    pub attributes: RoutineAttributes,
    pub configs: [RoutineConfig; MAX_ROUTINE_CONFIGS],
    pub config_count: usize,
    pub body_kind: RoutineBodyKind,
    pub body: StackStr<ROUTINE_SQL_MAX>,
    pub creation_path: StackStr<128>,
    pub dependencies: StoredQueryDependencies,
}

/// Executable languages represented by the engine. Catalog languages that do
/// not have an execution implementation never enter a routine definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineLanguage {
    Sql,
    PlPgSql,
    Internal,
}

impl RoutineLanguage {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Sql => 0,
            Self::PlPgSql => 1,
            Self::Internal => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Sql),
            1 => Some(Self::PlPgSql),
            2 => Some(Self::Internal),
            _ => None,
        }
    }
}

pub(crate) const MAX_ROUTINE_CONFIGS: usize = 16;
pub(crate) const ROUTINE_CONFIG_VALUE_MAX: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineConfig {
    pub name: SqlName,
    pub value: StackStr<ROUTINE_CONFIG_VALUE_MAX>,
}

impl RoutineConfig {
    pub(crate) const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        value: StackStr::new(),
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineBodyKind {
    String,
    Return,
    Atomic,
}

impl RoutineBodyKind {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::String => 0,
            Self::Return => 1,
            Self::Atomic => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::String),
            1 => Some(Self::Return),
            2 => Some(Self::Atomic),
            _ => None,
        }
    }
}

/// A routine's invocation contract. Keeping a function result inside the
/// function variant makes a procedure with a fabricated scalar result
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "aggregate contracts stay inline and Copy; catalog heap indirection is forbidden"
)]
pub(crate) enum RoutineKind {
    Function {
        result: RoutineResult,
    },
    SetFunction {
        result: RoutineResult,
    },
    /// OUT/INOUT parameters form a named record. The set bit is part of the
    /// kind, so a scalar record cannot enter a table-function execution path.
    RecordFunction {
        set_returning: bool,
    },
    TableFunction,
    Trigger,
    Procedure,
    Aggregate(AggregateRoutine),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateKind {
    Normal,
    OrderedSet,
    HypotheticalSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateFinalModify {
    ReadOnly,
    Shareable,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineParallel {
    Safe,
    Restricted,
    Unsafe,
}

impl RoutineParallel {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Safe => 0,
            Self::Restricted => 1,
            Self::Unsafe => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Safe),
            1 => Some(Self::Restricted),
            2 => Some(Self::Unsafe),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineVolatility {
    Immutable,
    Stable,
    Volatile,
}

impl RoutineVolatility {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Immutable => 0,
            Self::Stable => 1,
            Self::Volatile => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Immutable),
            1 => Some(Self::Stable),
            2 => Some(Self::Volatile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineAttributes {
    pub strict: bool,
    pub volatility: RoutineVolatility,
    pub parallel: RoutineParallel,
    pub security_definer: bool,
    pub leakproof: bool,
    pub cost_bits: Option<u64>,
    pub rows_bits: Option<u64>,
}

impl RoutineAttributes {
    pub(crate) const DEFAULT: Self = Self {
        strict: false,
        volatility: RoutineVolatility::Volatile,
        parallel: RoutineParallel::Unsafe,
        security_definer: false,
        leakproof: false,
        cost_bits: None,
        rows_bits: None,
    };

    pub(crate) const AGGREGATE: Self = Self::DEFAULT;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AggregateFinalRoutine {
    pub function_oid: i32,
    pub extra: bool,
    pub modify: AggregateFinalModify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AggregateSerde {
    pub serialize_oid: i32,
    pub deserialize_oid: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AggregatePartialRoutine {
    pub combine_oid: i32,
    pub combine_strict: bool,
    pub serde: Option<AggregateSerde>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AggregateMovingRoutine {
    pub transition_oid: i32,
    pub inverse_oid: i32,
    pub state_type: RoutineResult,
    pub state_space: Option<u32>,
    pub final_function: Option<AggregateFinalRoutine>,
    pub initial_condition: Option<StackStr<AGGREGATE_INIT_MAX>>,
}

/// A catalog aggregate is a distinct routine kind. Every support function is
/// held by stable object identifier, so rename and schema moves cannot leave a
/// stale textual reference and DROP can enforce dependencies at one boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AggregateRoutine {
    pub kind: AggregateKind,
    pub direct_argument_count: u8,
    pub variadic_argument: Option<u8>,
    pub state_type: RoutineResult,
    pub state_space: Option<u32>,
    pub result_type: RoutineResult,
    pub transition_oid: i32,
    pub final_function: Option<AggregateFinalRoutine>,
    pub partial: Option<AggregatePartialRoutine>,
    pub moving: Option<AggregateMovingRoutine>,
    pub initial_condition: Option<StackStr<AGGREGATE_INIT_MAX>>,
    pub sort_operator_oid: Option<i32>,
    pub parallel: RoutineParallel,
}

impl AggregateRoutine {
    pub(crate) fn uses_function_oid(self, oid: i32) -> bool {
        self.transition_oid == oid
            || self
                .final_function
                .is_some_and(|function| function.function_oid == oid)
            || self.partial.is_some_and(|partial| {
                partial.combine_oid == oid
                    || partial.serde.is_some_and(|serde| {
                        serde.serialize_oid == oid || serde.deserialize_oid == oid
                    })
            })
            || self.moving.is_some_and(|moving| {
                moving.transition_oid == oid
                    || moving.inverse_oid == oid
                    || moving
                        .final_function
                        .is_some_and(|function| function.function_oid == oid)
            })
    }

    pub(crate) fn encode_wire(self) -> StackStr<ROUTINE_SQL_MAX> {
        use core::fmt::Write;
        fn write_type(
            out: &mut StackStr<ROUTINE_SQL_MAX>,
            value: RoutineResult,
        ) -> core::fmt::Result {
            fn hex(out: &mut StackStr<ROUTINE_SQL_MAX>, value: &str) -> core::fmt::Result {
                if value.is_empty() {
                    return out.write_str("-");
                }
                for byte in value.as_bytes() {
                    write!(out, "{byte:02x}")?;
                }
                Ok(())
            }
            write!(out, "{} ", value.ctype.code())?;
            if let Some(identity) = value.user_type {
                hex(out, identity.schema.as_str())?;
                out.write_str(" ")?;
                hex(out, identity.name.as_str())
            } else {
                out.write_str("- -")
            }
        }
        fn hex(out: &mut StackStr<ROUTINE_SQL_MAX>, value: Option<&str>) -> core::fmt::Result {
            let Some(value) = value else {
                return out.write_str("-");
            };
            if value.is_empty() {
                return out.write_str("00");
            }
            for byte in value.as_bytes() {
                write!(out, "{byte:02x}")?;
            }
            Ok(())
        }
        let mut out = StackStr::new();
        let kind = match self.kind {
            AggregateKind::Normal => 0,
            AggregateKind::OrderedSet => 1,
            AggregateKind::HypotheticalSet => 2,
        };
        let parallel = match self.parallel {
            RoutineParallel::Safe => 0,
            RoutineParallel::Restricted => 1,
            RoutineParallel::Unsafe => 2,
        };
        let modify = |value: AggregateFinalModify| match value {
            AggregateFinalModify::ReadOnly => 0,
            AggregateFinalModify::Shareable => 1,
            AggregateFinalModify::ReadWrite => 2,
        };
        let _ = write!(
            out,
            "{kind} {} {} ",
            self.direct_argument_count,
            self.variadic_argument.map_or(-1, i16::from)
        );
        let _ = write_type(&mut out, self.state_type);
        let _ = write!(out, " {} ", self.state_space.map_or(-1, i64::from));
        let _ = write_type(&mut out, self.result_type);
        let _ = write!(out, " {} ", self.transition_oid);
        if let Some(final_function) = self.final_function {
            let _ = write!(
                out,
                "{} {} {} ",
                final_function.function_oid,
                u8::from(final_function.extra),
                modify(final_function.modify)
            );
        } else {
            let _ = out.write_str("-1 0 0 ");
        }
        if let Some(partial) = self.partial {
            let _ = write!(
                out,
                "{} {} ",
                partial.combine_oid,
                u8::from(partial.combine_strict)
            );
            if let Some(serde) = partial.serde {
                let _ = write!(out, "{} {} ", serde.serialize_oid, serde.deserialize_oid);
            } else {
                let _ = out.write_str("-1 -1 ");
            }
        } else {
            let _ = out.write_str("-1 0 -1 -1 ");
        }
        if let Some(moving) = self.moving {
            let _ = write!(out, "{} {} ", moving.transition_oid, moving.inverse_oid);
            let _ = write_type(&mut out, moving.state_type);
            let _ = write!(out, " {} ", moving.state_space.map_or(-1, i64::from));
            if let Some(final_function) = moving.final_function {
                let _ = write!(
                    out,
                    "{} {} {} ",
                    final_function.function_oid,
                    u8::from(final_function.extra),
                    modify(final_function.modify)
                );
            } else {
                let _ = out.write_str("-1 0 0 ");
            }
            let _ = hex(
                &mut out,
                moving.initial_condition.as_ref().map(StackStr::as_str),
            );
        } else {
            let _ = out.write_str("-1 -1 5 - - -1 -1 0 0 -");
        }
        let _ = out.write_str(" ");
        let _ = hex(
            &mut out,
            self.initial_condition.as_ref().map(StackStr::as_str),
        );
        let _ = write!(
            out,
            " {} {parallel}",
            self.sort_operator_oid.map_or(-1, i32::from)
        );
        out
    }

    pub(crate) fn decode_wire(encoded: &str) -> Option<Self> {
        fn decode_hex<const N: usize>(word: &str) -> Option<StackStr<N>> {
            if word == "-" {
                return None;
            }
            if word == "00" {
                return Some(StackStr::new());
            }
            if !word.len().is_multiple_of(2) {
                return None;
            }
            let bytes = word.as_bytes();
            if bytes.len() / 2 > N {
                return None;
            }
            let mut decoded = [0_u8; N];
            let mut index = 0;
            while index < bytes.len() {
                let pair = core::str::from_utf8(&bytes[index..index + 2]).ok()?;
                decoded[index / 2] = u8::from_str_radix(pair, 16).ok()?;
                index += 2;
            }
            let text = core::str::from_utf8(&decoded[..bytes.len() / 2]).ok()?;
            let mut out = StackStr::new();
            core::fmt::Write::write_str(&mut out, text).ok()?;
            (!out.is_truncated()).then_some(out)
        }
        fn word<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
            words.next()
        }
        fn number<'a, T: core::str::FromStr>(
            words: &mut impl Iterator<Item = &'a str>,
        ) -> Option<T> {
            word(words)?.parse().ok()
        }
        fn read_type<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<RoutineResult> {
            let ctype = ColType::from_code(number(words)?)?;
            let schema = word(words)?;
            let name = word(words)?;
            let user_type = if schema == "-" && name == "-" {
                None
            } else {
                let schema = decode_hex::<63>(schema)?;
                let name = decode_hex::<63>(name)?;
                Some(UserTypeName {
                    schema: SqlName::parse(schema.as_str()).ok()?,
                    name: SqlName::parse(name.as_str()).ok()?,
                })
            };
            Some(RoutineResult { ctype, user_type })
        }
        fn parse_final_function(
            oid: i32,
            extra: u8,
            modify: u8,
        ) -> Option<Option<AggregateFinalRoutine>> {
            if oid == -1 {
                return (extra == 0 && modify == 0).then_some(None);
            }
            Some(Some(AggregateFinalRoutine {
                function_oid: oid,
                extra: match extra {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
                modify: match modify {
                    0 => AggregateFinalModify::ReadOnly,
                    1 => AggregateFinalModify::Shareable,
                    2 => AggregateFinalModify::ReadWrite,
                    _ => return None,
                },
            }))
        }
        let mut words = encoded.split_whitespace();
        let kind = match number(&mut words)? {
            0u8 => AggregateKind::Normal,
            1 => AggregateKind::OrderedSet,
            2 => AggregateKind::HypotheticalSet,
            _ => return None,
        };
        let direct_argument_count = number(&mut words)?;
        let variadic = number::<i16>(&mut words)?;
        let variadic_argument = if variadic == -1 {
            None
        } else {
            Some(u8::try_from(variadic).ok()?)
        };
        let state_type = read_type(&mut words)?;
        let state_space = match number::<i64>(&mut words)? {
            -1 => None,
            value => Some(u32::try_from(value).ok()?),
        };
        let result_type = read_type(&mut words)?;
        let transition_oid = number(&mut words)?;
        let final_oid = number(&mut words)?;
        let final_extra = number(&mut words)?;
        let final_modify = number(&mut words)?;
        let final_function = parse_final_function(final_oid, final_extra, final_modify)?;
        let combine_oid = number::<i32>(&mut words)?;
        let combine_strict = match number::<u8>(&mut words)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let serialize_oid = number::<i32>(&mut words)?;
        let deserialize_oid = number::<i32>(&mut words)?;
        let partial = if combine_oid == -1 {
            if combine_strict || serialize_oid != -1 || deserialize_oid != -1 {
                return None;
            }
            None
        } else {
            let serde = if serialize_oid == -1 && deserialize_oid == -1 {
                None
            } else if serialize_oid != -1 && deserialize_oid != -1 {
                Some(AggregateSerde {
                    serialize_oid,
                    deserialize_oid,
                })
            } else {
                return None;
            };
            Some(AggregatePartialRoutine {
                combine_oid,
                combine_strict,
                serde,
            })
        };
        let moving_transition = number::<i32>(&mut words)?;
        let moving_inverse = number::<i32>(&mut words)?;
        let moving_state_type = read_type(&mut words)?;
        let moving_space = match number::<i64>(&mut words)? {
            -1 => None,
            value => Some(u32::try_from(value).ok()?),
        };
        let moving_final_oid = number(&mut words)?;
        let moving_final_extra = number(&mut words)?;
        let moving_final_modify = number(&mut words)?;
        let moving_init_word = word(&mut words)?;
        let moving = if moving_transition == -1 && moving_inverse == -1 {
            None
        } else if moving_transition != -1 && moving_inverse != -1 {
            Some(AggregateMovingRoutine {
                transition_oid: moving_transition,
                inverse_oid: moving_inverse,
                state_type: moving_state_type,
                state_space: moving_space,
                final_function: parse_final_function(
                    moving_final_oid,
                    moving_final_extra,
                    moving_final_modify,
                )?,
                initial_condition: decode_hex(moving_init_word),
            })
        } else {
            return None;
        };
        let initial_condition = decode_hex(word(&mut words)?);
        let sort_operator_oid = match number::<i32>(&mut words)? {
            -1 => None,
            oid => Some(oid),
        };
        let parallel = match number(&mut words)? {
            0u8 => RoutineParallel::Safe,
            1 => RoutineParallel::Restricted,
            2 => RoutineParallel::Unsafe,
            _ => return None,
        };
        if words.next().is_some() {
            return None;
        }
        Some(Self {
            kind,
            direct_argument_count,
            variadic_argument,
            state_type,
            state_space,
            result_type,
            transition_oid,
            final_function,
            partial,
            moving,
            initial_condition,
            sort_operator_oid,
            parallel,
        })
    }
}

impl RoutineKind {
    pub(crate) const fn function_result(self) -> Option<ColType> {
        match self {
            Self::Function { result } | Self::SetFunction { result } => Some(result.ctype),
            Self::RecordFunction { .. } | Self::TableFunction => Some(ColType::Record),
            Self::Trigger | Self::Procedure | Self::Aggregate(_) => None,
        }
    }

    pub(crate) const fn is_set_returning(self) -> bool {
        matches!(
            self,
            Self::SetFunction { .. }
                | Self::RecordFunction {
                    set_returning: true
                }
                | Self::TableFunction
        )
    }

    pub(crate) const fn catalog_kind(self) -> &'static str {
        match self {
            Self::Function { .. }
            | Self::SetFunction { .. }
            | Self::RecordFunction { .. }
            | Self::TableFunction
            | Self::Trigger => "f",
            Self::Procedure => "p",
            Self::Aggregate(_) => "a",
        }
    }

    pub(crate) const fn wire_code(self) -> u8 {
        match self {
            Self::Function { .. } => 0,
            Self::SetFunction { .. } => 2,
            Self::TableFunction => 3,
            Self::Procedure => 1,
            Self::Trigger => 4,
            Self::Aggregate(_) => 5,
            Self::RecordFunction {
                set_returning: false,
            } => 6,
            Self::RecordFunction {
                set_returning: true,
            } => 7,
        }
    }

    pub(crate) const fn from_wire_code(code: u8, result: RoutineResult) -> Option<Self> {
        match code {
            0 => Some(Self::Function { result }),
            1 => Some(Self::Procedure),
            2 => Some(Self::SetFunction { result }),
            3 => Some(Self::TableFunction),
            4 => Some(Self::Trigger),
            6 => Some(Self::RecordFunction {
                set_returning: false,
            }),
            7 => Some(Self::RecordFunction {
                set_returning: true,
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum RoutineCallKind {
    Scalar,
    Set,
    Trigger,
    Procedure,
    Aggregate,
}

impl RoutineCallKind {
    const fn accepts(self, kind: RoutineKind) -> bool {
        match self {
            Self::Scalar => kind.function_result().is_some() && !kind.is_set_returning(),
            Self::Set => kind.is_set_returning(),
            Self::Trigger => matches!(kind, RoutineKind::Trigger),
            Self::Procedure => matches!(kind, RoutineKind::Procedure),
            Self::Aggregate => matches!(kind, RoutineKind::Aggregate(_)),
        }
    }
}

/// The catalog identity of a routine definition. Replacement retains every
/// catalog-owned field; a new definition receives them once.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoutineIdentity {
    Allocate,
    Preserve {
        created_at: u64,
        ownership: Ownership,
    },
}

impl RoutineArgumentDef {
    pub(crate) const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        ctype: ColType::Text,
        user_type: None,
    };
}

/// A durable SQL-language routine. Arguments and result contracts are stored
/// as parsed types, so catalog rendering and execution cannot disagree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub(crate) pending_identity: Option<PendingRoutineIdentity>,
    pub(crate) pending_definition: Option<PendingRoutineDefinition>,
    pub arguments: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub argument_count: usize,
    pub parameters: [RoutineParameterDef; MAX_ROUTINE_ARGUMENTS],
    pub parameter_count: usize,
    pub kind: RoutineKind,
    pub(crate) result_columns: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub(crate) result_column_count: usize,
    pub language: RoutineLanguage,
    pub attributes: RoutineAttributes,
    pub configs: [RoutineConfig; MAX_ROUTINE_CONFIGS],
    pub config_count: usize,
    pub body_kind: RoutineBodyKind,
    pub body: StackStr<ROUTINE_SQL_MAX>,
    pub creation_path: StackStr<128>,
    pub ownership: Ownership,
    pub ddl_state: CatalogDdlState,
}

/// The mutable portion of a routine definition, staged for one transaction.
/// Keeping it as one value prevents a caller from observing a new body with
/// an old result contract (or vice versa).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingRoutineDefinition {
    pub(crate) txid: u32,
    pub(crate) arguments: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub(crate) argument_count: usize,
    pub(crate) parameters: [RoutineParameterDef; MAX_ROUTINE_ARGUMENTS],
    pub(crate) parameter_count: usize,
    pub(crate) kind: RoutineKind,
    pub(crate) result_columns: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub(crate) result_column_count: usize,
    pub(crate) language: RoutineLanguage,
    pub(crate) attributes: RoutineAttributes,
    pub(crate) configs: [RoutineConfig; MAX_ROUTINE_CONFIGS],
    pub(crate) config_count: usize,
    pub(crate) body_kind: RoutineBodyKind,
    pub(crate) body: StackStr<ROUTINE_SQL_MAX>,
    pub(crate) creation_path: StackStr<128>,
    pub(crate) dependency_slot: u32,
}

impl RoutineDef {
    pub(crate) const EMPTY: Self = Self {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        pending_identity: None,
        pending_definition: None,
        arguments: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
        argument_count: 0,
        parameters: [RoutineParameterDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
        parameter_count: 0,
        kind: RoutineKind::Function {
            result: RoutineResult::TEXT,
        },
        result_columns: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
        result_column_count: 0,
        language: RoutineLanguage::Sql,
        attributes: RoutineAttributes::DEFAULT,
        configs: [RoutineConfig::EMPTY; MAX_ROUTINE_CONFIGS],
        config_count: 0,
        body_kind: RoutineBodyKind::String,
        body: StackStr::new(),
        creation_path: StackStr::new(),
        ownership: Ownership::BOOTSTRAP,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn arguments(&self) -> &[RoutineArgumentDef] {
        &self.arguments[..self.argument_count]
    }

    pub(crate) fn parameters(&self) -> &[RoutineParameterDef] {
        &self.parameters[..self.parameter_count]
    }

    pub(crate) fn configs(&self) -> &[RoutineConfig] {
        &self.configs[..self.config_count]
    }

    pub(crate) fn required_argument_count(&self) -> usize {
        self.parameters()
            .iter()
            .filter(|parameter| parameter.mode.is_input())
            .take_while(|parameter| parameter.mode.default().is_none())
            .count()
    }

    pub(crate) fn accepts_input_arity(&self, arity: usize) -> bool {
        self.required_argument_count() <= arity && arity <= self.argument_count
    }

    pub(crate) fn parameter_for_input(&self, input_index: usize) -> Option<RoutineParameterDef> {
        self.parameters()
            .iter()
            .copied()
            .filter(|parameter| parameter.mode.is_input())
            .nth(input_index)
    }

    /// Maps call-site argument positions to the declared input signature.
    /// Missing slots are accepted only when the typed parameter owns a
    /// default.
    pub(crate) fn call_input_mapping(
        &self,
        argument_names: &[Option<&str>],
        argument_count: usize,
        explicit_variadic: bool,
    ) -> Option<[u8; MAX_ROUTINE_ARGUMENTS]> {
        let variadic_index = self
            .arguments()
            .iter()
            .enumerate()
            .find_map(|(input_index, _)| {
                matches!(
                    self.parameter_for_input(input_index)?.mode,
                    RoutineParameterMode::Variadic { .. }
                )
                .then_some(input_index)
            });
        if argument_count > MAX_ROUTINE_ARGUMENTS
            || (argument_count > self.argument_count && variadic_index.is_none())
            || (!argument_names.is_empty() && argument_names.len() != argument_count)
            || explicit_variadic && !argument_names.is_empty()
        {
            return None;
        }
        let mut mapping = [u8::MAX; MAX_ROUTINE_ARGUMENTS];
        let mut occupied = [false; MAX_ROUTINE_ARGUMENTS];
        for (call_index, mapped_input) in mapping.iter_mut().enumerate().take(argument_count) {
            let input_index = match argument_names.get(call_index).copied().flatten() {
                None if !explicit_variadic
                    && variadic_index.is_some_and(|variadic| call_index >= variadic) =>
                {
                    variadic_index?
                }
                None => call_index,
                Some(name) => (0..self.argument_count).find(|&input_index| {
                    self.parameter_for_input(input_index)
                        .is_some_and(|parameter| parameter.name.as_str().eq_ignore_ascii_case(name))
                })?,
            };
            let expanded_variadic =
                !explicit_variadic && variadic_index == Some(input_index) && occupied[input_index];
            if input_index >= self.argument_count || occupied[input_index] && !expanded_variadic {
                return None;
            }
            occupied[input_index] = true;
            *mapped_input = u8::try_from(input_index).ok()?;
        }
        for (input_index, occupied) in occupied.iter().enumerate().take(self.argument_count) {
            if !occupied {
                let mode = self.parameter_for_input(input_index)?.mode;
                let _ = mode.default()?;
            }
        }
        Some(mapping)
    }

    /// CALL syntax includes OUT placeholders even though overload identity and
    /// execution inputs do not. This mapping marks those positions with
    /// `u8::MAX` and maps IN/INOUT positions to the callable signature.
    pub(crate) fn procedure_call_mapping(
        &self,
        argument_names: &[Option<&str>],
        argument_count: usize,
    ) -> Option<[u8; MAX_ROUTINE_ARGUMENTS]> {
        if !matches!(self.kind, RoutineKind::Procedure)
            || argument_count > self.parameter_count
            || (!argument_names.is_empty() && argument_names.len() != argument_count)
        {
            return None;
        }
        let mut mapping = [u8::MAX; MAX_ROUTINE_ARGUMENTS];
        let mut occupied = [false; MAX_ROUTINE_ARGUMENTS];
        let mut input_of_parameter = [u8::MAX; MAX_ROUTINE_ARGUMENTS];
        let mut input_index = 0usize;
        for (parameter_index, parameter) in self.parameters().iter().enumerate() {
            if parameter.mode.is_input() {
                input_of_parameter[parameter_index] = u8::try_from(input_index).ok()?;
                input_index += 1;
            }
        }
        for (call_index, mapped_input) in mapping.iter_mut().enumerate().take(argument_count) {
            let parameter_index = match argument_names.get(call_index).copied().flatten() {
                None => call_index,
                Some(name) => self
                    .parameters()
                    .iter()
                    .position(|parameter| parameter.name.as_str().eq_ignore_ascii_case(name))?,
            };
            if occupied[parameter_index] {
                return None;
            }
            occupied[parameter_index] = true;
            *mapped_input = input_of_parameter[parameter_index];
        }
        for (parameter_index, occupied) in occupied.iter().enumerate().take(self.parameter_count) {
            if !occupied {
                let parameter = self.parameters[parameter_index];
                if !parameter.mode.is_input() || parameter.mode.default().is_none() {
                    return None;
                }
            }
        }
        Some(mapping)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> Self {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(*self, |pending| Self {
                arguments: pending.arguments,
                argument_count: pending.argument_count,
                parameters: pending.parameters,
                parameter_count: pending.parameter_count,
                kind: pending.kind,
                result_columns: pending.result_columns,
                result_column_count: pending.result_column_count,
                language: pending.language,
                attributes: pending.attributes,
                configs: pending.configs,
                config_count: pending.config_count,
                body_kind: pending.body_kind,
                body: pending.body,
                creation_path: pending.creation_path,
                pending_definition: None,
                ..*self
            })
    }

    pub(crate) fn table_columns(&self) -> Option<&[RoutineArgumentDef]> {
        matches!(
            self.kind,
            RoutineKind::TableFunction
                | RoutineKind::RecordFunction {
                    set_returning: true
                }
        )
        .then_some(&self.result_columns[..self.result_column_count])
    }

    /// PostgreSQL exposes a single OUT column as the set element itself, not
    /// as a record that can be expanded with field selection or `.*`.
    pub(crate) fn record_result_columns(&self) -> Option<&[RoutineArgumentDef]> {
        let columns = matches!(
            self.kind,
            RoutineKind::TableFunction | RoutineKind::RecordFunction { .. }
        )
        .then_some(&self.result_columns[..self.result_column_count])?;
        (columns.len() > 1).then_some(columns)
    }

    pub(crate) fn schema_for(&self, txid: u32) -> SqlName {
        self.pending_identity
            .filter(|pending| pending.txid == txid)
            .map_or(self.schema, |pending| pending.schema)
    }

    pub(crate) fn name_for(&self, txid: u32) -> SqlName {
        self.pending_identity
            .filter(|pending| pending.txid == txid)
            .map_or(self.name, |pending| pending.name)
    }
}

/// A durable trigger transition-relation declaration.  The variants preserve
/// the names as one validated state instead of allowing independently optional
/// aliases to drift into an invalid catalog record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerTransitionTables {
    None,
    Old(SqlName),
    New(SqlName),
    OldNew { old: SqlName, new: SqlName },
}

impl TriggerTransitionTables {
    pub(crate) fn from_names(old: Option<&str>, new: Option<&str>) -> Option<Self> {
        match (old, new) {
            (None, None) => Some(Self::None),
            (Some(old), None) => Some(Self::Old(SqlName::parse(old).ok()?)),
            (None, Some(new)) => Some(Self::New(SqlName::parse(new).ok()?)),
            (Some(old), Some(new)) if old.eq_ignore_ascii_case(new) => None,
            (Some(old), Some(new)) => Some(Self::OldNew {
                old: SqlName::parse(old).ok()?,
                new: SqlName::parse(new).ok()?,
            }),
        }
    }

    pub(crate) const fn old(self) -> Option<SqlName> {
        match self {
            Self::Old(old) | Self::OldNew { old, .. } => Some(old),
            Self::None | Self::New(_) => None,
        }
    }

    pub(crate) const fn new_table(self) -> Option<SqlName> {
        match self {
            Self::New(new) | Self::OldNew { new, .. } => Some(new),
            Self::None | Self::Old(_) => None,
        }
    }

    pub(crate) const fn is_valid_for(
        self,
        timing: crate::sql::ast::TriggerTiming,
        _level: crate::sql::ast::TriggerLevel,
        events: crate::sql::ast::TriggerEvents,
    ) -> bool {
        match self {
            Self::None => true,
            Self::Old(_) | Self::New(_) | Self::OldNew { .. } => {
                matches!(timing, crate::sql::ast::TriggerTiming::After)
                    && events.bits().count_ones() == 1
                    && !events.has_truncate()
                    && (self.old().is_none()
                        || events.contains(crate::sql::ast::TriggerEvents::UPDATE)
                        || events.contains(crate::sql::ast::TriggerEvents::DELETE))
                    && (self.new_table().is_none()
                        || events.contains(crate::sql::ast::TriggerEvents::INSERT)
                        || events.contains(crate::sql::ast::TriggerEvents::UPDATE))
            }
        }
    }
}

/// A durable trigger definition. The bit set is valid only when nonzero;
/// construction remains inside the storage choke point so an empty trigger
/// event set cannot enter the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerTarget {
    Table(u16),
    View(u16),
}

/// Constraint-trigger-only state is carried as one durable variant. Ordinary
/// triggers therefore cannot accidentally become deferrable or acquire a
/// referenced relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerKind {
    Ordinary,
    Constraint {
        referenced_table: Option<u16>,
        timing: ConstraintTiming,
    },
}

impl TriggerKind {
    pub(crate) const fn timing(self) -> ConstraintTiming {
        match self {
            Self::Ordinary => ConstraintTiming::NotDeferrable,
            Self::Constraint { timing, .. } => timing,
        }
    }

    pub(crate) const fn referenced_table(self) -> Option<u16> {
        match self {
            Self::Ordinary => None,
            Self::Constraint {
                referenced_table, ..
            } => referenced_table,
        }
    }
}

impl From<usize> for TriggerTarget {
    fn from(slot: usize) -> Self {
        Self::Table(u16::try_from(slot).expect("table slots fit the trigger target representation"))
    }
}

impl TriggerTarget {
    /// Internal comment identity. Relation slots are stable across renames and
    /// schema moves; the high bit separates table and view namespaces.
    pub(crate) const fn comment_subid(self) -> u32 {
        match self {
            Self::Table(slot) => slot as u32 + 1,
            Self::View(slot) => (1u32 << 31) | (slot as u32 + 1),
        }
    }

    const fn is_view(self) -> bool {
        matches!(self, Self::View(_))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TriggerDef {
    pub(crate) created_at: u64,
    pub(crate) name: SqlName,
    pub(crate) target: TriggerTarget,
    pub(crate) kind: TriggerKind,
    pub(crate) function: u16,
    pub(crate) timing: crate::sql::ast::TriggerTiming,
    pub(crate) level: crate::sql::ast::TriggerLevel,
    pub(crate) events: crate::sql::ast::TriggerEvents,
    pub(crate) update_columns: u64,
    pub(crate) transition_tables: TriggerTransitionTables,
    pub(crate) when: Option<StackStr<TRIGGER_WHEN_MAX>>,
    pub(crate) arguments: TriggerArguments,
    pub(crate) enabled: TriggerEnabled,
    pending_definition: Option<PendingTriggerDefinition>,
    pub(crate) ddl_state: CatalogDdlState,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingTriggerDefinition {
    pub(crate) txid: u32,
    pub(crate) definition: TriggerDefinition,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TriggerDefinition {
    pub(crate) name: SqlName,
    pub(crate) kind: TriggerKind,
    pub(crate) function: u16,
    pub(crate) timing: crate::sql::ast::TriggerTiming,
    pub(crate) level: crate::sql::ast::TriggerLevel,
    pub(crate) events: crate::sql::ast::TriggerEvents,
    pub(crate) update_columns: u64,
    pub(crate) transition_tables: TriggerTransitionTables,
    pub(crate) when: Option<StackStr<TRIGGER_WHEN_MAX>>,
    pub(crate) arguments: TriggerArguments,
    pub(crate) enabled: TriggerEnabled,
}

/// PostgreSQL's durable `pg_trigger.tgenabled` state.  A boolean cannot
/// represent the distinct origin, replication, and always execution modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerEnabled {
    Origin,
    Replica,
    Always,
    Disabled,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingPartitionTriggerState {
    txid: u32,
    enabled: TriggerEnabled,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PartitionTriggerState {
    trigger: u16,
    table: u16,
    enabled: TriggerEnabled,
    present: bool,
    pending: Option<PendingPartitionTriggerState>,
}

impl PartitionTriggerState {
    const EMPTY: Self = Self {
        trigger: u16::MAX,
        table: u16::MAX,
        enabled: TriggerEnabled::Origin,
        present: false,
        pending: None,
    };

    fn enabled_to(self, txid: u32) -> Option<TriggerEnabled> {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map(|pending| pending.enabled)
            .or(self.present.then_some(self.enabled))
    }
}

impl TriggerEnabled {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Origin => b'O',
            Self::Replica => b'R',
            Self::Always => b'A',
            Self::Disabled => b'D',
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            b'O' => Some(Self::Origin),
            b'R' => Some(Self::Replica),
            b'A' => Some(Self::Always),
            b'D' => Some(Self::Disabled),
            _ => None,
        }
    }

    pub(crate) const fn fires_for_origin(self) -> bool {
        matches!(self, Self::Origin | Self::Always)
    }

    pub(crate) const fn fires_for_replication(self) -> bool {
        matches!(self, Self::Replica | Self::Always)
    }
}

/// Separates an unchanged ALTER from a changed definition whose previous
/// state may itself be absent. An `Option<PendingTriggerDefinition>` alone
/// cannot encode that distinction safely.
#[derive(Clone, Copy)]
#[expect(
    clippy::large_enum_variant,
    reason = "indirection would allocate while trigger DDL is startup-memory bounded"
)]
pub(crate) enum TriggerAlter {
    Unchanged,
    Changed {
        prior: Option<PendingTriggerDefinition>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct TriggerSpec {
    pub(crate) name: SqlName,
    pub(crate) target: TriggerTarget,
    pub(crate) kind: TriggerKind,
    pub(crate) function: usize,
    pub(crate) timing: crate::sql::ast::TriggerTiming,
    pub(crate) level: crate::sql::ast::TriggerLevel,
    pub(crate) events: crate::sql::ast::TriggerEvents,
    pub(crate) update_columns: u64,
    pub(crate) transition_tables: TriggerTransitionTables,
    pub(crate) when: Option<StackStr<TRIGGER_WHEN_MAX>>,
    pub(crate) arguments: TriggerArguments,
}

impl TriggerSpec {
    /// This is the durable catalog boundary for legal PostgreSQL trigger
    /// shapes. Parser and executor checks provide user-facing diagnostics;
    /// every create, replacement, and restore still passes through here.
    pub(crate) const fn is_valid(self) -> bool {
        trigger_shape_is_valid(
            self.target.is_view(),
            matches!(self.kind, TriggerKind::Constraint { .. }),
            self.timing,
            self.level,
            self.events,
            self.update_columns,
            self.transition_tables,
        )
    }
}

pub(crate) const fn trigger_shape_is_valid(
    target_is_view: bool,
    constraint: bool,
    timing: crate::sql::ast::TriggerTiming,
    level: crate::sql::ast::TriggerLevel,
    events: crate::sql::ast::TriggerEvents,
    update_columns: u64,
    transition_tables: TriggerTransitionTables,
) -> bool {
    let has_transition_tables = !matches!(transition_tables, TriggerTransitionTables::None);
    if (matches!(level, crate::sql::ast::TriggerLevel::Row) && events.has_truncate())
        || (has_transition_tables && update_columns != 0)
        || !transition_tables.is_valid_for(timing, level, events)
    {
        return false;
    }

    if target_is_view {
        if constraint || events.has_truncate() || has_transition_tables {
            return false;
        }
        return matches!(
            (timing, level),
            (
                crate::sql::ast::TriggerTiming::InsteadOf,
                crate::sql::ast::TriggerLevel::Row
            ) | (
                crate::sql::ast::TriggerTiming::Before | crate::sql::ast::TriggerTiming::After,
                crate::sql::ast::TriggerLevel::Statement
            )
        );
    }

    if matches!(timing, crate::sql::ast::TriggerTiming::InsteadOf) {
        return false;
    }
    !constraint
        || (matches!(timing, crate::sql::ast::TriggerTiming::After)
            && matches!(level, crate::sql::ast::TriggerLevel::Row)
            && !has_transition_tables)
}

/// Maximum source length of a durable row-trigger `WHEN` predicate.
pub(crate) const TRIGGER_WHEN_MAX: usize = CHECK_SQL_MAX;

pub(crate) fn trigger_when_stackstr(source: &str) -> Result<StackStr<TRIGGER_WHEN_MAX>, SqlError> {
    let value = StackStr::from_str(source);
    if value.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "trigger WHEN predicate exceeds {} bytes",
            TRIGGER_WHEN_MAX
        ));
    }
    Ok(value)
}

/// A policy command uses PostgreSQL's pg_policy codes directly at durable and
/// catalog boundaries, while remaining a closed typed set in the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyCommandKind {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommandKind {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::All => b'*',
            Self::Select => b'r',
            Self::Insert => b'a',
            Self::Update => b'w',
            Self::Delete => b'd',
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            b'*' => Some(Self::All),
            b'r' => Some(Self::Select),
            b'a' => Some(Self::Insert),
            b'w' => Some(Self::Update),
            b'd' => Some(Self::Delete),
            _ => None,
        }
    }

    pub(crate) const fn applies_to(self, command: Self) -> bool {
        matches!(self, Self::All) || self as u8 == command as u8
    }
}

/// Resolved policy roles. PUBLIC uses the catalog-wide sentinel; every other
/// entry is a stable role slot, so role renames cannot stale policy behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PolicyRoles {
    entries: [u16; MAX_POLICY_ROLES],
    count: u8,
}

impl PolicyRoles {
    pub(crate) const PUBLIC: Self = Self {
        entries: [PUBLIC_ROLE; MAX_POLICY_ROLES],
        count: 1,
    };

    pub(crate) fn from_slice(roles: &[u16]) -> Result<Self, SqlError> {
        if roles.is_empty() || roles.len() > MAX_POLICY_ROLES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a policy can target between 1 and {} roles",
                MAX_POLICY_ROLES
            ));
        }
        let mut value = Self {
            entries: [PUBLIC_ROLE; MAX_POLICY_ROLES],
            count: roles.len() as u8,
        };
        value.entries[..roles.len()].copy_from_slice(roles);
        Ok(value)
    }

    pub(crate) fn entries(&self) -> &[u16] {
        &self.entries[..usize::from(self.count)]
    }

    pub(crate) fn applies_to(&self, storage: &Storage, role: usize, txid: u32) -> bool {
        self.entries().iter().any(|target| {
            *target == PUBLIC_ROLE
                || usize::from(*target) == role
                || storage.role_is_member_of(role, usize::from(*target), txid)
        })
    }
}

pub(crate) fn policy_expression(source: &str) -> Result<StackStr<POLICY_EXPRESSION_MAX>, SqlError> {
    let value = StackStr::from_str(source);
    if value.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "policy expression exceeds {} bytes",
            POLICY_EXPRESSION_MAX
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PolicyDefinition {
    pub(crate) roles: PolicyRoles,
    pub(crate) using: Option<StackStr<POLICY_EXPRESSION_MAX>>,
    pub(crate) with_check: Option<StackStr<POLICY_EXPRESSION_MAX>>,
    pub(crate) dependencies: StoredQueryDependencies,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingPolicyDefinition {
    pub(crate) txid: u32,
    pub(crate) definition: PolicyDefinition,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PolicyDef {
    pub(crate) created_at: u64,
    pub(crate) name: SqlName,
    pub(crate) table: u16,
    pub(crate) command: PolicyCommandKind,
    pub(crate) permissive: bool,
    pub(crate) definition: PolicyDefinition,
    pub(crate) pending_definition: Option<PendingPolicyDefinition>,
    pub(crate) ddl_state: CatalogDdlState,
}

impl PolicyDef {
    pub(crate) const EMPTY: Self = Self {
        created_at: 0,
        name: SqlName::EMPTY,
        table: u16::MAX,
        command: PolicyCommandKind::All,
        permissive: true,
        definition: PolicyDefinition {
            roles: PolicyRoles::PUBLIC,
            using: None,
            with_check: None,
            dependencies: StoredQueryDependencies::EMPTY,
        },
        pending_definition: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> PolicyDefinition {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(self.definition, |pending| pending.definition)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PolicySpec {
    pub(crate) name: SqlName,
    pub(crate) table: usize,
    pub(crate) command: PolicyCommandKind,
    pub(crate) permissive: bool,
    pub(crate) definition: PolicyDefinition,
}

impl TriggerDef {
    pub(crate) const EMPTY: Self = Self {
        created_at: 0,
        name: SqlName::EMPTY,
        target: TriggerTarget::Table(u16::MAX),
        kind: TriggerKind::Ordinary,
        function: u16::MAX,
        timing: crate::sql::ast::TriggerTiming::Before,
        level: crate::sql::ast::TriggerLevel::Row,
        events: crate::sql::ast::TriggerEvents::from_bits(1).expect("INSERT event is valid"),
        update_columns: 0,
        transition_tables: TriggerTransitionTables::None,
        when: None,
        arguments: TriggerArguments::EMPTY,
        enabled: TriggerEnabled::Disabled,
        pending_definition: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn name_to(&self, txid: u32) -> SqlName {
        self.definition_to(txid).name
    }

    pub(crate) fn enabled_to(&self, txid: u32) -> TriggerEnabled {
        self.definition_to(txid).enabled
    }

    pub(crate) fn definition_to(&self, txid: u32) -> TriggerDefinition {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or_else(|| self.definition(), |pending| pending.definition)
    }

    fn definition(&self) -> TriggerDefinition {
        TriggerDefinition {
            name: self.name,
            kind: self.kind,
            function: self.function,
            timing: self.timing,
            level: self.level,
            events: self.events,
            update_columns: self.update_columns,
            transition_tables: self.transition_tables,
            when: self.when,
            arguments: self.arguments,
            enabled: self.enabled,
        }
    }

    fn apply_definition(&mut self, definition: TriggerDefinition) {
        self.name = definition.name;
        self.kind = definition.kind;
        self.function = definition.function;
        self.timing = definition.timing;
        self.level = definition.level;
        self.events = definition.events;
        self.update_columns = definition.update_columns;
        self.transition_tables = definition.transition_tables;
        self.when = definition.when;
        self.arguments = definition.arguments;
        self.enabled = definition.enabled;
    }

    fn effective_to(&self, txid: u32) -> Self {
        let mut trigger = *self;
        trigger.apply_definition(self.definition_to(txid));
        trigger.pending_definition = None;
        trigger
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingRoutineIdentity {
    pub txid: u32,
    pub schema: SqlName,
    pub name: SqlName,
}
pub(crate) const MAX_DOMAIN_CHECKS: usize = 4;

/// A `CREATE DOMAIN` type: a base type (with its typmod) plus optional
/// `NOT NULL`, `DEFAULT` and `CHECK (VALUE ...)` constraints, enforced when a
/// value is coerced into a column of the domain (or cast to it). Its
/// *existence* is transactional catalog state, mirroring [`SequenceDef`];
/// a domain carries no per-value state.
#[derive(Debug, Clone, Copy)]
pub struct DomainDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub ownership: Ownership,
    /// Immediate parent when this domain was declared over another domain.
    /// The value representation is flattened to `base`, but the parent chain
    /// remains explicit so every inherited NOT NULL/CHECK is enforced.
    pub base_domain: Option<UserTypeName>,
    /// Stable identity of a directly-declared enum or composite base.  The
    /// execution type carries a runtime slot, but a slot cannot survive a
    /// cold rebuild or a schema/name move.
    pub base_user_type: Option<UserTypeName>,
    pub base: ColType,
    /// The base type's atttypmod (e.g. `varchar(5)` → 9), applied to a value
    /// before the domain's own constraints.
    pub base_type_mod: i32,
    pub not_null: bool,
    pub default_expr: Option<StackStr<DEFAULT_EXPR_MAX>>,
    pub checks: [CheckConstraint; MAX_DOMAIN_CHECKS],
    pub n_checks: usize,
    pub(crate) pending_definition: Option<PendingDomainDefinition>,
    pub ddl_state: CatalogDdlState,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingDomainDefinition {
    pub txid: u32,
    pub spec: DomainSpec,
    pub identity: Option<PendingDomainIdentity>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingDomainIdentity {
    pub schema: SqlName,
    pub name: SqlName,
}

impl DomainDef {
    pub(crate) const EMPTY: Self = DomainDef {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        ownership: Ownership::BOOTSTRAP,
        base_domain: None,
        base_user_type: None,
        base: ColType::Bool,
        base_type_mod: -1,
        not_null: false,
        default_expr: None,
        checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
        n_checks: 0,
        pending_definition: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub fn checks(&self) -> &[CheckConstraint] {
        &self.checks[..self.n_checks]
    }

    pub(crate) fn definition_for(&self, txid: u32) -> Self {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(*self, |pending| Self {
                schema: pending
                    .identity
                    .map_or(self.schema, |identity| identity.schema),
                name: pending.identity.map_or(self.name, |identity| identity.name),
                base_domain: pending.spec.base_domain,
                base_user_type: pending.spec.base_user_type,
                base: pending.spec.base,
                base_type_mod: pending.spec.base_type_mod,
                not_null: pending.spec.not_null,
                default_expr: pending.spec.default_expr,
                checks: pending.spec.checks,
                n_checks: pending.spec.n_checks,
                pending_definition: None,
                ..*self
            })
    }
}

/// The validated parameters of a `CREATE DOMAIN` / `ALTER DOMAIN`, computed by
/// the executor and handed to storage (apart from the `live`/`pending` state).
#[derive(Debug, Clone, Copy)]
pub struct DomainSpec {
    pub base_domain: Option<UserTypeName>,
    pub base_user_type: Option<UserTypeName>,
    pub base: ColType,
    pub base_type_mod: i32,
    pub not_null: bool,
    pub default_expr: Option<StackStr<DEFAULT_EXPR_MAX>>,
    pub checks: [CheckConstraint; MAX_DOMAIN_CHECKS],
    pub n_checks: usize,
}

/// How many enum types may exist at once, and how many labels a single enum
/// may carry. Bounded conservatively: an `EnumDef` inlines its label array, so
/// the catalog's static footprint is `MAX_ENUMS * MAX_ENUM_LABELS * size_of::<EnumMember>()`.
pub(crate) const MAX_ENUMS: usize = 32;
pub(crate) const MAX_ENUM_LABELS: usize = 64;
/// Named composite types and their attributes are fixed at startup along with
/// every other catalog registry. A composite cannot be partially defined.
pub(crate) const MAX_COMPOSITES: usize = 32;
pub(crate) const MAX_COMPOSITE_FIELDS: usize = 16;

/// One member of an enum type: a label plus its sort key. Ordering among enum
/// values is by `sort` (PostgreSQL's `pg_enum.enumsortorder`), *not* by label
/// text, so `ALTER TYPE ... ADD VALUE ... BEFORE/AFTER` inserts a value between
/// two others by choosing a fractional sort key without renumbering the rest.
#[derive(Debug, Clone, Copy)]
pub struct EnumMember {
    pub label: SqlName,
    pub sort: f64,
}

impl EnumMember {
    pub(crate) const EMPTY: Self = EnumMember {
        label: SqlName::EMPTY,
        sort: 0.0,
    };
}

/// A `CREATE TYPE ... AS ENUM (...)` type: an ordered set of string labels. Its
/// *existence* and label set are transactional catalog state, mirroring
/// [`DomainDef`]; an enum value stored in a column carries its own label and
/// sort key inline, so decoding a row needs no catalog lookup.
#[derive(Debug, Clone, Copy)]
pub struct EnumDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub ownership: Ownership,
    pub members: [EnumMember; MAX_ENUM_LABELS],
    pub n_members: usize,
    pub(crate) pending_definition: Option<PendingEnumDefinition>,
    pub ddl_state: CatalogDdlState,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingEnumDefinition {
    pub txid: u32,
    pub schema: SqlName,
    pub name: SqlName,
    pub members: [EnumMember; MAX_ENUM_LABELS],
    pub n_members: usize,
}

impl EnumDef {
    pub(crate) const EMPTY: Self = EnumDef {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        ownership: Ownership::BOOTSTRAP,
        members: [EnumMember::EMPTY; MAX_ENUM_LABELS],
        n_members: 0,
        pending_definition: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub fn members(&self) -> &[EnumMember] {
        &self.members[..self.n_members]
    }

    /// The sort key of a label, or `None` if the label is not a member.
    pub fn sort_of(&self, label: &str) -> Option<f64> {
        self.members()
            .iter()
            .find(|m| m.label.as_str() == label)
            .map(|m| m.sort)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> Self {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(*self, |pending| Self {
                schema: pending.schema,
                name: pending.name,
                members: pending.members,
                n_members: pending.n_members,
                pending_definition: None,
                ..*self
            })
    }
}

/// The validated parameters of a `CREATE TYPE ... AS ENUM` / `ALTER TYPE`,
/// computed by the executor and handed to storage (apart from `live`/`pending`).
#[derive(Clone, Copy)]
pub struct EnumSpec {
    pub members: [EnumMember; MAX_ENUM_LABELS],
    pub n_members: usize,
}

/// One validated attribute of a named composite. `user_type` preserves the
/// stable name used to rebind catalog slots after recovery.
#[derive(Debug, Clone, Copy)]
pub struct CompositeFieldDef {
    /// Stable physical attribute number. Dropping an attribute never changes
    /// another attribute's number, which keeps old row values decodable.
    pub attribute_number: u16,
    pub name: SqlName,
    pub ctype: ColType,
    pub type_mod: i32,
    pub collation: Collation,
    pub user_type: Option<UserTypeName>,
    pub dropped: bool,
    pub not_null: bool,
}

impl CompositeFieldDef {
    pub(crate) const EMPTY: Self = Self {
        attribute_number: 0,
        name: SqlName::EMPTY,
        ctype: ColType::Bool,
        type_mod: -1,
        collation: Collation::None,
        user_type: None,
        dropped: true,
        not_null: false,
    };
}

/// A durable, named record layout. The type and field list are one catalog
/// object, so field access never needs to reparse a declaration string.
#[derive(Debug, Clone, Copy)]
pub struct CompositeDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub ownership: Ownership,
    pub fields: [CompositeFieldDef; MAX_COMPOSITE_FIELDS],
    pub n_fields: usize,
    pub(crate) pending_definition: Option<PendingCompositeDefinition>,
    pub ddl_state: CatalogDdlState,
}

/// A transaction-private composite layout. Attribute numbers are physical and
/// therefore remain stable across add/drop/rename operations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingCompositeDefinition {
    pub txid: u32,
    pub schema: SqlName,
    pub name: SqlName,
    pub fields: [CompositeFieldDef; MAX_COMPOSITE_FIELDS],
    pub n_fields: usize,
}

impl CompositeDef {
    pub(crate) const EMPTY: Self = Self {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        ownership: Ownership::BOOTSTRAP,
        fields: [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS],
        n_fields: 0,
        pending_definition: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub fn fields(&self) -> &[CompositeFieldDef] {
        &self.fields[..self.n_fields]
    }

    pub fn active_fields(&self) -> impl Iterator<Item = &CompositeFieldDef> {
        self.fields().iter().filter(|field| !field.dropped)
    }

    /// The field layout visible to one transaction. Returning a borrow into
    /// the catalog, rather than a copied definition, preserves the identity of
    /// transaction-private attribute names through Describe/RowDescription.
    pub fn fields_for(&self, txid: u32) -> &[CompositeFieldDef] {
        match self
            .pending_definition
            .as_ref()
            .filter(|pending| pending.txid == txid)
        {
            Some(pending) => &pending.fields[..pending.n_fields],
            None => self.fields(),
        }
    }

    pub fn active_fields_for(&self, txid: u32) -> impl Iterator<Item = &CompositeFieldDef> {
        self.fields_for(txid).iter().filter(|field| !field.dropped)
    }

    pub fn active_field_count(&self) -> usize {
        self.active_fields().count()
    }

    pub fn active_field(&self, index: usize) -> Option<&CompositeFieldDef> {
        self.active_fields().nth(index)
    }

    pub fn active_field_index(&self, name: &str) -> Option<usize> {
        self.fields()
            .iter()
            .position(|field| !field.dropped && field.name.as_str() == name)
    }

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> Self {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(*self, |pending| Self {
                schema: pending.schema,
                name: pending.name,
                fields: pending.fields,
                n_fields: pending.n_fields,
                pending_definition: None,
                ..*self
            })
    }
}

#[derive(Clone, Copy)]
pub struct CompositeSpec {
    pub fields: [CompositeFieldDef; MAX_COMPOSITE_FIELDS],
    pub n_fields: usize,
}

/// A sequence's declared integer type: it sets the default MIN/MAXVALUE and the
/// `pg_sequence.seqtypid` the catalog reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeqType {
    Smallint,
    Integer,
    Bigint,
}

impl SeqType {
    /// The type's representable range, bounding explicit MIN/MAXVALUE.
    pub fn bounds(self) -> (i64, i64) {
        match self {
            SeqType::Smallint => (i16::MIN as i64, i16::MAX as i64),
            SeqType::Integer => (i32::MIN as i64, i32::MAX as i64),
            SeqType::Bigint => (i64::MIN, i64::MAX),
        }
    }

    /// `pg_type` OID (`int2`/`int4`/`int8`), for `pg_sequence.seqtypid`.
    pub fn oid(self) -> i32 {
        match self {
            SeqType::Smallint => 21,
            SeqType::Integer => 23,
            SeqType::Bigint => 20,
        }
    }

    pub fn sql_name(self) -> &'static str {
        match self {
            SeqType::Smallint => "smallint",
            SeqType::Integer => "integer",
            SeqType::Bigint => "bigint",
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            SeqType::Smallint => 0,
            SeqType::Integer => 1,
            SeqType::Bigint => 2,
        }
    }

    pub fn from_u8(v: u8) -> SeqType {
        match v {
            0 => SeqType::Smallint,
            1 => SeqType::Integer,
            _ => SeqType::Bigint,
        }
    }
}

/// A named sequence generator. Its *existence* (`live`/`pending`) is
/// transactional catalog state, mirroring [`ViewDef`]. Ordinary value advances
/// survive `ROLLBACK`, while a staged definition owns a private value image
/// until commit. The cells let the allocation-free expression evaluator update
/// either image through a shared `&Storage` borrow.
#[derive(Clone)]
pub struct SequenceDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
    pub ownership: Ownership,
    pub data_type: SeqType,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub start_value: i64,
    pub cache: i64,
    pub cycle: bool,
    /// The table column whose lifetime owns this sequence. Names, rather than
    /// slots, keep the dependency stable across checkpoint restore and
    /// catalog-slot reuse.
    pub owner: Option<SequenceOwner>,
    /// The serial/identity column whose omitted values this sequence generates.
    /// This is deliberately separate from `owner`: PostgreSQL permits a serial
    /// sequence's OWNED BY dependency to be removed without changing the
    /// column's `nextval` default.
    pub generator_for: Option<SequenceOwner>,
    /// The last value handed out (meaningful only when `is_called`); on CREATE /
    /// RESTART it holds the start value with `is_called == false`, so the first
    /// `nextval` returns it unchanged (PostgreSQL's `setval(seq, start, false)`).
    pub last_value: Cell<i64>,
    pub is_called: Cell<bool>,
    /// The value state changed since it was last journaled; `commit_txn` writes a
    /// `SequenceAdvance` and clears it, regardless of whether the surrounding
    /// transaction committed (advances are non-transactional).
    pub dirty: Cell<bool>,
    pub(crate) pending_definition: Option<PendingSequenceDefinition>,
    pending_last_value: Cell<i64>,
    pending_is_called: Cell<bool>,
    pending_dirty: Cell<bool>,
    ddl_state: CatalogDdlState,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingSequenceDefinition {
    pub txid: u32,
    pub schema: SqlName,
    pub spec: SeqSpec,
    pub owner: Option<SequenceOwner>,
    pub generator_for: Option<SequenceOwner>,
    pub last_value: i64,
    pub is_called: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SequenceValueState {
    Committed {
        last_value: i64,
        is_called: bool,
        dirty: bool,
    },
    Pending {
        last_value: i64,
        is_called: bool,
        dirty: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceOwner {
    pub table_schema: SqlName,
    pub table: SqlName,
    pub column: SqlName,
}

fn rebind_sequence_column(
    link: Option<SequenceOwner>,
    old: &TableDef,
    new: &TableDef,
    column_mapping: &[Option<SqlName>; MAX_COLUMNS],
    require_generator: bool,
) -> Option<SequenceOwner> {
    let mut link = link?;
    if link.table_schema != old.schema || link.table != old.name {
        return Some(link);
    }
    let old_column = old.column_index(link.column.as_str())?;
    let target_name = column_mapping[old_column]?;
    let target = new
        .column_index(target_name.as_str())
        .filter(|&column| !require_generator || new.columns()[column].auto_increment)?;
    link.table_schema = new.schema;
    link.table = new.name;
    link.column = new.columns()[target].name;
    Some(link)
}

/// The tunable parameters of a sequence, computed and validated by the executor
/// from the CREATE/ALTER options, then handed to storage. Kept apart from the
/// live value state ([`SequenceDef`]'s `Cell` fields).
#[derive(Debug, Clone, Copy)]
pub struct SeqSpec {
    pub data_type: SeqType,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub start_value: i64,
    pub cache: i64,
    pub cycle: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SequenceAlteration {
    pub schema: SqlName,
    pub spec: SeqSpec,
    pub owner: Option<SequenceOwner>,
    pub generator_for: Option<SequenceOwner>,
    pub restart: Option<i64>,
}

impl SequenceDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> Self {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or_else(
                || self.clone(),
                |pending| Self {
                    schema: pending.schema,
                    data_type: pending.spec.data_type,
                    increment: pending.spec.increment,
                    min_value: pending.spec.min_value,
                    max_value: pending.spec.max_value,
                    start_value: pending.spec.start_value,
                    cache: pending.spec.cache,
                    cycle: pending.spec.cycle,
                    owner: pending.owner,
                    generator_for: pending.generator_for,
                    pending_definition: None,
                    ..self.clone()
                },
            )
    }

    /// Advances the generator and returns the next value, or the 2200H overflow
    /// error when a non-cycling sequence runs off its bound. Mutates value state
    /// through the `Cell` fields (a `&Storage` borrow is all the caller holds).
    pub fn next_value(&self) -> Result<i64, SqlError> {
        self.next_value_with(&self.last_value, &self.is_called, Some(&self.dirty))
    }

    fn next_value_with(
        &self,
        last_value: &Cell<i64>,
        is_called: &Cell<bool>,
        dirty: Option<&Cell<bool>>,
    ) -> Result<i64, SqlError> {
        if !is_called.get() {
            // First call after CREATE/RESTART yields the start value unchanged.
            is_called.set(true);
            if let Some(dirty) = dirty {
                dirty.set(true);
            }
            return Ok(last_value.get());
        }
        let current = last_value.get();
        let next = current.checked_add(self.increment);
        let value = if self.increment > 0 {
            match next {
                Some(n) if n <= self.max_value => n,
                _ if self.cycle => self.min_value,
                _ => {
                    return Err(sql_err!(
                        sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                        "nextval: reached maximum value of sequence \"{}\" ({})",
                        self.name.as_str(),
                        self.max_value
                    ));
                }
            }
        } else {
            match next {
                Some(n) if n >= self.min_value => n,
                _ if self.cycle => self.max_value,
                _ => {
                    return Err(sql_err!(
                        sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                        "nextval: reached minimum value of sequence \"{}\" ({})",
                        self.name.as_str(),
                        self.min_value
                    ));
                }
            }
        };
        last_value.set(value);
        if let Some(dirty) = dirty {
            dirty.set(true);
        }
        Ok(value)
    }

    /// Validates a `setval` target is within `[min, max]` (22003), without
    /// moving the generator.
    pub fn check_setval(&self, value: i64) -> Result<(), SqlError> {
        if value < self.min_value || value > self.max_value {
            return Err(sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "setval: value {} is out of bounds for sequence \"{}\" ({}..{})",
                value,
                self.name.as_str(),
                self.min_value,
                self.max_value
            ));
        }
        Ok(())
    }

    /// `setval`: positions the generator, validating the value is in range
    /// (22003). `is_called == false` makes the next `nextval` return `value`.
    pub fn set_value(&self, value: i64, is_called: bool) -> Result<i64, SqlError> {
        self.check_setval(value)?;
        self.last_value.set(value);
        self.is_called.set(is_called);
        self.dirty.set(true);
        Ok(value)
    }

    fn set_value_with(
        &self,
        value: i64,
        is_called: bool,
        last_value: &Cell<i64>,
        called: &Cell<bool>,
        dirty: Option<&Cell<bool>>,
    ) -> Result<i64, SqlError> {
        self.check_setval(value)?;
        last_value.set(value);
        called.set(is_called);
        if let Some(dirty) = dirty {
            dirty.set(true);
        }
        Ok(value)
    }
}

/// Maximum columns in an index key.
pub(crate) const MAX_INDEX_COLS: usize = 8;

fn hash_table_key(definition: &TableDef, values: &[Datum], columns: &[u16]) -> u64 {
    let mut collations = [Collation::None; MAX_INDEX_COLS];
    for (index, column) in columns.iter().enumerate() {
        collations[index] = definition.columns()[*column as usize].collation;
    }
    hash_key_collated(values, columns, &collations[..columns.len()])
}
/// Maximum stored source length of a partial-index membership predicate.
///
/// Predicate text is catalog data, not request-owned parser memory. A fixed
/// representation keeps the definition replayable without runtime growth.
pub(crate) const INDEX_PREDICATE_MAX: usize = CHECK_SQL_MAX;
/// Maximum canonical source length of one expression index key.
pub(crate) const INDEX_EXPRESSION_MAX: usize = CHECK_SQL_MAX;

/// Copies validated partial-index predicate source into its durable bounded
/// representation.
pub(crate) fn index_predicate_stackstr(
    predicate: &str,
) -> Result<StackStr<INDEX_PREDICATE_MAX>, SqlError> {
    let value = StackStr::from_str(predicate);
    if value.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index predicate exceeds {} bytes",
            INDEX_PREDICATE_MAX
        ));
    }
    Ok(value)
}

/// Copies validated expression-key source into its durable bounded form.
pub(crate) fn index_expression_stackstr(
    expression: &str,
) -> Result<StackStr<INDEX_EXPRESSION_MAX>, SqlError> {
    let value = StackStr::from_str(expression);
    if value.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index expression exceeds {} bytes",
            INDEX_EXPRESSION_MAX
        ));
    }
    Ok(value)
}

pub(crate) const MAX_TABLESPACES: usize = 64;
pub(crate) const TABLESPACE_LOCATION_MAX: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TablespaceOptions {
    pub random_page_cost: Option<TablespaceCost>,
    pub seq_page_cost: Option<TablespaceCost>,
    pub effective_io_concurrency: Option<i32>,
    pub maintenance_io_concurrency: Option<i32>,
}

impl TablespaceOptions {
    pub(crate) const DEFAULT: Self = Self {
        random_page_cost: None,
        seq_page_cost: None,
        effective_io_concurrency: None,
        maintenance_io_concurrency: None,
    };
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingTablespaceDefinition {
    pub txid: u32,
    pub name: SqlName,
    pub options: TablespaceOptions,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TablespaceDef {
    pub created_at: u64,
    pub name: SqlName,
    pub location: StackStr<TABLESPACE_LOCATION_MAX>,
    pub options: TablespaceOptions,
    pub ownership: Ownership,
    pub(crate) pending: Option<PendingTablespaceDefinition>,
    pub ddl_state: CatalogDdlState,
}

struct TablespaceImage {
    created_at: u64,
    name: SqlName,
    location: StackStr<TABLESPACE_LOCATION_MAX>,
    options: TablespaceOptions,
    owner: u16,
}

impl TablespaceDef {
    pub(crate) fn name_for(&self, txid: u32) -> SqlName {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.name, |pending| pending.name)
    }

    pub(crate) fn options_for(&self, txid: u32) -> TablespaceOptions {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.options, |pending| pending.options)
    }

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexStorageOptions {
    pub fillfactor: Option<u8>,
    pub deduplicate_items: Option<bool>,
}

impl IndexStorageOptions {
    pub const DEFAULT: Self = Self {
        fillfactor: None,
        deduplicate_items: None,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    Ordinary,
    Partitioned { valid: bool },
}

impl IndexKind {
    pub const fn valid(self) -> bool {
        match self {
            Self::Ordinary => true,
            Self::Partitioned { valid } => valid,
        }
    }

    pub const fn is_partitioned(self) -> bool {
        matches!(self, Self::Partitioned { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexMutableDefinition {
    pub tablespace: u16,
    pub options: IndexStorageOptions,
    pub statistics: [i16; MAX_INDEX_COLS],
    pub parent: Option<u16>,
    pub kind: IndexKind,
}

impl IndexMutableDefinition {
    pub const DEFAULT: Self = Self {
        tablespace: 0,
        options: IndexStorageOptions::DEFAULT,
        statistics: [-1; MAX_INDEX_COLS],
        parent: None,
        kind: IndexKind::Ordinary,
    };
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingIndexDefinition {
    pub txid: u32,
    pub definition: IndexMutableDefinition,
}

/// A named btree index over a table's columns.
#[derive(Clone, Copy)]
pub struct IndexDef {
    pub created_at: u64,
    /// The schema of both the index and its table (an index always lives in
    /// its table's schema).
    pub schema: SqlName,
    pub name: SqlName,
    pub(crate) pending_name: Option<PendingIndexName>,
    pub table: SqlName,
    pub ownership: Ownership,
    pub columns: [u16; MAX_INDEX_COLS],
    /// `Some` is a canonical expression key; `None` uses the resolved table
    /// column in the matching `columns` slot.
    pub expressions: [Option<StackStr<INDEX_EXPRESSION_MAX>>; MAX_INDEX_COLS],
    /// Covering columns are carried separately from key columns: they are
    /// readable from the index relation but cannot affect key semantics.
    pub include_columns: [u16; MAX_INDEX_COLS],
    pub collations: [Collation; MAX_INDEX_COLS],
    pub explicit_collations: [bool; MAX_INDEX_COLS],
    pub operator_classes: [Option<crate::sql::types::BtreeOperatorClass>; MAX_INDEX_COLS],
    pub descending: [bool; MAX_INDEX_COLS],
    pub nulls_first: [bool; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub n_include_cols: usize,
    /// `true` makes NULL key values collide in this unique index.
    pub nulls_not_distinct: bool,
    /// `None` denotes a full-table index. `Some` is the only representation
    /// of a partial index and is re-parsed before any membership decision.
    pub predicate: Option<StackStr<INDEX_PREDICATE_MAX>>,
    pub unique: bool,
    pub mutable: IndexMutableDefinition,
    pub(crate) pending_definition: Option<PendingIndexDefinition>,
    pub ddl_state: CatalogDdlState,
}

impl IndexDef {
    /// Whether `txid` sees this index exist.
    pub fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub fn name_for(&self, txid: u32) -> SqlName {
        self.pending_name
            .filter(|pending| pending.txid == txid)
            .map_or(self.name, |pending| pending.name)
    }

    pub fn mutable_for(&self, txid: u32) -> IndexMutableDefinition {
        self.pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(self.mutable, |pending| pending.definition)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingIndexName {
    pub txid: u32,
    pub name: SqlName,
}

/// How many schemas may exist at once, including the built-in "public".
pub(crate) const MAX_SCHEMAS: usize = 32;

pub(crate) const MAX_EXTENSIONS: usize = 64;
pub(crate) const MAX_EXTENSION_DEPENDENCIES: usize = 512;
pub(crate) const MAX_EXTENSION_REQUIRES: usize = 8;
pub(crate) const MAX_EXTENSION_CONFIG_RELATIONS: usize = 36;
pub(crate) const EXTENSION_CONFIG_CONDITION_BYTES: usize = 1024;

/// A version name accepted by PostgreSQL's extension file grammar. It is
/// distinct from an SQL identifier: dots and internal hyphens are ordinary
/// version characters, while path separators and update delimiters are not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtensionVersion(SqlName);

impl ExtensionVersion {
    pub(crate) fn parse(value: &str) -> Result<Self, SqlError> {
        if value.is_empty()
            || value.starts_with('-')
            || value.ends_with('-')
            || value.contains("--")
            || value.contains(['/', '\\'])
        {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid extension version name: {}",
                value
            ));
        }
        Ok(Self(SqlName::parse(value)?))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtensionDefinition {
    pub txid: u32,
    pub namespace: u16,
    pub relocatable: bool,
    pub version: ExtensionVersion,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtensionDef {
    pub created_at: u64,
    pub name: SqlName,
    pub namespace: u16,
    pub relocatable: bool,
    pub version: ExtensionVersion,
    pub ownership: Ownership,
    pub pending: Option<PendingExtensionDefinition>,
    pub ddl_state: CatalogDdlState,
}

impl ExtensionDef {
    const EMPTY: Self = Self {
        created_at: 0,
        name: SqlName::EMPTY,
        namespace: 0,
        relocatable: false,
        version: ExtensionVersion(SqlName::EMPTY),
        ownership: Ownership::BOOTSTRAP,
        pending: None,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_to(&self, txid: u32) -> (u16, bool, ExtensionVersion) {
        self.pending.filter(|pending| pending.txid == txid).map_or(
            (self.namespace, self.relocatable, self.version),
            |pending| (pending.namespace, pending.relocatable, pending.version),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionDependencyKind {
    Member,
    Automatic,
    Required,
}

impl ExtensionDependencyKind {
    pub(crate) const fn to_u8(self) -> u8 {
        match self {
            Self::Member => 0,
            Self::Automatic => 1,
            Self::Required => 2,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Member),
            1 => Some(Self::Automatic),
            2 => Some(Self::Required),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtensionDependency {
    pub txid: u32,
    pub exists: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtensionDependency {
    pub extension: u16,
    pub object: AccessObject,
    pub kind: ExtensionDependencyKind,
    pub live: bool,
    pub pending: Option<PendingExtensionDependency>,
}

impl ExtensionDependency {
    const EMPTY: Self = Self {
        extension: u16::MAX,
        object: AccessObject {
            class: AccessClass::Table,
            slot: 0,
        },
        kind: ExtensionDependencyKind::Member,
        live: false,
        pending: None,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.live, |pending| pending.exists)
    }
}

/// A configuration relation has exactly one of the two relation kinds
/// PostgreSQL accepts at `pg_extension_config_dump`'s boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionConfigRelation {
    Table(u16),
    Sequence(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionConfigRelationKind {
    Table,
    Sequence,
}

impl ExtensionConfigRelationKind {
    pub(crate) const fn to_u8(self) -> u8 {
        match self {
            Self::Table => 0,
            Self::Sequence => 1,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Table),
            1 => Some(Self::Sequence),
            _ => None,
        }
    }
}

impl ExtensionConfigRelation {
    pub(crate) const fn kind(self) -> ExtensionConfigRelationKind {
        match self {
            Self::Table(_) => ExtensionConfigRelationKind::Table,
            Self::Sequence(_) => ExtensionConfigRelationKind::Sequence,
        }
    }
    pub(crate) const fn access_object(self) -> AccessObject {
        match self {
            Self::Table(slot) => AccessObject {
                class: AccessClass::Table,
                slot,
            },
            Self::Sequence(slot) => AccessObject {
                class: AccessClass::Sequence,
                slot,
            },
        }
    }

    pub(crate) const fn from_access_object(object: AccessObject) -> Option<Self> {
        match object.class {
            AccessClass::Table => Some(Self::Table(object.slot)),
            AccessClass::Sequence => Some(Self::Sequence(object.slot)),
            _ => None,
        }
    }
}

pub(crate) type ExtensionConfigCondition = StackStr<EXTENSION_CONFIG_CONDITION_BYTES>;

pub(crate) fn extension_config_condition(
    value: &str,
) -> Result<ExtensionConfigCondition, SqlError> {
    let condition = StackStr::from_str(value);
    if condition.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "extension configuration condition exceeds {} bytes",
            EXTENSION_CONFIG_CONDITION_BYTES
        ));
    }
    Ok(condition)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtensionConfig {
    pub txid: u32,
    pub exists: bool,
    pub condition: ExtensionConfigCondition,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtensionConfig {
    pub extension: u16,
    pub ordinal: u16,
    pub relation: ExtensionConfigRelation,
    pub condition: ExtensionConfigCondition,
    pub live: bool,
    pub pending: Option<PendingExtensionConfig>,
}

impl ExtensionConfig {
    const EMPTY: Self = Self {
        extension: u16::MAX,
        ordinal: 0,
        relation: ExtensionConfigRelation::Table(0),
        condition: ExtensionConfigCondition::new(),
        live: false,
        pending: None,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.live, |pending| pending.exists)
    }

    pub(crate) fn condition_to(&self, txid: u32) -> ExtensionConfigCondition {
        self.pending
            .filter(|pending| pending.txid == txid && pending.exists)
            .map_or(self.condition, |pending| pending.condition)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionPackageCode {
    Sql,
    NativeLibrary,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtensionPackage {
    pub name: SqlName,
    pub default_version: Option<ExtensionVersion>,
    pub schema: Option<SqlName>,
    pub relocatable: bool,
    pub superuser: bool,
    pub trusted: bool,
    pub code: ExtensionPackageCode,
    pub comment: StackStr<COMMENT_MAX>,
    pub requires: [SqlName; MAX_EXTENSION_REQUIRES],
    pub require_count: u8,
    pub no_relocate: [SqlName; MAX_EXTENSION_REQUIRES],
    pub no_relocate_count: u8,
}

impl ExtensionPackage {
    pub(crate) const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        default_version: None,
        schema: None,
        relocatable: false,
        superuser: true,
        trusted: false,
        code: ExtensionPackageCode::Sql,
        comment: StackStr::new(),
        requires: [SqlName::EMPTY; MAX_EXTENSION_REQUIRES],
        require_count: 0,
        no_relocate: [SqlName::EMPTY; MAX_EXTENSION_REQUIRES],
        no_relocate_count: 0,
    };

    pub(crate) fn requires(&self) -> &[SqlName] {
        &self.requires[..self.require_count as usize]
    }

    pub(crate) fn no_relocate(&self) -> &[SqlName] {
        &self.no_relocate[..self.no_relocate_count as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionPackageSource {
    Configured,
    Durable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtensionScript {
    pub package: u16,
    pub from: Option<ExtensionVersion>,
    pub to: ExtensionVersion,
    pub offset: u32,
    pub length: u32,
    pub effective: ExtensionPackage,
}

struct ParsedExtensionControl {
    package: ExtensionPackage,
    directory: Option<String>,
    specified: u16,
}

fn extension_control_value(value: &str) -> Result<&str, SqlError> {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            return Ok(&value[1..value.len() - 1]);
        }
    }
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid extension control value"
        ));
    }
    Ok(value)
}

fn extension_control_bool(value: &str, parameter: &str) -> Result<bool, SqlError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid value for extension control parameter \"{}\": {}",
            parameter,
            value
        )),
    }
}

fn extension_control_names(
    value: &str,
    output: &mut [SqlName; MAX_EXTENSION_REQUIRES],
    parameter: &str,
) -> Result<u8, SqlError> {
    let mut count = 0usize;
    for raw in value.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension control parameter \"{}\" exceeds {} entries",
                parameter,
                output.len()
            ));
        }
        let parsed = SqlName::parse(name)?;
        if output[..count].contains(&parsed) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "extension control parameter \"{}\" repeats \"{}\"",
                parameter,
                name
            ));
        }
        output[count] = parsed;
        count += 1;
    }
    Ok(count as u8)
}

fn strip_extension_control_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, byte) in line.bytes().enumerate() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), close) if open == close => quote = None,
            (None, b'#') => return &line[..index],
            _ => {}
        }
    }
    line
}

fn parse_extension_control(name: SqlName, text: &str) -> Result<ParsedExtensionControl, SqlError> {
    let mut package = ExtensionPackage {
        name,
        ..ExtensionPackage::EMPTY
    };
    let mut directory = None;
    let mut seen = [SqlName::EMPTY; 12];
    let mut seen_count = 0usize;
    let mut specified = 0u16;
    for (line_index, raw) in text.lines().enumerate() {
        let line = strip_extension_control_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((parameter, raw_value)) = line.split_once('=') else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid extension control line {} for \"{}\"",
                line_index + 1,
                name.as_str()
            ));
        };
        let parameter = parameter.trim().to_ascii_lowercase();
        let parameter_name = SqlName::parse(&parameter)?;
        if seen[..seen_count].contains(&parameter_name) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "extension control parameter \"{}\" specified more than once",
                parameter
            ));
        }
        if seen_count == seen.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many extension control parameters"
            ));
        }
        seen[seen_count] = parameter_name;
        seen_count += 1;
        let value = extension_control_value(raw_value)?;
        match parameter.as_str() {
            "directory" => {
                specified |= 1 << 0;
                directory = Some(value.to_string());
            }
            "default_version" => {
                specified |= 1 << 1;
                package.default_version = Some(ExtensionVersion::parse(value)?)
            }
            "comment" => {
                specified |= 1 << 2;
                package.comment = StackStr::from_str(value);
                if package.comment.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "extension comment exceeds {} bytes",
                        COMMENT_MAX
                    ));
                }
            }
            "encoding" => {
                specified |= 1 << 3;
                if !value.eq_ignore_ascii_case("UTF8") && !value.eq_ignore_ascii_case("UTF-8") {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "extension \"{}\" uses unsupported script encoding \"{}\"",
                        name.as_str(),
                        value
                    ));
                }
            }
            "module_pathname" => {
                specified |= 1 << 4;
                package.code = ExtensionPackageCode::NativeLibrary;
            }
            "requires" => {
                specified |= 1 << 5;
                package.require_count =
                    extension_control_names(value, &mut package.requires, "requires")?
            }
            "no_relocate" => {
                specified |= 1 << 6;
                package.no_relocate_count =
                    extension_control_names(value, &mut package.no_relocate, "no_relocate")?
            }
            "superuser" => {
                specified |= 1 << 7;
                package.superuser = extension_control_bool(value, &parameter)?;
            }
            "trusted" => {
                specified |= 1 << 8;
                package.trusted = extension_control_bool(value, &parameter)?;
            }
            "relocatable" => {
                specified |= 1 << 9;
                package.relocatable = extension_control_bool(value, &parameter)?;
            }
            "schema" => {
                specified |= 1 << 10;
                package.schema = Some(SqlName::parse(value)?);
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized extension control parameter \"{}\"",
                    parameter
                ));
            }
        }
    }
    if package.relocatable && package.schema.is_some() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "extension \"{}\" cannot be both relocatable and schema-bound",
            name.as_str()
        ));
    }
    for dependency in package.no_relocate() {
        if !package.requires().contains(dependency) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "extension \"{}\" names \"{}\" in no_relocate but not requires",
                name.as_str(),
                dependency.as_str()
            ));
        }
    }
    Ok(ParsedExtensionControl {
        package,
        directory,
        specified,
    })
}

fn merge_extension_control(
    primary: ExtensionPackage,
    secondary: ParsedExtensionControl,
) -> Result<ExtensionPackage, SqlError> {
    if secondary.specified & ((1 << 0) | (1 << 1)) != 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "secondary extension control files cannot set directory or default_version"
        ));
    }
    let mut effective = primary;
    let written = secondary.package;
    if secondary.specified & (1 << 2) != 0 {
        effective.comment = written.comment;
    }
    if secondary.specified & (1 << 4) != 0 {
        effective.code = written.code;
    }
    if secondary.specified & (1 << 5) != 0 {
        effective.requires = written.requires;
        effective.require_count = written.require_count;
    }
    if secondary.specified & (1 << 6) != 0 {
        effective.no_relocate = written.no_relocate;
        effective.no_relocate_count = written.no_relocate_count;
    }
    if secondary.specified & (1 << 7) != 0 {
        effective.superuser = written.superuser;
    }
    if secondary.specified & (1 << 8) != 0 {
        effective.trusted = written.trusted;
    }
    if secondary.specified & (1 << 9) != 0 {
        effective.relocatable = written.relocatable;
    }
    if secondary.specified & (1 << 10) != 0 {
        effective.schema = written.schema;
    }
    if effective.relocatable && effective.schema.is_some() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "extension \"{}\" cannot be both relocatable and schema-bound",
            effective.name.as_str()
        ));
    }
    for dependency in effective.no_relocate() {
        if !effective.requires().contains(dependency) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "extension \"{}\" has an invalid no_relocate dependency",
                effective.name.as_str()
            ));
        }
    }
    Ok(effective)
}

/// A named schema (namespace for tables, views and indexes).
#[derive(Clone, Copy)]
pub struct SchemaDef {
    pub name: SqlName,
    pub ownership: Ownership,
    ddl_state: CatalogDdlState,
}

impl SchemaDef {
    /// Whether `txid` sees this schema exist.
    pub fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }
}

/// Transactional owner metadata shared by every user-created catalog object.
/// Role slots, rather than names, make ownership survive role renames; object
/// slots make it survive object renames. WAL and manifests resolve the slots
/// from durable names at replay boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ownership {
    pub owner: u16,
    pub pending: Option<PendingOwnership>,
}

impl Ownership {
    pub const BOOTSTRAP: Self = Self {
        owner: 0,
        pending: None,
    };

    pub fn owner_to(self, txid: u32) -> u16 {
        match self.pending {
            Some(pending) if pending.txid == txid => pending.owner,
            _ => self.owner,
        }
    }

    /// A committed WAL image has no transaction-private ownership overlay.
    pub const fn committed(self) -> Self {
        Self {
            owner: match self.pending {
                Some(pending) => pending.owner,
                None => self.owner,
            },
            pending: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingOwnership {
    pub txid: u32,
    pub owner: u16,
}

/// Stable object classes used by ownership and ACL state. Relation covers
/// tables, plain views, and materialized views; their slots are disambiguated
/// by the dedicated view classes because each registry has its own slot space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum AccessClass {
    Table = 0,
    View = 1,
    MaterializedView = 2,
    Sequence = 3,
    Schema = 4,
    Domain = 5,
    Enum = 6,
    Index = 7,
    Routine = 8,
    Composite = 9,
    Tablespace = 10,
    Statistics = 11,
    Extension = 12,
    Trigger = 13,
}

/// Object classes addressable by ALTER DEFAULT PRIVILEGES.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DefaultPrivilegeClass {
    Table = 0,
    Sequence = 1,
    Function = 2,
    Type = 3,
    Schema = 4,
}

impl DefaultPrivilegeClass {
    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Table,
            1 => Self::Sequence,
            2 => Self::Function,
            3 => Self::Type,
            4 => Self::Schema,
            _ => return None,
        })
    }

    pub(crate) const fn all_privileges(self) -> PrivilegeSet {
        match self {
            Self::Table => PrivilegeSet::TABLE_ALL,
            Self::Sequence => PrivilegeSet::SEQUENCE_ALL,
            Self::Function => PrivilegeSet::FUNCTION_ALL,
            Self::Type => PrivilegeSet::TYPE_ALL,
            Self::Schema => PrivilegeSet::SCHEMA_ALL,
        }
    }

    pub(crate) const fn default_public_privileges(self) -> PrivilegeSet {
        match self {
            Self::Function => PrivilegeSet::EXECUTE,
            Self::Type => PrivilegeSet::USAGE,
            Self::Table | Self::Sequence | Self::Schema => PrivilegeSet::NONE,
        }
    }
}

impl AccessClass {
    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Table,
            1 => Self::View,
            2 => Self::MaterializedView,
            3 => Self::Sequence,
            4 => Self::Schema,
            5 => Self::Domain,
            6 => Self::Enum,
            7 => Self::Index,
            8 => Self::Routine,
            9 => Self::Composite,
            10 => Self::Tablespace,
            11 => Self::Statistics,
            12 => Self::Extension,
            13 => Self::Trigger,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccessObject {
    pub class: AccessClass,
    pub slot: u16,
}

/// PostgreSQL object privileges represented as a compact set. Unsupported
/// object/privilege combinations are rejected before they reach storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrivilegeSet(pub u16);

impl PrivilegeSet {
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const SELECT: Self = Self(1 << 0);
    pub(crate) const INSERT: Self = Self(1 << 1);
    pub(crate) const UPDATE: Self = Self(1 << 2);
    pub(crate) const DELETE: Self = Self(1 << 3);
    pub(crate) const TRUNCATE: Self = Self(1 << 4);
    pub(crate) const REFERENCES: Self = Self(1 << 5);
    pub(crate) const TRIGGER: Self = Self(1 << 6);
    pub(crate) const USAGE: Self = Self(1 << 7);
    pub(crate) const CREATE: Self = Self(1 << 8);
    pub(crate) const EXECUTE: Self = Self(1 << 9);
    pub(crate) const MAINTAIN: Self = Self(1 << 10);

    pub(crate) const TABLE_ALL: Self = Self(
        Self::SELECT.0
            | Self::INSERT.0
            | Self::UPDATE.0
            | Self::DELETE.0
            | Self::TRUNCATE.0
            | Self::REFERENCES.0
            | Self::TRIGGER.0
            | Self::MAINTAIN.0,
    );
    pub(crate) const SEQUENCE_ALL: Self = Self(Self::USAGE.0 | Self::SELECT.0 | Self::UPDATE.0);
    pub(crate) const SCHEMA_ALL: Self = Self(Self::USAGE.0 | Self::CREATE.0);
    pub(crate) const TYPE_ALL: Self = Self::USAGE;
    pub(crate) const FUNCTION_ALL: Self = Self::EXECUTE;

    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

pub(crate) const PUBLIC_ROLE: u16 = u16::MAX;
pub(crate) const MAX_ACL_ENTRIES: usize = 512;
pub(crate) const MAX_DEFAULT_ACL_ENTRIES: usize = 256;
pub(crate) const DEFAULT_ACL_ALL_SCHEMAS: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingAcl {
    pub txid: u32,
    pub grantee: u16,
    pub grantor: u16,
    pub privileges: PrivilegeSet,
    pub grant_options: PrivilegeSet,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AclEntry {
    pub object: AccessObject,
    pub grantee: u16,
    pub grantor: u16,
    pub privileges: PrivilegeSet,
    pub grant_options: PrivilegeSet,
    pub live: bool,
    pub pending: Option<PendingAcl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingDefaultAcl {
    pub txid: u32,
    /// A zero-valued entry can be meaningful: it is the tombstone produced by
    /// revoking a built-in PUBLIC default from types or functions.
    pub defined: bool,
    pub privileges: PrivilegeSet,
    pub grant_options: PrivilegeSet,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DefaultAclKey {
    pub owner: u16,
    pub schema: u16,
    pub class: DefaultPrivilegeClass,
    pub grantee: u16,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DefaultAclEntry {
    pub owner: u16,
    /// DEFAULT_ACL_ALL_SCHEMAS denotes the global default; otherwise this is a
    /// stable schema slot.
    pub schema: u16,
    pub class: DefaultPrivilegeClass,
    pub grantee: u16,
    pub defined: bool,
    pub privileges: PrivilegeSet,
    pub grant_options: PrivilegeSet,
    pub pending: Option<PendingDefaultAcl>,
}

/// Startup-bounded PostgreSQL role catalog. Role metadata is catalog state and
/// therefore follows the same transaction/WAL/manifest lifecycle as schemas;
/// it never lives in a process-global authentication side table.
pub(crate) const MAX_ROLES: usize = 64;
pub(crate) const MAX_ROLE_MEMBERSHIPS: usize = 256;
pub(crate) const ROLE_PASSWORD_MAX: usize = 128;
pub(crate) const ROLE_VALID_UNTIL_MAX: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct RolePassword {
    pub salt: [u8; 16],
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
    pub iterations: u32,
}

impl RolePassword {
    pub const EMPTY: Self = Self {
        salt: [0; 16],
        stored_key: [0; 32],
        server_key: [0; 32],
        iterations: 0,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct RoleAttributes {
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_database: bool,
    pub can_login: bool,
    pub replication: bool,
    pub bypass_row_level_security: bool,
    pub connection_limit: i32,
    pub password: RolePassword,
    pub has_password: bool,
    pub valid_until: StackStr<ROLE_VALID_UNTIL_MAX>,
    pub has_valid_until: bool,
}

impl RoleAttributes {
    pub const ORDINARY: Self = Self {
        superuser: false,
        inherit: true,
        create_role: false,
        create_database: false,
        can_login: false,
        replication: false,
        bypass_row_level_security: false,
        connection_limit: -1,
        password: RolePassword::EMPTY,
        has_password: false,
        valid_until: StackStr::new(),
        has_valid_until: false,
    };

    pub const BOOTSTRAP: Self = Self {
        superuser: true,
        inherit: true,
        create_role: true,
        create_database: true,
        can_login: true,
        replication: true,
        bypass_row_level_security: true,
        ..Self::ORDINARY
    };
}

#[derive(Clone, Copy, Debug)]
pub struct PendingRole {
    pub txid: u32,
    pub exists: bool,
    pub name: SqlName,
    pub attributes: RoleAttributes,
}

#[derive(Clone, Copy, Debug)]
pub struct RoleDef {
    pub name: SqlName,
    pub attributes: RoleAttributes,
    pub live: bool,
    pub pending: Option<PendingRole>,
}

impl RoleDef {
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(pending) if pending.txid == txid => pending.exists,
            _ => self.live,
        }
    }

    pub fn attributes_to(&self, txid: u32) -> RoleAttributes {
        match self.pending {
            Some(pending) if pending.txid == txid && pending.exists => pending.attributes,
            _ => self.attributes,
        }
    }

    pub fn name_to(&self, txid: u32) -> SqlName {
        match self.pending {
            Some(pending) if pending.txid == txid && pending.exists => pending.name,
            _ => self.name,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RoleMembershipOptions {
    pub admin: bool,
    pub inherit: bool,
    pub set: bool,
}

impl RoleMembershipOptions {
    pub const DEFAULT: Self = Self {
        admin: false,
        inherit: true,
        set: true,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct PendingRoleMembership {
    pub txid: u32,
    pub exists: bool,
    pub options: RoleMembershipOptions,
}

#[derive(Clone, Copy, Debug)]
pub struct RoleMembership {
    pub role: u16,
    pub member: u16,
    pub grantor: u16,
    pub options: RoleMembershipOptions,
    pub live: bool,
    pub pending: Option<PendingRoleMembership>,
}

impl RoleMembership {
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(pending) if pending.txid == txid => pending.exists,
            _ => self.live,
        }
    }

    pub fn options_to(&self, txid: u32) -> RoleMembershipOptions {
        match self.pending {
            Some(pending) if pending.txid == txid && pending.exists => pending.options,
            _ => self.options,
        }
    }
}

/// How many distinct objects may carry a comment at once.
pub(crate) const MAX_COMMENTS: usize = 64;

/// Copies comment text into a fixed buffer, or a loud error if it is longer
/// than [`COMMENT_MAX`] (never a silent truncation).
pub(crate) fn comment_stackstr(text: &str) -> Result<StackStr<COMMENT_MAX>, SqlError> {
    use core::fmt::Write;
    let mut stored = StackStr::<COMMENT_MAX>::new();
    let _ = write!(stored, "{text}");
    if stored.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "comment exceeds {} bytes",
            COMMENT_MAX
        ));
    }
    Ok(stored)
}

/// The longest comment text we store. A longer `COMMENT ON` is a loud
/// `PROGRAM_LIMIT_EXCEEDED`, never a silent truncation (static-memory rule).
/// Sized so a `DdlUndo::CommentSet` (which carries a prior comment inline)
/// stays within the transaction undo log's largest pre-existing entry.
pub(crate) const COMMENT_MAX: usize = 192;

/// Which catalog a comment's object lives in. `Relation` covers every
/// `pg_class` object (table, view, materialized view, index, sequence — and a
/// column, via a non-zero `subid`); `Schema` covers `pg_namespace`; `Type`
/// covers built-in and user-defined rows of `pg_type`; `Tablespace` covers
/// shared `pg_tablespace` objects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommentClass {
    Relation,
    Schema,
    Type,
    Tablespace,
    Extension,
    Trigger,
}

impl CommentClass {
    pub fn to_u8(self) -> u8 {
        match self {
            CommentClass::Relation => 0,
            CommentClass::Schema => 1,
            CommentClass::Type => 2,
            CommentClass::Tablespace => 3,
            CommentClass::Extension => 4,
            CommentClass::Trigger => 5,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => CommentClass::Relation,
            1 => CommentClass::Schema,
            2 => CommentClass::Type,
            3 => CommentClass::Tablespace,
            4 => CommentClass::Extension,
            5 => CommentClass::Trigger,
            _ => return None,
        })
    }
}

/// The kind of a relation, for `COMMENT ON` kind-checking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoredRelKind {
    Table,
    View,
    Matview,
    Index,
    Sequence,
}

/// A transaction's uncommitted comment write overlaying the committed `live`
/// text (catalog MVCC, mirroring `Table`'s row overlay): `text == None` is an
/// uncommitted removal.
#[derive(Clone, Copy, Debug)]
pub struct PendingComment {
    pub txid: u32,
    pub text: Option<StackStr<COMMENT_MAX>>,
}

#[derive(Clone, Copy, Debug)]
struct PendingCommentIdentity {
    txid: u32,
    name: SqlName,
}

/// A comment attached to a database object, keyed by `(class, schema, name,
/// subid)` — restart-stable, since object OIDs derive from catalog slots but
/// names do not. `subid` is 0 for a relation or schema and the 1-based column
/// number for a column comment. `live` is the committed text (`None` once
/// removed), `pending` the owning transaction's uncommitted overlay.
#[derive(Clone, Copy, Debug)]
pub struct CommentEntry {
    pub used: bool,
    pub class: CommentClass,
    pub schema: SqlName,
    pub name: SqlName,
    pub subid: u32,
    pub live: Option<StackStr<COMMENT_MAX>>,
    pub pending: Option<PendingComment>,
    pending_identity: Option<PendingCommentIdentity>,
}

impl CommentEntry {
    fn empty() -> Self {
        Self {
            used: false,
            class: CommentClass::Relation,
            schema: SqlName::EMPTY,
            name: SqlName::EMPTY,
            subid: 0,
            live: None,
            pending: None,
            pending_identity: None,
        }
    }

    fn matches(&self, class: CommentClass, schema: &str, name: &str, subid: u32) -> bool {
        self.used
            && self.class == class
            && self.subid == subid
            && self.name.as_str() == name
            && self.schema.as_str() == schema
    }

    fn matches_to(
        &self,
        class: CommentClass,
        schema: &str,
        name: &str,
        subid: u32,
        txid: u32,
    ) -> bool {
        self.used
            && self.class == class
            && self.subid == subid
            && self.schema.as_str() == schema
            && self
                .pending_identity
                .filter(|identity| identity.txid == txid)
                .map_or(self.name, |identity| identity.name)
                .as_str()
                == name
    }

    /// The text `txid` sees: its own uncommitted overlay when present, else the
    /// committed value.
    fn visible_text(&self, txid: u32) -> Option<&str> {
        match &self.pending {
            Some(p) if p.txid == txid => p.text.as_ref().map(StackStr::as_str),
            _ => self.live.as_ref().map(StackStr::as_str),
        }
    }
}

/// One element of the effective search path: a live schema slot, or the
/// implicit/explicit `pg_catalog` position.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PathEntry {
    Schema(u16),
    Catalog,
}

/// How many schemas a search_path may name.
pub(crate) const MAX_PATH_ENTRIES: usize = 16;

/// The effective search path of the running statement: the visible schemas
/// the session's `search_path` names, in order, with `pg_catalog` interleaved
/// at its explicit position or implicitly first. Set by the engine before each
/// statement (and swapped while a view body — which resolves under its
/// creator's path — expands); every name resolution reads it.
#[derive(Clone, Copy)]
pub struct PathContext {
    entries: [PathEntry; MAX_PATH_ENTRIES],
    n: usize,
    /// Whether the path names pg_catalog explicitly. An explicit first
    /// pg_catalog is the creation target (which then fails with permission
    /// denied, as PostgreSQL); the implicit one never is.
    explicit_catalog: bool,
}

impl PathContext {
    /// A path of exactly `public` (slot 0) with implicit pg_catalog, the
    /// state before any session context is computed (journal replay, tests).
    pub const fn public_only() -> Self {
        let mut entries = [PathEntry::Catalog; MAX_PATH_ENTRIES];
        entries[0] = PathEntry::Catalog;
        entries[1] = PathEntry::Schema(0);
        PathContext {
            entries,
            n: 2,
            explicit_catalog: false,
        }
    }

    pub fn entries(&self) -> &[PathEntry] {
        &self.entries[..self.n]
    }

    pub fn explicit_catalog(&self) -> bool {
        self.explicit_catalog
    }

    /// The first schema entry: creation target and `current_schema()`.
    pub fn first_schema(&self) -> Option<u16> {
        self.entries().iter().find_map(|e| match e {
            PathEntry::Schema(slot) => Some(*slot),
            PathEntry::Catalog => None,
        })
    }
}

/// What a relation name resolved to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResolvedRelation {
    Table(usize),
    View(usize),
    /// A `pg_catalog` / `information_schema` relation (synthesized rows).
    Catalog,
}

#[derive(Clone, Copy)]
struct TableLock {
    owner: u32,
    table: u32,
    /// Acquisition sequence for each table-lock mode. PostgreSQL permits a
    /// transaction to hold multiple incomparable modes on one relation.
    modes: [u64; 8],
}

impl TableLock {
    fn mask(&self) -> u8 {
        self.modes
            .iter()
            .enumerate()
            .fold(0, |mask, (index, sequence)| {
                mask | u8::from(*sequence != 0) << index
            })
    }
}

#[derive(Clone, Copy)]
struct ReplayTableRewrite {
    table: usize,
    preserve_rows: bool,
    column_mapping: [u16; MAX_COLUMNS],
}

pub struct Storage {
    pub heap: RowHeap,
    tables: FixedVec<Table>,
    pending_table_defs: FixedVec<PendingTableDefSlot>,
    pending_table_statistics: FixedVec<PendingTableStatisticsSlot>,
    views: FixedVec<ViewDef>,
    routines: FixedVec<RoutineDef>,
    routine_dependencies: FixedVec<StoredQueryDependencies>,
    pending_routine_dependencies: FixedVec<PendingRoutineDependencies>,
    triggers: FixedVec<TriggerDef>,
    partition_trigger_states: FixedVec<PartitionTriggerState>,
    policies: FixedVec<PolicyDef>,
    extended_statistics: FixedVec<ExtendedStatisticsDef>,
    pending_extended_statistics_data: FixedVec<PendingExtendedStatisticsDataSlot>,
    publications: FixedVec<PublicationDef>,
    replication_slots: FixedVec<ReplicationSlotDef>,
    subscriptions: FixedVec<SubscriptionDef>,
    subscription_relations: FixedVec<SubscriptionRelation>,
    view_dependencies: FixedVec<StoredQueryDependencies>,
    matviews: FixedVec<MatviewDef>,
    matview_dependencies: FixedVec<StoredQueryDependencies>,
    sequences: FixedVec<SequenceDef>,
    domains: FixedVec<DomainDef>,
    enums: FixedVec<EnumDef>,
    composites: FixedVec<CompositeDef>,
    indexes: FixedVec<IndexDef>,
    tablespaces: FixedVec<TablespaceDef>,
    schemas: FixedVec<SchemaDef>,
    extensions: FixedVec<ExtensionDef>,
    extension_dependencies: FixedVec<ExtensionDependency>,
    extension_configs: FixedVec<ExtensionConfig>,
    extension_packages: FixedVec<ExtensionPackage>,
    extension_scripts: FixedVec<ExtensionScript>,
    extension_script_source: FixedBuf,
    extension_package_source: ExtensionPackageSource,
    roles: FixedVec<RoleDef>,
    role_memberships: FixedVec<RoleMembership>,
    acl_entries: FixedVec<AclEntry>,
    default_acl_entries: FixedVec<DefaultAclEntry>,
    /// Object comments (`COMMENT ON ...`), keyed by object identity. A slab of
    /// fixed slots reused as comments are added and removed.
    comments: FixedVec<CommentEntry>,
    /// The running statement's effective search path (see [`PathContext`]).
    path: PathContext,
    /// Monotonic stamp for `created_at` fields.
    catalog_seq: u64,
    next_rowid: u64,
    /// Command snapshot for reads: a row's own-transaction pending change is
    /// visible only if its command-id is `< read_snapshot`. [`SNAPSHOT_ALL`]
    /// (the default, reset at every statement) sees every own write; a
    /// data-modifying `WITH` statement lowers it to that statement's command-id
    /// so its main query does not see its CTEs' changes.
    read_snapshot: u32,
    /// Durable commit-LSN snapshot for the running statement.
    commit_snapshot: u64,
    /// Repeatable-read snapshots held by live connections. This registry is
    /// startup-sized to max_connections and drives version/WAL/SST retention.
    active_snapshots: FixedVec<(u32, u64)>,
    /// PostgreSQL relation locks. Each mode is tracked independently because
    /// SHARE and SHARE UPDATE EXCLUSIVE are incomparable, and savepoint
    /// rollback releases only modes acquired by the rolled-back
    /// subtransaction.
    table_locks: std::cell::RefCell<FixedVec<TableLock>>,
    /// PostgreSQL row locks and their wait-for graph. The registry is sized at
    /// startup from the per-transaction row bound and connection count.
    row_locks: std::cell::RefCell<crate::sql::lock::LockManager>,
    /// Shared acquisition clock for table and row locks. It lets a savepoint
    /// restore both registries to one exact transaction boundary.
    lock_sequence: Cell<u64>,
    /// Table generations captured at a SERIALIZABLE transaction's first
    /// snapshot. Scans mark entries read; a read-write transaction validates
    /// them before WAL publication to reject phantoms and write skew.
    serializable_snapshots: std::cell::RefCell<FixedVec<(u32, u32, u64, bool)>>,
    /// Log sequence number of the latest write; becomes the WAL position.
    lsn: u64,
    /// ALTER TABLE replay is encoded as a compact identity/mapping marker
    /// followed immediately by the ordinary final table definition.
    replay_table_rewrite: Option<ReplayTableRewrite>,
    /// The read path for spilled rows: the tiered block stack shared with the
    /// checkpointer, plus owned reader scratch. `None` without object storage
    /// — then rows never spill and the heap-full error stands.
    spill: Option<SpillReader>,
    /// Startup-allocated value indexes shared by every table's enforcers. Held
    /// in an `Option` so a rebuild can take it out for the duration of a row
    /// walk (which borrows the rest of `self`) and put it back.
    value_indexes: Option<ValueIndexPool>,
    /// The process-independent locale state selected at engine startup.
    collation: Option<CollationRuntime>,
}

struct CollationScratch {
    left: FixedBuf,
    right: FixedBuf,
}

struct CollationRuntime {
    locale: libc::locale_t,
    scratch: std::cell::RefCell<CollationScratch>,
}

impl CollationRuntime {
    fn new(config: &Config, budget: &mut Budget) -> Result<Self, SqlError> {
        const LOCALE_NAME_LIMIT: usize = 128;
        let bytes = config.database_collation_locale.as_bytes();
        if bytes.len() >= LOCALE_NAME_LIMIT || bytes.contains(&0) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "database_collation_locale is not a valid locale name"
            ));
        }
        let mut name = [0i8; LOCALE_NAME_LIMIT];
        for (index, byte) in bytes.iter().enumerate() {
            name[index] = *byte as i8;
        }
        // SAFETY: `name` is NUL-terminated and the returned locale is owned
        // by this runtime until Drop. No process-global locale is changed.
        let locale =
            unsafe { libc::newlocale(libc::LC_COLLATE_MASK, name.as_ptr(), core::ptr::null_mut()) };
        if locale.is_null() {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "database collation locale \"{}\" is unavailable",
                config.database_collation_locale
            ));
        }
        // Force libc to finish locale initialization while startup may still
        // allocate. Runtime comparisons must remain inside the frozen-memory
        // contract.
        let empty = b"\0";
        let compared = unsafe { strcoll_l(empty.as_ptr().cast(), empty.as_ptr().cast(), locale) };
        if compared != 0 {
            unsafe { libc::freelocale(locale) };
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "database collation locale did not compare equal strings equally"
            ));
        }
        let scratch = CollationScratch {
            left: FixedBuf::new(
                budget,
                "collation left scratch",
                config.collation_scratch_bytes,
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::OUT_OF_MEMORY,
                    "startup memory budget exhausted for collation scratch"
                )
            })?,
            right: FixedBuf::new(
                budget,
                "collation right scratch",
                config.collation_scratch_bytes,
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::OUT_OF_MEMORY,
                    "startup memory budget exhausted for collation scratch"
                )
            })?,
        };
        Ok(Self {
            locale,
            scratch: std::cell::RefCell::new(scratch),
        })
    }

    fn compare(&self, left: &str, right: &str) -> Result<core::cmp::Ordering, SqlError> {
        self.validate(left)?;
        self.validate(right)?;
        let mut scratch = self.scratch.try_borrow_mut().map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "locale comparison scratch is already in use"
            )
        })?;
        scratch.left.clear();
        scratch.right.clear();
        if !scratch.left.append(left.as_bytes())
            || !scratch.left.append(&[0])
            || !scratch.right.append(right.as_bytes())
            || !scratch.right.append(&[0])
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "text value exceeds configured collation scratch capacity"
            ));
        }
        // SAFETY: both buffers are NUL-terminated UTF-8 byte strings held for
        // the call; `locale` was created by newlocale and remains live.
        let compared = unsafe {
            strcoll_l(
                scratch.left.readable().as_ptr().cast(),
                scratch.right.readable().as_ptr().cast(),
                self.locale,
            )
        };
        Ok(compared.cmp(&0))
    }

    fn validate(&self, value: &str) -> Result<(), SqlError> {
        if value.len().saturating_add(1) > self.scratch.borrow().left.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "text value exceeds configured collation scratch capacity"
            ));
        }
        Ok(())
    }
}

impl Drop for CollationRuntime {
    fn drop(&mut self) {
        // SAFETY: locale is owned by this runtime and freed exactly once.
        unsafe { libc::freelocale(self.locale) };
    }
}

unsafe extern "C" {
    fn strcoll_l(
        left: *const libc::c_char,
        right: *const libc::c_char,
        locale: libc::locale_t,
    ) -> libc::c_int;
}

/// Fetches spilled rows back through the cache tiers. The buffers are owned
/// and startup-reserved; the stack is shared with the checkpointer through a
/// `RefCell` (single-threaded engine, short borrows).
pub(crate) struct SpillReader {
    blocks:
        std::rc::Rc<std::cell::RefCell<crate::store::TieredStore<crate::store::OwnedObjectStore>>>,
    /// Two scratch sets so one consume-in-place fetch may nest inside another
    /// (a validation scan holding one row while checking it against the
    /// rest). Deeper nesting is a loud error, not a deadlock.
    scratch: [std::cell::RefCell<SpillScratch>; 2],
    /// Merged-enumeration contexts: one per concurrently-live row-state
    /// walk (a join scans a table per depth, and a constraint scan can run
    /// inside another walk's callback). Each holds a resident data block
    /// per spill-list member plus an index buffer for cursor advances.
    /// Exhaustion is a loud error naming the bound.
    scan_contexts: Box<[std::cell::RefCell<ScanContext>]>,
    next_walk_id: std::cell::Cell<u64>,
    /// Independent buffers for persistent value probes. A probe may invoke an
    /// authoritative spilled-row recheck, so it must not borrow row scratch.
    value_scratch: [std::cell::RefCell<ValueIndexScratch>; 2],
    /// Nested materializers lease independent external-run producers. Their
    /// buffers and merge fan-in are fixed at startup; run blocks travel
    /// through `blocks`, never a provider-specific path.
    external_sorters: Box<[std::cell::RefCell<Box<crate::sql::external::ExternalSorter>>]>,
    /// Immutable-run cursors leased by nested materialized row sources.
    /// Their scratch is independent from the sorter, so consuming a completed
    /// run never prevents a deeper operator from producing another.
    external_readers: std::rc::Rc<[std::cell::RefCell<crate::sql::external::ExternalRunReader>]>,
}

/// Copyable access to immutable external runs.
///
/// The pointed-to allocations are owned by `Rc`s in [`SpillReader`] and never
/// move or detach after engine startup. The handle deliberately does not
/// borrow [`Storage`], so an immutable run can remain readable while a DML
/// executor mutates catalog or row state. It is crate-private and may only be
/// retained by an executor that also retains the engine. All cursor buffers
/// were reserved during startup.
#[derive(Clone, Copy)]
pub(crate) struct ExternalRunAccess {
    blocks: *const std::cell::RefCell<crate::store::TieredStore<crate::store::OwnedObjectStore>>,
    readers: *const [std::cell::RefCell<crate::sql::external::ExternalRunReader>],
}

impl ExternalRunAccess {
    pub(crate) fn reader(
        &self,
    ) -> Result<
        std::cell::RefMut<'_, crate::sql::external::ExternalRunReader>,
        crate::sql::eval::SqlError,
    > {
        // SAFETY: both allocations are pinned by `SpillReader`'s `Rc`s for
        // the engine lifetime; the crate-private handle is only installed in
        // executor state that cannot outlive that engine.
        let readers = unsafe { &*self.readers };
        for reader in readers.iter() {
            if let Ok(lease) = reader.try_borrow_mut() {
                return Ok(lease);
            }
        }
        Err(crate::sql_err!(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "external query run reader pool exhausted (maximum nesting {})",
            EXTERNAL_RUN_CONTEXTS
        ))
    }

    pub(crate) fn with_blocks<R>(
        &self,
        operation: impl FnOnce(&mut dyn crate::store::BlockStore) -> R,
    ) -> R {
        // SAFETY: see `reader`; access is serialized by the `RefCell`.
        let mut blocks = unsafe { &*self.blocks }.borrow_mut();
        operation(&mut *blocks)
    }
}

/// A row-state walk releases its block context before invoking its callback.
/// Nested walks invalidate only buffer residency; the cursor retains enough
/// logical position to reload the same immutable block on resumption.
const SCAN_CONTEXTS: usize = 1;
const EXTERNAL_RUN_CONTEXTS: usize = 8;

/// One merged walk's working memory: the current data block per member and
/// a shared buffer for index-block navigation on block advances.
struct ScanContext {
    owner: u64,
    member_blocks: [Box<[u8]>; MAX_SPILL_SSTS],
    member_raw_blocks: [Box<[u8]>; MAX_SPILL_SSTS],
    pax_column_buf: Box<[u8]>,
    pax_values_buf: Box<[u8]>,
    pax_value_extents: [Option<(usize, usize)>; MAX_COLUMNS],
    pax_values_owner: Option<(usize, usize)>,
    pax_row_buf: Box<[u8]>,
    index_buf: Box<[u8]>,
}

/// One member's cursor position inside a merged walk.
#[derive(Clone, Copy)]
struct MemberCursor {
    /// Which data block the cursor stands in (ordinal in the sparse index).
    ordinal: usize,
    /// Byte offset of the next entry inside that block.
    offset: usize,
    /// Byte offset of `head` inside the currently resident data block.
    head_offset: usize,
    /// Which ordinal the context's resident buffer currently holds, if any,
    /// and how many bytes of it are the block (the buffer is oversized).
    loaded: Option<usize>,
    loaded_len: usize,
    raw_len: usize,
    raw_row: usize,
    head_raw_row: usize,
    pax_layout: Option<crate::store::PaxLayout>,
    loaded_type: Option<crate::store::BlockType>,
    prefetched_leaf: Option<(usize, crate::store::BlockId)>,
    prefetched_data: Option<(usize, crate::store::DataBlockRef)>,
    /// The head entry, parsed as one immutable row-version key.
    head: Option<(crate::store::SstKey, bool, u32)>,
    done: bool,
}

#[derive(Clone, Copy)]
struct SpillVersion {
    len: Option<u32>,
    member: u8,
    commit_lsn: u64,
}

/// The reader's owned block buffers (index, data, physical column, chain
/// assembly, and packed-range staging).
struct SpillScratch {
    index_buf: Box<[u8]>,
    data_buf: Box<[u8]>,
    decoded_buf: Box<[u8]>,
    column_buf: Box<[u8]>,
    assembly_buf: Box<[u8]>,
    bounce_buf: Box<[u8]>,
    decoded_data_ref: Option<(crate::store::SstHandle, crate::store::DataBlockRef, usize)>,
}

struct ValueIndexScratch {
    roster: Box<[u8]>,
    data: Box<[u8]>,
}

impl SpillReader {
    /// Startup-only: reserves the reader scratch from the budget.
    pub(crate) fn new(
        budget: &mut Budget,
        blocks: std::rc::Rc<
            std::cell::RefCell<crate::store::TieredStore<crate::store::OwnedObjectStore>>,
        >,
    ) -> Result<Self, BudgetError> {
        budget.draw(
            2 * (5 * crate::store::MAX_PAYLOAD
                + crate::store::MAX_ASSEMBLED
                + core::mem::size_of::<SpillScratch>()),
            "spill reader",
        )?;
        budget.draw(
            SCAN_CONTEXTS
                * ((2 * MAX_SPILL_SSTS + 4) * crate::store::MAX_PAYLOAD
                    + core::mem::size_of::<std::cell::RefCell<ScanContext>>()),
            "row-state walk contexts",
        )?;
        budget.draw(
            4 * crate::store::MAX_PAYLOAD,
            "persistent value-index readers",
        )?;
        budget.draw_array(
            EXTERNAL_RUN_CONTEXTS,
            core::mem::size_of::<std::cell::RefCell<Box<crate::sql::external::ExternalSorter>>>(),
            "external query run producer slots",
        )?;
        let mut external_sorters = Vec::with_capacity(EXTERNAL_RUN_CONTEXTS);
        for _ in 0..EXTERNAL_RUN_CONTEXTS {
            external_sorters.push(std::cell::RefCell::new(Box::new(
                crate::sql::external::ExternalSorter::new(budget)?,
            )));
        }
        budget.draw(
            EXTERNAL_RUN_CONTEXTS * crate::sql::external::ExternalRunReader::budget_bytes(),
            "external query run readers",
        )?;
        let external_readers = (0..EXTERNAL_RUN_CONTEXTS)
            .map(|_| std::cell::RefCell::new(crate::sql::external::ExternalRunReader::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .into();
        let fresh = || {
            std::cell::RefCell::new(SpillScratch {
                index_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                data_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                decoded_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                column_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                assembly_buf: vec![0u8; crate::store::MAX_ASSEMBLED].into_boxed_slice(),
                bounce_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                decoded_data_ref: None,
            })
        };
        let context = || {
            std::cell::RefCell::new(ScanContext {
                owner: 0,
                member_blocks: core::array::from_fn(|_| {
                    vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice()
                }),
                member_raw_blocks: core::array::from_fn(|_| {
                    vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice()
                }),
                pax_column_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                pax_values_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                pax_value_extents: [None; MAX_COLUMNS],
                pax_values_owner: None,
                pax_row_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                index_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            })
        };
        let value = || {
            std::cell::RefCell::new(ValueIndexScratch {
                roster: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                data: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            })
        };
        let mut scan_contexts = Vec::with_capacity(SCAN_CONTEXTS);
        for _ in 0..SCAN_CONTEXTS {
            scan_contexts.push(context());
        }
        Ok(Self {
            blocks,
            scratch: [fresh(), fresh()],
            scan_contexts: scan_contexts.into_boxed_slice(),
            next_walk_id: std::cell::Cell::new(1),
            value_scratch: [value(), value()],
            external_sorters: external_sorters.into_boxed_slice(),
            external_readers,
        })
    }

    /// The budget the contexts and scratch draw, for memory-plan estimates.
    pub(crate) fn budget_bytes() -> usize {
        2 * (5 * crate::store::MAX_PAYLOAD
            + crate::store::MAX_ASSEMBLED
            + core::mem::size_of::<SpillScratch>())
            + SCAN_CONTEXTS
                * ((2 * MAX_SPILL_SSTS + 4) * crate::store::MAX_PAYLOAD
                    + core::mem::size_of::<std::cell::RefCell<ScanContext>>())
            + 4 * crate::store::MAX_PAYLOAD
            + EXTERNAL_RUN_CONTEXTS * crate::sql::external::ExternalSorter::budget_bytes()
            + EXTERNAL_RUN_CONTEXTS
                * core::mem::size_of::<std::cell::RefCell<Box<crate::sql::external::ExternalSorter>>>(
                )
            + EXTERNAL_RUN_CONTEXTS * crate::sql::external::ExternalRunReader::budget_bytes()
    }
}

#[inline(never)]
fn stored_query_dependency_slots(
    budget: &mut Budget,
    name: &'static str,
    count: usize,
) -> Result<FixedVec<StoredQueryDependencies>, BudgetError> {
    let mut slots = FixedVec::new(budget, name, count)?;
    for _ in 0..count {
        slots
            .push(StoredQueryDependencies::EMPTY)
            .expect("sized to catalog slots");
    }
    Ok(slots)
}

impl Storage {
    pub fn rebind_stored_query_dependencies(
        &self,
        serialized: StoredQueryDependencies,
        txid: u32,
    ) -> Result<StoredQueryDependencies, SqlError> {
        let mut rebound = StoredQueryDependencies::EMPTY;
        for dependency in serialized.entries() {
            let schema = dependency.schema.as_str();
            let name = dependency.name.as_str();
            let slot = match dependency.class {
                DependencyClass::Table => self.find_visible(schema, name, txid),
                DependencyClass::View => self.views.iter().position(|view| {
                    view.visible_to(txid)
                        && view.schema.as_str() == schema
                        && view.name.as_str() == name
                }),
                DependencyClass::Domain => self.domain_slot(schema, name, txid),
                DependencyClass::Enum => self.enum_slot(schema, name, txid),
                DependencyClass::Sequence => self.sequence_slot(schema, name, txid),
                DependencyClass::Composite => self.composite_slot(schema, name, txid),
                DependencyClass::Routine => match dependency.identity {
                    StoredDependencyIdentity::RoutineOid(oid) => {
                        self.routine_slot_by_oid(oid, txid)
                    }
                    StoredDependencyIdentity::Name => None,
                },
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "stored-query dependency {}.{} does not exist",
                    schema,
                    name
                )
            })?;
            rebound.push(StoredQueryDependency {
                class: dependency.class,
                slot: slot as u16,
                identity: dependency.identity,
                referenced_columns: dependency.referenced_columns,
                schema: dependency.schema,
                name: dependency.name,
                referenced_schema: dependency.referenced_schema,
                referenced_name: dependency.referenced_name,
            })?;
        }
        Ok(rebound)
    }

    pub fn rebind_all_stored_query_dependencies(&mut self) -> Result<(), SqlError> {
        for slot in 0..self.views.len() {
            if self.views[slot].ddl_state == CatalogDdlState::Present {
                let serialized = self.view_dependencies[slot];
                self.view_dependencies[slot] =
                    self.rebind_stored_query_dependencies(serialized, 0)?;
            }
        }
        for slot in 0..self.matviews.len() {
            if self.matviews[slot].ddl_state == CatalogDdlState::Present {
                let serialized = self.matview_dependencies[slot];
                self.matview_dependencies[slot] =
                    self.rebind_stored_query_dependencies(serialized, 0)?;
            }
        }
        for slot in 0..self.policies.len() {
            if self.policies[slot].ddl_state == CatalogDdlState::Present {
                let serialized = self.policies[slot].definition.dependencies;
                self.policies[slot].definition.dependencies =
                    self.rebind_stored_query_dependencies(serialized, 0)?;
            }
        }
        for slot in 0..self.routines.len() {
            if self.routines[slot].ddl_state == CatalogDdlState::Present {
                let serialized = self.routine_dependencies[slot];
                self.routine_dependencies[slot] =
                    self.rebind_stored_query_dependencies(serialized, 0)?;
            }
        }
        Ok(())
    }

    fn rename_stored_query_dependency(
        &mut self,
        class: DependencyClass,
        slot: usize,
        schema: SqlName,
        name: SqlName,
    ) {
        for view_slot in 0..self.views.len() {
            if self.views[view_slot].ddl_state != CatalogDdlState::Absent {
                self.view_dependencies[view_slot].rename(class, slot, schema, name);
            }
        }
        for matview_slot in 0..self.matviews.len() {
            if self.matviews[matview_slot].ddl_state != CatalogDdlState::Absent {
                self.matview_dependencies[matview_slot].rename(class, slot, schema, name);
            }
        }
        for policy in self.policies.iter_mut() {
            if policy.ddl_state != CatalogDdlState::Absent {
                policy
                    .definition
                    .dependencies
                    .rename(class, slot, schema, name);
                if let Some(pending) = &mut policy.pending_definition {
                    pending
                        .definition
                        .dependencies
                        .rename(class, slot, schema, name);
                }
            }
        }
        for routine_slot in 0..self.routines.len() {
            if self.routines[routine_slot].ddl_state != CatalogDdlState::Absent {
                self.routine_dependencies[routine_slot].rename(class, slot, schema, name);
            }
        }
        for pending in self.pending_routine_dependencies.iter_mut() {
            if pending.used {
                pending.dependencies.rename(class, slot, schema, name);
            }
        }
    }

    fn replace_stored_query_dependency_slot(
        &mut self,
        class: DependencyClass,
        old_slot: usize,
        new_slot: usize,
        schema: SqlName,
        name: SqlName,
    ) {
        for view_slot in 0..self.views.len() {
            if self.views[view_slot].ddl_state != CatalogDdlState::Absent {
                self.view_dependencies[view_slot]
                    .replace_slot(class, old_slot, new_slot, schema, name);
            }
        }
        for matview_slot in 0..self.matviews.len() {
            if self.matviews[matview_slot].ddl_state != CatalogDdlState::Absent {
                self.matview_dependencies[matview_slot]
                    .replace_slot(class, old_slot, new_slot, schema, name);
            }
        }
        for policy in self.policies.iter_mut() {
            if policy.ddl_state != CatalogDdlState::Absent {
                policy
                    .definition
                    .dependencies
                    .replace_slot(class, old_slot, new_slot, schema, name);
                if let Some(pending) = &mut policy.pending_definition {
                    pending
                        .definition
                        .dependencies
                        .replace_slot(class, old_slot, new_slot, schema, name);
                }
            }
        }
        for routine_slot in 0..self.routines.len() {
            if self.routines[routine_slot].ddl_state != CatalogDdlState::Absent {
                self.routine_dependencies[routine_slot]
                    .replace_slot(class, old_slot, new_slot, schema, name);
            }
        }
        for pending in self.pending_routine_dependencies.iter_mut() {
            if pending.used {
                pending
                    .dependencies
                    .replace_slot(class, old_slot, new_slot, schema, name);
            }
        }
    }

    /// Bytes drawn beyond the row heap itself, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        2 * config.collation_scratch_bytes
            + config.max_tables
                * (size_of::<Table>()
                    + FixedMap::<u64, RowState>::budget_bytes(config.table_rows)
                    + size_of::<ViewDef>()
                    + size_of::<RoutineDef>()
                    + size_of::<StoredQueryDependencies>()
                    + MAX_PENDING_TABLE_DEFS * size_of::<PendingRoutineDependencies>()
                    + size_of::<TriggerDef>()
                    + config.max_tables * size_of::<PartitionTriggerState>()
                    + MAX_POLICIES_PER_TABLE * size_of::<PolicyDef>()
                    + MAX_EXTENDED_STATISTICS_PER_TABLE * size_of::<ExtendedStatisticsDef>()
                    + size_of::<PublicationDef>()
                    + size_of::<StoredQueryDependencies>()
                    + size_of::<MatviewDef>()
                    + size_of::<StoredQueryDependencies>()
                    + size_of::<IndexDef>())
            + config.max_replication_slots * size_of::<ReplicationSlotDef>()
            + config.max_subscriptions * size_of::<SubscriptionDef>()
            + config.max_subscriptions
                * config.subscription_relation_capacity
                * size_of::<SubscriptionRelation>()
            + config.max_tables * MAX_PENDING_TABLE_DEFS * size_of::<PendingTableDefSlot>()
            + config.max_tables * MAX_PENDING_TABLE_DEFS * size_of::<PendingTableStatisticsSlot>()
            + pending_extended_statistics_capacity(config)
                * size_of::<PendingExtendedStatisticsDataSlot>()
            + MAX_SCHEMAS * size_of::<SchemaDef>()
            + MAX_EXTENSIONS * size_of::<ExtensionDef>()
            + MAX_EXTENSION_DEPENDENCIES * size_of::<ExtensionDependency>()
            + MAX_EXTENSION_CONFIG_RELATIONS * size_of::<ExtensionConfig>()
            + MAX_EXTENSIONS * size_of::<ExtensionPackage>()
            + config.max_extension_scripts * size_of::<ExtensionScript>()
            + config.extension_script_bytes
            + MAX_ROLES * size_of::<RoleDef>()
            + MAX_ROLE_MEMBERSHIPS * size_of::<RoleMembership>()
            + MAX_ACL_ENTRIES * size_of::<AclEntry>()
            + MAX_DEFAULT_ACL_ENTRIES * size_of::<DefaultAclEntry>()
            + MAX_SEQUENCES * size_of::<SequenceDef>()
            + MAX_DOMAINS * size_of::<DomainDef>()
            + MAX_ENUMS * size_of::<EnumDef>()
            + MAX_COMPOSITES * size_of::<CompositeDef>()
            + MAX_TABLESPACES * size_of::<TablespaceDef>()
            + MAX_COMMENTS * size_of::<CommentEntry>()
            + config.max_connections as usize * size_of::<(u32, u64)>()
            + config.max_connections as usize * config.max_tables * size_of::<TableLock>()
            + crate::sql::lock::LockManager::budget_bytes(
                config.max_connections as usize * config.txn_rows,
                config.max_connections as usize,
            )
            + config.max_connections as usize
                * config.max_tables
                * size_of::<(u32, u32, u64, bool)>()
            + ValueIndexPool::budget_bytes(config.max_value_indexes, config.value_index_rows)
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let heap = RowHeap::new(budget, config.memtable_bytes)?;
        let mut tables = FixedVec::new(budget, "tables", config.max_tables)?;
        let pending_table_defs = FixedVec::new(
            budget,
            "pending_table_defs",
            config.max_tables * MAX_PENDING_TABLE_DEFS,
        )?;
        let pending_table_statistics = FixedVec::new(
            budget,
            "pending_table_statistics",
            config.max_tables * MAX_PENDING_TABLE_DEFS,
        )?;
        for _ in 0..config.max_tables {
            tables
                .push(Table {
                    def: TableDef {
                        name: SqlName::parse("").expect("empty name fits"),
                        columns: [ColumnMeta {
                            name: SqlName::parse("").expect("empty name fits"),
                            ctype: ColType::Bool,
                            type_mod: -1,
                            collation: Collation::None,
                            not_null: NotNullOrigin::Nullable,
                            unique: false,
                            primary: false,
                            auto_increment: false,
                            default: ColumnDefault::NONE,
                            is_identity: false,
                            identity_always: false,
                            auto_increment_step: 1,
                            user_type: None,
                        }; MAX_COLUMNS],
                        n_columns: 0,
                        ..TableDef::empty()
                    },
                    ownership: Ownership::BOOTSTRAP,
                    pending_def_slots: [u32::MAX; MAX_PENDING_TABLE_DEFS],
                    n_pending_defs: 0,
                    pending_def_txid: None,
                    rows: FixedMap::new(budget, "table_rows", config.table_rows)?,
                    created_at: 0,
                    live: false,
                    pending_ddl: None,
                    dirty: false,
                    generation: 1,
                    statistics: TableStatistics::EMPTY,
                    statistics_dirty: false,
                    statistics_wal_dirty: false,
                    pending_statistics_slots: [u32::MAX; MAX_PENDING_TABLE_DEFS],
                    n_pending_statistics: 0,
                    pending_statistics_txid: None,
                    serial_last: [0; MAX_COLUMNS],
                    serial_dirty: false,
                    spill_ssts: [None; MAX_SPILL_SSTS],
                    n_spill_ssts: 0,
                    tombstones: [0; MAX_TOMBSTONES],
                    n_tombstones: 0,
                    tombstones_overflow: false,
                    enforcers: [None; MAX_VALUE_ENFORCERS],
                    n_enforcers: 0,
                })
                .expect("sized to max_tables");
        }
        let mut views = FixedVec::new(budget, "views", config.max_tables)?;
        for _ in 0..config.max_tables {
            views
                .push(ViewDef {
                    created_at: 0,
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    sql: StackStr::new(),
                    creation_path: StackStr::new(),
                    security: ViewSecurity::Definer,
                    ownership: Ownership::BOOTSTRAP,
                    pending_schema: None,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_tables");
        }
        let mut routines = FixedVec::new(budget, "routines", config.max_tables)?;
        for _ in 0..config.max_tables {
            routines
                .push(RoutineDef::EMPTY)
                .expect("sized to max_tables");
        }
        let routine_dependencies =
            stored_query_dependency_slots(budget, "routine_dependencies", config.max_tables)?;
        let mut pending_routine_dependencies = FixedVec::new(
            budget,
            "pending_routine_dependencies",
            config.max_tables * MAX_PENDING_TABLE_DEFS,
        )?;
        for _ in 0..config.max_tables * MAX_PENDING_TABLE_DEFS {
            pending_routine_dependencies
                .push(PendingRoutineDependencies::EMPTY)
                .expect("sized to pending routine definitions");
        }
        let mut triggers = FixedVec::new(budget, "triggers", config.max_tables)?;
        for _ in 0..config.max_tables {
            triggers
                .push(TriggerDef::EMPTY)
                .expect("sized to max_tables");
        }
        let partition_trigger_state_capacity = config.max_tables * config.max_tables;
        let mut partition_trigger_states = FixedVec::new(
            budget,
            "partition_trigger_states",
            partition_trigger_state_capacity,
        )?;
        for _ in 0..partition_trigger_state_capacity {
            partition_trigger_states
                .push(PartitionTriggerState::EMPTY)
                .expect("sized to trigger-table pairs");
        }
        let policy_capacity = config.max_tables * MAX_POLICIES_PER_TABLE;
        let mut policies = FixedVec::new(budget, "policies", policy_capacity)?;
        for _ in 0..policy_capacity {
            policies
                .push(PolicyDef::EMPTY)
                .expect("sized to policy capacity");
        }
        let extended_statistics_capacity = config.max_tables * MAX_EXTENDED_STATISTICS_PER_TABLE;
        let mut extended_statistics =
            FixedVec::new(budget, "extended_statistics", extended_statistics_capacity)?;
        for _ in 0..extended_statistics_capacity {
            extended_statistics
                .push(ExtendedStatisticsDef::EMPTY)
                .expect("sized to extended statistics capacity");
        }
        let pending_extended_statistics_data = FixedVec::new(
            budget,
            "pending_extended_statistics_data",
            pending_extended_statistics_capacity(config),
        )?;
        let view_dependencies =
            stored_query_dependency_slots(budget, "view_dependencies", config.max_tables)?;
        let mut publications = FixedVec::new(budget, "publications", config.max_tables)?;
        for _ in 0..config.max_tables {
            publications
                .push(PublicationDef {
                    created_at: 0,
                    name: SqlName::parse("").expect("empty name fits"),
                    pending_name: None,
                    all_tables: false,
                    tables: [u16::MAX; MAX_PUBLICATION_TABLES],
                    table_column_masks: [0; MAX_PUBLICATION_TABLES],
                    table_filters: PublicationFilters::EMPTY,
                    table_count: 0,
                    schemas: [u8::MAX; MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: true,
                    publish_delete: true,
                    publish_truncate: true,
                    publish_via_partition_root: false,
                    publish_generated_columns: PublishGeneratedColumns::None,
                    pending_definition: None,
                    ownership: Ownership::BOOTSTRAP,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_tables");
        }
        let mut replication_slots =
            FixedVec::new(budget, "replication_slots", config.max_replication_slots)?;
        for _ in 0..config.max_replication_slots {
            replication_slots
                .push(ReplicationSlotDef {
                    name: SqlName::EMPTY,
                    restart_lsn: 0,
                    confirmed_flush_lsn: 0,
                    behavior: ReplicationSlotBehavior::DEFAULT,
                    active: false,
                    live: false,
                })
                .expect("sized to max_replication_slots");
        }
        let mut subscriptions = FixedVec::new(budget, "subscriptions", config.max_subscriptions)?;
        for _ in 0..config.max_subscriptions {
            subscriptions
                .push(SubscriptionDef {
                    created_at: 0,
                    definition_generation: 0,
                    name: SqlName::EMPTY,
                    pending_name: None,
                    connection: SubscriptionConnInfo::parse(
                        "host=127.0.0.1 port=1 user=disabled dbname=disabled sslmode=disable",
                    )
                    .expect("static subscription placeholder is valid"),
                    publications: [SqlName::EMPTY; MAX_SUBSCRIPTION_PUBLICATIONS],
                    publication_count: 0,
                    pending_definition: None,
                    enabled: false,
                    pending_enabled: None,
                    slot: SubscriptionSlot::Absent,
                    behavior: SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                    bootstrap: SubscriptionBootstrap::Deferred,
                    pending_bootstrap: None,
                    cleanup: SubscriptionCleanup::None,
                    failure: None,
                    confirmed_lsn: 0,
                    ownership: Ownership::BOOTSTRAP,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_subscriptions");
        }
        let mut subscription_relations = FixedVec::new(
            budget,
            "subscription_relation_catalog",
            config.max_subscriptions * config.subscription_relation_capacity,
        )?;
        for _ in 0..config.max_subscriptions * config.subscription_relation_capacity {
            subscription_relations
                .push(SubscriptionRelation {
                    subscription_created_at: 0,
                    definition_generation: 0,
                    table_slot: u16::MAX,
                    state: SubscriptionRelationState::Initializing,
                    synchronization_lsn: 0,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to subscription relation capacity");
        }
        let mut matviews = FixedVec::new(budget, "matviews", config.max_tables)?;
        for _ in 0..config.max_tables {
            matviews
                .push(MatviewDef {
                    created_at: 0,
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    sql: StackStr::new(),
                    creation_path: StackStr::new(),
                    ownership: Ownership::BOOTSTRAP,
                    populated: false,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_tables");
        }
        let matview_dependencies =
            stored_query_dependency_slots(budget, "matview_dependencies", config.max_tables)?;
        let mut sequences = FixedVec::new(budget, "sequences", MAX_SEQUENCES)?;
        for _ in 0..MAX_SEQUENCES {
            sequences
                .push(SequenceDef {
                    created_at: 0,
                    schema: SqlName::EMPTY,
                    name: SqlName::EMPTY,
                    ownership: Ownership::BOOTSTRAP,
                    data_type: SeqType::Bigint,
                    increment: 1,
                    min_value: 1,
                    max_value: i64::MAX,
                    start_value: 1,
                    cache: 1,
                    cycle: false,
                    owner: None,
                    generator_for: None,
                    last_value: Cell::new(1),
                    is_called: Cell::new(false),
                    dirty: Cell::new(false),
                    pending_definition: None,
                    pending_last_value: Cell::new(1),
                    pending_is_called: Cell::new(false),
                    pending_dirty: Cell::new(false),
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to MAX_SEQUENCES");
        }
        let mut domains = FixedVec::new(budget, "domains", MAX_DOMAINS)?;
        for _ in 0..MAX_DOMAINS {
            domains
                .push(DomainDef::EMPTY)
                .expect("sized to MAX_DOMAINS");
        }
        let mut enums = FixedVec::new(budget, "enums", MAX_ENUMS)?;
        for _ in 0..MAX_ENUMS {
            enums.push(EnumDef::EMPTY).expect("sized to MAX_ENUMS");
        }
        let mut composites = FixedVec::new(budget, "composites", MAX_COMPOSITES)?;
        for _ in 0..MAX_COMPOSITES {
            composites
                .push(CompositeDef::EMPTY)
                .expect("sized to MAX_COMPOSITES");
        }
        let mut schemas = FixedVec::new(budget, "schemas", MAX_SCHEMAS)?;
        for i in 0..MAX_SCHEMAS {
            schemas
                .push(SchemaDef {
                    name: if i == 0 {
                        SqlName::parse("public").expect("fits")
                    } else {
                        SqlName::EMPTY
                    },
                    ownership: Ownership::BOOTSTRAP,
                    ddl_state: if i == 0 {
                        CatalogDdlState::Present
                    } else {
                        CatalogDdlState::Absent
                    },
                })
                .expect("sized to MAX_SCHEMAS");
        }
        let mut extensions = FixedVec::new(budget, "extensions", MAX_EXTENSIONS)?;
        for _ in 0..MAX_EXTENSIONS {
            extensions
                .push(ExtensionDef::EMPTY)
                .expect("sized to MAX_EXTENSIONS");
        }
        let mut extension_dependencies =
            FixedVec::new(budget, "extension_dependencies", MAX_EXTENSION_DEPENDENCIES)?;
        for _ in 0..MAX_EXTENSION_DEPENDENCIES {
            extension_dependencies
                .push(ExtensionDependency::EMPTY)
                .expect("sized to MAX_EXTENSION_DEPENDENCIES");
        }
        let mut extension_configs =
            FixedVec::new(budget, "extension_configs", MAX_EXTENSION_CONFIG_RELATIONS)?;
        for _ in 0..MAX_EXTENSION_CONFIG_RELATIONS {
            extension_configs
                .push(ExtensionConfig::EMPTY)
                .expect("sized to MAX_EXTENSION_CONFIG_RELATIONS");
        }
        let extension_packages = FixedVec::new(budget, "extension_packages", MAX_EXTENSIONS)?;
        let extension_scripts =
            FixedVec::new(budget, "extension_scripts", config.max_extension_scripts)?;
        let extension_script_source = FixedBuf::new(
            budget,
            "extension_script_source",
            config.extension_script_bytes,
        )?;
        let mut comments = FixedVec::new(budget, "comments", MAX_COMMENTS)?;
        for _ in 0..MAX_COMMENTS {
            comments
                .push(CommentEntry::empty())
                .expect("sized to MAX_COMMENTS");
        }
        let mut roles = FixedVec::new(budget, "roles", MAX_ROLES)?;
        for slot in 0..MAX_ROLES {
            roles
                .push(RoleDef {
                    name: if slot == 0 {
                        SqlName::parse("postgres").expect("bootstrap role name fits")
                    } else {
                        SqlName::EMPTY
                    },
                    attributes: if slot == 0 {
                        RoleAttributes::BOOTSTRAP
                    } else {
                        RoleAttributes::ORDINARY
                    },
                    live: slot == 0,
                    pending: None,
                })
                .expect("sized to MAX_ROLES");
        }
        let mut role_memberships = FixedVec::new(budget, "role_memberships", MAX_ROLE_MEMBERSHIPS)?;
        for _ in 0..MAX_ROLE_MEMBERSHIPS {
            role_memberships
                .push(RoleMembership {
                    role: 0,
                    member: 0,
                    grantor: 0,
                    options: RoleMembershipOptions::DEFAULT,
                    live: false,
                    pending: None,
                })
                .expect("sized to MAX_ROLE_MEMBERSHIPS");
        }
        let mut acl_entries = FixedVec::new(budget, "acl_entries", MAX_ACL_ENTRIES)?;
        acl_entries
            .push(AclEntry {
                object: AccessObject {
                    class: AccessClass::Schema,
                    slot: 0,
                },
                grantee: PUBLIC_ROLE,
                grantor: 0,
                privileges: PrivilegeSet::USAGE,
                grant_options: PrivilegeSet::NONE,
                live: true,
                pending: None,
            })
            .expect("ACL pool has room for the public schema default");
        let default_acl_entries =
            FixedVec::new(budget, "default_acl_entries", MAX_DEFAULT_ACL_ENTRIES)?;
        let mut indexes = FixedVec::new(budget, "indexes", config.max_tables)?;
        for _ in 0..config.max_tables {
            indexes
                .push(IndexDef {
                    created_at: 0,
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    pending_name: None,
                    table: SqlName::parse("").expect("empty name fits"),
                    ownership: Ownership::BOOTSTRAP,
                    columns: [0; MAX_INDEX_COLS],
                    expressions: [None; MAX_INDEX_COLS],
                    include_columns: [0; MAX_INDEX_COLS],
                    collations: [Collation::Default; MAX_INDEX_COLS],
                    explicit_collations: [false; MAX_INDEX_COLS],
                    operator_classes: [None; MAX_INDEX_COLS],
                    descending: [false; MAX_INDEX_COLS],
                    nulls_first: [false; MAX_INDEX_COLS],
                    n_cols: 0,
                    n_include_cols: 0,
                    nulls_not_distinct: false,
                    predicate: None,
                    unique: false,
                    mutable: IndexMutableDefinition::DEFAULT,
                    pending_definition: None,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_tables");
        }
        let mut tablespaces = FixedVec::new(budget, "tablespaces", MAX_TABLESPACES)?;
        for _ in 0..MAX_TABLESPACES {
            tablespaces
                .push(TablespaceDef {
                    created_at: 0,
                    name: SqlName::EMPTY,
                    location: StackStr::new(),
                    options: TablespaceOptions::DEFAULT,
                    ownership: Ownership::BOOTSTRAP,
                    pending: None,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to MAX_TABLESPACES");
        }
        let value_indexes =
            ValueIndexPool::new(budget, config.max_value_indexes, config.value_index_rows)?;
        let active_snapshots =
            FixedVec::new(budget, "active_snapshots", config.max_connections as usize)?;
        let table_locks = std::cell::RefCell::new(FixedVec::new(
            budget,
            "table_locks",
            config.max_connections as usize * config.max_tables,
        )?);
        let row_locks = std::cell::RefCell::new(crate::sql::lock::LockManager::new(
            budget,
            config.max_connections as usize * config.txn_rows,
            config.max_connections as usize,
        )?);
        let serializable_snapshots = std::cell::RefCell::new(FixedVec::new(
            budget,
            "serializable_snapshots",
            config.max_connections as usize * config.max_tables,
        )?);
        Ok(Self {
            heap,
            tables,
            pending_table_defs,
            pending_table_statistics,
            views,
            routines,
            routine_dependencies,
            pending_routine_dependencies,
            triggers,
            partition_trigger_states,
            policies,
            extended_statistics,
            pending_extended_statistics_data,
            publications,
            replication_slots,
            subscriptions,
            subscription_relations,
            view_dependencies,
            matviews,
            matview_dependencies,
            sequences,
            domains,
            enums,
            composites,
            indexes,
            tablespaces,
            schemas,
            extensions,
            extension_dependencies,
            extension_configs,
            extension_packages,
            extension_scripts,
            extension_script_source,
            extension_package_source: ExtensionPackageSource::Durable,
            roles,
            role_memberships,
            acl_entries,
            default_acl_entries,
            comments,
            path: PathContext::public_only(),
            catalog_seq: 0,
            read_snapshot: SNAPSHOT_ALL,
            commit_snapshot: u64::MAX,
            active_snapshots,
            table_locks,
            row_locks,
            lock_sequence: Cell::new(0),
            serializable_snapshots,
            next_rowid: 1,
            lsn: 0,
            replay_table_rewrite: None,
            spill: None,
            value_indexes: Some(value_indexes),
            collation: None,
        })
    }

    /// Installs the configured database-default collation before recovery or
    /// query execution begins.
    pub fn configure_collation(
        &mut self,
        config: &Config,
        budget: &mut Budget,
    ) -> Result<(), SqlError> {
        debug_assert!(self.collation.is_none());
        self.collation = Some(CollationRuntime::new(config, budget)?);
        Ok(())
    }

    /// Loads PostgreSQL control and SQL files before the allocator freezes.
    /// The retained package catalog and script bytes are startup-bounded; an
    /// installed extension's durable definition is separate from this local
    /// availability catalog, matching PostgreSQL's package/database split.
    pub(crate) fn load_extension_packages(&mut self, config: &Config) -> Result<(), SqlError> {
        self.extension_packages.clear();
        self.extension_scripts.clear();
        self.extension_script_source.clear();
        self.extension_package_source = if config.extension_control_path.is_empty() {
            ExtensionPackageSource::Durable
        } else {
            ExtensionPackageSource::Configured
        };
        if self.extension_package_source == ExtensionPackageSource::Durable {
            return Ok(());
        }
        for root in config
            .extension_control_path
            .split(':')
            .filter(|root| !root.is_empty())
        {
            let root = std::path::Path::new(root);
            let control_directory = root.join("extension");
            let entries = match std::fs::read_dir(&control_directory) {
                Ok(entries) => entries,
                Err(error) => {
                    return Err(sql_err!(
                        sqlstate::IO_ERROR,
                        "cannot read extension control directory \"{}\": {}",
                        control_directory.display(),
                        error
                    ));
                }
            };
            let mut controls = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| {
                    sql_err!(
                        sqlstate::IO_ERROR,
                        "cannot enumerate extension control directory: {}",
                        error
                    )
                })?;
                let path = entry.path();
                let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if let Some(name) = file.strip_suffix(".control")
                    && !name.contains("--")
                {
                    controls.push((name.to_string(), path));
                }
            }
            controls.sort_by(|left, right| left.0.cmp(&right.0));
            for (written_name, control_path) in controls {
                if self
                    .extension_packages
                    .iter()
                    .any(|package| package.name.as_str() == written_name)
                {
                    continue;
                }
                let name = SqlName::parse(&written_name)?;
                let control_text = std::fs::read_to_string(&control_path).map_err(|error| {
                    sql_err!(
                        sqlstate::IO_ERROR,
                        "cannot read extension control file \"{}\": {}",
                        control_path.display(),
                        error
                    )
                })?;
                let parsed = parse_extension_control(name, &control_text)?;
                let primary_package = parsed.package;
                let script_directory = parsed.directory.as_ref().map_or_else(
                    || control_directory.clone(),
                    |directory| {
                        let path = std::path::Path::new(directory);
                        if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            root.join(path)
                        }
                    },
                );
                let package_slot = self.extension_packages.len();
                self.extension_packages.push(parsed.package).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many available extension packages (limit {})",
                        self.extension_packages.capacity()
                    )
                })?;
                let prefix = format!("{}--", written_name);
                let entries = std::fs::read_dir(&script_directory).map_err(|error| {
                    sql_err!(
                        sqlstate::IO_ERROR,
                        "cannot read extension script directory \"{}\": {}",
                        script_directory.display(),
                        error
                    )
                })?;
                let mut scripts = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        sql_err!(
                            sqlstate::IO_ERROR,
                            "cannot enumerate extension script directory: {}",
                            error
                        )
                    })?;
                    let path = entry.path();
                    let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    let Some(rest) = file
                        .strip_prefix(&prefix)
                        .and_then(|name| name.strip_suffix(".sql"))
                    else {
                        continue;
                    };
                    let parts: Vec<&str> = rest.split("--").collect();
                    let (from, to) = match parts.as_slice() {
                        [to] => (None, ExtensionVersion::parse(to)?),
                        [from, to] => (
                            Some(ExtensionVersion::parse(from)?),
                            ExtensionVersion::parse(to)?,
                        ),
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "invalid extension script file name \"{}\"",
                                file
                            ));
                        }
                    };
                    scripts.push((from, to, path));
                }
                scripts.sort_by(|left, right| {
                    left.0
                        .as_ref()
                        .map(ExtensionVersion::as_str)
                        .cmp(&right.0.as_ref().map(ExtensionVersion::as_str))
                        .then(left.1.as_str().cmp(right.1.as_str()))
                });
                for (from, to, path) in scripts {
                    if self.extension_scripts.iter().any(|script| {
                        script.package as usize == package_slot
                            && script.from == from
                            && script.to == to
                    }) {
                        return Err(sql_err!(
                            sqlstate::DUPLICATE_OBJECT,
                            "duplicate extension update edge for \"{}\"",
                            written_name
                        ));
                    }
                    let source = std::fs::read_to_string(&path).map_err(|error| {
                        sql_err!(
                            sqlstate::IO_ERROR,
                            "cannot read extension script \"{}\": {}",
                            path.display(),
                            error
                        )
                    })?;
                    let secondary_path = control_directory.join(format!(
                        "{}--{}.control",
                        written_name,
                        to.as_str()
                    ));
                    let effective = if secondary_path.exists() {
                        let text = std::fs::read_to_string(&secondary_path).map_err(|error| {
                            sql_err!(
                                sqlstate::IO_ERROR,
                                "cannot read secondary extension control file \"{}\": {}",
                                secondary_path.display(),
                                error
                            )
                        })?;
                        merge_extension_control(
                            primary_package,
                            parse_extension_control(name, &text)?,
                        )?
                    } else {
                        primary_package
                    };
                    let mut normalized = String::with_capacity(source.len());
                    for line in source.lines() {
                        if line.trim_start().starts_with("\\echo") {
                            normalized.push_str("-- ");
                        }
                        normalized.push_str(line);
                        normalized.push('\n');
                    }
                    let offset = self.extension_script_source.len();
                    if !self.extension_script_source.append(normalized.as_bytes()) {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "extension scripts exceed extension_script_bytes ({})",
                            self.extension_script_source.capacity()
                        ));
                    }
                    self.extension_scripts
                        .push(ExtensionScript {
                            package: package_slot as u16,
                            from,
                            to,
                            offset: offset as u32,
                            length: normalized.len() as u32,
                            effective,
                        })
                        .map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "too many extension scripts (limit {})",
                                self.extension_scripts.capacity()
                            )
                        })?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn extension_package_source(&self) -> ExtensionPackageSource {
        self.extension_package_source
    }

    pub(crate) fn install_durable_extension_package(
        &mut self,
        package: ExtensionPackage,
    ) -> Result<usize, SqlError> {
        if self.extension_package_source != ExtensionPackageSource::Durable {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "durable extension package loaded while configured packages are authoritative"
            ));
        }
        if self
            .extension_packages
            .iter()
            .any(|known| known.name == package.name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "duplicate durable extension package \"{}\"",
                package.name.as_str()
            ));
        }
        let slot = self.extension_packages.len();
        self.extension_packages.push(package).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many durable extension packages (limit {})",
                self.extension_packages.capacity()
            )
        })?;
        Ok(slot)
    }

    pub(crate) fn install_durable_extension_script(
        &mut self,
        package: usize,
        from: Option<ExtensionVersion>,
        to: ExtensionVersion,
        effective: ExtensionPackage,
        source: &[u8],
    ) -> Result<(), SqlError> {
        if self.extension_package_source != ExtensionPackageSource::Durable {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid durable package source"
            ));
        }
        if package >= self.extension_packages.len()
            || effective.name != self.extension_packages[package].name
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid durable extension script package"
            ));
        }
        if self.extension_scripts.iter().any(|script| {
            script.package as usize == package && script.from == from && script.to == to
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "duplicate durable extension script"
            ));
        }
        let offset = self.extension_script_source.len();
        if !self.extension_script_source.append(source) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "durable extension scripts exceed extension_script_bytes ({})",
                self.extension_script_source.capacity()
            ));
        }
        self.extension_scripts
            .push(ExtensionScript {
                package: package as u16,
                from,
                to,
                offset: offset as u32,
                length: source.len() as u32,
                effective,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many durable extension scripts (limit {})",
                    self.extension_scripts.capacity()
                )
            })
    }

    pub(crate) fn extension_package(&self, name: &str) -> Option<(usize, &ExtensionPackage)> {
        self.extension_packages
            .iter()
            .enumerate()
            .find(|(_, package)| package.name.as_str() == name)
    }

    pub(crate) fn extension_packages(&self) -> impl Iterator<Item = (usize, &ExtensionPackage)> {
        self.extension_packages.iter().enumerate()
    }

    pub(crate) fn extension_relocation_blocker(&self, name: &str, txid: u32) -> Option<SqlName> {
        self.extensions_visible_to(txid).find_map(|(_, installed)| {
            let (_, package) = self.extension_package(installed.name.as_str())?;
            package
                .no_relocate()
                .iter()
                .any(|required| required.as_str() == name)
                .then_some(installed.name)
        })
    }

    pub(crate) fn extension_scripts_for(
        &self,
        package: usize,
    ) -> impl Iterator<Item = (usize, &ExtensionScript)> {
        self.extension_scripts
            .iter()
            .enumerate()
            .filter(move |(_, script)| script.package as usize == package)
    }

    pub(crate) fn extension_package_for_version(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Result<(usize, ExtensionVersion, ExtensionPackage), SqlError> {
        let (package_slot, package) = self.extension_package(name).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "extension \"{}\" is not available",
                name
            )
        })?;
        let target = match version {
            Some(version) => ExtensionVersion::parse(version)?,
            None => package.default_version.ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "version to install must be specified"
                )
            })?,
        };
        let effective = self
            .extension_scripts_for(package_slot)
            .find(|(_, script)| script.to == target)
            .map(|(_, script)| script.effective)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "extension has no script for version \"{}\"",
                    target.as_str()
                )
            })?;
        Ok((package_slot, target, effective))
    }

    pub(crate) fn extension_script(&self, slot: usize) -> ExtensionScript {
        self.extension_scripts[slot]
    }

    pub(crate) fn extension_script_source(&self, script: ExtensionScript) -> &str {
        let start = script.offset as usize;
        let end = start + script.length as usize;
        core::str::from_utf8(&self.extension_script_source.readable()[start..end])
            .expect("extension scripts were loaded as UTF-8")
    }

    /// Compares textual values under a resolved SQL collation identity.
    pub fn compare_text(
        &self,
        collation: Collation,
        left: &str,
        right: &str,
    ) -> Result<core::cmp::Ordering, SqlError> {
        match collation {
            Collation::None | Collation::C | Collation::Posix | Collation::UcsBasic => {
                Ok(left.cmp(right))
            }
            Collation::Default => self
                .collation
                .as_ref()
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "database collation was not initialized before comparison"
                    )
                })?
                .compare(left, right),
        }
    }

    /// Validates a value before it enters a sorting comparator, whose callback
    /// type cannot return SQL errors. The subsequent comparison is therefore
    /// infallible for capacity purposes and never turns an error into a tie.
    pub fn validate_text_collation(
        &self,
        collation: Collation,
        value: &Datum<'_>,
    ) -> Result<(), SqlError> {
        if collation != Collation::Default {
            return Ok(());
        }
        let value = match value {
            Datum::Text(value) => *value,
            Datum::Bpchar(value) => value.trim_end_matches(' '),
            _ => return Ok(()),
        };
        self.collation
            .as_ref()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "database collation was not initialized before comparison"
                )
            })?
            .validate(value)
    }

    /// Committed-catalog schema lookup (ignores uncommitted DDL): journal
    /// replay and the durable image.
    pub fn find_schema(&self, name: &str) -> Option<usize> {
        self.schemas.iter().position(|schema| {
            schema.ddl_state == CatalogDdlState::Present && schema.name.as_str() == name
        })
    }

    /// Transaction-scoped schema lookup: `txid` sees its own uncommitted
    /// CREATE/DROP and every committed schema.
    pub fn find_schema_visible(&self, name: &str, txid: u32) -> Option<usize> {
        self.schemas
            .iter()
            .position(|n| n.visible_to(txid) && n.name.as_str() == name)
    }

    pub fn schema_def(&self, slot: usize) -> &SchemaDef {
        &self.schemas[slot]
    }

    pub fn role_count(&self) -> usize {
        self.roles.len()
    }

    pub fn role(&self, slot: usize) -> &RoleDef {
        &self.roles[slot]
    }

    pub fn role_name(&self, slot: usize, txid: u32) -> SqlName {
        self.roles[slot].name_to(txid)
    }

    /// The current database is owned by the bootstrap role. `CREATEDB` is a
    /// cluster role attribute and must never stand in for database `CREATE`.
    pub(crate) fn has_current_database_create_privilege(&self, role: usize, txid: u32) -> bool {
        role == 0 || self.role(role).attributes_to(txid).superuser
    }

    pub fn live_roles(&self) -> impl Iterator<Item = (usize, &RoleDef)> {
        self.roles.iter().enumerate().filter(|(_, role)| role.live)
    }

    pub fn find_role(&self, name: &str) -> Option<usize> {
        self.roles
            .iter()
            .position(|role| role.live && role.name.as_str() == name)
    }

    pub fn find_role_visible(&self, name: &str, txid: u32) -> Option<usize> {
        self.roles
            .iter()
            .position(|role| role.visible_to(txid) && role.name_to(txid).as_str() == name)
    }

    /// PostgreSQL preassigns OIDs to built-in roles. The bootstrap role keeps
    /// OID 10 for compatibility with the existing catalog; user roles occupy
    /// a deterministic catalog range.
    pub fn role_oid(slot: usize) -> i32 {
        if slot == 0 { 10 } else { 16_384 + slot as i32 }
    }

    pub(crate) fn role_slot_by_oid(&self, oid: i32, txid: u32) -> Option<usize> {
        self.roles.iter().enumerate().find_map(|(slot, role)| {
            (role.visible_to(txid) && Self::role_oid(slot) == oid).then_some(slot)
        })
    }

    fn ownership(&self, object: AccessObject) -> &Ownership {
        let slot = object.slot as usize;
        match object.class {
            AccessClass::Table => &self.tables[slot].ownership,
            AccessClass::View => &self.views[slot].ownership,
            AccessClass::MaterializedView => &self.matviews[slot].ownership,
            AccessClass::Sequence => &self.sequences[slot].ownership,
            AccessClass::Schema => &self.schemas[slot].ownership,
            AccessClass::Domain => &self.domains[slot].ownership,
            AccessClass::Enum => &self.enums[slot].ownership,
            AccessClass::Index => &self.indexes[slot].ownership,
            AccessClass::Routine => &self.routines[slot].ownership,
            AccessClass::Composite => &self.composites[slot].ownership,
            AccessClass::Tablespace => &self.tablespaces[slot].ownership,
            AccessClass::Statistics => &self.extended_statistics[slot].ownership,
            AccessClass::Extension => &self.extensions[slot].ownership,
            AccessClass::Trigger => match self.triggers[slot].target {
                TriggerTarget::Table(table) => &self.tables[usize::from(table)].ownership,
                TriggerTarget::View(view) => &self.views[usize::from(view)].ownership,
            },
        }
    }

    fn ownership_mut(&mut self, object: AccessObject) -> &mut Ownership {
        let slot = object.slot as usize;
        match object.class {
            AccessClass::Table => &mut self.tables[slot].ownership,
            AccessClass::View => &mut self.views[slot].ownership,
            AccessClass::MaterializedView => &mut self.matviews[slot].ownership,
            AccessClass::Sequence => &mut self.sequences[slot].ownership,
            AccessClass::Schema => &mut self.schemas[slot].ownership,
            AccessClass::Domain => &mut self.domains[slot].ownership,
            AccessClass::Enum => &mut self.enums[slot].ownership,
            AccessClass::Index => &mut self.indexes[slot].ownership,
            AccessClass::Routine => &mut self.routines[slot].ownership,
            AccessClass::Composite => &mut self.composites[slot].ownership,
            AccessClass::Tablespace => &mut self.tablespaces[slot].ownership,
            AccessClass::Statistics => &mut self.extended_statistics[slot].ownership,
            AccessClass::Extension => &mut self.extensions[slot].ownership,
            AccessClass::Trigger => {
                unreachable!("triggers inherit relation ownership and cannot be reassigned")
            }
        }
    }

    fn initial_ownership(&self, txid: u32) -> Ownership {
        if txid == 0 {
            return Ownership::BOOTSTRAP;
        }
        let role_name = crate::sql::eval::funcs::system::current_user_owned();
        let owner = self
            .find_role_visible(role_name.as_str(), txid)
            .unwrap_or(0) as u16;
        Ownership {
            owner: 0,
            pending: Some(PendingOwnership { txid, owner }),
        }
    }

    pub(crate) fn object_owner(&self, object: AccessObject, txid: u32) -> usize {
        self.ownership(object).owner_to(txid) as usize
    }

    pub(crate) fn current_role_slot(&self, txid: u32) -> Option<usize> {
        let role = crate::sql::eval::funcs::system::current_user_owned();
        self.find_role_visible(role.as_str(), txid)
    }

    pub(crate) fn table_access_object(&self, slot: usize, txid: u32) -> AccessObject {
        let definition = self.table_def(slot, txid);
        self.matview_slot(definition.schema.as_str(), definition.name.as_str(), txid)
            .map_or(
                AccessObject {
                    class: AccessClass::Table,
                    slot: slot as u16,
                },
                |matview| AccessObject {
                    class: AccessClass::MaterializedView,
                    slot: matview as u16,
                },
            )
    }

    pub(crate) fn resolve_access_object(
        &self,
        class: AccessClass,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Option<AccessObject> {
        let slot = match class {
            AccessClass::Table => self.find_visible(schema, name, txid),
            AccessClass::View => self.views.iter().position(|view| {
                view.visible_to(txid)
                    && view.schema_for(txid).as_str() == schema
                    && view.name.as_str() == name
            }),
            AccessClass::MaterializedView => self.matview_slot(schema, name, txid),
            AccessClass::Sequence => self.sequence_slot(schema, name, txid),
            AccessClass::Schema => self.find_schema_visible(name, txid),
            AccessClass::Domain => self.domain_slot(schema, name, txid),
            AccessClass::Enum => self.enum_slot(schema, name, txid),
            AccessClass::Index => self.indexes.iter().position(|index| {
                index.visible_to(txid)
                    && index.schema.as_str() == schema
                    && index.name_for(txid).as_str() == name
            }),
            AccessClass::Routine => self.routines.iter().position(|routine| {
                routine.visible_to(txid)
                    && routine.schema_for(txid).as_str() == schema
                    && routine.name_for(txid).as_str() == name
            }),
            AccessClass::Composite => self.composite_slot(schema, name, txid),
            AccessClass::Tablespace => self.tablespace_slot(name, txid),
            AccessClass::Statistics => self.extended_statistics_slot(schema, name, txid),
            AccessClass::Extension => self.extension_slot(name, txid),
            AccessClass::Trigger => None,
        }?;
        u16::try_from(slot)
            .ok()
            .map(|slot| AccessObject { class, slot })
    }

    pub(crate) fn access_object_name(&self, object: AccessObject) -> (SqlName, SqlName) {
        self.access_object_name_to(object, 0)
    }

    pub(crate) fn access_object_name_to(
        &self,
        object: AccessObject,
        txid: u32,
    ) -> (SqlName, SqlName) {
        let slot = object.slot as usize;
        match object.class {
            AccessClass::Table => {
                let definition = self.table_def(slot, txid);
                (definition.schema, definition.name)
            }
            AccessClass::View => {
                let definition = &self.views[slot];
                (definition.schema_for(txid), definition.name)
            }
            AccessClass::MaterializedView => {
                let definition = &self.matviews[slot];
                let backing = self.tables.iter().position(|table| {
                    table.def.schema == definition.schema && table.def.name == definition.name
                });
                backing.map_or((definition.schema, definition.name), |table| {
                    let table = self.table_def(table, txid);
                    (table.schema, table.name)
                })
            }
            AccessClass::Sequence => {
                let definition = self.sequence_for(slot, txid);
                (definition.schema, definition.name)
            }
            AccessClass::Schema => (SqlName::EMPTY, self.schemas[slot].name),
            AccessClass::Domain => {
                let definition = self.domain_for(slot, txid);
                (definition.schema, definition.name)
            }
            AccessClass::Enum => {
                let definition = self.enum_for(slot, txid);
                (definition.schema, definition.name)
            }
            AccessClass::Index => {
                let definition = &self.indexes[slot];
                (definition.schema, definition.name_for(txid))
            }
            AccessClass::Routine => {
                let definition = &self.routines[slot];
                (definition.schema_for(txid), definition.name_for(txid))
            }
            AccessClass::Composite => {
                let definition = self.composite_for(slot, txid);
                (definition.schema, definition.name)
            }
            AccessClass::Tablespace => (SqlName::EMPTY, self.tablespaces[slot].name_for(txid)),
            AccessClass::Statistics => {
                let definition = self.extended_statistics[slot].definition_for(txid);
                (definition.schema, definition.name)
            }
            AccessClass::Extension => (SqlName::EMPTY, self.extensions[slot].name),
            AccessClass::Trigger => {
                let trigger = &self.triggers[slot];
                let schema = match trigger.target {
                    TriggerTarget::Table(table) => self.table_def(usize::from(table), txid).schema,
                    TriggerTarget::View(view) => self.views[usize::from(view)].schema,
                };
                (schema, trigger.name_to(txid))
            }
        }
    }

    pub(crate) fn access_object_is_live(&self, object: AccessObject) -> bool {
        let slot = object.slot as usize;
        match object.class {
            AccessClass::Table => self.tables[slot].live,
            AccessClass::View => self.views[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::MaterializedView => {
                self.matviews[slot].ddl_state == CatalogDdlState::Present
            }
            AccessClass::Sequence => self.sequences[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Schema => self.schemas[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Domain => self.domains[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Enum => self.enums[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Index => self.indexes[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Routine => self.routines[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Composite => self.composites[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Tablespace => self.tablespaces[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Statistics => {
                self.extended_statistics[slot].ddl_state == CatalogDdlState::Present
            }
            AccessClass::Extension => self.extensions[slot].ddl_state == CatalogDdlState::Present,
            AccessClass::Trigger => self.triggers[slot].ddl_state == CatalogDdlState::Present,
        }
    }

    pub(crate) fn access_object_visible_to(&self, object: AccessObject, txid: u32) -> bool {
        let slot = object.slot as usize;
        match object.class {
            AccessClass::Table => self.tables[slot].visible_to(txid),
            AccessClass::View => self.views[slot].visible_to(txid),
            AccessClass::MaterializedView => self.matviews[slot].visible_to(txid),
            AccessClass::Sequence => self.sequences[slot].visible_to(txid),
            AccessClass::Schema => self.schemas[slot].visible_to(txid),
            AccessClass::Domain => self.domains[slot].visible_to(txid),
            AccessClass::Enum => self.enums[slot].visible_to(txid),
            AccessClass::Index => self.indexes[slot].visible_to(txid),
            AccessClass::Routine => self.routines[slot].visible_to(txid),
            AccessClass::Composite => self.composites[slot].visible_to(txid),
            AccessClass::Tablespace => self.tablespaces[slot].visible_to(txid),
            AccessClass::Statistics => self.extended_statistics[slot].visible_to(txid),
            AccessClass::Extension => self.extensions[slot].visible_to(txid),
            AccessClass::Trigger => self.triggers[slot].visible_to(txid),
        }
    }

    pub(crate) fn access_class_slots(&self, class: AccessClass) -> usize {
        match class {
            AccessClass::Table => self.tables.len(),
            AccessClass::View => self.views.len(),
            AccessClass::MaterializedView => self.matviews.len(),
            AccessClass::Sequence => self.sequences.len(),
            AccessClass::Schema => self.schemas.len(),
            AccessClass::Domain => self.domains.len(),
            AccessClass::Enum => self.enums.len(),
            AccessClass::Index => self.indexes.len(),
            AccessClass::Routine => self.routines.len(),
            AccessClass::Composite => self.composites.len(),
            AccessClass::Tablespace => self.tablespaces.len(),
            AccessClass::Statistics => self.extended_statistics.len(),
            AccessClass::Extension => self.extensions.len(),
            AccessClass::Trigger => self.triggers.len(),
        }
    }

    /// Removes privilege rows belonging to a catalog slot before that slot is
    /// reused. ACL identity includes the fixed registry slot, so retaining a
    /// dropped object's rows would otherwise grant privileges on an unrelated
    /// object later allocated into the same slot.
    fn clear_object_acl_entries(&mut self, object: AccessObject) {
        for entry in self.acl_entries.iter_mut() {
            if entry.object == object {
                entry.live = false;
                entry.privileges = PrivilegeSet::NONE;
                entry.grant_options = PrivilegeSet::NONE;
                entry.pending = None;
                entry.object.slot = u16::MAX;
            }
        }
    }

    pub(crate) fn role_has_object_dependents(&self, role: usize, txid: u32) -> bool {
        let owned = [
            (AccessClass::Table, self.tables.len()),
            (AccessClass::View, self.views.len()),
            (AccessClass::MaterializedView, self.matviews.len()),
            (AccessClass::Sequence, self.sequences.len()),
            (AccessClass::Schema, self.schemas.len()),
            (AccessClass::Domain, self.domains.len()),
            (AccessClass::Enum, self.enums.len()),
            (AccessClass::Index, self.indexes.len()),
            (AccessClass::Routine, self.routines.len()),
            (AccessClass::Composite, self.composites.len()),
            (AccessClass::Tablespace, self.tablespaces.len()),
            (AccessClass::Statistics, self.extended_statistics.len()),
            (AccessClass::Extension, self.extensions.len()),
        ]
        .into_iter()
        .any(|(class, count)| {
            (0..count).any(|slot| {
                let object = AccessObject {
                    class,
                    slot: slot as u16,
                };
                self.access_object_visible_to(object, txid)
                    && self.object_owner(object, txid) == role
            })
        });
        owned
            || self.acl_entries.iter().any(|entry| {
                let (visible, grantee, grantor, _, _) = Self::acl_visible(entry, txid);
                visible
                    && self.access_object_visible_to(entry.object, txid)
                    && (grantee == role as u16 || grantor == role as u16)
            })
            || self.default_acl_entries.iter().any(|entry| {
                let (defined, _, _) = Self::default_acl_visible(entry, txid);
                defined && (entry.owner == role as u16 || entry.grantee == role as u16)
            })
            || self
                .policies_with_slots_visible_to(txid)
                .any(|(_, policy)| {
                    policy
                        .definition_for(txid)
                        .roles
                        .entries()
                        .contains(&(role as u16))
                })
    }

    pub(crate) fn set_object_owner(
        &mut self,
        object: AccessObject,
        owner: usize,
        txid: u32,
    ) -> Option<PendingOwnership> {
        let ownership = self.ownership_mut(object);
        let prior = ownership.pending;
        if txid == 0 {
            ownership.owner = owner as u16;
            ownership.pending = None;
        } else {
            ownership.pending = Some(PendingOwnership {
                txid,
                owner: owner as u16,
            });
        }
        prior
    }

    pub(crate) fn commit_object_owner(&mut self, object: AccessObject, txid: u32) {
        let ownership = self.ownership_mut(object);
        if let Some(pending) = ownership.pending
            && pending.txid == txid
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn restore_object_owner(
        &mut self,
        object: AccessObject,
        prior: Option<PendingOwnership>,
    ) {
        self.ownership_mut(object).pending = prior;
    }

    fn acl_visible(entry: &AclEntry, txid: u32) -> (bool, u16, u16, PrivilegeSet, PrivilegeSet) {
        match entry.pending {
            Some(pending) if pending.txid == txid => (
                pending.privileges.0 != 0,
                pending.grantee,
                pending.grantor,
                pending.privileges,
                pending.grant_options,
            ),
            _ => (
                entry.live,
                entry.grantee,
                entry.grantor,
                entry.privileges,
                entry.grant_options,
            ),
        }
    }

    pub(crate) fn change_acl(
        &mut self,
        object: AccessObject,
        grantee: u16,
        grantor: u16,
        privileges: PrivilegeSet,
        grant_options: PrivilegeSet,
        txid: u32,
    ) -> Result<(usize, Option<PendingAcl>), SqlError> {
        let slot = self
            .acl_entries
            .iter()
            .position(|entry| {
                let (_, visible_grantee, visible_grantor, _, _) = Self::acl_visible(entry, txid);
                entry.object == object
                    && visible_grantee == grantee
                    && visible_grantor == grantor
                    && (entry.live || entry.pending.is_some())
            })
            .or_else(|| {
                self.acl_entries.iter().position(|entry| {
                    entry.object.slot == u16::MAX && !entry.live && entry.pending.is_none()
                })
            })
            .unwrap_or(self.acl_entries.len());
        if slot == self.acl_entries.len() {
            self.acl_entries
                .push(AclEntry {
                    object,
                    grantee,
                    grantor,
                    privileges: PrivilegeSet::NONE,
                    grant_options: PrivilegeSet::NONE,
                    live: false,
                    pending: None,
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many object privilege entries (limit {})",
                        MAX_ACL_ENTRIES
                    )
                })?;
        } else if self.acl_entries[slot].object.slot == u16::MAX {
            self.acl_entries[slot].object = object;
            self.acl_entries[slot].grantee = grantee;
            self.acl_entries[slot].grantor = grantor;
        }
        let entry = &mut self.acl_entries[slot];
        let prior = entry.pending;
        if txid == 0 {
            entry.grantee = grantee;
            entry.grantor = grantor;
            entry.privileges = privileges;
            entry.grant_options = grant_options;
            entry.live = privileges.0 != 0;
            entry.pending = None;
        } else {
            entry.pending = Some(PendingAcl {
                txid,
                grantee,
                grantor,
                privileges,
                grant_options,
            });
        }
        Ok((slot, prior))
    }

    pub(crate) fn acl_to(
        &self,
        object: AccessObject,
        grantee: u16,
        txid: u32,
    ) -> (PrivilegeSet, PrivilegeSet) {
        self.acl_entries
            .iter()
            .filter(|entry| {
                let (_, visible_grantee, _, _, _) = Self::acl_visible(entry, txid);
                entry.object == object && visible_grantee == grantee
            })
            .fold(
                (PrivilegeSet::NONE, PrivilegeSet::NONE),
                |(privileges, grant_options), entry| {
                    let (visible, _, _, entry_privileges, entry_grant_options) =
                        Self::acl_visible(entry, txid);
                    if visible {
                        (
                            privileges.union(entry_privileges),
                            grant_options.union(entry_grant_options),
                        )
                    } else {
                        (privileges, grant_options)
                    }
                },
            )
    }

    pub(crate) fn acl_from(
        &self,
        object: AccessObject,
        grantee: u16,
        grantor: u16,
        txid: u32,
    ) -> (PrivilegeSet, PrivilegeSet) {
        self.acl_entries
            .iter()
            .filter(|entry| {
                let (_, visible_grantee, visible_grantor, _, _) = Self::acl_visible(entry, txid);
                entry.object == object
                    && visible_grantee == grantee
                    && visible_grantor == grantor
                    && (entry.live || entry.pending.is_some())
            })
            .fold(
                (PrivilegeSet::NONE, PrivilegeSet::NONE),
                |(privileges, grant_options), entry| {
                    let (visible, _, _, entry_privileges, entry_grant_options) =
                        Self::acl_visible(entry, txid);
                    if visible {
                        (
                            privileges.union(entry_privileges),
                            grant_options.union(entry_grant_options),
                        )
                    } else {
                        (privileges, grant_options)
                    }
                },
            )
    }

    pub(crate) fn acl_state(&self, slot: usize, txid: u32) -> (PrivilegeSet, PrivilegeSet) {
        let (visible, _, _, privileges, grant_options) =
            Self::acl_visible(&self.acl_entries[slot], txid);
        if visible {
            (privileges, grant_options)
        } else {
            (PrivilegeSet::NONE, PrivilegeSet::NONE)
        }
    }

    pub(crate) fn commit_acl(&mut self, slot: usize, txid: u32) {
        let Some(pending) = self.acl_entries[slot]
            .pending
            .filter(|pending| pending.txid == txid)
        else {
            return;
        };
        {
            let entry = &mut self.acl_entries[slot];
            entry.grantee = pending.grantee;
            entry.grantor = pending.grantor;
            entry.privileges = pending.privileges;
            entry.grant_options = pending.grant_options;
            entry.live = pending.privileges.0 != 0;
            entry.pending = None;
        }
        self.deduplicate_acl(slot);
    }

    fn deduplicate_acl(&mut self, slot: usize) {
        let object = self.acl_entries[slot].object;
        let grantee = self.acl_entries[slot].grantee;
        let grantor = self.acl_entries[slot].grantor;
        let Some(canonical) = self
            .acl_entries
            .iter()
            .enumerate()
            .find_map(|(candidate, entry)| {
                (candidate != slot
                    && entry.object == object
                    && entry.grantee == grantee
                    && entry.grantor == grantor
                    && entry.object.slot != u16::MAX
                    && entry.pending.is_none())
                .then_some(candidate)
            })
        else {
            return;
        };
        let privileges = self.acl_entries[slot].privileges;
        let grant_options = self.acl_entries[slot].grant_options;
        self.acl_entries[canonical].privileges =
            self.acl_entries[canonical].privileges.union(privileges);
        self.acl_entries[canonical].grant_options = self.acl_entries[canonical]
            .grant_options
            .union(grant_options);
        self.acl_entries[canonical].live = self.acl_entries[canonical].privileges.0 != 0;
        self.acl_entries[slot].object.slot = u16::MAX;
        self.acl_entries[slot].privileges = PrivilegeSet::NONE;
        self.acl_entries[slot].grant_options = PrivilegeSet::NONE;
        self.acl_entries[slot].live = false;
    }

    pub(crate) fn restore_acl_pending(&mut self, slot: usize, prior: Option<PendingAcl>) {
        self.acl_entries[slot].pending = prior;
    }

    pub(crate) fn acl_identity(&self, slot: usize, txid: u32) -> (u16, u16) {
        let (_, grantee, grantor, _, _) = Self::acl_visible(&self.acl_entries[slot], txid);
        (grantee, grantor)
    }

    pub(crate) fn change_acl_identity(
        &mut self,
        slot: usize,
        grantee: u16,
        grantor: u16,
        txid: u32,
    ) -> Option<PendingAcl> {
        let entry = &mut self.acl_entries[slot];
        let prior = entry.pending;
        let (_, _, _, privileges, grant_options) = Self::acl_visible(entry, txid);
        if txid == 0 {
            entry.grantee = grantee;
            entry.grantor = grantor;
            entry.pending = None;
        } else {
            entry.pending = Some(PendingAcl {
                txid,
                grantee,
                grantor,
                privileges,
                grant_options,
            });
        }
        if txid == 0 {
            self.deduplicate_acl(slot);
        }
        prior
    }

    fn default_acl_visible(
        entry: &DefaultAclEntry,
        txid: u32,
    ) -> (bool, PrivilegeSet, PrivilegeSet) {
        match entry.pending {
            Some(pending) if pending.txid == txid => {
                (pending.defined, pending.privileges, pending.grant_options)
            }
            _ => (entry.defined, entry.privileges, entry.grant_options),
        }
    }

    pub(crate) const fn default_acl_baseline(
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
    ) -> (PrivilegeSet, PrivilegeSet) {
        if schema != DEFAULT_ACL_ALL_SCHEMAS {
            return (PrivilegeSet::NONE, PrivilegeSet::NONE);
        }
        if grantee == owner {
            let all = class.all_privileges();
            return (all, PrivilegeSet::NONE);
        }
        if grantee == PUBLIC_ROLE {
            return (class.default_public_privileges(), PrivilegeSet::NONE);
        }
        (PrivilegeSet::NONE, PrivilegeSet::NONE)
    }

    pub(crate) fn default_acl_state(
        &self,
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
        txid: u32,
    ) -> (bool, PrivilegeSet, PrivilegeSet) {
        self.default_acl_entries
            .iter()
            .find(|entry| {
                entry.owner == owner
                    && entry.schema == schema
                    && entry.class == class
                    && entry.grantee == grantee
            })
            .map(|entry| Self::default_acl_visible(entry, txid))
            .unwrap_or((false, PrivilegeSet::NONE, PrivilegeSet::NONE))
    }

    pub(crate) fn default_acl_effective(
        &self,
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
        txid: u32,
    ) -> (PrivilegeSet, PrivilegeSet) {
        let (defined, privileges, grant_options) =
            self.default_acl_state(owner, schema, class, grantee, txid);
        if defined {
            (privileges, grant_options)
        } else {
            Self::default_acl_baseline(owner, schema, class, grantee)
        }
    }

    pub(crate) fn change_default_acl(
        &mut self,
        key: DefaultAclKey,
        defined: bool,
        privileges: PrivilegeSet,
        grant_options: PrivilegeSet,
        txid: u32,
    ) -> Result<(usize, Option<PendingDefaultAcl>), SqlError> {
        let DefaultAclKey {
            owner,
            schema,
            class,
            grantee,
        } = key;
        let slot = self
            .default_acl_entries
            .iter()
            .position(|entry| {
                entry.owner == owner
                    && entry.schema == schema
                    && entry.class == class
                    && entry.grantee == grantee
            })
            .or_else(|| {
                self.default_acl_entries
                    .iter()
                    .position(|entry| entry.owner == PUBLIC_ROLE && entry.pending.is_none())
            })
            .unwrap_or(self.default_acl_entries.len());
        if slot == self.default_acl_entries.len() {
            self.default_acl_entries
                .push(DefaultAclEntry {
                    owner,
                    schema,
                    class,
                    grantee,
                    defined: false,
                    privileges: PrivilegeSet::NONE,
                    grant_options: PrivilegeSet::NONE,
                    pending: None,
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many default privilege entries (limit {})",
                        MAX_DEFAULT_ACL_ENTRIES
                    )
                })?;
        } else if self.default_acl_entries[slot].owner == PUBLIC_ROLE {
            let entry = &mut self.default_acl_entries[slot];
            entry.owner = owner;
            entry.schema = schema;
            entry.class = class;
            entry.grantee = grantee;
            entry.defined = false;
            entry.privileges = PrivilegeSet::NONE;
            entry.grant_options = PrivilegeSet::NONE;
        }
        let entry = &mut self.default_acl_entries[slot];
        let prior = entry.pending;
        if txid == 0 {
            entry.defined = defined;
            entry.privileges = privileges;
            entry.grant_options = grant_options;
            entry.pending = None;
            if !defined {
                entry.owner = PUBLIC_ROLE;
            }
        } else {
            entry.pending = Some(PendingDefaultAcl {
                txid,
                defined,
                privileges,
                grant_options,
            });
        }
        Ok((slot, prior))
    }

    pub(crate) fn default_acl_entry(&self, slot: usize) -> &DefaultAclEntry {
        &self.default_acl_entries[slot]
    }

    pub(crate) fn default_acl_entries(&self) -> impl Iterator<Item = (usize, &DefaultAclEntry)> {
        self.default_acl_entries.iter().enumerate()
    }

    pub(crate) fn commit_default_acl(&mut self, slot: usize, txid: u32) {
        let entry = &mut self.default_acl_entries[slot];
        if let Some(pending) = entry.pending
            && pending.txid == txid
        {
            entry.defined = pending.defined;
            entry.privileges = pending.privileges;
            entry.grant_options = pending.grant_options;
            entry.pending = None;
            if !entry.defined {
                entry.owner = PUBLIC_ROLE;
            }
        }
    }

    pub(crate) fn restore_default_acl_pending(
        &mut self,
        slot: usize,
        prior: Option<PendingDefaultAcl>,
    ) {
        let entry = &mut self.default_acl_entries[slot];
        entry.pending = prior;
        if prior.is_none() && !entry.defined {
            entry.owner = PUBLIC_ROLE;
        }
    }

    pub(crate) fn live_default_acls(&self) -> impl Iterator<Item = (usize, &DefaultAclEntry)> {
        self.default_acl_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.defined)
    }

    pub(crate) fn live_acls(&self) -> impl Iterator<Item = (usize, &AclEntry)> {
        self.acl_entries.iter().enumerate().filter(|(_, entry)| {
            entry.live
                || (entry.object
                    == (AccessObject {
                        class: AccessClass::Schema,
                        slot: 0,
                    })
                    && entry.grantee == PUBLIC_ROLE
                    && entry.grantor == 0)
                || (matches!(
                    entry.object.class,
                    AccessClass::Domain
                        | AccessClass::Enum
                        | AccessClass::Composite
                        | AccessClass::Routine
                ) && entry.object.slot != u16::MAX
                    && entry.grantee == PUBLIC_ROLE)
        })
    }

    pub(crate) fn acl_entry(&self, slot: usize) -> &AclEntry {
        &self.acl_entries[slot]
    }

    pub(crate) fn acl_entries(&self) -> impl Iterator<Item = (usize, &AclEntry)> {
        self.acl_entries.iter().enumerate()
    }

    pub(crate) fn dependent_acl_slots(
        &self,
        object: AccessObject,
        grantor: u16,
        privileges: PrivilegeSet,
        txid: u32,
        output: &mut [usize; MAX_ACL_ENTRIES],
    ) -> usize {
        let mut count = 0usize;
        for (slot, entry) in self.acl_entries.iter().enumerate() {
            let (visible, _, entry_grantor, entry_privileges, _) = Self::acl_visible(entry, txid);
            if visible
                && entry.object == object
                && entry_grantor == grantor
                && entry_privileges.0 & privileges.0 != 0
            {
                output[count] = slot;
                count += 1;
            }
        }
        count
    }

    fn inherited_roles(&self, member: usize, txid: u32, out: &mut [bool; MAX_ROLES]) {
        if out[member] {
            return;
        }
        out[member] = true;
        if !self.role(member).attributes_to(txid).inherit {
            return;
        }
        for membership in self.role_memberships.iter() {
            if membership.visible_to(txid)
                && membership.member as usize == member
                && membership.options_to(txid).inherit
            {
                self.inherited_roles(membership.role as usize, txid, out);
            }
        }
    }

    /// Whether a role's grants are visible through the current effective role.
    /// PostgreSQL information-schema privilege views expose grants held or
    /// issued by enabled roles, not every catalog ACL entry.
    pub(crate) fn role_is_enabled(&self, role: u16, txid: u32) -> bool {
        if role == PUBLIC_ROLE {
            return true;
        }
        let Some(current) = self.current_role_slot(txid) else {
            return false;
        };
        if self.role(current).attributes_to(txid).superuser {
            return true;
        }
        let mut roles = [false; MAX_ROLES];
        self.inherited_roles(current, txid, &mut roles);
        roles.get(role as usize).copied().unwrap_or(false)
    }

    pub(crate) fn has_object_privilege(
        &self,
        object: AccessObject,
        role: usize,
        privilege: PrivilegeSet,
        txid: u32,
    ) -> bool {
        if self.role(role).attributes_to(txid).superuser || self.object_owner(object, txid) == role
        {
            return true;
        }
        let mut roles = [false; MAX_ROLES];
        self.inherited_roles(role, txid, &mut roles);
        let public_acl_defined = self.acl_entries.iter().any(|entry| {
            let (_, grantee, _, _, _) = Self::acl_visible(entry, txid);
            entry.object == object && grantee == PUBLIC_ROLE && entry.object.slot != u16::MAX
        });
        let mut effective = if !public_acl_defined {
            match object.class {
                AccessClass::Domain | AccessClass::Enum => PrivilegeSet::USAGE,
                AccessClass::Routine => PrivilegeSet::EXECUTE,
                _ => PrivilegeSet::NONE,
            }
        } else {
            self.acl_to(object, PUBLIC_ROLE, txid).0
        };
        for (slot, inherited) in roles.into_iter().enumerate() {
            if inherited {
                effective = effective.union(self.acl_to(object, slot as u16, txid).0);
            }
        }
        effective.contains(privilege)
    }

    pub(crate) fn require_schema_create(&self, schema: &str, txid: u32) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        self.require_schema_create_as(schema, role, txid)
    }

    pub(crate) fn require_schema_create_as(
        &self,
        schema: &str,
        role: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let schema_slot = self.find_schema_visible(schema, txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            )
        })?;
        let object = AccessObject {
            class: AccessClass::Schema,
            slot: schema_slot as u16,
        };
        if self.has_object_privilege(object, role, PrivilegeSet::CREATE, txid) {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for schema {}",
                schema
            ))
        }
    }

    pub(crate) fn require_schema_usage(&self, schema: &str, txid: u32) -> Result<(), SqlError> {
        if txid == 0 || schema == "pg_catalog" {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        self.require_schema_usage_as(schema, role, txid)
    }

    pub(crate) fn require_schema_usage_as(
        &self,
        schema: &str,
        role: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        if txid == 0 || schema == "pg_catalog" {
            return Ok(());
        }
        let schema_slot = self.find_schema_visible(schema, txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            )
        })?;
        let object = AccessObject {
            class: AccessClass::Schema,
            slot: schema_slot as u16,
        };
        if self.has_object_privilege(object, role, PrivilegeSet::USAGE, txid) {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for schema {}",
                schema
            ))
        }
    }

    pub(crate) fn require_type_usage(
        &self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<(), SqlError> {
        self.require_schema_usage(schema, txid)?;
        let object = self
            .domain_slot(schema, name, txid)
            .map(|slot| AccessObject {
                class: AccessClass::Domain,
                slot: slot as u16,
            })
            .or_else(|| {
                self.enum_slot(schema, name, txid).map(|slot| AccessObject {
                    class: AccessClass::Enum,
                    slot: slot as u16,
                })
            })
            .or_else(|| {
                self.composite_slot(schema, name, txid)
                    .map(|slot| AccessObject {
                        class: AccessClass::Composite,
                        slot: slot as u16,
                    })
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}.{}\" does not exist",
                    schema,
                    name
                )
            })?;
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        if self.has_object_privilege(object, role, PrivilegeSet::USAGE, txid) {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for type {}",
                name
            ))
        }
    }

    pub(crate) fn require_owner(
        &self,
        object: AccessObject,
        txid: u32,
        object_type: &str,
    ) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let owner = self.object_owner(object, txid);
        if self.role(role).attributes_to(txid).superuser
            || owner == role
            || self.role_can_set(role, owner, txid)
        {
            return Ok(());
        }
        let (_, name) = self.access_object_name(object);
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be owner of {} {}",
            object_type,
            name.as_str()
        ))
    }

    pub(crate) fn require_routine_owner(&self, slot: usize, txid: u32) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let routine = self.routine(slot);
        let owner = routine.ownership.owner_to(txid) as usize;
        if self.role(role).attributes_to(txid).superuser
            || owner == role
            || self.role_can_set(role, owner, txid)
        {
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be owner of function {}",
            routine.name_for(txid).as_str()
        ))
    }

    pub(crate) fn require_routine_execute(&self, slot: usize, txid: u32) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let object = Self::routine_access_object(slot);
        if self.has_object_privilege(object, role, PrivilegeSet::EXECUTE, txid) {
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for function {}",
            self.routine(slot).name.as_str()
        ))
    }

    pub(crate) fn has_object_grant_option(
        &self,
        object: AccessObject,
        role: usize,
        privilege: PrivilegeSet,
        txid: u32,
    ) -> bool {
        if self.role(role).attributes_to(txid).superuser || self.object_owner(object, txid) == role
        {
            return true;
        }
        let mut roles = [false; MAX_ROLES];
        self.inherited_roles(role, txid, &mut roles);
        let mut effective = self.acl_to(object, PUBLIC_ROLE, txid).1;
        for (slot, inherited) in roles.into_iter().enumerate() {
            if inherited {
                effective = effective.union(self.acl_to(object, slot as u16, txid).1);
            }
        }
        effective.contains(privilege)
    }

    pub fn create_role(
        &mut self,
        name: SqlName,
        attributes: RoleAttributes,
        txid: u32,
    ) -> Result<(usize, Option<PendingRole>), SqlError> {
        if self.find_role_visible(name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "role \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(owner) = self.roles.iter().find_map(|role| {
            (role.name == name)
                .then_some(role.pending)
                .flatten()
                .filter(|pending| pending.txid != txid)
                .map(|pending| pending.txid)
        }) {
            self.row_locks.borrow_mut().wait_for(txid, owner)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on role \"{}\"",
                name.as_str()
            ));
        }
        let Some(slot) = self
            .roles
            .iter()
            .position(|role| !role.live && role.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many roles (limit {})",
                self.roles.len()
            ));
        };
        let prior = self.roles[slot].pending;
        self.roles[slot] = RoleDef {
            name,
            attributes: RoleAttributes::ORDINARY,
            live: false,
            pending: Some(PendingRole {
                txid,
                exists: true,
                name,
                attributes,
            }),
        };
        Ok((slot, prior))
    }

    pub fn alter_role(
        &mut self,
        slot: usize,
        attributes: RoleAttributes,
        txid: u32,
    ) -> Result<Option<PendingRole>, SqlError> {
        if let Some(pending) = self.roles[slot].pending
            && pending.txid != txid
        {
            self.row_locks.borrow_mut().wait_for(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on role \"{}\"",
                self.roles[slot].name.as_str()
            ));
        }
        let prior = self.roles[slot].pending;
        let name = self.roles[slot].name_to(txid);
        self.roles[slot].pending = Some(PendingRole {
            txid,
            exists: true,
            name,
            attributes,
        });
        Ok(prior)
    }

    pub fn rename_role(
        &mut self,
        slot: usize,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingRole>, SqlError> {
        if self
            .find_role_visible(name.as_str(), txid)
            .is_some_and(|existing| existing != slot)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "role \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(pending) = self.roles[slot].pending
            && pending.txid != txid
        {
            self.row_locks.borrow_mut().wait_for(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on role \"{}\"",
                self.roles[slot].name.as_str()
            ));
        }
        let prior = self.roles[slot].pending;
        let attributes = self.roles[slot].attributes_to(txid);
        self.roles[slot].pending = Some(PendingRole {
            txid,
            exists: true,
            name,
            attributes,
        });
        Ok(prior)
    }

    pub fn drop_role_in(
        &mut self,
        slot: usize,
        txid: u32,
    ) -> Result<Option<PendingRole>, SqlError> {
        if let Some(pending) = self.roles[slot].pending
            && pending.txid != txid
        {
            self.row_locks.borrow_mut().wait_for(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on role \"{}\"",
                self.roles[slot].name.as_str()
            ));
        }
        let prior = self.roles[slot].pending;
        let name = self.roles[slot].name_to(txid);
        let attributes = self.roles[slot].attributes_to(txid);
        self.roles[slot].pending = Some(PendingRole {
            txid,
            exists: false,
            name,
            attributes,
        });
        Ok(prior)
    }

    pub fn commit_role_change(&mut self, slot: usize) {
        let Some(pending) = self.roles[slot].pending.take() else {
            return;
        };
        self.roles[slot].live = pending.exists;
        self.roles[slot].name = pending.name;
        self.roles[slot].attributes = pending.attributes;
        if !pending.exists {
            self.roles[slot].name = SqlName::EMPTY;
        }
    }

    pub fn rollback_role_change(&mut self, slot: usize, prior: Option<PendingRole>) {
        self.roles[slot].pending = prior;
        if !self.roles[slot].live && prior.is_none() {
            self.roles[slot].name = SqlName::EMPTY;
            self.roles[slot].attributes = RoleAttributes::ORDINARY;
        }
    }

    /// Committed role install used by WAL and manifest recovery.
    pub fn install_role(
        &mut self,
        name: SqlName,
        attributes: RoleAttributes,
    ) -> Result<usize, SqlError> {
        if let Some(slot) = self.find_role(name.as_str()) {
            self.roles[slot].attributes = attributes;
            return Ok(slot);
        }
        let Some(slot) = self
            .roles
            .iter()
            .position(|role| !role.live && role.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many roles (limit {})",
                self.roles.len()
            ));
        };
        self.roles[slot] = RoleDef {
            name,
            attributes,
            live: true,
            pending: None,
        };
        Ok(slot)
    }

    pub fn remove_role(&mut self, name: &str) {
        if let Some(slot) = self.find_role(name) {
            for entry in self.acl_entries.iter_mut() {
                if entry.grantee == slot as u16 || entry.grantor == slot as u16 {
                    entry.object.slot = u16::MAX;
                    entry.live = false;
                    entry.privileges = PrivilegeSet::NONE;
                    entry.grant_options = PrivilegeSet::NONE;
                    entry.pending = None;
                }
            }
            for entry in self.default_acl_entries.iter_mut() {
                if entry.owner == slot as u16 || entry.grantee == slot as u16 {
                    entry.owner = PUBLIC_ROLE;
                    entry.defined = false;
                    entry.privileges = PrivilegeSet::NONE;
                    entry.grant_options = PrivilegeSet::NONE;
                    entry.pending = None;
                }
            }
            self.roles[slot] = RoleDef {
                name: SqlName::EMPTY,
                attributes: RoleAttributes::ORDINARY,
                live: false,
                pending: None,
            };
        }
    }

    pub fn role_membership_count(&self) -> usize {
        self.role_memberships.len()
    }

    pub fn role_membership(&self, slot: usize) -> &RoleMembership {
        &self.role_memberships[slot]
    }

    pub fn live_role_memberships(&self) -> impl Iterator<Item = (usize, &RoleMembership)> {
        self.role_memberships
            .iter()
            .enumerate()
            .filter(|(_, membership)| membership.live)
    }

    pub fn find_role_membership_visible(
        &self,
        role: usize,
        member: usize,
        txid: u32,
    ) -> Option<usize> {
        self.role_memberships.iter().position(|membership| {
            membership.visible_to(txid)
                && membership.role as usize == role
                && membership.member as usize == member
        })
    }

    pub fn change_role_membership(
        &mut self,
        role: usize,
        member: usize,
        grantor: usize,
        options: RoleMembershipOptions,
        exists: bool,
        txid: u32,
    ) -> Result<(usize, Option<PendingRoleMembership>), SqlError> {
        let existing = self.role_memberships.iter().position(|membership| {
            (membership.live || membership.pending.is_some())
                && membership.role as usize == role
                && membership.member as usize == member
        });
        let slot = match existing {
            Some(slot) => slot,
            None if !exists => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role membership does not exist"
                ));
            }
            None => self
                .role_memberships
                .iter()
                .position(|membership| !membership.live && membership.pending.is_none())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many role memberships (limit {})",
                        self.role_memberships.len()
                    )
                })?,
        };
        if let Some(pending) = self.role_memberships[slot].pending
            && pending.txid != txid
        {
            self.row_locks.borrow_mut().wait_for(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent role membership DDL"
            ));
        }
        let prior = self.role_memberships[slot].pending;
        if existing.is_none() {
            self.role_memberships[slot].role = role as u16;
            self.role_memberships[slot].member = member as u16;
            self.role_memberships[slot].grantor = grantor as u16;
        }
        self.role_memberships[slot].pending = Some(PendingRoleMembership {
            txid,
            exists,
            options,
        });
        Ok((slot, prior))
    }

    pub fn commit_role_membership_change(&mut self, slot: usize) {
        let Some(pending) = self.role_memberships[slot].pending.take() else {
            return;
        };
        self.role_memberships[slot].live = pending.exists;
        self.role_memberships[slot].options = pending.options;
        if !pending.exists {
            self.role_memberships[slot].role = 0;
            self.role_memberships[slot].member = 0;
            self.role_memberships[slot].grantor = 0;
        }
    }

    pub fn rollback_role_membership_change(
        &mut self,
        slot: usize,
        prior: Option<PendingRoleMembership>,
    ) {
        self.role_memberships[slot].pending = prior;
        if !self.role_memberships[slot].live && prior.is_none() {
            self.role_memberships[slot].role = 0;
            self.role_memberships[slot].member = 0;
            self.role_memberships[slot].grantor = 0;
        }
    }

    pub fn install_role_membership(
        &mut self,
        role_name: &str,
        member_name: &str,
        grantor_name: &str,
        options: RoleMembershipOptions,
    ) -> Result<usize, SqlError> {
        let role = self.find_role(role_name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "membership role \"{}\" does not exist",
                role_name
            )
        })?;
        let member = self.find_role(member_name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "membership member \"{}\" does not exist",
                member_name
            )
        })?;
        let grantor = self.find_role(grantor_name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "membership grantor \"{}\" does not exist",
                grantor_name
            )
        })?;
        if let Some(slot) = self.find_role_membership_visible(role, member, 0) {
            self.role_memberships[slot].options = options;
            return Ok(slot);
        }
        let slot = self
            .role_memberships
            .iter()
            .position(|membership| !membership.live && membership.pending.is_none())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many role memberships (limit {})",
                    self.role_memberships.len()
                )
            })?;
        self.role_memberships[slot] = RoleMembership {
            role: role as u16,
            member: member as u16,
            grantor: grantor as u16,
            options,
            live: true,
            pending: None,
        };
        Ok(slot)
    }

    pub fn remove_role_membership(&mut self, role_name: &str, member_name: &str) {
        let (Some(role), Some(member)) = (self.find_role(role_name), self.find_role(member_name))
        else {
            return;
        };
        if let Some(slot) = self.find_role_membership_visible(role, member, 0) {
            self.role_memberships[slot].live = false;
        }
    }

    /// Whether `member` may SET ROLE to `target`, following membership edges
    /// whose SET option is true. Fixed catalog size gives the traversal a
    /// fixed stack and visited bitmap.
    pub fn role_can_set(&self, member: usize, target: usize, txid: u32) -> bool {
        self.role_reaches(member, target, txid, true)
    }

    pub fn role_is_member_of(&self, member: usize, target: usize, txid: u32) -> bool {
        self.role_reaches(member, target, txid, false)
    }

    fn role_reaches(&self, member: usize, target: usize, txid: u32, require_set: bool) -> bool {
        if member == target {
            return true;
        }
        let mut visited = [false; MAX_ROLES];
        let mut stack = [0u16; MAX_ROLES];
        let mut count = 1usize;
        stack[0] = member as u16;
        visited[member] = true;
        while count > 0 {
            count -= 1;
            let current = stack[count] as usize;
            for membership in self.role_memberships.iter() {
                if !membership.visible_to(txid)
                    || membership.member as usize != current
                    || (require_set && !membership.options_to(txid).set)
                {
                    continue;
                }
                let next = membership.role as usize;
                if next == target {
                    return true;
                }
                if !visited[next] {
                    visited[next] = true;
                    stack[count] = next as u16;
                    count += 1;
                }
            }
        }
        false
    }

    pub fn role_can_admin(&self, member: usize, target: usize, txid: u32) -> bool {
        self.role_memberships.iter().any(|membership| {
            membership.visible_to(txid)
                && membership.role as usize == target
                && membership.options_to(txid).admin
                && self.role_is_member_of(member, membership.member as usize, txid)
        })
    }

    /// Committed schemas with their slot indices, for checkpoint and catalog
    /// output.
    pub fn live_schemas(&self) -> impl Iterator<Item = (usize, &SchemaDef)> {
        self.schemas
            .iter()
            .enumerate()
            .filter(|(_, schema)| schema.ddl_state == CatalogDdlState::Present)
    }

    /// Schemas visible to `txid`, for catalog output inside a transaction.
    pub fn visible_schemas(&self, txid: u32) -> impl Iterator<Item = (usize, &SchemaDef)> {
        self.schemas
            .iter()
            .enumerate()
            .filter(move |(_, n)| n.visible_to(txid))
    }

    // --- Object comments (`COMMENT ON ...`) ---

    /// The comment text `txid` sees on this object, or `None` for none. Reads
    /// the transaction's own uncommitted overlay when present, else committed.
    pub fn comment_text(
        &self,
        class: CommentClass,
        schema: &str,
        name: &str,
        subid: u32,
        txid: u32,
    ) -> Option<&str> {
        self.comments
            .iter()
            .find(|c| c.matches_to(class, schema, name, subid, txid))
            .and_then(|c| c.visible_text(txid))
    }

    /// Committed comment entries carrying text, for the checkpoint and
    /// `pg_description`.
    pub fn live_comments(&self) -> impl Iterator<Item = &CommentEntry> {
        self.comments.iter().filter(|c| c.used && c.live.is_some())
    }

    /// Comments `txid` can see (own uncommitted overlay, else committed) that
    /// carry text, as `(class, schema, name, subid, text)` — for
    /// `pg_description`, `obj_description` and `col_description`.
    pub fn comments_visible(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (CommentClass, &str, &str, u32, &str)> {
        self.comments.iter().filter_map(move |c| {
            if !c.used {
                return None;
            }
            let name = c
                .pending_identity
                .as_ref()
                .filter(|identity| identity.txid == txid)
                .map_or(c.name.as_str(), |identity| identity.name.as_str());
            c.visible_text(txid)
                .map(|t| (c.class, c.schema.as_str(), name, c.subid, t))
        })
    }

    fn stage_trigger_comment_rename(
        &mut self,
        old_name: SqlName,
        new_name: SqlName,
        subid: u32,
        txid: u32,
    ) {
        for comment in self.comments.iter_mut().filter(|comment| {
            comment.matches_to(CommentClass::Trigger, "", old_name.as_str(), subid, txid)
        }) {
            comment.pending_identity = Some(PendingCommentIdentity {
                txid,
                name: new_name,
            });
        }
    }

    /// Sets (or, with `text == None`, removes) a comment as `txid`'s
    /// uncommitted overlay. Returns the slot and the prior overlay, for the
    /// transaction's undo log. A fresh key claims a free slot; exhausting the
    /// slab is a loud error.
    pub fn set_comment(
        &mut self,
        class: CommentClass,
        schema: SqlName,
        name: SqlName,
        subid: u32,
        text: Option<StackStr<COMMENT_MAX>>,
        txid: u32,
    ) -> Result<(usize, Option<PendingComment>), SqlError> {
        if let Some(slot) = self
            .comments
            .iter()
            .position(|c| c.matches_to(class, schema.as_str(), name.as_str(), subid, txid))
        {
            let prior = self.comments[slot].pending.take();
            self.comments[slot].pending = Some(PendingComment { txid, text });
            return Ok((slot, prior));
        }
        let Some(slot) = self.comments.iter().position(|c| !c.used) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many object comments (limit {})",
                MAX_COMMENTS
            ));
        };
        self.comments[slot] = CommentEntry {
            used: true,
            class,
            schema,
            name,
            subid,
            live: None,
            pending: Some(PendingComment { txid, text }),
            pending_identity: None,
        };
        Ok((slot, None))
    }

    /// Rollback: restores the comment slot's prior uncommitted overlay,
    /// freeing the slot if it now holds nothing.
    pub fn restore_comment_pending(&mut self, slot: usize, prior: Option<PendingComment>) {
        self.comments[slot].pending = prior;
        self.reap_comment(slot);
    }

    /// Commit: promotes `txid`'s overlay to the committed value and returns the
    /// object identity plus the new committed text, for journaling.
    pub fn commit_comment(
        &mut self,
        slot: usize,
        txid: u32,
    ) -> Option<(
        CommentClass,
        SqlName,
        SqlName,
        u32,
        Option<StackStr<COMMENT_MAX>>,
    )> {
        let entry = &mut self.comments[slot];
        match entry.pending {
            Some(p) if p.txid == txid => {
                entry.pending = None;
                entry.live = p.text;
                let out = (
                    entry.class,
                    entry.schema,
                    entry.name,
                    entry.subid,
                    entry.live,
                );
                self.reap_comment(slot);
                Some(out)
            }
            _ => None,
        }
    }

    /// Committed apply (journal replay and checkpoint load): sets the committed
    /// text directly, with no transactional overlay.
    pub fn apply_comment(
        &mut self,
        class: CommentClass,
        schema: SqlName,
        name: SqlName,
        subid: u32,
        text: Option<StackStr<COMMENT_MAX>>,
    ) -> Result<(), SqlError> {
        if let Some(slot) = self
            .comments
            .iter()
            .position(|c| c.matches(class, schema.as_str(), name.as_str(), subid))
        {
            self.comments[slot].live = text;
            self.reap_comment(slot);
            return Ok(());
        }
        if text.is_none() {
            return Ok(());
        }
        let Some(slot) = self.comments.iter().position(|c| !c.used) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many object comments (limit {})",
                MAX_COMMENTS
            ));
        };
        self.comments[slot] = CommentEntry {
            used: true,
            class,
            schema,
            name,
            subid,
            live: text,
            pending: None,
            pending_identity: None,
        };
        Ok(())
    }

    /// Drops every comment on a relation (all columns and the relation itself)
    /// or a schema, for when the object is dropped. Committed removal.
    pub fn drop_object_comments(&mut self, class: CommentClass, schema: &str, name: &str) {
        for slot in 0..self.comments.len() {
            let c = &self.comments[slot];
            if c.used
                && c.class == class
                && c.pending.is_none()
                && c.name.as_str() == name
                && c.schema.as_str() == schema
            {
                self.comments[slot].live = None;
                self.reap_comment(slot);
            }
        }
    }

    /// Frees a comment slot that holds neither a committed value nor an
    /// uncommitted overlay.
    fn reap_comment(&mut self, slot: usize) {
        let c = &mut self.comments[slot];
        if c.live.is_none() && c.pending.is_none() && c.pending_identity.is_none() {
            *c = CommentEntry::empty();
        }
    }

    /// Committed create (journal replay): the schema is immediately part of
    /// the durable image.
    pub fn create_schema(&mut self, name: SqlName) -> Result<usize, SqlError> {
        if self.find_schema(name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_SCHEMA,
                "schema \"{}\" already exists",
                name.as_str()
            ));
        }
        self.alloc_schema(name, CatalogDdlState::Present)
    }

    /// Transactional create: the schema exists only for `txid` until commit.
    pub fn create_schema_in(&mut self, name: SqlName, txid: u32) -> Result<usize, SqlError> {
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        if !self.has_current_database_create_privilege(role, txid) {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for database pos3ql"
            ));
        }
        if self.find_schema_visible(name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_SCHEMA,
                "schema \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(owner) = self.schemas.iter().find_map(|schema| {
            (schema.name.as_str() == name.as_str())
                .then_some(schema.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            self.row_locks.borrow_mut().wait_for(txid, owner)?;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on schema \"{}\"",
                name.as_str(),
            ));
        }
        self.alloc_schema(name, CatalogDdlState::PendingCreate { txid })
    }

    fn alloc_schema(
        &mut self,
        name: SqlName,
        ddl_state: CatalogDdlState,
    ) -> Result<usize, SqlError> {
        let Some(slot) = self
            .schemas
            .iter()
            .position(|schema| schema.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many schemas (limit {})",
                self.schemas.len()
            ));
        };
        let ownership = self.initial_ownership(ddl_state.pending_txid().unwrap_or(0));
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Schema,
            slot: slot as u16,
        });
        self.schemas[slot] = SchemaDef {
            name,
            ownership,
            ddl_state,
        };
        Ok(slot)
    }

    /// Committed drop (journal replay).
    pub fn drop_schema(&mut self, slot: usize) {
        let name = self.schemas[slot].name;
        self.drop_object_comments(CommentClass::Schema, "", name.as_str());
        self.schemas[slot].ddl_state = CatalogDdlState::Absent;
    }

    /// Transactional drop: the schema stays visible to other transactions
    /// until `txid` commits. The owner's own pending-create evaporates.
    pub fn drop_schema_in(&mut self, slot: usize, txid: u32) {
        let schema = &mut self.schemas[slot];
        schema.ddl_state = schema.ddl_state.drop_by(txid);
    }

    /// Promotes an uncommitted CREATE SCHEMA into the committed catalog.
    pub fn commit_schema_create(&mut self, slot: usize) {
        self.schemas[slot].ddl_state = self.schemas[slot].ddl_state.commit_create();
    }

    /// Applies a committed DROP SCHEMA.
    pub fn commit_schema_drop(&mut self, slot: usize) {
        let name = self.schemas[slot].name;
        self.drop_object_comments(CommentClass::Schema, "", name.as_str());
        self.schemas[slot].ddl_state = self.schemas[slot].ddl_state.commit_drop();
    }

    /// Rolls back an uncommitted CREATE SCHEMA, freeing the slot.
    pub fn rollback_schema_create(&mut self, slot: usize) {
        self.schemas[slot].ddl_state = self.schemas[slot].ddl_state.rollback_create();
    }

    /// Rolls back an uncommitted DROP SCHEMA: it returns to the committed
    /// image unchanged.
    pub fn rollback_schema_drop(&mut self, slot: usize, txid: u32) {
        self.schemas[slot].ddl_state = self.schemas[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn extension(&self, slot: usize) -> &ExtensionDef {
        &self.extensions[slot]
    }

    pub(crate) fn extensions_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ExtensionDef)> {
        self.extensions
            .iter()
            .enumerate()
            .filter(move |(_, extension)| extension.visible_to(txid))
    }

    pub(crate) fn live_extensions(&self) -> impl Iterator<Item = (usize, &ExtensionDef)> {
        self.extensions
            .iter()
            .enumerate()
            .filter(|(_, extension)| extension.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn extension_slot(&self, name: &str, txid: u32) -> Option<usize> {
        self.extensions
            .iter()
            .position(|extension| extension.visible_to(txid) && extension.name.as_str() == name)
    }

    pub(crate) fn create_extension(
        &mut self,
        name: SqlName,
        namespace: usize,
        relocatable: bool,
        version: ExtensionVersion,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if namespace >= self.schemas.len() || !self.schemas[namespace].visible_to(txid) {
            return Err(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema for extension \"{}\" does not exist",
                name.as_str()
            ));
        }
        if self.extension_slot(name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "extension \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(owner) = self.extensions.iter().find_map(|extension| {
            (extension.name == name)
                .then_some(extension.ddl_state.pending_txid()?)
                .filter(|owner| *owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, owner, name.as_str()));
        }
        let slot = self
            .extensions
            .iter()
            .position(|extension| extension.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many extensions (limit {})",
                    self.extensions.len()
                )
            })?;
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Extension,
            slot: slot as u16,
        });
        self.catalog_seq = self.catalog_seq.saturating_add(1);
        let created_at = self.catalog_seq;
        self.extensions[slot] = ExtensionDef {
            created_at,
            name,
            namespace: namespace as u16,
            relocatable,
            version,
            ownership: self.initial_ownership(txid),
            pending: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub(crate) fn install_extension(
        &mut self,
        name: SqlName,
        namespace: usize,
        relocatable: bool,
        version: ExtensionVersion,
        owner: usize,
        created_at: u64,
    ) -> Result<usize, SqlError> {
        if let Some(slot) = self.extension_slot(name.as_str(), 0) {
            self.extensions[slot] = ExtensionDef {
                created_at,
                name,
                namespace: namespace as u16,
                relocatable,
                version,
                ownership: Ownership {
                    owner: owner as u16,
                    pending: None,
                },
                pending: None,
                ddl_state: CatalogDdlState::Present,
            };
            return Ok(slot);
        }
        let slot = self
            .extensions
            .iter()
            .position(|extension| extension.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many extensions (limit {})",
                    self.extensions.len()
                )
            })?;
        self.extensions[slot] = ExtensionDef {
            created_at,
            name,
            namespace: namespace as u16,
            relocatable,
            version,
            ownership: Ownership {
                owner: owner as u16,
                pending: None,
            },
            pending: None,
            ddl_state: CatalogDdlState::Present,
        };
        self.catalog_seq = self.catalog_seq.max(created_at);
        Ok(slot)
    }

    pub(crate) fn alter_extension_definition(
        &mut self,
        slot: usize,
        namespace: usize,
        relocatable: bool,
        version: ExtensionVersion,
        txid: u32,
    ) -> Result<Option<PendingExtensionDefinition>, SqlError> {
        if namespace >= self.schemas.len() || !self.schemas[namespace].visible_to(txid) {
            return Err(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema does not exist"
            ));
        }
        if let Some(pending) = self.extensions[slot].pending
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.extensions[slot].name.as_str(),
            ));
        }
        let prior = self.extensions[slot].pending;
        self.extensions[slot].pending = Some(PendingExtensionDefinition {
            txid,
            namespace: namespace as u16,
            relocatable,
            version,
        });
        Ok(prior)
    }

    pub(crate) fn commit_extension_create(&mut self, slot: usize, txid: u32) {
        self.extensions[slot].ddl_state = self.extensions[slot].ddl_state.commit_create();
        self.commit_object_owner(
            AccessObject {
                class: AccessClass::Extension,
                slot: slot as u16,
            },
            txid,
        );
    }

    pub(crate) fn rollback_extension_create(&mut self, slot: usize) {
        self.extensions[slot].ddl_state = self.extensions[slot].ddl_state.rollback_create();
        self.extensions[slot].pending = None;
        self.rollback_extension_dependencies_for(slot, 0);
        self.clear_extension_configs_for(slot);
    }

    pub(crate) fn commit_extension_alter(&mut self, slot: usize, txid: u32) {
        if let Some(pending) = self.extensions[slot].pending
            && pending.txid == txid
        {
            self.extensions[slot].namespace = pending.namespace;
            self.extensions[slot].relocatable = pending.relocatable;
            self.extensions[slot].version = pending.version;
            self.extensions[slot].pending = None;
        }
    }

    pub(crate) fn rollback_extension_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingExtensionDefinition>,
    ) {
        self.extensions[slot].pending = prior;
    }

    pub(crate) fn drop_extension_in(&mut self, slot: usize, txid: u32) {
        self.extensions[slot].ddl_state = self.extensions[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn commit_extension_drop(&mut self, slot: usize) {
        let name = self.extensions[slot].name;
        self.extensions[slot].ddl_state = self.extensions[slot].ddl_state.commit_drop();
        self.extensions[slot].pending = None;
        self.drop_object_comments(CommentClass::Extension, "", name.as_str());
        let object = AccessObject {
            class: AccessClass::Extension,
            slot: slot as u16,
        };
        for dependency in self.extension_dependencies.iter_mut() {
            if dependency.extension as usize == slot || dependency.object == object {
                *dependency = ExtensionDependency::EMPTY;
            }
        }
        self.clear_extension_configs_for(slot);
    }

    pub(crate) fn rollback_extension_drop(&mut self, slot: usize, txid: u32) {
        self.extensions[slot].ddl_state = self.extensions[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn extension_dependencies_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ExtensionDependency)> {
        self.extension_dependencies
            .iter()
            .enumerate()
            .filter(move |(_, dependency)| dependency.visible_to(txid))
    }

    pub(crate) fn extension_dependency(&self, slot: usize) -> &ExtensionDependency {
        &self.extension_dependencies[slot]
    }

    fn clear_extension_dependencies_for_object(&mut self, object: AccessObject) {
        for dependency in self.extension_dependencies.iter_mut() {
            if dependency.object == object {
                *dependency = ExtensionDependency::EMPTY;
            }
        }
    }

    pub(crate) fn extension_member_of(&self, object: AccessObject, txid: u32) -> Option<usize> {
        self.extension_dependencies
            .iter()
            .find(|dependency| {
                dependency.visible_to(txid)
                    && dependency.kind == ExtensionDependencyKind::Member
                    && dependency.object == object
            })
            .map(|dependency| dependency.extension as usize)
    }

    pub(crate) fn require_not_extension_member(
        &self,
        object: AccessObject,
        txid: u32,
        kind: &str,
    ) -> Result<(), SqlError> {
        let Some(extension) = self.extension_member_of(object, txid) else {
            return Ok(());
        };
        let (_, name) = self.access_object_name_to(object, txid);
        Err(sql_err!(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop {} \"{}\" because extension \"{}\" requires it",
            kind,
            name.as_str(),
            self.extensions[extension].name.as_str()
        ))
    }

    pub(crate) fn change_extension_dependency(
        &mut self,
        extension: usize,
        object: AccessObject,
        kind: ExtensionDependencyKind,
        exists: bool,
        txid: u32,
    ) -> Result<(usize, Option<PendingExtensionDependency>), SqlError> {
        if !self.extensions[extension].visible_to(txid) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "extension does not exist"
            ));
        }
        if !self.access_object_visible_to(object, txid)
            || (object.class == AccessClass::Extension && kind != ExtensionDependencyKind::Required)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "extension dependency target does not exist"
            ));
        }
        if exists
            && kind == ExtensionDependencyKind::Member
            && let Some(other) = self.extension_member_of(object, txid)
            && other != extension
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "object is already a member of extension \"{}\"",
                self.extensions[other].name.as_str()
            ));
        }
        let existing = self.extension_dependencies.iter().position(|dependency| {
            dependency.extension as usize == extension
                && dependency.object == object
                && dependency.kind == kind
                && (dependency.live || dependency.pending.is_some())
        });
        let slot = if let Some(slot) = existing {
            slot
        } else {
            if !exists {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "object is not a member of extension \"{}\"",
                    self.extensions[extension].name.as_str()
                ));
            }
            self.extension_dependencies
                .iter()
                .position(|dependency| !dependency.live && dependency.pending.is_none())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many extension dependencies (limit {})",
                        self.extension_dependencies.len()
                    )
                })?
        };
        if let Some(pending) = self.extension_dependencies[slot].pending
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.extensions[extension].name.as_str(),
            ));
        }
        let prior = self.extension_dependencies[slot].pending;
        if existing.is_none() {
            self.extension_dependencies[slot].extension = extension as u16;
            self.extension_dependencies[slot].object = object;
            self.extension_dependencies[slot].kind = kind;
        }
        self.extension_dependencies[slot].pending =
            Some(PendingExtensionDependency { txid, exists });
        Ok((slot, prior))
    }

    pub(crate) fn commit_extension_dependency(&mut self, slot: usize, txid: u32) {
        let dependency = &mut self.extension_dependencies[slot];
        if let Some(pending) = dependency.pending
            && pending.txid == txid
        {
            dependency.live = pending.exists;
            dependency.pending = None;
            if !dependency.live {
                *dependency = ExtensionDependency::EMPTY;
            }
        }
    }

    pub(crate) fn rollback_extension_dependency(
        &mut self,
        slot: usize,
        prior: Option<PendingExtensionDependency>,
    ) {
        self.extension_dependencies[slot].pending = prior;
        if !self.extension_dependencies[slot].live && prior.is_none() {
            self.extension_dependencies[slot] = ExtensionDependency::EMPTY;
        }
    }

    fn rollback_extension_dependencies_for(&mut self, extension: usize, txid: u32) {
        for dependency in self.extension_dependencies.iter_mut() {
            if dependency.extension as usize == extension
                && (txid == 0
                    || dependency
                        .pending
                        .is_some_and(|pending| pending.txid == txid))
            {
                *dependency = ExtensionDependency::EMPTY;
            }
        }
    }

    pub(crate) fn extension_configs_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ExtensionConfig)> {
        self.extension_configs
            .iter()
            .enumerate()
            .filter(move |(_, config)| config.visible_to(txid))
    }

    pub(crate) fn extension_config(&self, slot: usize) -> &ExtensionConfig {
        &self.extension_configs[slot]
    }

    pub(crate) fn extension_config_slot(
        &self,
        extension: usize,
        relation: ExtensionConfigRelation,
        txid: u32,
    ) -> Option<usize> {
        self.extension_configs.iter().position(|config| {
            config.extension as usize == extension
                && config.relation == relation
                && config.visible_to(txid)
        })
    }

    pub(crate) fn change_extension_config(
        &mut self,
        extension: usize,
        relation: ExtensionConfigRelation,
        condition: ExtensionConfigCondition,
        exists: bool,
        txid: u32,
    ) -> Result<(usize, Option<PendingExtensionConfig>), SqlError> {
        self.change_extension_config_with_ordinal(
            extension, relation, condition, exists, None, txid,
        )
    }

    pub(crate) fn replay_extension_config(
        &mut self,
        extension: usize,
        relation: ExtensionConfigRelation,
        condition: ExtensionConfigCondition,
        exists: bool,
        ordinal: u16,
    ) -> Result<(usize, Option<PendingExtensionConfig>), SqlError> {
        self.change_extension_config_with_ordinal(
            extension,
            relation,
            condition,
            exists,
            Some(ordinal),
            0,
        )
    }

    fn change_extension_config_with_ordinal(
        &mut self,
        extension: usize,
        relation: ExtensionConfigRelation,
        condition: ExtensionConfigCondition,
        exists: bool,
        recovered_ordinal: Option<u16>,
        txid: u32,
    ) -> Result<(usize, Option<PendingExtensionConfig>), SqlError> {
        if !self.extensions[extension].visible_to(txid) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "extension does not exist"
            ));
        }
        let object = relation.access_object();
        if !self.access_object_visible_to(object, txid) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "extension configuration relation does not exist"
            ));
        }
        if exists && self.extension_member_of(object, txid) != Some(extension) {
            let (_, name) = self.access_object_name_to(object, txid);
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "table \"{}\" is not a member of the extension being created",
                name.as_str()
            ));
        }
        let existing = self.extension_configs.iter().position(|config| {
            config.extension as usize == extension
                && config.relation == relation
                && (config.live || config.pending.is_some())
        });
        let slot = if let Some(slot) = existing {
            slot
        } else {
            if !exists {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "relation is not an extension configuration relation"
                ));
            }
            self.extension_configs
                .iter()
                .position(|config| !config.live && config.pending.is_none())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many extension configuration relations (limit {})",
                        self.extension_configs.len()
                    )
                })?
        };
        if let Some(pending) = self.extension_configs[slot].pending
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.extensions[extension].name.as_str(),
            ));
        }
        let prior = self.extension_configs[slot].pending;
        if existing.is_none() {
            self.extension_configs[slot].extension = extension as u16;
            let ordinal = if let Some(ordinal) = recovered_ordinal {
                if self.extension_configs.iter().any(|config| {
                    config.extension as usize == extension
                        && (config.live || config.pending.is_some())
                        && config.ordinal == ordinal
                }) {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "duplicate extension configuration ordinal"
                    ));
                }
                ordinal
            } else {
                self.extension_configs
                    .iter()
                    .filter(|config| {
                        config.extension as usize == extension
                            && (config.live || config.pending.is_some())
                    })
                    .map(|config| config.ordinal)
                    .max()
                    .map_or(0, |ordinal| ordinal.saturating_add(1))
            };
            self.extension_configs[slot].ordinal = ordinal;
            self.extension_configs[slot].relation = relation;
        } else if let Some(ordinal) = recovered_ordinal
            && self.extension_configs[slot].ordinal != ordinal
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "extension configuration ordinal changed during recovery"
            ));
        }
        self.extension_configs[slot].pending = Some(PendingExtensionConfig {
            txid,
            exists,
            condition,
        });
        Ok((slot, prior))
    }

    pub(crate) fn commit_extension_config(&mut self, slot: usize, txid: u32) {
        let config = &mut self.extension_configs[slot];
        if let Some(pending) = config.pending
            && pending.txid == txid
        {
            config.live = pending.exists;
            config.condition = pending.condition;
            config.pending = None;
            if !config.live {
                *config = ExtensionConfig::EMPTY;
            }
        }
    }

    pub(crate) fn rollback_extension_config(
        &mut self,
        slot: usize,
        prior: Option<PendingExtensionConfig>,
    ) {
        self.extension_configs[slot].pending = prior;
        if !self.extension_configs[slot].live && prior.is_none() {
            self.extension_configs[slot] = ExtensionConfig::EMPTY;
        }
    }

    fn clear_extension_configs_for(&mut self, extension: usize) {
        for config in self.extension_configs.iter_mut() {
            if config.extension as usize == extension {
                *config = ExtensionConfig::EMPTY;
            }
        }
    }

    /// Computes the effective path a raw `search_path` value denotes for this
    /// session: `"$user"` becomes the session user, missing schemas are
    /// skipped (PostgreSQL validates lazily, not at SET), and `pg_catalog` is
    /// implicit first unless the path places it explicitly.
    pub fn compute_path(&self, raw: &str, user: &str, txid: u32) -> PathContext {
        let mut entries = [PathEntry::Catalog; MAX_PATH_ENTRIES];
        let mut n = 0;
        let mut explicit_catalog = false;
        let mut name_buf = [0u8; 63];
        // Elements split on commas outside double quotes (the stored form is
        // canonical: only double-quoted elements may embed commas).
        let mut rest = raw.trim();
        while !rest.is_empty() {
            let mut in_quotes = false;
            let mut split = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    '"' => in_quotes = !in_quotes,
                    ',' if !in_quotes => {
                        split = i;
                        break;
                    }
                    _ => {}
                }
            }
            let element = rest[..split].trim();
            rest = rest.get(split + 1..).unwrap_or("").trim_start();
            if element.is_empty() || n == MAX_PATH_ENTRIES {
                continue;
            }
            // Unquote a `"quoted name"` element ("" is an embedded quote).
            let name: &str = if element.starts_with('"') {
                let inner = element.trim_matches('"');
                let mut len = 0;
                let mut bytes = inner.bytes().peekable();
                while let Some(b) = bytes.next() {
                    if len == name_buf.len() {
                        break;
                    }
                    name_buf[len] = b;
                    len += 1;
                    if b == b'"' {
                        // "" collapses to one quote.
                        bytes.next();
                    }
                }
                core::str::from_utf8(&name_buf[..len]).unwrap_or(inner)
            } else {
                element
            };
            let name = if name == "$user" { user } else { name };
            if name == "pg_catalog" {
                if !explicit_catalog {
                    entries[n] = PathEntry::Catalog;
                    n += 1;
                    explicit_catalog = true;
                }
                continue;
            }
            if let Some(slot) = self.find_schema_visible(name, txid) {
                let entry = PathEntry::Schema(slot as u16);
                if !entries[..n].contains(&entry) {
                    entries[n] = entry;
                    n += 1;
                }
            }
        }
        if !explicit_catalog {
            // Implicit pg_catalog precedes everything, as PostgreSQL has it.
            let mut shifted = [PathEntry::Catalog; MAX_PATH_ENTRIES];
            shifted[1..=n.min(MAX_PATH_ENTRIES - 1)]
                .copy_from_slice(&entries[..n.min(MAX_PATH_ENTRIES - 1)]);
            return PathContext {
                entries: shifted,
                n: n + 1,
                explicit_catalog: false,
            };
        }
        PathContext {
            entries,
            n,
            explicit_catalog: true,
        }
    }

    pub fn path(&self) -> &PathContext {
        &self.path
    }

    /// Installs the running statement's path, returning the previous one so a
    /// nested resolution context (a view body under its creator's path) can
    /// restore it.
    pub fn swap_path(&mut self, path: PathContext) -> PathContext {
        core::mem::replace(&mut self.path, path)
    }

    /// Resolves a possibly-qualified relation name under the current path.
    /// `None` means no visible relation matches (the caller owns the 42P01
    /// wording, which differs between qualified and bare spellings).
    pub fn resolve_relation(
        &self,
        qualifier: Option<&str>,
        name: &str,
        txid: u32,
    ) -> Option<ResolvedRelation> {
        self.resolve_relation_under(&self.path, qualifier, name, txid)
    }

    /// [`Self::resolve_relation`] under an explicit path — a view body
    /// resolves under its creator's path, not the running statement's.
    pub fn resolve_relation_under(
        &self,
        path: &PathContext,
        qualifier: Option<&str>,
        name: &str,
        txid: u32,
    ) -> Option<ResolvedRelation> {
        if crate::sql::catalog::is_catalog_relation(qualifier, name) {
            return Some(ResolvedRelation::Catalog);
        }
        if let Some(schema) = qualifier {
            return self.relation_in(schema, name, txid);
        }
        for entry in path.entries() {
            match entry {
                PathEntry::Catalog => {
                    if crate::sql::catalog::is_catalog_relation(None, name) {
                        return Some(ResolvedRelation::Catalog);
                    }
                }
                PathEntry::Schema(slot) => {
                    let schema_name = self.schemas[*slot as usize].name;
                    if let Some(found) = self.relation_in(schema_name.as_str(), name, txid) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    pub(crate) fn schema_is_on_path(&self, schema: SqlName) -> bool {
        self.path.entries().iter().any(|entry| {
            matches!(entry, PathEntry::Schema(slot) if self.schemas[*slot as usize].name == schema)
        })
    }

    fn relation_in(&self, schema: &str, name: &str, txid: u32) -> Option<ResolvedRelation> {
        if let Some(t) = self.find_visible(schema, name, txid) {
            return Some(ResolvedRelation::Table(t));
        }
        self.views
            .iter()
            .position(|v| {
                v.visible_to(txid)
                    && v.schema_for(txid).as_str() == schema
                    && v.name.as_str() == name
            })
            .map(ResolvedRelation::View)
    }

    /// The kind of a relation named `name` in `schema` (visible to `txid`), or
    /// `None` if no relation of that name exists there. A materialized view
    /// shares its backing table's slot, so it is tested before a plain table.
    pub fn relation_kind_in(&self, schema: &str, name: &str, txid: u32) -> Option<StoredRelKind> {
        if self.find_matview(schema, name, txid).is_some() {
            return Some(StoredRelKind::Matview);
        }
        if self.find_visible(schema, name, txid).is_some() {
            return Some(StoredRelKind::Table);
        }
        if self.find_view(schema, name, txid).is_some() {
            return Some(StoredRelKind::View);
        }
        if self.find_sequence(schema, name, txid).is_some() {
            return Some(StoredRelKind::Sequence);
        }
        if self
            .indexes
            .iter()
            .any(|i| i.visible_to(txid) && i.schema.as_str() == schema && i.name.as_str() == name)
        {
            return Some(StoredRelKind::Index);
        }
        None
    }

    /// Resolves a possibly-qualified relation name to `(schema, kind)` under the
    /// current path, for `COMMENT ON`. PostgreSQL binds the name to the first
    /// schema on the path that holds *any* relation of that name, then checks
    /// the kind — so this returns that first match regardless of kind.
    pub fn classify_relation(
        &self,
        qualifier: Option<&str>,
        name: &str,
        txid: u32,
    ) -> Option<(SqlName, StoredRelKind)> {
        if let Some(schema) = qualifier {
            return self
                .relation_kind_in(schema, name, txid)
                .map(|k| (SqlName::parse(schema).unwrap_or(SqlName::EMPTY), k));
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema_name = self.schemas[*slot as usize].name;
                if let Some(k) = self.relation_kind_in(schema_name.as_str(), name, txid) {
                    return Some((schema_name, k));
                }
            }
        }
        None
    }

    /// The 1-based column number of `column` in the relation at `table_slot`, or
    /// `None` if the relation has no such column.
    pub fn column_number(&self, table_slot: usize, column: &str) -> Option<u32> {
        let def = &self.tables[table_slot].def;
        def.columns()
            .iter()
            .position(|c| c.name.as_str() == column)
            .map(|i| i as u32 + 1)
    }

    /// The schema a new relation lands in: the qualifier if it names a
    /// visible schema, else the first schema of the path. `relation` is only
    /// for the error message.
    pub fn creation_schema(
        &self,
        qualifier: Option<&str>,
        relation: &str,
        txid: u32,
    ) -> Result<SqlName, SqlError> {
        if let Some(schema) = qualifier {
            if schema == "pg_catalog" || schema == "information_schema" {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::INSUFFICIENT_PRIVILEGE,
                    "permission denied to create \"{}.{}\"",
                    schema,
                    relation
                ));
            }
            if self.find_schema_visible(schema, txid).is_none() {
                return Err(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema
                ));
            }
            return SqlName::parse(schema);
        }
        // An explicit pg_catalog at the head of the path is the creation
        // target, which PostgreSQL then refuses.
        if self.path.explicit_catalog && self.path.entries().first() == Some(&PathEntry::Catalog) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to create \"pg_catalog.{}\"",
                relation
            ));
        }
        let Some(slot) = self.path.first_schema() else {
            return Err(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "no schema has been selected to create in"
            ));
        };
        Ok(self.schemas[slot as usize].name)
    }

    /// Live tables with their slot indices.
    pub fn live_tables(&self) -> impl Iterator<Item = (usize, &Table)> {
        self.tables.iter().enumerate().filter(|(_, t)| t.live)
    }

    /// Floors every serial column's sequence at the maximum value stored in
    /// its rows. Run once after recovery: a journal or checkpoint written
    /// before sequences were journaled carries no positions, and handing out
    /// a value at or below an existing row's would violate the key.
    pub fn reconcile_serials(&mut self) {
        for i in 0..self.tables.len() {
            if !self.tables[i].live {
                continue;
            }
            let n_columns = self.tables[i].def.n_columns;
            let mut auto = [false; MAX_COLUMNS];
            let mut any = false;
            for (c, slot) in auto.iter_mut().enumerate().take(n_columns) {
                *slot = self.tables[i].def.columns()[c].auto_increment;
                any |= *slot;
            }
            if !any {
                continue;
            }
            let mut schema = [crate::sql::types::ColType::Bool; MAX_COLUMNS];
            self.tables[i].def.schema(&mut schema);
            let mut max = [0i64; MAX_COLUMNS];
            let mut rowids: Vec<(u64, RowHome)> = Vec::new();
            for (&rowid, state) in self.tables[i].rows.iter() {
                if let Some(home) = state.visible_to(0) {
                    rowids.push((rowid, home));
                }
            }
            for (rowid, home) in rowids {
                let mut vals = [0i64; MAX_COLUMNS];
                let mut have = [false; MAX_COLUMNS];
                self.with_row_bytes(i, rowid, home, |bytes| {
                    let mut row = [crate::sql::types::Datum::Null; MAX_COLUMNS];
                    if rowenc::decode(bytes, &schema[..n_columns], &mut row).is_err() {
                        return Ok(());
                    }
                    for c in 0..n_columns {
                        if !auto[c] {
                            continue;
                        }
                        let v = match row[c] {
                            crate::sql::types::Datum::Int2(x) => i64::from(x),
                            crate::sql::types::Datum::Int4(x) => i64::from(x),
                            crate::sql::types::Datum::Int8(x) => x,
                            _ => continue,
                        };
                        vals[c] = v;
                        have[c] = true;
                    }
                    Ok(())
                })
                .unwrap_or(());
                for c in 0..n_columns {
                    if have[c] {
                        max[c] = max[c].max(vals[c]);
                    }
                }
            }
            for c in 0..n_columns {
                if auto[c] {
                    self.tables[i].serial_last[c] = self.tables[i].serial_last[c].max(max[c]);
                }
            }
        }
    }

    /// Attaches the spilled-row read path (engine setup, object storage on).
    pub(crate) fn attach_spill(&mut self, reader: SpillReader) {
        self.spill = Some(reader);
    }

    pub fn spill_attached(&self) -> bool {
        self.spill.is_some()
    }

    /// Cumulative traffic through the provider-neutral block stack.
    ///
    /// A database without object storage has no spill stack and therefore
    /// reports zeros. Callers compare snapshots around an operation; the
    /// counters themselves are deliberately process-local observability.
    pub(crate) fn block_io_stats(&self) -> crate::store::BlockIoStats {
        self.spill
            .as_ref()
            .map(|reader| reader.blocks.borrow().io_stats())
            .unwrap_or_default()
    }

    /// Leases one startup-sized external-run producer. A nested materializer
    /// gets a distinct producer instead of resetting an outer run in progress.
    pub(crate) fn external_sorter(
        &self,
    ) -> Result<
        std::cell::RefMut<'_, Box<crate::sql::external::ExternalSorter>>,
        crate::sql::eval::SqlError,
    > {
        let Some(spill) = self.spill.as_ref() else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "external query runs require durable object storage"
            ));
        };
        for sorter in spill.external_sorters.iter() {
            if let Ok(lease) = sorter.try_borrow_mut() {
                return Ok(lease);
            }
        }
        Err(crate::sql_err!(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "external query run producer pool exhausted (maximum nesting {})",
            EXTERNAL_RUN_CONTEXTS
        ))
    }

    /// Leases one independent immutable-run cursor. Exhaustion is loud instead
    /// of aliasing scratch across nested materializers.
    pub(crate) fn external_run_reader(
        &self,
    ) -> Result<
        std::cell::RefMut<'_, crate::sql::external::ExternalRunReader>,
        crate::sql::eval::SqlError,
    > {
        let Some(spill) = self.spill.as_ref() else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "external query runs require durable object storage"
            ));
        };
        for reader in spill.external_readers.iter() {
            if let Ok(lease) = reader.try_borrow_mut() {
                return Ok(lease);
            }
        }
        Err(crate::sql_err!(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "external query run reader pool exhausted (maximum nesting {})",
            EXTERNAL_RUN_CONTEXTS
        ))
    }

    /// Returns an owned capability for consuming immutable external runs
    /// without retaining an immutable borrow of the mutable database state.
    pub(crate) fn external_run_access(
        &self,
    ) -> Result<ExternalRunAccess, crate::sql::eval::SqlError> {
        let Some(spill) = self.spill.as_ref() else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "external query runs require durable object storage"
            ));
        };
        Ok(ExternalRunAccess {
            blocks: std::rc::Rc::as_ptr(&spill.blocks),
            readers: std::rc::Rc::as_ptr(&spill.external_readers),
        })
    }

    /// Runs one short operation against the provider-neutral tiered block
    /// stack. The borrow must not cross a source-row callback: a spilled scan
    /// releases its own block context before invoking that callback precisely
    /// so execution work can issue nested reads or writes here.
    pub(crate) fn with_block_store<R>(
        &self,
        operation: impl FnOnce(&mut dyn crate::store::BlockStore) -> R,
    ) -> Option<R> {
        let reader = self.spill.as_ref()?;
        let mut blocks = reader.blocks.borrow_mut();
        Some(operation(&mut *blocks))
    }

    /// Number of immutable row generations currently backing a table.
    pub(crate) fn spill_generation_count(&self, table_slot: usize) -> usize {
        self.tables[table_slot].n_spill_ssts
    }

    /// The bytes of a visible row, wherever they live: a heap row borrows the
    /// heap directly; a spilled row is fetched through the cache tiers into
    /// `arena`. The two lifetimes unify, so call sites keep their shapes.
    /// The merged walk behind the row-state seam: every SST-resident rowid
    /// of `slot`'s spill list that no map entry shadows, in ascending rowid
    /// order — the newest member's verdict wins a rowid, and a tombstone
    /// verdict suppresses it. Cursors keep one resident data block per
    /// member (leased from the spill reader's context pool), advancing
    /// through the sparse index; only keys are parsed here — row bytes are
    /// fetched later, by `row_bytes`, exactly as for any spilled row.
    fn spill_merged_walk(
        &self,
        slot: usize,
        emit: &mut dyn FnMut(u64, u32, u8, u64) -> Result<core::ops::ControlFlow<()>, SqlError>,
    ) -> Result<(), SqlError> {
        let table = &self.tables[slot];
        let n = table.n_spill_ssts;
        if n == 0 {
            return Ok(());
        }
        let Some(spill) = &self.spill else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table has spill SSTs but no spill reader is attached"
            ));
        };
        let walk_id = spill.next_walk_id.get();
        spill.next_walk_id.set(walk_id.wrapping_add(1).max(1));
        let mut cursors = [MemberCursor {
            ordinal: 0,
            offset: 0,
            head_offset: 0,
            loaded: None,
            loaded_len: 0,
            raw_len: 0,
            raw_row: 0,
            head_raw_row: 0,
            pax_layout: None,
            loaded_type: None,
            prefetched_leaf: None,
            prefetched_data: None,
            head: None,
            done: false,
        }; MAX_SPILL_SSTS];
        {
            let Some(mut context) = spill
                .scan_contexts
                .iter()
                .find_map(|candidate| candidate.try_borrow_mut().ok())
            else {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row-state block context is already in use"
                ));
            };
            context.owner = walk_id;
            context.pax_values_owner = None;
            for (member, cursor) in cursors[..n].iter_mut().enumerate() {
                Self::cursor_advance(spill, table, member, cursor, &mut context)?;
            }
        }
        loop {
            let mut min: Option<u64> = None;
            for cursor in cursors[..n].iter() {
                if let Some((key, ..)) = cursor.head {
                    min = Some(min.map_or(key.rowid, |rowid: u64| rowid.min(key.rowid)));
                }
            }
            let Some(rowid) = min else { return Ok(()) };
            let Some(mut context) = spill
                .scan_contexts
                .iter()
                .find_map(|candidate| candidate.try_borrow_mut().ok())
            else {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row-state block context is already in use"
                ));
            };
            if context.owner != walk_id {
                // A nested walk ran while this walk's row callback was active
                // and reused the buffers. Parsed heads are owned values and
                // remain valid; only buffer-residency claims are stale.
                for cursor in &mut cursors[..n] {
                    cursor.loaded = None;
                    cursor.loaded_len = 0;
                    cursor.loaded_type = None;
                }
                context.owner = walk_id;
                context.pax_values_owner = None;
            }
            // Consume every version of this row from every member. The
            // greatest commit LSN admitted by the statement snapshot wins;
            // equal keys prefer the newer list member.
            let mut verdict: Option<SpillVersion> = None;
            for (member, cursor) in cursors[..n].iter_mut().enumerate() {
                while let Some((key, tombstone, len)) = cursor.head
                    && key.rowid == rowid
                {
                    if key.commit_lsn <= self.commit_snapshot
                        && verdict.is_none_or(|current| {
                            key.commit_lsn > current.commit_lsn
                                || (key.commit_lsn == current.commit_lsn
                                    && member as u8 > current.member)
                        })
                    {
                        verdict = Some(SpillVersion {
                            len: (!tombstone).then_some(len),
                            member: member as u8,
                            commit_lsn: key.commit_lsn,
                        });
                    }
                    Self::cursor_advance(spill, table, member, cursor, &mut context)?;
                }
            }
            drop(context);
            if let Some(SpillVersion {
                len: Some(len),
                member,
                commit_lsn,
            }) = verdict
                && self.tables[slot].rows.get(&rowid).is_none()
                && emit(rowid, len, member, commit_lsn)?.is_break()
            {
                return Ok(());
            }
        }
    }

    /// The payload-carrying form of [`Self::spill_merged_walk`]. A sequential
    /// scan already has the winning entry's data block resident while it
    /// merges versions. Copying that entry into statement storage before the
    /// cursor advances avoids turning every row in the block into a second
    /// SST point lookup. The context is released before `emit`, so nested
    /// execution still uses the normal bounded reader pool.
    fn spill_merged_walk_bytes<'a>(
        &self,
        slot: usize,
        arena: &'a crate::mem::arena::Arena,
        recycle_rows: bool,
        decoded_columns: Option<&[bool; MAX_COLUMNS]>,
        emit: &mut dyn FnMut(
            u64,
            SpilledRowRepresentation<'a>,
        ) -> Result<core::ops::ControlFlow<()>, SqlError>,
    ) -> Result<(), SqlError> {
        let table = &self.tables[slot];
        let n = table.n_spill_ssts;
        if n == 0 {
            return Ok(());
        }
        let Some(spill) = &self.spill else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table has spill SSTs but no spill reader is attached"
            ));
        };
        let walk_id = spill.next_walk_id.get();
        spill.next_walk_id.set(walk_id.wrapping_add(1).max(1));
        let mut cursors = [MemberCursor {
            ordinal: 0,
            offset: 0,
            head_offset: 0,
            loaded: None,
            loaded_len: 0,
            raw_len: 0,
            raw_row: 0,
            head_raw_row: 0,
            pax_layout: None,
            loaded_type: None,
            prefetched_leaf: None,
            prefetched_data: None,
            head: None,
            done: false,
        }; MAX_SPILL_SSTS];
        {
            let Some(mut context) = spill
                .scan_contexts
                .iter()
                .find_map(|candidate| candidate.try_borrow_mut().ok())
            else {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row-state block context is already in use"
                ));
            };
            context.owner = walk_id;
            context.pax_values_owner = None;
            for (member, cursor) in cursors[..n].iter_mut().enumerate() {
                Self::cursor_advance(spill, table, member, cursor, &mut context)?;
            }
        }
        loop {
            let mark = recycle_rows.then(|| arena.mark());
            let mut min: Option<u64> = None;
            for cursor in cursors[..n].iter() {
                if let Some((key, ..)) = cursor.head {
                    min = Some(min.map_or(key.rowid, |rowid: u64| rowid.min(key.rowid)));
                }
            }
            let Some(rowid) = min else { return Ok(()) };
            let Some(mut context) = spill
                .scan_contexts
                .iter()
                .find_map(|candidate| candidate.try_borrow_mut().ok())
            else {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row-state block context is already in use"
                ));
            };
            if context.owner != walk_id {
                for cursor in &mut cursors[..n] {
                    cursor.loaded = None;
                    cursor.loaded_len = 0;
                    cursor.loaded_type = None;
                    cursor.pax_layout = None;
                    if cursor.head.is_some() {
                        cursor.raw_row = cursor.head_raw_row;
                        cursor.head = None;
                    }
                }
                context.owner = walk_id;
                context.pax_values_owner = None;
                for (member, cursor) in cursors[..n].iter_mut().enumerate() {
                    if !cursor.done {
                        Self::cursor_advance(spill, table, member, cursor, &mut context)?;
                    }
                }
            }
            let mut verdict: Option<SpillVersion> = None;
            for (member, cursor) in cursors[..n].iter().enumerate() {
                if let Some((key, tombstone, len)) = cursor.head
                    && key.rowid == rowid
                    && key.commit_lsn <= self.commit_snapshot
                    && verdict.is_none_or(|current| {
                        key.commit_lsn > current.commit_lsn
                            || (key.commit_lsn == current.commit_lsn
                                && member as u8 > current.member)
                    })
                {
                    verdict = Some(SpillVersion {
                        len: (!tombstone).then_some(len),
                        member: member as u8,
                        commit_lsn: key.commit_lsn,
                    });
                }
            }
            let representation = if let Some(SpillVersion {
                len: Some(len),
                member,
                commit_lsn,
            }) = verdict
                && self.tables[slot].rows.get(&rowid).is_none()
            {
                let cursor = &cursors[member as usize];
                let (key, tombstone, _copied) = cursor.head.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "selected spill version has no resident cursor head"
                    )
                })?;
                let representation = if cursor.loaded_type
                    == Some(crate::store::BlockType::SstDataPaxV2)
                {
                    let layout = cursor.pax_layout.ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX descriptor has no validated layout"
                        )
                    })?;
                    let owner = (member as usize, cursor.ordinal);
                    if context.pax_values_owner != Some(owner) {
                        let ScanContext {
                            member_blocks,
                            member_raw_blocks,
                            pax_column_buf,
                            pax_values_buf,
                            pax_value_extents,
                            pax_values_owner,
                            pax_row_buf,
                            ..
                        } = &mut *context;
                        let mut blocks = spill.blocks.borrow_mut();
                        let mut at = 0usize;
                        pax_value_extents.fill(None);
                        for column in 0..layout.columns() {
                            if decoded_columns.is_some_and(|columns| !columns[column]) {
                                continue;
                            }
                            let reference = layout
                                .column_ref(
                                    &member_raw_blocks[member as usize][..cursor.raw_len],
                                    column,
                                )
                                .map_err(spill_read_error)?;
                            let (len, block_type) = crate::store::read_data_block_raw_ref(
                                &mut *blocks,
                                reference,
                                pax_column_buf,
                                &mut member_blocks[member as usize],
                            )
                            .map_err(spill_read_error)?;
                            if block_type != crate::store::BlockType::SstDataPaxColumnV1 {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "PAX descriptor column reference has the wrong block type"
                                ));
                            }
                            let end = at.checked_add(len).ok_or_else(|| {
                                sql_err!(sqlstate::INTERNAL_ERROR, "PAX extent length overflows")
                            })?;
                            if end > pax_values_buf.len() {
                                return Err(sql_err!(
                                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                    "PAX column extents exceed the fixed scan vector buffer"
                                ));
                            }
                            pax_values_buf[at..end].copy_from_slice(&pax_column_buf[..len]);
                            pax_value_extents[column] = Some((at, end));
                            at = end;
                        }
                        *pax_values_owner = Some(owner);
                        let _ = (member_raw_blocks, pax_row_buf);
                    }
                    let encoded_len = {
                        let ScanContext {
                            member_raw_blocks,
                            pax_values_buf,
                            pax_value_extents,
                            pax_row_buf,
                            ..
                        } = &mut *context;
                        crate::store::copy_pax_v2_row_from_extents(
                            &layout,
                            &member_raw_blocks[member as usize][..cursor.raw_len],
                            cursor.head_raw_row,
                            decoded_columns,
                            pax_values_buf,
                            pax_value_extents,
                            pax_row_buf,
                        )
                        .map_err(spill_read_error)?
                    };
                    if decoded_columns.is_none() && encoded_len != len as usize {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX descriptor row length does not match its cursor header"
                        ));
                    }
                    let mut schema = [ColType::Bool; MAX_COLUMNS];
                    table.def.schema(&mut schema);
                    let encoded = arena.alloc_slice_with(encoded_len, |_| 0u8).map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "spilled PAX rows exceed the statement arena; raise work_arena_bytes"
                        )
                    })?;
                    encoded.copy_from_slice(&context.pax_row_buf[..encoded_len]);
                    let values = arena
                        .alloc_slice_with(layout.columns(), |_| Datum::Null)
                        .map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "spilled PAX values exceed the statement arena; raise work_arena_bytes"
                            )
                        })?;
                    rowenc::decode(encoded, &schema[..layout.columns()], values)?;
                    SpilledRowRepresentation::Values(&*values)
                } else {
                    let output = arena.alloc_slice_with(len as usize, |_| 0u8).map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "spilled scan rows exceed the statement arena; raise work_arena_bytes"
                        )
                    })?;
                    let (copied_key, copied_tombstone, copied) = {
                        let mut blocks = spill.blocks.borrow_mut();
                        crate::store::copy_block_entry_at(
                            &mut *blocks,
                            &context.member_blocks[member as usize][..cursor.loaded_len],
                            cursor.head_offset,
                            output,
                        )
                        .map_err(spill_read_error)?
                    };
                    if copied_key != key || copied_tombstone != tombstone {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "merged spill cursor payload does not match its selected version"
                        ));
                    }
                    if copied != len as usize {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "merged spill cursor payload length does not match its selected version"
                        ));
                    }
                    SpilledRowRepresentation::Encoded(&*output)
                };
                if tombstone || key.rowid != rowid || key.commit_lsn != commit_lsn {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "merged spill cursor payload does not match its selected version"
                    ));
                }
                Some(representation)
            } else {
                None
            };
            for (member, cursor) in cursors[..n].iter_mut().enumerate() {
                while cursor.head.is_some_and(|(key, ..)| key.rowid == rowid) {
                    Self::cursor_advance(spill, table, member, cursor, &mut context)?;
                }
            }
            drop(context);
            let emitted = if let Some(representation) = representation {
                emit(rowid, representation)
            } else {
                Ok(core::ops::ControlFlow::Continue(()))
            };
            if let Some(mark) = mark {
                // SAFETY: recycling callers consume the copied row and all
                // values derived from it synchronously in `emit`.
                unsafe { arena.rewind_to(mark) };
            }
            if emitted?.is_break() {
                return Ok(());
            }
        }
    }

    /// Streams every spill-only row in bounded batches with bytes already
    /// carried by the merged data-block cursor. Overlay rows are intentionally
    /// excluded: they remain in the row-state seam, where transaction
    /// visibility is resolved. The outer physical scan combines these rows
    /// with that seam in its established physical order.
    pub(crate) fn for_each_spilled_row_batch<'a, 'callback>(
        &self,
        table_slot: usize,
        arena: &'a crate::mem::arena::Arena,
        recycle_rows: bool,
        decoded_columns: Option<u64>,
        each: &mut SpilledRowBatchVisitor<'a, 'callback>,
    ) -> Result<(), SqlError> {
        let decoded_columns =
            decoded_columns.map(|mask| core::array::from_fn(|column| mask & (1u64 << column) != 0));
        let rows = arena
            .alloc_slice_with(SPILL_SCAN_BATCH_ROWS, |_| SpilledRow {
                rowid: 0,
                representation: SpilledRowRepresentation::Encoded(&[]),
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "spilled scan batch exceeds the statement arena; raise work_arena_bytes"
                )
            })?;
        let rows = &mut *rows;
        let batch_mark = recycle_rows.then(|| arena.mark());
        let mut len = 0usize;
        let mut stopped = false;
        let mut result = self.spill_merged_walk_bytes(
            table_slot,
            arena,
            false,
            decoded_columns.as_ref(),
            &mut |rowid, representation| {
                rows[len] = SpilledRow {
                    rowid,
                    representation,
                };
                len += 1;
                if len != rows.len() {
                    return Ok(core::ops::ControlFlow::Continue(()));
                }
                match each(&rows[..len])? {
                    core::ops::ControlFlow::Continue(()) => {
                        if let Some(mark) = batch_mark {
                            // SAFETY: `each` consumed this batch synchronously.
                            unsafe { arena.rewind_to(mark) };
                        }
                        len = 0;
                        Ok(core::ops::ControlFlow::Continue(()))
                    }
                    core::ops::ControlFlow::Break(()) => {
                        stopped = true;
                        Ok(core::ops::ControlFlow::Break(()))
                    }
                }
            },
        );
        if result.is_ok() && !stopped && len != 0 {
            result = each(&rows[..len]).map(|_| ());
        }
        if let Some(mark) = batch_mark {
            // SAFETY: the batch callback consumes every row synchronously.
            unsafe { arena.rewind_to(mark) };
        }
        result
    }

    /// Whether the spill list has no resident row-state overlay. In that
    /// state a physical scan can stream its already-merged rows directly;
    /// any overlay requires the ordinary row-state seam to preserve MVCC
    /// shadowing and mixed physical ordering.
    pub(crate) fn spill_rows_are_unshadowed(&self, table_slot: usize) -> bool {
        self.tables[table_slot].rows.is_empty()
    }

    /// Whether an unshadowed sequential spill walk costs no more durable block
    /// traffic than a single-column index probe. Both estimates use only
    /// manifest and ANALYZE metadata, so choosing the access path cannot warm
    /// the cache or perform a speculative read.
    pub(crate) fn sequential_spill_scan_is_cheaper(
        &self,
        table_slot: usize,
        expected_rows: u64,
        txid: u32,
    ) -> bool {
        if !self.spill_rows_are_unshadowed(table_slot) {
            return false;
        }
        let generations = self.spill_generation_count(table_slot) as u64;
        if generations == 0 {
            return false;
        }
        let statistics = self.table_statistics(table_slot, txid);
        let rows = self.planning_row_estimate(table_slot);
        let width = if statistics.valid {
            statistics.average_row_width.max(1)
        } else {
            32
        };
        let full_scan_blocks = rows
            .saturating_mul(u64::from(width))
            .div_ceil(crate::store::MAX_PAYLOAD as u64)
            .saturating_add(generations.saturating_mul(2));
        let index_blocks = generations.saturating_mul(3).saturating_add(
            expected_rows
                .saturating_mul(u64::from(width))
                .div_ceil(crate::store::MAX_PAYLOAD as u64),
        );
        full_scan_blocks <= index_blocks
    }

    /// Steps one member cursor to its next entry, loading blocks (through
    /// the cache tiers) as it crosses block boundaries.
    fn cursor_advance(
        spill: &SpillReader,
        table: &Table,
        member: usize,
        cursor: &mut MemberCursor,
        context: &mut ScanContext,
    ) -> Result<(), SqlError> {
        cursor.head = None;
        if cursor.done {
            return Ok(());
        }
        let handle = table.spill_ssts[member].expect("cursor members exist");
        loop {
            if let Some((ordinal, leaf)) = cursor.prefetched_leaf {
                let mut blocks = spill.blocks.borrow_mut();
                if let Some(reference) = crate::store::take_prefetched_index_first_data(
                    &mut *blocks,
                    &leaf,
                    &mut context.index_buf,
                    handle.packed,
                )
                .map_err(spill_read_error)?
                {
                    cursor.prefetched_leaf = None;
                    if let crate::store::DataBlockRef::Direct(id) = reference {
                        crate::store::prefetch_data_block(&mut *blocks, Some(id))
                            .map_err(spill_read_error)?;
                        cursor.prefetched_data = Some((ordinal, reference));
                    }
                }
            }
            if cursor.loaded != Some(cursor.ordinal) {
                let resume_raw_row = cursor.raw_row;
                let mut blocks = spill.blocks.borrow_mut();
                // Both index shapes resolve through one helper; the index
                // buffer is scratch for the descent and the decompression
                // bounce alike.
                let id = if let Some((ordinal, id)) = cursor.prefetched_data
                    && ordinal == cursor.ordinal
                {
                    cursor.prefetched_data = None;
                    id
                } else {
                    let Some((id, next)) = crate::store::locate_data_block_with_next(
                        &mut *blocks,
                        &handle,
                        &mut context.index_buf,
                        cursor.ordinal,
                    )
                    .map_err(spill_read_error)?
                    else {
                        cursor.done = true;
                        return Ok(());
                    };
                    match next {
                        Some(crate::store::DataBlockLookahead::Data(next)) => {
                            if let crate::store::DataBlockRef::Direct(next_id) = next {
                                crate::store::prefetch_data_block(&mut *blocks, Some(next_id))
                                    .map_err(spill_read_error)?;
                            }
                            cursor.prefetched_data = Some((cursor.ordinal + 1, next));
                        }
                        Some(crate::store::DataBlockLookahead::Leaf(leaf)) => {
                            crate::store::BlockStore::prefetch(&mut *blocks, &leaf).map_err(
                                |error| spill_read_error(crate::store::SstError::Store(error)),
                            )?;
                            cursor.prefetched_leaf = Some((cursor.ordinal + 1, leaf));
                        }
                        None => {}
                    }
                    id
                };
                let (raw_len, loaded_type) = crate::store::read_data_block_raw_ref(
                    &mut *blocks,
                    id,
                    &mut context.member_raw_blocks[member],
                    &mut context.member_blocks[member],
                )
                .map_err(spill_read_error)?;
                let loaded_len = if loaded_type == crate::store::BlockType::SstDataPaxV2 {
                    0
                } else {
                    crate::store::decode_data_block(
                        &context.member_raw_blocks[member][..raw_len],
                        loaded_type,
                        &mut context.member_blocks[member],
                    )
                    .map_err(spill_read_error)?
                };
                cursor.loaded_len = loaded_len;
                cursor.raw_len = raw_len;
                cursor.raw_row = 0;
                cursor.pax_layout = (loaded_type == crate::store::BlockType::SstDataPaxV2)
                    .then(|| {
                        crate::store::pax_layout(&context.member_raw_blocks[member][..raw_len])
                    })
                    .transpose()
                    .map_err(spill_read_error)?;
                if cursor.pax_layout.is_some() && cursor.head.is_some() {
                    cursor.raw_row = resume_raw_row;
                }
                cursor.loaded_type = Some(loaded_type);
                cursor.loaded = Some(cursor.ordinal);
                cursor.offset = 0;
            }
            if cursor.loaded_type == Some(crate::store::BlockType::SstDataPaxV2) {
                let layout = cursor.pax_layout.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "PAX block has no validated layout"
                    )
                })?;
                if cursor.raw_row == layout.rows() {
                    cursor.ordinal += 1;
                    continue;
                }
                let row = cursor.raw_row;
                let (key, tombstone) = layout
                    .row_key(&context.member_raw_blocks[member][..cursor.raw_len], row)
                    .map_err(spill_read_error)?;
                let len = if tombstone {
                    0
                } else {
                    layout
                        .row_len(&context.member_raw_blocks[member][..cursor.raw_len], row)
                        .map_err(spill_read_error)?
                };
                cursor.head_raw_row = row;
                cursor.raw_row += 1;
                cursor.head_offset = 0;
                cursor.head = Some((key, tombstone, len));
                return Ok(());
            }
            let head_offset = cursor.offset;
            if cursor.loaded_type.is_none() || cursor.raw_len == 0 {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "spill cursor advanced without a complete loaded block state"
                ));
            }
            match crate::store::block_keys_at(
                &context.member_blocks[member][..cursor.loaded_len],
                head_offset,
            ) {
                Some((key, tombstone, len, next)) => {
                    cursor.offset = next;
                    cursor.head_offset = head_offset;
                    cursor.head = Some((key, tombstone, len));
                    return Ok(());
                }
                None => {
                    cursor.ordinal += 1;
                }
            }
        }
    }

    /// The newest object-resident version admitted by `snapshot`. Every SST
    /// is consulted because a newer run may contain only too-new versions;
    /// a tombstone remains a first-class verdict.
    fn spill_probe_at(
        &self,
        slot: usize,
        rowid: u64,
        snapshot: u64,
    ) -> Result<Option<SpillVersion>, SqlError> {
        let table = &self.tables[slot];
        if table.n_spill_ssts == 0 {
            return Ok(None);
        }
        let Some(spill) = &self.spill else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table has spill SSTs but no spill reader is attached"
            ));
        };
        let Some(mut scratch) = spill.scratch.iter().find_map(|c| c.try_borrow_mut().ok()) else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "spill fetches nested deeper than the reader scratch"
            ));
        };
        let scratch = &mut *scratch;
        // This multi-SST probe reuses the decoded buffer without preserving a
        // single row block, so it invalidates the point-reader cache.
        scratch.decoded_data_ref = None;
        let mut reader = crate::store::SstReader::over(
            &mut scratch.index_buf,
            &mut scratch.data_buf,
            &mut scratch.decoded_buf,
            &mut scratch.column_buf,
            &mut scratch.assembly_buf,
        );
        let mut best: Option<SpillVersion> = None;
        for member in 0..table.n_spill_ssts {
            let handle = table.spill_ssts[member].expect("counted");
            let verdict = reader
                .probe_at(&mut *spill.blocks.borrow_mut(), &handle, rowid, snapshot)
                .map_err(spill_read_error)?;
            if let Some(probe) = verdict
                && best.is_none_or(|current| {
                    probe.key.commit_lsn > current.commit_lsn
                        || (probe.key.commit_lsn == current.commit_lsn
                            && member as u8 > current.member)
                })
            {
                best = Some(SpillVersion {
                    len: probe.len,
                    member: member as u8,
                    commit_lsn: probe.key.commit_lsn,
                });
            }
        }
        Ok(best)
    }

    /// The one place a table's row states are enumerated. The bounded map is
    /// an overlay of pending changes and hot/resident rows; the merged SST
    /// walk synthesizes every remaining snapshot-visible state directly from
    /// the provider-neutral object store through the cache tiers. The callback
    /// therefore takes state by value, and errors stay visible to every SQL
    /// consumer.
    ///
    /// `Break` stops the walk early; the callback's own error aborts it.
    pub fn for_each_row_state(
        &self,
        table_slot: usize,
        each: &mut dyn FnMut(u64, RowState) -> Result<core::ops::ControlFlow<()>, SqlError>,
    ) -> Result<(), SqlError> {
        // The overlay first: pending changes and hot rows, whose entries
        // shadow anything the spill list holds for the same rowid.
        for (&rowid, state) in self.tables[table_slot].rows.iter() {
            if each(rowid, *state)?.is_break() {
                return Ok(());
            }
        }
        // Then everything that lives only in the bucket, synthesized.
        self.spill_merged_walk(table_slot, &mut |rowid, len, member, commit_lsn| {
            each(
                rowid,
                RowState {
                    committed: Some(RowHome::Spilled {
                        len,
                        sst: member,
                        commit_lsn,
                    }),
                    committed_lsn: commit_lsn,
                    history: CommittedHistory::empty(),
                    pending: PendingVersions::empty(),
                },
            )
        })
    }

    /// One row's state by id, through the same seam as the enumeration.
    pub fn row_state(&self, table_slot: usize, rowid: u64) -> Result<Option<RowState>, SqlError> {
        if let Some(state) = self.tables[table_slot].rows.get(&rowid) {
            return Ok(Some(*state));
        }
        Ok(self
            .spill_probe_at(table_slot, rowid, u64::MAX)?
            .and_then(|version| {
                version.len.map(|len| RowState {
                    committed: Some(RowHome::Spilled {
                        len,
                        sst: version.member,
                        commit_lsn: version.commit_lsn,
                    }),
                    committed_lsn: version.commit_lsn,
                    history: CommittedHistory::empty(),
                    pending: PendingVersions::empty(),
                })
            }))
    }

    /// The single visibility choke point for heap and object-resident row
    /// versions. Pending command visibility wins first; then the resident
    /// committed chain; finally immutable SSTs supply an older admissible
    /// image when the resident chain no longer carries it.
    pub fn visible_row_home(
        &self,
        table_slot: usize,
        rowid: u64,
        state: RowState,
        txid: u32,
    ) -> Result<Option<RowHome>, SqlError> {
        self.visible_row_home_at(
            table_slot,
            rowid,
            state,
            txid,
            self.read_snapshot,
            self.commit_snapshot,
        )
    }

    /// Visibility with explicit command and commit snapshots. DDL validation
    /// uses `SNAPSHOT_ALL` to include every change made earlier in the current
    /// transaction; ordinary scans call `visible_row_home`.
    pub fn visible_row_home_at(
        &self,
        table_slot: usize,
        rowid: u64,
        state: RowState,
        txid: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Result<Option<RowHome>, SqlError> {
        match state.pending.visible_at(txid, command_snapshot) {
            Some(location) => return Ok(location.map(RowHome::Heap)),
            None if state.committed_lsn <= commit_snapshot => return Ok(state.committed),
            None => {}
        }
        if let Some(home) = state.history.visible_at(commit_snapshot) {
            return Ok(home);
        }
        Ok(self
            .spill_probe_at(table_slot, rowid, commit_snapshot)?
            .and_then(|version| {
                version.len.map(|len| RowHome::Spilled {
                    len,
                    sst: version.member,
                    commit_lsn: version.commit_lsn,
                })
            }))
    }

    /// How many rows `txid` sees, through the same seam — under the current
    /// command snapshot, so it matches what the scan loop iterates.
    pub fn visible_row_count(&self, table_slot: usize, txid: u32) -> Result<usize, SqlError> {
        let mut count = 0usize;
        self.for_each_row_state(table_slot, &mut |rowid, state| {
            if self
                .visible_row_home(table_slot, rowid, state, txid)?
                .is_some()
            {
                count += 1;
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        Ok(count)
    }

    /// Rebuilds planner statistics from the same MVCC-visible row seam every
    /// query uses. An empty `selected_columns` means every column; otherwise
    /// only the named column statistics are replaced while table cardinality
    /// and row width are always refreshed.
    pub(crate) fn analyze_table(
        &mut self,
        table_slot: usize,
        txid: u32,
        selected_columns: &[usize],
    ) -> Result<TableStatistics, SqlError> {
        const REGISTERS: usize = 64;

        fn add_distinct(registers: &mut [u8; REGISTERS], hash: u64) {
            let index = (hash as usize) & (REGISTERS - 1);
            let tail = hash >> REGISTERS.trailing_zeros();
            let rank = tail.leading_zeros().saturating_add(1).min(63) as u8;
            registers[index] = registers[index].max(rank);
        }

        fn distinct_estimate(registers: &[u8; REGISTERS]) -> u64 {
            let mut inverse_sum = 0.0f64;
            let mut zeros = 0usize;
            for &register in registers {
                inverse_sum += 2.0f64.powi(-i32::from(register));
                zeros += usize::from(register == 0);
            }
            let count = REGISTERS as f64;
            let raw = 0.709 * count * count / inverse_sum;
            let corrected = if raw <= 2.5 * count && zeros > 0 {
                count * (count / zeros as f64).ln()
            } else {
                raw
            };
            corrected.round().max(0.0) as u64
        }

        let definition = *self.table_def(table_slot, txid);
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = definition.schema(&mut schema);
        let mut selected = [false; MAX_COLUMNS];
        if selected_columns.is_empty() {
            selected[..n_columns].fill(true);
        } else {
            for &column in selected_columns {
                selected[column] = true;
            }
        }

        let mut rows = 0u64;
        let mut row_bytes = 0u64;
        let mut nulls = [0u64; MAX_COLUMNS];
        let mut widths = [0u64; MAX_COLUMNS];
        let mut non_nulls = [0u64; MAX_COLUMNS];
        let mut registers = [[0u8; REGISTERS]; MAX_COLUMNS];
        self.for_each_row_state(table_slot, &mut |rowid, state| {
            let Some(home) = self.visible_row_home(table_slot, rowid, state, txid)? else {
                return Ok(core::ops::ControlFlow::Continue(()));
            };
            self.with_row_bytes(table_slot, rowid, home, |bytes| {
                let mut values = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, &schema[..n_columns], &mut values)?;
                rows = rows.saturating_add(1);
                row_bytes = row_bytes.saturating_add(bytes.len() as u64);
                for column in 0..n_columns {
                    if !selected[column] {
                        continue;
                    }
                    let value = values[column];
                    if value.is_null() {
                        nulls[column] = nulls[column].saturating_add(1);
                        continue;
                    }
                    non_nulls[column] = non_nulls[column].saturating_add(1);
                    // A one-column row encoding contributes a two-byte count
                    // and one bitmap byte; remove that framing to retain the
                    // value's actual stored width.
                    let width =
                        rowenc::encoded_len(core::slice::from_ref(&value)).saturating_sub(3);
                    widths[column] = widths[column].saturating_add(width as u64);
                    let index = [column as u16];
                    add_distinct(&mut registers[column], hash_key(&values, &index));
                }
                Ok(())
            })?;
            Ok(core::ops::ControlFlow::Continue(()))
        })?;

        let mut statistics = self.table_statistics(table_slot, txid);
        statistics.valid = true;
        statistics.rows = rows;
        statistics.average_row_width = row_bytes
            .checked_div(rows)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32;
        statistics.analyzed_generation = self.tables[table_slot].generation;
        for column in 0..n_columns {
            if !selected[column] {
                continue;
            }
            let distinct_values = distinct_estimate(&registers[column]).min(non_nulls[column]);
            statistics.columns[column] = ColumnStatistics {
                valid: rows != 0,
                null_fraction_ppm: nulls[column]
                    .saturating_mul(1_000_000)
                    .checked_div(rows)
                    .unwrap_or(0)
                    .min(u64::from(u32::MAX)) as u32,
                distinct_values,
                distinct_fraction_ppm: if rows != 0 && distinct_values.saturating_mul(10) > rows {
                    distinct_values
                        .saturating_mul(1_000_000)
                        .checked_div(rows)
                        .unwrap_or(0)
                        .min(1_000_000) as u32
                } else {
                    0
                },
                average_width: widths[column]
                    .checked_div(non_nulls[column])
                    .unwrap_or(0)
                    .min(u64::from(u32::MAX)) as u32,
            };
        }
        self.write_table_statistics(table_slot, txid, statistics)?;
        // PostgreSQL updates pg_class's relation statistics in place:
        // reltuples/relpages remain changed even if the surrounding
        // transaction rolls back. Column pg_statistic rows stay in the
        // transaction-private version written above.
        let committed = &mut self.tables[table_slot].statistics;
        committed.valid = statistics.valid;
        committed.rows = statistics.rows;
        committed.average_row_width = statistics.average_row_width;
        committed.analyzed_generation = statistics.analyzed_generation;
        self.tables[table_slot].statistics_dirty = true;
        self.tables[table_slot].statistics_wal_dirty = true;
        Ok(statistics)
    }

    pub(crate) fn table_statistics(&self, table_slot: usize, txid: u32) -> TableStatistics {
        if self.tables[table_slot].pending_statistics_txid == Some(txid)
            && let Some(position) = self.tables[table_slot].n_pending_statistics.checked_sub(1)
        {
            let slot = self.tables[table_slot].pending_statistics_slots[position as usize] as usize;
            return self.pending_table_statistics[slot].statistics;
        }
        self.tables[table_slot].statistics
    }

    pub(crate) fn pending_table_statistics(
        &self,
        table_slot: usize,
        txid: u32,
    ) -> Option<TableStatistics> {
        (self.tables[table_slot].pending_statistics_txid == Some(txid))
            .then(|| self.tables[table_slot].n_pending_statistics.checked_sub(1))
            .flatten()
            .map(|position| {
                let slot =
                    self.tables[table_slot].pending_statistics_slots[position as usize] as usize;
                self.pending_table_statistics[slot].statistics
            })
    }

    fn write_table_statistics(
        &mut self,
        table_slot: usize,
        txid: u32,
        statistics: TableStatistics,
    ) -> Result<(), SqlError> {
        if let Some(owner) = self.tables[table_slot].pending_statistics_txid
            && owner != txid
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "could not serialize ANALYZE of relation \"{}\"",
                self.tables[table_slot].def.name.as_str()
            ));
        }
        if self.tables[table_slot].n_pending_statistics as usize == MAX_PENDING_TABLE_DEFS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one transaction analyzes relation \"{}\" more than {} times",
                self.tables[table_slot].def.name.as_str(),
                MAX_PENDING_TABLE_DEFS
            ));
        }
        let slot = match self
            .pending_table_statistics
            .iter()
            .position(|entry| !entry.used)
        {
            Some(slot) => {
                self.pending_table_statistics[slot] = PendingTableStatisticsSlot {
                    used: true,
                    statistics,
                };
                slot
            }
            None => {
                let slot = self.pending_table_statistics.len();
                self.pending_table_statistics
                    .push(PendingTableStatisticsSlot {
                        used: true,
                        statistics,
                    })
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "pending table-statistics pool is exhausted"
                        )
                    })?;
                slot
            }
        };
        let position = self.tables[table_slot].n_pending_statistics as usize;
        self.tables[table_slot].pending_statistics_slots[position] = slot as u32;
        self.tables[table_slot].n_pending_statistics += 1;
        self.tables[table_slot].pending_statistics_txid = Some(txid);
        Ok(())
    }

    pub(crate) fn rollback_table_statistics(&mut self, table_slot: usize, txid: u32) {
        if self.tables[table_slot].pending_statistics_txid != Some(txid) {
            return;
        }
        let Some(position) = self.tables[table_slot].n_pending_statistics.checked_sub(1) else {
            return;
        };
        let slot = self.tables[table_slot].pending_statistics_slots[position as usize] as usize;
        self.pending_table_statistics[slot].used = false;
        self.tables[table_slot].pending_statistics_slots[position as usize] = u32::MAX;
        self.tables[table_slot].n_pending_statistics = position;
        if position == 0 {
            self.tables[table_slot].pending_statistics_txid = None;
        }
    }

    fn clear_pending_table_statistics(&mut self, table_slot: usize) {
        let count = self.tables[table_slot].n_pending_statistics as usize;
        for position in 0..count {
            let slot = self.tables[table_slot].pending_statistics_slots[position] as usize;
            self.pending_table_statistics[slot].used = false;
            self.tables[table_slot].pending_statistics_slots[position] = u32::MAX;
        }
        self.tables[table_slot].n_pending_statistics = 0;
        self.tables[table_slot].pending_statistics_txid = None;
    }

    pub(crate) fn commit_table_statistics(&mut self, table_slot: usize, txid: u32) {
        let Some(statistics) = self.pending_table_statistics(table_slot, txid) else {
            return;
        };
        self.tables[table_slot].statistics = statistics;
        self.tables[table_slot].statistics_dirty = true;
        self.clear_pending_table_statistics(table_slot);
    }

    /// Cardinality estimate that never performs object I/O. ANALYZE wins; an
    /// unspilled table can otherwise use its complete resident map exactly,
    /// while a spilled table uses a conservative floor until statistics are
    /// collected.
    pub(crate) fn planning_row_estimate(&self, table_slot: usize) -> u64 {
        let table = &self.tables[table_slot];
        if table.statistics.valid {
            table.statistics.rows
        } else if table.n_spill_ssts == 0 {
            table.rows.len() as u64
        } else {
            (table.rows.len() as u64).max(1_000)
        }
    }

    pub(crate) fn statistics_dirty(&self) -> bool {
        self.tables
            .iter()
            .any(|table| table.live && table.statistics_dirty)
    }

    pub(crate) fn statistics_wal_dirty(&self, table_slot: usize) -> bool {
        self.tables[table_slot].statistics_wal_dirty
    }

    pub(crate) fn clear_statistics_wal_dirty(&mut self, table_slot: usize) {
        self.tables[table_slot].statistics_wal_dirty = false;
    }

    pub(crate) fn install_table_statistics(
        &mut self,
        table_slot: usize,
        statistics: TableStatistics,
    ) {
        self.tables[table_slot].statistics = statistics;
        self.tables[table_slot].statistics_dirty = false;
        self.tables[table_slot].statistics_wal_dirty = false;
        self.clear_pending_table_statistics(table_slot);
    }

    pub(crate) fn replay_table_statistics(
        &mut self,
        table_slot: usize,
        statistics: TableStatistics,
    ) {
        self.tables[table_slot].statistics = statistics;
        self.tables[table_slot].statistics_dirty = true;
        self.tables[table_slot].statistics_wal_dirty = false;
        self.clear_pending_table_statistics(table_slot);
    }

    pub fn row_bytes<'a>(
        &'a self,
        table_slot: usize,
        rowid: u64,
        home: RowHome,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<&'a [u8], SqlError> {
        match home {
            RowHome::Heap(loc) => Ok(self.heap.get(loc)),
            RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            } => {
                let Some(spill) = &self.spill else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but no spill reader is attached"
                    ));
                };
                let Some(handle) = self.tables[table_slot]
                    .spill_ssts
                    .get(sst as usize)
                    .copied()
                    .flatten()
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but its table has no spill SST"
                    ));
                };
                let out = arena.alloc_slice_with(len as usize, |_| 0u8).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "spilled rows exceed the statement arena; raise work_arena_bytes"
                    )
                })?;
                // Both borrows are per-fetch; the copy into the arena ends
                // them before returning.
                let Some(mut scratch) = spill.scratch.iter().find_map(|c| c.try_borrow_mut().ok())
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled-row fetches nested deeper than the reader supports"
                    ));
                };
                let mut blocks = spill.blocks.borrow_mut();
                let SpillScratch {
                    index_buf,
                    data_buf,
                    decoded_buf,
                    column_buf,
                    assembly_buf,
                    decoded_data_ref,
                    ..
                } = &mut *scratch;
                let mut reader = crate::store::SstReader::over(
                    index_buf,
                    data_buf,
                    decoded_buf,
                    column_buf,
                    assembly_buf,
                );
                reader.restore_cached_data_block((*decoded_data_ref).and_then(
                    |(cached_handle, reference, cached_len)| {
                        (cached_handle == handle).then_some((reference, cached_len))
                    },
                ));
                let got = reader
                    .get_at(&mut *blocks, &handle, rowid, commit_lsn, out)
                    .map_err(spill_read_error)?;
                *decoded_data_ref = reader
                    .cached_data_block()
                    .map(|(reference, cached_len)| (handle, reference, cached_len));
                match got {
                    Some(probe) if probe.key.commit_lsn == commit_lsn && probe.len == Some(len) => {
                        Ok(&out[..len as usize])
                    }
                    Some(_) => Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled row version mismatch"
                    )),
                    None => Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled row missing from its SST"
                    )),
                }
            }
        }
    }

    /// Hands a visible row's bytes to `f` without arena residency: a heap row
    /// borrows the heap; a spilled row is fetched into the spill reader's own
    /// scratch for the duration of the call. For consume-in-place readers
    /// (constraint scans) whose decoded values do not outlive the closure.
    /// `f` must not fetch another spilled row (the scratch is singular).
    pub fn with_row_bytes<R>(
        &self,
        table_slot: usize,
        rowid: u64,
        home: RowHome,
        f: impl FnOnce(&[u8]) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        match home {
            RowHome::Heap(loc) => f(self.heap.get(loc)),
            RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            } => {
                let Some(spill) = &self.spill else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but no spill reader is attached"
                    ));
                };
                let Some(handle) = self.tables[table_slot]
                    .spill_ssts
                    .get(sst as usize)
                    .copied()
                    .flatten()
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but its table has no spill SST"
                    ));
                };
                let Some(mut scratch) = spill.scratch.iter().find_map(|c| c.try_borrow_mut().ok())
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled-row fetches nested deeper than the reader supports"
                    ));
                };
                let SpillScratch {
                    index_buf,
                    data_buf,
                    decoded_buf,
                    column_buf,
                    assembly_buf,
                    bounce_buf,
                    decoded_data_ref,
                } = &mut *scratch;
                // The assembly buffer doubles as the row destination: `get`
                // assembles a chained row into the caller buffer directly, so
                // the two uses never overlap. The reader's own staging slot is
                // the bounce buffer (a compressed data block decompresses
                // through it).
                let row_buf = &mut assembly_buf[..len as usize];
                let got = {
                    let mut blocks = spill.blocks.borrow_mut();
                    let mut reader = crate::store::SstReader::over(
                        index_buf,
                        data_buf,
                        decoded_buf,
                        column_buf,
                        bounce_buf,
                    );
                    reader.restore_cached_data_block((*decoded_data_ref).and_then(
                        |(cached_handle, reference, cached_len)| {
                            (cached_handle == handle).then_some((reference, cached_len))
                        },
                    ));
                    let got = reader
                        .get_at(&mut *blocks, &handle, rowid, commit_lsn, row_buf)
                        .map_err(spill_read_error)?;
                    *decoded_data_ref = reader
                        .cached_data_block()
                        .map(|(reference, cached_len)| (handle, reference, cached_len));
                    got
                };
                match got {
                    Some(probe) if probe.key.commit_lsn == commit_lsn && probe.len == Some(len) => {
                        f(&row_buf[..len as usize])
                    }
                    Some(_) => Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled row version mismatch"
                    )),
                    None => Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled row missing from its SST"
                    )),
                }
            }
        }
    }

    /// Marks every committed heap row of every live table as spilled to its
    /// just-checkpointed SST, so the following compaction drops the bytes
    /// from RAM. Only called after a successful checkpoint whose handles are
    /// installed on the tables; rows with no SST (empty tables) are left.
    pub fn evict_committed(&mut self) {
        for i in 0..self.tables.len() {
            if !self.tables[i].live || self.tables[i].n_spill_ssts == 0 {
                continue;
            }
            let table = &mut self.tables[i];
            // The newest SST is the delta the checkpoint just wrote, and it
            // holds every committed heap row of this table.
            let newest = (table.n_spill_ssts - 1) as u8;
            for (_, state) in table.rows.iter_mut() {
                if let Some(RowHome::Heap(loc)) = state.committed {
                    state.committed = Some(RowHome::Spilled {
                        len: loc.len,
                        sst: newest,
                        commit_lsn: state.committed_lsn,
                    });
                }
            }
        }
    }

    /// A full rewrite: the new SST holds every committed row, so the list
    /// collapses to it and every spilled map entry is remapped to slot 0.
    /// Clears the tombstones the rewrite made moot.
    pub(crate) fn collapse_spill(&mut self, slot: usize, handle: crate::store::SstHandle) {
        let table = &mut self.tables[slot];
        table.spill_ssts = [None; MAX_SPILL_SSTS];
        table.spill_ssts[0] = Some(handle);
        table.n_spill_ssts = 1;
        for (_, state) in table.rows.iter_mut() {
            if let Some(RowHome::Spilled {
                len, commit_lsn, ..
            }) = state.committed
            {
                state.committed = Some(RowHome::Spilled {
                    len,
                    sst: 0,
                    commit_lsn,
                });
            }
        }
    }

    /// Paced compaction merged the adjacent spill-SST pair at (`at`,
    /// `at + 1`) into one (`None` when nothing in the pair survived): the
    /// merged member takes position `at`, later members shift down, and every
    /// spilled row's index follows. A live row can only reference a dropped
    /// pair when the merge kept it, so `None` never strands one.
    pub(crate) fn merge_spill_pair(
        &mut self,
        slot: usize,
        at: usize,
        handle: Option<crate::store::SstHandle>,
    ) {
        let table = &mut self.tables[slot];
        let removed = if handle.is_some() { 1u8 } else { 2u8 };
        let mut ssts = [None; MAX_SPILL_SSTS];
        let mut n = 0;
        for i in 0..at {
            ssts[n] = table.spill_ssts[i];
            n += 1;
        }
        if let Some(h) = handle {
            ssts[n] = Some(h);
            n += 1;
        }
        for i in at + 2..table.n_spill_ssts {
            ssts[n] = table.spill_ssts[i];
            n += 1;
        }
        table.spill_ssts = ssts;
        table.n_spill_ssts = n;
        let at = at as u8;
        for (_, state) in table.rows.iter_mut() {
            if let Some(RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            }) = state.committed
            {
                let sst = if sst < at {
                    sst
                } else if sst == at || sst == at + 1 {
                    at
                } else {
                    sst - removed
                };
                state.committed = Some(RowHome::Spilled {
                    len,
                    sst,
                    commit_lsn,
                });
            }
        }
    }

    /// A delta flush: the new SST (heap rows + tombstones) joins the list;
    /// existing spilled entries keep their slots. Clears the flushed
    /// tombstones. The caller guarantees the list has room.
    pub(crate) fn append_spill(&mut self, slot: usize, handle: crate::store::SstHandle) {
        let table = &mut self.tables[slot];
        assert!(
            table.n_spill_ssts < MAX_SPILL_SSTS,
            "delta flush into a full list"
        );
        table.spill_ssts[table.n_spill_ssts] = Some(handle);
        table.n_spill_ssts += 1;
    }

    /// Installs a cold-start spill list verbatim (entries were installed with
    /// their slots by the manifest scan).
    pub(crate) fn set_spill_list(&mut self, slot: usize, handles: &[crate::store::SstHandle]) {
        let table = &mut self.tables[slot];
        table.spill_ssts = [None; MAX_SPILL_SSTS];
        for (i, h) in handles.iter().take(MAX_SPILL_SSTS).enumerate() {
            table.spill_ssts[i] = Some(*h);
        }
        table.n_spill_ssts = handles.len().min(MAX_SPILL_SSTS);
        table.n_tombstones = 0;
        table.tombstones_overflow = false;
    }

    /// Clears a table's remembered tombstones — called only once the manifest
    /// referencing the SST that carries them has *published*. A failed
    /// publish keeps them, so the retry flushes them again rather than losing
    /// a delete.
    pub(crate) fn clear_tombstones(&mut self, slot: usize) {
        let table = &mut self.tables[slot];
        table.n_tombstones = 0;
        table.tombstones_overflow = false;
        // The install that cleared the buffer has made the SSTs themselves
        // carry (or moot) every recorded deletion, so the shadowing markers
        // are done shadowing.
        loop {
            let mut batch = [0u64; 512];
            let mut n = 0usize;
            for (&rowid, state) in table.rows.iter() {
                if state.committed.is_none() && state.history.is_empty() && state.pending.is_none()
                {
                    batch[n] = rowid;
                    n += 1;
                    if n == batch.len() {
                        break;
                    }
                }
            }
            if n == 0 {
                return;
            }
            for &rowid in &batch[..n] {
                table.rows.remove(&rowid);
            }
        }
    }

    /// What the next checkpoint should do for this table: a delta flush (the
    /// spill list has room and every remembered tombstone fits), or a full
    /// rewrite.
    /// Records a committed-row removal for the next delta checkpoint, so a
    /// cold start cannot resurrect an older SST's version of the row. Only
    /// meaningful while the table has spilled SSTs.
    fn record_tombstone(table: &mut Table, rowid: u64) {
        if table.n_spill_ssts == 0 || table.tombstones_overflow {
            return;
        }
        if table.n_tombstones == MAX_TOMBSTONES {
            // Never drop one: the next checkpoint falls back to a full
            // rewrite, which needs no tombstones at all.
            table.tombstones_overflow = true;
            return;
        }
        table.tombstones[table.n_tombstones] = rowid;
        table.n_tombstones += 1;
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Resolves a logical insert target to its owning leaf.  One typed catalog
    /// representation drives VALUES, COPY, MERGE and logical apply alike.
    pub fn partition_target(
        &self,
        table: usize,
        values: &[crate::sql::types::Datum],
        txid: u32,
    ) -> Result<usize, SqlError> {
        let mut current = table;
        loop {
            let definition = self.table_def(current, txid);
            let Some(PartitionScheme {
                strategy,
                keys,
                n_keys,
            }) = definition.partition.scheme
            else {
                if let Some(PartitionAttachment { parent, bound }) = definition.partition.attachment
                    && !self.partition_attachment_accepts(
                        usize::from(parent),
                        current,
                        bound,
                        values,
                        txid,
                    )?
                {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::CHECK_VIOLATION,
                        "new row for relation \"{}\" violates partition constraint",
                        definition.name.as_str()
                    ));
                }
                return Ok(current);
            };
            current = self.partition_child_target(current, strategy, keys, n_keys, values, txid)?;
        }
    }

    /// Verifies the complete attachment chain of a leaf after `BEFORE ROW`
    /// triggers have had an opportunity to rewrite its partition keys.
    pub(crate) fn validate_partition_target(
        &self,
        leaf: usize,
        values: &[crate::sql::types::Datum],
        txid: u32,
    ) -> Result<(), SqlError> {
        let mut child = leaf;
        while let Some(PartitionAttachment { parent, bound }) =
            self.table_def(child, txid).partition.attachment
        {
            if !self.partition_attachment_accepts(
                usize::from(parent),
                child,
                bound,
                values,
                txid,
            )? {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::CHECK_VIOLATION,
                    "new row for relation \"{}\" violates partition constraint",
                    self.table_def(child, txid).name.as_str()
                ));
            }
            child = usize::from(parent);
        }
        Ok(())
    }

    pub(crate) fn partition_child_target(
        &self,
        parent_slot: usize,
        strategy: PartitionStrategy,
        keys: [u16; MAX_PARTITION_KEYS],
        n_keys: u8,
        values: &[crate::sql::types::Datum],
        txid: u32,
    ) -> Result<usize, SqlError> {
        let mut default = None;
        let mut match_slot = None;
        for child in 0..self.table_count() {
            if !self.table(child).visible_to(txid) {
                continue;
            }
            let Some(PartitionAttachment { parent, bound }) =
                self.table_def(child, txid).partition.attachment
            else {
                continue;
            };
            if usize::from(parent) != parent_slot {
                continue;
            }
            if matches!(bound, PartitionBound::Default) {
                default = Some(child);
            } else if partition_bound_matches(strategy, keys, n_keys, bound, values)?
                && match_slot.replace(child).is_some()
            {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::INTERNAL_ERROR,
                    "overlapping partition bounds in relation \"{}\"",
                    self.table_def(parent_slot, txid).name.as_str()
                ));
            }
        }
        match_slot.or(default).ok_or_else(|| {
            sql_err!(
                crate::sql::eval::sqlstate::CHECK_VIOLATION,
                "no partition of relation \"{}\" found for row",
                self.table_def(parent_slot, txid).name.as_str()
            )
        })
    }

    pub(crate) fn partition_attachment_accepts(
        &self,
        parent: usize,
        child: usize,
        bound: PartitionBound,
        values: &[crate::sql::types::Datum],
        txid: u32,
    ) -> Result<bool, SqlError> {
        let Some(PartitionScheme {
            strategy,
            keys,
            n_keys,
        }) = self.table_def(parent, txid).partition.scheme
        else {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "partition parent is not partitioned"
            ));
        };
        if !matches!(bound, PartitionBound::Default) {
            return partition_bound_matches(strategy, keys, n_keys, bound, values);
        }
        for sibling in 0..self.table_count() {
            if sibling == child || !self.table(sibling).visible_to(txid) {
                continue;
            }
            let Some(PartitionAttachment {
                parent: sibling_parent,
                bound,
            }) = self.table_def(sibling, txid).partition.attachment
            else {
                continue;
            };
            if usize::from(sibling_parent) == parent
                && !matches!(bound, PartitionBound::Default)
                && partition_bound_matches(strategy, keys, n_keys, bound, values)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Expands a logical relation to physical scan leaves without allocating.
    /// The caller supplies startup-bounded/statement-arena storage sized from
    /// the configured table catalog.
    pub fn partition_leaf_slots(
        &self,
        root: usize,
        txid: u32,
        out: &mut [usize],
    ) -> Result<usize, SqlError> {
        fn visit(
            storage: &Storage,
            root: usize,
            txid: u32,
            out: &mut [usize],
            n: &mut usize,
        ) -> Result<(), SqlError> {
            let mut found = false;
            for child in 0..storage.table_count() {
                if !storage.table(child).visible_to(txid) {
                    continue;
                }
                let Some(PartitionAttachment { parent, .. }) =
                    storage.table_def(child, txid).partition.attachment
                else {
                    continue;
                };
                if usize::from(parent) != root {
                    continue;
                }
                found = true;
                visit(storage, child, txid, out, n)?;
            }
            if !found && !storage.table_def(root, txid).partition.is_partitioned() {
                if *n == out.len() {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "partition scan exceeds configured table capacity"
                    ));
                }
                out[*n] = root;
                *n += 1;
            }
            Ok(())
        }
        let mut n = 0;
        visit(self, root, txid, out, &mut n)?;
        Ok(n)
    }

    /// Whether `table` is a physical descendant of `ancestor` in the catalog
    /// snapshot. Attachment slots are typed and acyclic at DDL time, so this
    /// walk cannot reinterpret an unrelated table as part of the hierarchy.
    pub(crate) fn partition_descends_from(
        &self,
        mut table: usize,
        ancestor: usize,
        txid: u32,
    ) -> bool {
        while let Some(attachment) = self.table_def(table, txid).partition.attachment {
            table = usize::from(attachment.parent);
            if table == ancestor {
                return true;
            }
        }
        false
    }

    /// Finds the physical owner of a row identity emitted while scanning a
    /// logical partitioned relation. Row identifiers are cluster-wide, so the
    /// search cannot confuse identically numbered rows from two leaves.
    pub fn partition_row_owner(
        &self,
        root: usize,
        rowid: u64,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        for candidate in 0..self.table_count() {
            if !self.table(candidate).visible_to(txid)
                || self.table_def(candidate, txid).partition.is_partitioned()
                || self.row_state(candidate, rowid)?.is_none()
            {
                continue;
            }
            let mut current = candidate;
            loop {
                if current == root {
                    return Ok(Some(candidate));
                }
                let Some(PartitionAttachment { parent, .. }) =
                    self.table_def(current, txid).partition.attachment
                else {
                    break;
                };
                current = usize::from(parent);
            }
        }
        Ok(None)
    }

    /// Clears only table images captured by a published checkpoint.
    pub fn clear_dirty_through(&mut self, generations: &[u64]) {
        for (slot, t) in self.tables.iter_mut().enumerate() {
            if generations.get(slot).copied() == Some(t.generation) {
                t.dirty = false;
            }
            t.statistics_dirty = false;
            t.statistics_wal_dirty = false;
        }
    }

    /// Rewrites the row heap so it contains only live row images
    /// (committed and pending alike), in ascending offset order, repointing
    /// every table's map. Reclaims the garbage left by updates and deletes;
    /// runs at checkpoint. `scratch` must hold every live image.
    pub fn compact_heap(
        &mut self,
        scratch: &mut FixedVec<(u32, u64, u8, RowLoc)>,
    ) -> Result<(), SqlError> {
        scratch.clear();
        for (index, table) in self.tables.iter().enumerate() {
            // A transaction may have inserted rows into its pending CREATE
            // before a concurrent checkpoint. Those bytes are private, not
            // garbage: preserve them even though the committed table image is
            // not live yet. A genuinely dead slot has neither committed nor
            // pending existence.
            if !table.live && table.pending_ddl.is_none() {
                continue;
            }
            for (&rowid, state) in table.rows.iter() {
                let overflow = |e| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "heap compaction scratch overflow: {}",
                        e
                    )
                };
                if let Some(RowHome::Heap(loc)) = state.committed {
                    scratch
                        .push((index as u32, rowid, u8::MAX, loc))
                        .map_err(overflow)?;
                }
                for history_index in 0..state.history.len() {
                    if let Some(CommittedVersion {
                        home: Some(RowHome::Heap(loc)),
                        ..
                    }) = state.history.get(history_index)
                    {
                        scratch
                            .push((index as u32, rowid, 0x80 | history_index as u8, loc))
                            .map_err(overflow)?;
                    }
                }
                for pending_index in 0..state.pending.len() {
                    if let Some(PendingChange { loc: Some(loc), .. }) =
                        state.pending.get(pending_index)
                    {
                        scratch
                            .push((index as u32, rowid, pending_index as u8, loc))
                            .map_err(overflow)?;
                    }
                }
            }
        }
        // Moving rows in ascending source order means every copy target is
        // at or below its source — copy_within stays safe.
        scratch
            .as_mut_slice()
            .sort_unstable_by_key(|(_, _, _, loc)| loc.offset);
        let mut write_at = 0usize;
        for i in 0..scratch.len() {
            let (table_index, rowid, pending_index, loc) = scratch[i];
            let len = loc.len as usize;
            let src = loc.offset as usize;
            debug_assert!(write_at <= src, "targets never overtake sources");
            if src != write_at {
                self.heap.buffer.copy_within(src..src + len, write_at);
            }
            let new_loc = RowLoc {
                offset: write_at as u32,
                len: loc.len,
            };
            let table = &mut self.tables[table_index as usize];
            let state = table
                .rows
                .get_mut(&rowid)
                .expect("scratch entries come from the maps");
            if pending_index == u8::MAX {
                state.committed = Some(RowHome::Heap(new_loc));
            } else if pending_index & 0x80 != 0 {
                let history_index = (pending_index & 0x7f) as usize;
                state.history.entries[history_index].home = Some(RowHome::Heap(new_loc));
            } else {
                let p = state
                    .pending
                    .get_mut(pending_index as usize)
                    .expect("pending image existed");
                p.loc = Some(new_loc);
            }
            write_at += len;
        }
        self.heap.used = write_at;
        Ok(())
    }

    /// Records an uncommitted change to a row. Returns whether this is the
    /// transaction's first touch of the row (the caller then remembers it
    /// for commit/rollback). A conflicting transaction becomes a wait-graph
    /// edge; the protocol parks and retries after the owner ends.
    pub fn write_pending(
        &mut self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<Option<Option<RowLoc>>, SqlError> {
        if let Some(owner) = self.tables[table_index]
            .pending_def_txid
            .filter(|owner| *owner != txid)
        {
            self.row_locks.borrow_mut().wait_for(txid, owner)?;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent table definition change"
            ));
        }
        let oldest_snapshot = self.oldest_snapshot();
        let conflicting_owner = self.tables[table_index]
            .rows
            .get(&rowid)
            .and_then(|state| state.locked_by_other(txid));
        if let Some(owner) = conflicting_owner {
            self.row_locks.borrow_mut().wait_for(txid, owner)?;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent row update"
            ));
        }
        let table = &mut self.tables[table_index];
        if let Some(state) = table.rows.get_mut(&rowid) {
            if let Some(last) = state.pending.last_mut()
                && last.cid == cid
            {
                let prior = Some(last.loc);
                last.loc = loc;
                return Ok(prior);
            }
            state.history.prune(oldest_snapshot);
            if oldest_snapshot.is_some()
                && state.pending.is_none()
                && (state.committed.is_some() || state.committed_lsn != 0)
                && state.history.len() == MAX_COMMITTED_ROW_VERSIONS
            {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "active snapshot history for row {} is full; end the old snapshot or raise the compiled version bound",
                    rowid
                ));
            }
            state.pending.push(PendingChange { txid, cid, loc })?;
            return Ok(None);
        }
        // An absent entry no longer means an absent row: the spill list may
        // hold its committed image, and that image must ride into the entry
        // — a pending change with `committed: None` would hide the old
        // value from uniqueness scans and resurrect wrongly on rollback.
        let committed = self
            .spill_probe_at(table_index, rowid, u64::MAX)?
            .and_then(|version| {
                version.len.map(|len| RowHome::Spilled {
                    len,
                    sst: version.member,
                    commit_lsn: version.commit_lsn,
                })
            });
        let committed_lsn = committed.map_or(0, |home| match home {
            RowHome::Heap(_) => 0,
            RowHome::Spilled { commit_lsn, .. } => commit_lsn,
        });
        let table = &mut self.tables[table_index];
        if table.rows.len() == table.rows.capacity() {
            // Entries the spill lists reproduce are droppable on demand.
            self.evict_redundant_entries(table_index);
            if self.tables[table_index].rows.len() == self.tables[table_index].rows.capacity() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table row limit reached ({} rows in memtable)",
                    self.tables[table_index].rows.capacity()
                ));
            }
        }
        self.tables[table_index]
            .rows
            .insert(
                rowid,
                RowState {
                    committed,
                    committed_lsn,
                    history: CommittedHistory::empty(),
                    pending: {
                        let mut versions = PendingVersions::empty();
                        versions.push(PendingChange { txid, cid, loc })?;
                        versions
                    },
                },
            )
            .expect("capacity checked above");
        Ok(None)
    }

    /// Drops map entries the spill lists reproduce exactly — committed,
    /// spilled, no pending change. A `Spilled` entry's member is the list's
    /// newest mention of its rowid (installs are the only thing that moves
    /// one, and they run at publish), so the merged walk and the point
    /// probe synthesize the identical state after the entry is gone. This
    /// is what unbinds a table's row count from `table_rows`: the map holds
    /// the working set, the bucket holds the rest.
    pub fn evict_redundant_entries(&mut self, slot: usize) {
        let table = &mut self.tables[slot];
        if table.n_spill_ssts == 0 {
            return;
        }
        loop {
            let mut batch = [0u64; 512];
            let mut n = 0usize;
            for (&rowid, state) in table.rows.iter() {
                if matches!(state.committed, Some(RowHome::Spilled { .. }))
                    && state.history.is_empty()
                    && state.pending.is_none()
                {
                    batch[n] = rowid;
                    n += 1;
                    if n == batch.len() {
                        break;
                    }
                }
            }
            if n == 0 {
                return;
            }
            for &rowid in &batch[..n] {
                table.rows.remove(&rowid);
            }
        }
    }

    /// Whether any live table's overlay is at least half full — the map's
    /// analogue of heap pressure. Rows are counted against entries, not
    /// bytes: a table of tiny rows fills its map long before its heap.
    pub fn map_pressure(&self) -> bool {
        self.tables
            .iter()
            .any(|t| t.live && t.n_spill_ssts > 0 && t.rows.len() * 100 >= t.rows.capacity() * 50)
    }

    /// Starts an object flush before the resident safety window can fill.
    /// Published versions are then read through the immutable SST forest and
    /// the resident side chain is released.
    pub fn history_pressure(&self) -> bool {
        self.tables.iter().any(|table| {
            table.live
                && table
                    .rows
                    .iter()
                    .any(|(_, state)| state.history.len() + 2 >= MAX_COMMITTED_ROW_VERSIONS)
        })
    }

    /// A successful manifest publish made every resident historical image
    /// reachable through the table's installed versioned SST list.
    pub fn release_durable_histories(&mut self) {
        for table in self.tables.iter_mut().filter(|table| table.live) {
            for (_, state) in table.rows.iter_mut() {
                state.history.prune(None);
            }
        }
    }

    /// The map-occupancy pass after a publish: any table whose overlay is
    /// half full sheds its redundant entries.
    pub fn evict_entries(&mut self) {
        for i in 0..self.tables.len() {
            let table = &self.tables[i];
            if !table.live
                || table.n_spill_ssts == 0
                || table.rows.len() * 100 < table.rows.capacity() * 50
            {
                continue;
            }
            self.evict_redundant_entries(i);
        }
    }

    /// Restores a row's pending change to a prior image (for `ROLLBACK TO
    /// SAVEPOINT` and error unwinding). `prior` is what `write_pending`
    /// returned: `None` clears the pending entirely (removing the row if it
    /// was never committed); `Some(loc)` reinstates a pending change.
    pub fn restore_pending(
        &mut self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        prior: Option<Option<RowLoc>>,
    ) {
        let table = &mut self.tables[table_index];
        let Some(state) = table.rows.get_mut(&rowid) else {
            return;
        };
        // Only touch a pending change this transaction owns (or an empty slot).
        if let Some(p) = state.pending.last()
            && p.txid != txid
        {
            return;
        }
        match prior {
            None => {
                state.pending.pop();
                if state.committed.is_none() && state.history.is_empty() && state.pending.is_none()
                {
                    table.rows.remove(&rowid);
                }
            }
            Some(loc) => {
                if let Some(last) = state.pending.last_mut() {
                    last.loc = loc;
                }
            }
        }
    }

    /// Promotes a row's pending change to committed. The WAL record must
    /// already be durable.
    /// Removes a committed row outright (journal replay of a DELETE),
    /// recording the tombstone a later delta checkpoint needs.
    pub fn remove_committed(&mut self, table_index: usize, rowid: u64, commit_lsn: u64) {
        let table = &mut self.tables[table_index];
        if table.n_spill_ssts == 0 {
            if table.rows.remove(&rowid).is_some() {
                table.mark_dirty();
            }
            return;
        }
        // The spill list may hold this row, so the delete must both
        // tombstone (for the next flush) and leave a shadowing marker (for
        // reads until then) — same discipline as a committed DELETE.
        let _ = table.rows.insert(
            rowid,
            RowState {
                committed: None,
                committed_lsn: commit_lsn,
                history: CommittedHistory::empty(),
                pending: PendingVersions::empty(),
            },
        );
        Self::record_tombstone(table, rowid);
        table.mark_dirty();
    }

    pub fn commit_row(&mut self, table_index: usize, rowid: u64, txid: u32, commit_lsn: u64) {
        // Read the transition without holding a mutable borrow.
        let (old_committed, old_lsn, new_loc) = {
            let Some(state) = self.tables[table_index].rows.get(&rowid) else {
                return;
            };
            match state.pending.last() {
                Some(p) if p.txid == txid => (state.committed, state.committed_lsn, p.loc),
                _ => return,
            }
        };
        // Maintain the value indexes: drop the old committed value's key, add
        // the new one. The row images are still readable (committed not yet
        // repointed, new bytes already in the heap).
        self.maintain_indexes_on_commit(table_index, rowid, new_loc);

        let retain_history = !self.active_snapshots.is_empty();
        let table = &mut self.tables[table_index];
        let state = table.rows.get_mut(&rowid).expect("row present after read");
        if retain_history && (old_committed.is_some() || old_lsn != 0) {
            state
                .history
                .push_newest(CommittedVersion {
                    home: old_committed,
                    lsn: old_lsn,
                })
                .expect("write_pending reserved historical-version capacity");
        } else if !retain_history {
            state.history.prune(None);
        }
        state.committed = new_loc.map(RowHome::Heap);
        state.committed_lsn = commit_lsn;
        state.pending.clear();
        if state.committed.is_none() {
            // A rowid that ever reached an SST — even if its latest version was
            // heap-resident — must tombstone, or a cold start resurrects the
            // SST's version. And until that tombstone is *flushed*, the entry
            // itself stays behind as a marker (`committed: None, pending:
            // None`): the merged walk treats any entry as shadowing the spill
            // list, so the marker is what keeps the deleted row invisible right
            // now. `clear_tombstones` purges the markers once an install has
            // made the SSTs themselves say deleted.
            if table.n_spill_ssts == 0 {
                table.rows.remove(&rowid);
            }
            Self::record_tombstone(table, rowid);
        }
        table.mark_dirty();
    }

    /// Promotes a row rewritten under a pending table definition. Value
    /// indexes are rebuilt once after every row and the definition are
    /// promoted, because the old and new encodings cannot share an index
    /// decoder.
    pub fn commit_rewritten_row(
        &mut self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        commit_lsn: u64,
    ) {
        let new_loc = {
            let Some(state) = self.tables[table_index].rows.get(&rowid) else {
                return;
            };
            match state.pending.last() {
                Some(pending) if pending.txid == txid => pending.loc,
                _ => return,
            }
        };
        let table = &mut self.tables[table_index];
        let state = table.rows.get_mut(&rowid).expect("row present after read");
        // Definition rewrites are rejected while a historical snapshot is
        // active, so no old-schema row image can be retained here.
        state.history.prune(None);
        state.committed = new_loc.map(RowHome::Heap);
        state.committed_lsn = commit_lsn;
        state.pending.clear();
        if state.committed.is_none() {
            if table.n_spill_ssts == 0 {
                table.rows.remove(&rowid);
            }
            Self::record_tombstone(table, rowid);
        }
        table.mark_dirty();
    }

    /// Decodes the row at `home` and hashes every enforcer key whose columns are
    /// all non-NULL (a NULL key is SQL-distinct, never indexed), writing
    /// `(enforcer_index, hash)` for each into `out`. Returns the count.
    fn row_enforcer_hashes(
        &self,
        table_index: usize,
        rowid: u64,
        home: RowHome,
        out: &mut [(usize, u64); MAX_VALUE_ENFORCERS],
    ) -> Result<usize, SqlError> {
        let table = &self.tables[table_index];
        let n_enf = table.n_enforcers;
        if n_enf == 0 {
            return Ok(0);
        }
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = table.def.schema(&mut schema);
        let mut cols = [([0u16; MAX_INDEX_COLS], 0usize); MAX_VALUE_ENFORCERS];
        for (i, entry) in cols.iter_mut().enumerate().take(n_enf) {
            let e = table.enforcers[i].expect("enforcer present");
            *entry = (e.columns, e.n_cols);
        }
        self.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..n_columns], &mut values)?;
            let mut n_out = 0;
            for (i, (c, n)) in cols.iter().take(n_enf).enumerate() {
                let columns = &c[..*n];
                if columns.iter().any(|&col| values[col as usize].is_null()) {
                    continue;
                }
                out[n_out] = (i, hash_table_key(&table.def, &values, columns));
                n_out += 1;
            }
            Ok(n_out)
        })
    }

    /// The value-index maintenance for one committed row transition: remove the
    /// old entry by row identity, then insert the new value's key. Removing by
    /// identity keeps publication independent of object-store availability:
    /// the old row may be object-resident, while the new encoded bytes are
    /// already in the heap. Physical headroom guarantees the insert fits.
    fn maintain_indexes_on_commit(
        &mut self,
        table_index: usize,
        rowid: u64,
        new_loc: Option<RowLoc>,
    ) {
        if self.tables[table_index].n_enforcers == 0 {
            return;
        }
        let mut inserts = [(0usize, 0u64); MAX_VALUE_ENFORCERS];
        let n_inserts = match new_loc {
            Some(loc) => self
                .row_enforcer_hashes(table_index, rowid, RowHome::Heap(loc), &mut inserts)
                .expect("new row decodes"),
            None => 0,
        };
        let mut slots = [u32::MAX; MAX_VALUE_ENFORCERS];
        for (i, s) in slots
            .iter_mut()
            .enumerate()
            .take(self.tables[table_index].n_enforcers)
        {
            *s = self.tables[table_index].enforcers[i]
                .expect("enforcer")
                .slot;
        }
        let pool = self
            .value_indexes
            .as_mut()
            .expect("value index pool present");
        for &slot in &slots[..self.tables[table_index].n_enforcers] {
            pool.get_mut(slot).remove_rowid(rowid);
        }
        for &(ei, hash) in &inserts[..n_inserts] {
            let index = pool.get_mut(slots[ei]);
            if index.insert(hash, rowid).is_err() {
                // This structure is an acceleration cache, not durable state.
                // Once it cannot represent the complete committed image, a
                // negative probe must fall through to the authoritative rows.
                index.mark_incomplete();
            }
        }
    }

    /// Probes the value cache for the indexed tuple covering exactly `columns`,
    /// visiting every candidate rowid whose key hashes to `hash`. Returns true
    /// only when the cache is complete, so a caller may trust a negative
    /// answer; false means the authoritative row store must be scanned.
    pub fn probe_value(
        &self,
        table_index: usize,
        columns: &[u16],
        hash: u64,
        mut visit: impl FnMut(u64),
    ) -> Result<bool, SqlError> {
        let table = &self.tables[table_index];
        for i in 0..table.n_enforcers {
            let e = table.enforcers[i].expect("enforcer present");
            if e.columns() == columns {
                let index = self
                    .value_indexes
                    .as_ref()
                    .expect("value index pool present")
                    .get(e.slot);
                index.probe(hash, &mut visit);
                if index.is_complete() {
                    return Ok(true);
                }
                let Some(handle) = e.durable else {
                    return Ok(false);
                };
                if self.commit_snapshot < handle.published_lsn {
                    return Ok(false);
                }
                let Some(spill) = &self.spill else {
                    return Ok(false);
                };
                let Some(mut scratch) = spill
                    .value_scratch
                    .iter()
                    .find_map(|candidate| candidate.try_borrow_mut().ok())
                else {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "persistent value probes nested deeper than reader scratch"
                    ));
                };
                let scratch = &mut *scratch;
                crate::store::ValueIndexReader::over(&mut scratch.roster, &mut scratch.data)
                    .probe(
                        &mut *spill.blocks.borrow_mut(),
                        &handle,
                        hash,
                        |rowid, _, _| visit(rowid),
                    )
                    .map_err(|error| {
                        sql_err!(
                            sqlstate::IO_ERROR,
                            "persistent value-index read: {:?}",
                            error
                        )
                    })?;
                // The published generation is a complete base. Every later
                // committed change remains in the bounded resident overlay
                // until its replacement generation publishes.
                let mut hashes = [(0usize, 0u64); MAX_VALUE_ENFORCERS];
                for (&rowid, state) in self.tables[table_index].rows.iter() {
                    let Some(home) = state.committed else {
                        continue;
                    };
                    let n = self.row_enforcer_hashes(table_index, rowid, home, &mut hashes)?;
                    if hashes[..n]
                        .iter()
                        .any(|(binding, candidate)| *binding == i && *candidate == hash)
                    {
                        visit(rowid);
                    }
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn value_cache_complete(&self, table_index: usize, columns: &[u16]) -> bool {
        let table = &self.tables[table_index];
        (0..table.n_enforcers).any(|index| {
            let enforcer = table.enforcers[index].expect("enforcer present");
            enforcer.columns() == columns
                && self
                    .value_indexes
                    .as_ref()
                    .expect("value index pool present")
                    .get(enforcer.slot)
                    .is_complete()
        })
    }

    pub fn value_probe_complete(&self, table_index: usize, columns: &[u16]) -> bool {
        let table = &self.tables[table_index];
        (0..table.n_enforcers).any(|index| {
            let enforcer = table.enforcers[index].expect("enforcer present");
            enforcer.columns() == columns
                && (self
                    .value_indexes
                    .as_ref()
                    .expect("value index pool present")
                    .get(enforcer.slot)
                    .is_complete()
                    || enforcer
                        .durable
                        .is_some_and(|handle| self.commit_snapshot >= handle.published_lsn))
        })
    }

    pub fn value_durable_complete(&self, table_index: usize, columns: &[u16]) -> bool {
        let table = &self.tables[table_index];
        (0..table.n_enforcers).any(|index| {
            let enforcer = table.enforcers[index].expect("enforcer present");
            enforcer.columns() == columns
                && enforcer
                    .durable
                    .is_some_and(|handle| self.commit_snapshot >= handle.published_lsn)
        })
    }

    /// Walks a manifest-published key generation and the resident changes
    /// newer than it. Encoded keys borrow reader scratch only for the callback.
    pub fn walk_value_index(
        &self,
        table_index: usize,
        columns: &[u16],
        mut visit: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<bool, SqlError> {
        let table = &self.tables[table_index];
        let Some((binding, handle)) = (0..table.n_enforcers).find_map(|binding| {
            let enforcer = table.enforcers[binding].expect("enforcer");
            (enforcer.columns() == columns).then_some((binding, enforcer.durable?))
        }) else {
            return Ok(false);
        };
        if self.commit_snapshot < handle.published_lsn {
            return Ok(false);
        }
        let Some(spill) = &self.spill else {
            return Ok(false);
        };
        let Some(mut scratch) = spill
            .value_scratch
            .iter()
            .find_map(|candidate| candidate.try_borrow_mut().ok())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "persistent value scans nested deeper than reader scratch"
            ));
        };
        {
            let ValueIndexScratch { roster, data } = &mut *scratch;
            let mut callback_error = Ok(());
            crate::store::ValueIndexReader::over(roster, data)
                .walk(
                    &mut *spill.blocks.borrow_mut(),
                    &handle,
                    |_, rowid, _, key| {
                        if callback_error.is_ok()
                            && let Err(error) = visit(rowid, key)
                        {
                            callback_error = Err(error);
                        }
                    },
                )
                .map_err(|error| {
                    sql_err!(
                        sqlstate::IO_ERROR,
                        "persistent value-index read: {:?}",
                        error
                    )
                })?;
            callback_error?;
        }
        // Overlay entries supersede or extend the published base. Duplicate
        // rowids are harmless because the ordinary WHERE/MVCC path rechecks
        // them; callers sort and deduplicate candidates before execution.
        for (&rowid, state) in table.rows.iter() {
            let Some(home) = state.committed else {
                continue;
            };
            let key_buffer = &mut scratch.roster;
            let (len, _) =
                self.encode_value_binding_key(table_index, binding, rowid, home, key_buffer)?;
            visit(rowid, &key_buffer[..len])?;
        }
        Ok(true)
    }

    pub(crate) fn value_binding_count(&self, table_index: usize) -> usize {
        self.tables[table_index].n_enforcers
    }

    pub(crate) fn value_binding_columns(
        &self,
        table_index: usize,
        binding: usize,
    ) -> ([u16; MAX_INDEX_COLS], usize) {
        let enforcer = self.tables[table_index].enforcers[binding].expect("binding");
        (enforcer.columns, enforcer.n_cols)
    }

    pub(crate) fn value_binding_handle(
        &self,
        table_index: usize,
        binding: usize,
    ) -> Option<crate::store::ValueIndexHandle> {
        self.tables[table_index].enforcers[binding]
            .expect("binding")
            .durable
    }

    pub(crate) fn value_binding_is_committed(&self, table_index: usize, binding: usize) -> bool {
        let enforcer = self.tables[table_index].enforcers[binding].expect("binding");
        let columns = enforcer.columns();
        let definition = &self.tables[table_index].def;
        definition
            .columns()
            .iter()
            .enumerate()
            .any(|(column, metadata)| metadata.unique && columns == [column as u16])
            || definition
                .uniques()
                .iter()
                .any(|unique| unique.columns() == columns)
            || self.indexes.iter().any(|index| {
                index.ddl_state == CatalogDdlState::Present
                    && index.schema == definition.schema
                    && index.table == definition.name
                    && &index.columns[..index.n_cols] == columns
            })
    }

    pub(crate) fn install_value_binding(
        &mut self,
        table_index: usize,
        columns: &[u16],
        handle: Option<crate::store::ValueIndexHandle>,
    ) -> Result<(), SqlError> {
        let n_enforcers = self.tables[table_index].n_enforcers;
        let Some(enforcer) = self.tables[table_index].enforcers[..n_enforcers]
            .iter_mut()
            .flatten()
            .find(|enforcer| enforcer.columns() == columns)
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "manifest value index has no catalog binding"
            ));
        };
        enforcer.durable = handle;
        Ok(())
    }

    /// Encodes one binding's key tuple into caller-owned checkpoint scratch.
    pub(crate) fn encode_value_binding_key(
        &self,
        table_index: usize,
        binding: usize,
        rowid: u64,
        home: RowHome,
        output: &mut [u8],
    ) -> Result<(usize, u64), SqlError> {
        let table = &self.tables[table_index];
        let enforcer = table.enforcers[binding].expect("binding");
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = table.def.schema(&mut schema);
        self.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..n_columns], &mut values)?;
            let mut key = [Datum::Null; MAX_INDEX_COLS];
            for (at, column) in enforcer.columns().iter().enumerate() {
                key[at] = values[*column as usize];
            }
            let key = &key[..enforcer.n_cols];
            let len = rowenc::encoded_len(key);
            if len > crate::store::VALUE_INDEX_KEY_MAX || len > output.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "index tuple exceeds the persistent block-key limit"
                ));
            }
            rowenc::encode(key, &mut output[..len]);
            Ok((len, hash_table_key(&table.def, &values, enforcer.columns())))
        })
    }

    /// Whether any row in the resident overlay has an uncommitted image.
    /// Access paths over the committed value cache conservatively decline
    /// while this is true; the ordinary scan then supplies exact command
    /// visibility for every transaction.
    pub fn has_pending_rows(&self, table_index: usize) -> bool {
        self.tables[table_index]
            .rows
            .iter()
            .any(|(_, state)| state.pending.is_some())
    }

    /// Releases a table's enforcer index slots back to the pool and clears its
    /// enforcer list. Called before a slot is reused and when a table is
    /// dropped.
    fn release_enforcers(&mut self, table_index: usize) {
        let n = self.tables[table_index].n_enforcers;
        if n == 0 {
            return;
        }
        let mut slots = [u32::MAX; MAX_VALUE_ENFORCERS];
        for (i, s) in slots.iter_mut().enumerate().take(n) {
            if let Some(e) = self.tables[table_index].enforcers[i] {
                *s = e.slot;
            }
        }
        if let Some(pool) = self.value_indexes.as_mut() {
            for &slot in slots.iter().take(n) {
                if slot != u32::MAX {
                    pool.release(slot);
                }
            }
        }
        self.tables[table_index].enforcers = [None; MAX_VALUE_ENFORCERS];
        self.tables[table_index].n_enforcers = 0;
    }

    /// Rebuilds every live table's value-index enforcers from its committed
    /// rows. Called once at startup after journal replay (whose row installs
    /// bypass the per-row commit maintenance), so the indexes reflect the
    /// recovered committed state before any query runs.
    pub fn rebuild_all_enforcers(&mut self) -> Result<(), SqlError> {
        for i in 0..self.tables.len() {
            if self.tables[i].live {
                self.refresh_enforcers(i)?;
            }
        }
        Ok(())
    }

    /// Rebuilds a table's value-cache enforcers from its current definition and
    /// named indexes, then repopulates them from the committed rows. Idempotent
    /// — call it whenever the definition, the index set, or the committed rows
    /// change outside the per-row [`Self::commit_row`] maintenance (ALTER, a
    /// committed CREATE, CREATE/DROP INDEX, cold-start replay).
    pub fn refresh_enforcers(&mut self, table_index: usize) -> Result<(), SqlError> {
        self.refresh_enforcers_visible(table_index, None)
    }

    /// Builds the cache shape visible to the owner of a pending CREATE INDEX
    /// before its WAL record becomes durable. Pool exhaustion is therefore a
    /// pre-commit DDL error, never a committed index followed by an error.
    pub fn prepare_index_enforcers(
        &mut self,
        table_index: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        self.refresh_enforcers_visible(table_index, Some(txid))
    }

    fn refresh_enforcers_visible(
        &mut self,
        table_index: usize,
        txid: Option<u32>,
    ) -> Result<(), SqlError> {
        // DDL reshapes the cache slots, but an unchanged column tuple keeps
        // its manifest-published object generation.
        let mut published = [([0u16; MAX_INDEX_COLS], 0usize, None); MAX_VALUE_ENFORCERS];
        let n_published = self.tables[table_index].n_enforcers;
        for (index, entry) in published.iter_mut().enumerate().take(n_published) {
            let enforcer = self.tables[table_index].enforcers[index].expect("enforcer");
            *entry = (enforcer.columns, enforcer.n_cols, enforcer.durable);
        }
        self.release_enforcers(table_index);
        let mut want = [([0u16; MAX_INDEX_COLS], 0usize); MAX_VALUE_ENFORCERS];
        let mut n_want = 0usize;
        let too_many = || {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a table can have at most {} distinct value-indexed column tuples",
                MAX_VALUE_ENFORCERS
            )
        };
        {
            let def = &self.tables[table_index].def;
            for (i, col) in def.columns().iter().enumerate() {
                if col.unique {
                    if n_want == MAX_VALUE_ENFORCERS {
                        return Err(too_many());
                    }
                    want[n_want].0[0] = i as u16;
                    want[n_want].1 = 1;
                    n_want += 1;
                }
            }
            for uk in def.uniques() {
                if n_want == MAX_VALUE_ENFORCERS {
                    return Err(too_many());
                }
                let cols = uk.columns();
                want[n_want].0[..cols.len()].copy_from_slice(cols);
                want[n_want].1 = cols.len();
                n_want += 1;
            }
        }
        // Named indexes use the same cache. Identical column tuples
        // share one enforcer: PostgreSQL permits redundant indexes, but a
        // second copy cannot improve the equality probe and would waste a
        // startup-reserved pool slot.
        let table_schema = self.tables[table_index].def.schema;
        let table_name = self.tables[table_index].def.name;
        for index in self.indexes.iter().filter(|index| {
            txid.map_or(index.ddl_state == CatalogDdlState::Present, |owner| {
                index.visible_to(owner)
            }) && index.schema == table_schema
                && index.table == table_name
                // A value enforcer represents every table row for its key.
                // Partial membership is predicate-defined, so it has a
                // separate authoritative enforcement path.
                && index.predicate.is_none()
                // Expression keys cannot be represented by a column-tuple
                // cache without changing their SQL semantics.
                && index.expressions[..index.n_cols].iter().all(Option::is_none)
        }) {
            let columns = &index.columns[..index.n_cols];
            if want[..n_want]
                .iter()
                .any(|(cached, n)| &cached[..*n] == columns)
            {
                continue;
            }
            if n_want == MAX_VALUE_ENFORCERS {
                return Err(too_many());
            }
            want[n_want].0[..columns.len()].copy_from_slice(columns);
            want[n_want].1 = columns.len();
            n_want += 1;
        }
        for (w, (wanted_columns, wanted_count)) in want.iter().take(n_want).enumerate() {
            let slot = match self.value_indexes.as_mut().expect("pool present").acquire() {
                Some(s) => s,
                None => {
                    // Give back what this call already took, then fail loudly.
                    self.release_enforcers(table_index);
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "value index pool exhausted (raise max_value_indexes)"
                    ));
                }
            };
            self.tables[table_index].enforcers[w] = Some(Enforcer {
                slot,
                columns: *wanted_columns,
                n_cols: *wanted_count,
                durable: published[..n_published]
                    .iter()
                    .find(|(columns, n_columns, _)| {
                        *n_columns == *wanted_count
                            && columns[..*n_columns] == wanted_columns[..*wanted_count]
                    })
                    .and_then(|(_, _, handle)| *handle),
            });
            // Keep the installed prefix visible to `release_enforcers`, so an
            // acquire failure later in this loop returns every slot already
            // taken by this rebuild.
            self.tables[table_index].n_enforcers = w + 1;
        }
        self.populate_enforcers(table_index)?;
        Ok(())
    }

    /// Populates a table's enforcer indexes from its committed rows. Takes the
    /// pool out of `self` so the row walk (which borrows the rest of `self`) and
    /// the index inserts do not overlap.
    fn populate_enforcers(&mut self, table_index: usize) -> Result<(), SqlError> {
        if self.tables[table_index].n_enforcers == 0 {
            return Ok(());
        }
        let mut pool = self.value_indexes.take().expect("pool present");
        let result = self.populate_into(table_index, &mut pool);
        self.value_indexes = Some(pool);
        result
    }

    fn populate_into(&self, table_index: usize, pool: &mut ValueIndexPool) -> Result<(), SqlError> {
        let n_enf = self.tables[table_index].n_enforcers;
        let mut slots = [u32::MAX; MAX_VALUE_ENFORCERS];
        for (i, s) in slots.iter_mut().enumerate().take(n_enf) {
            *s = self.tables[table_index].enforcers[i]
                .expect("enforcer")
                .slot;
        }
        let mut decode_error: Result<(), SqlError> = Ok(());
        let mut incomplete = false;
        let mut buf = [(0usize, 0u64); MAX_VALUE_ENFORCERS];
        self.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(home) = state.committed else {
                return Ok(ControlFlow::Continue(()));
            };
            let n = match self.row_enforcer_hashes(table_index, rowid, home, &mut buf) {
                Ok(n) => n,
                Err(e) => {
                    decode_error = Err(e);
                    return Ok(ControlFlow::Break(()));
                }
            };
            for &(ei, hash) in &buf[..n] {
                let index = pool.get_mut(slots[ei]);
                if index.insert(hash, rowid).is_err() {
                    incomplete = true;
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
        decode_error?;
        if incomplete {
            // The walk stopped at the first exhausted cache. Every enforcer
            // may therefore be missing later rows, including caches that did
            // not themselves fill, so completeness is invalidated as a set.
            for &slot in &slots[..n_enf] {
                pool.get_mut(slot).mark_incomplete();
            }
        }
        Ok(())
    }

    /// Committed-catalog lookup (ignores uncommitted DDL): used by journal
    /// replay and any context that operates on the durable image.
    pub fn find_table(&self, schema: &str, name: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.live && t.def.schema.as_str() == schema && t.def.name.as_str() == name)
    }

    /// Transaction-scoped lookup: `txid` sees its own uncommitted CREATE/DROP
    /// and every committed table, but not another transaction's uncommitted
    /// DDL.
    pub fn find_visible(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.tables.iter().enumerate().position(|(index, table)| {
            table.visible_to(txid)
                && self.table_def(index, txid).schema.as_str() == schema
                && self.table_def(index, txid).name.as_str() == name
        })
    }

    pub fn table(&self, index: usize) -> &Table {
        &self.tables[index]
    }

    pub fn table_mut(&mut self, index: usize) -> &mut Table {
        &mut self.tables[index]
    }

    pub fn table_def(&self, index: usize, txid: u32) -> &TableDef {
        match self.pending_table_def(index) {
            Some(pending) if pending.txid == txid => &pending.def,
            _ => &self.tables[index].def,
        }
    }

    pub fn has_pending_table_def(&self, index: usize, txid: u32) -> bool {
        self.tables[index].pending_def_txid == Some(txid)
    }

    fn pending_table_def(&self, index: usize) -> Option<&PendingTableDef> {
        let table = &self.tables[index];
        let position = table.n_pending_defs.checked_sub(1)? as usize;
        let slot = table.pending_def_slots[position] as usize;
        Some(&self.pending_table_defs[slot].version)
    }

    fn clear_pending_table_defs(&mut self, index: usize) {
        let count = self.tables[index].n_pending_defs as usize;
        for position in 0..count {
            let slot = self.tables[index].pending_def_slots[position] as usize;
            self.pending_table_defs[slot].used = false;
            self.tables[index].pending_def_slots[position] = u32::MAX;
        }
        self.tables[index].n_pending_defs = 0;
        self.tables[index].pending_def_txid = None;
    }

    /// Installs the next transaction-owned table shape. `column_mapping`
    /// describes the transition from the previously visible definition to
    /// `def`; the stored mapping is composed back to the committed definition.
    pub fn write_table_def(
        &mut self,
        index: usize,
        txid: u32,
        def: TableDef,
        column_mapping: &[Option<SqlName>; MAX_COLUMNS],
        rewrites_rows: bool,
    ) -> Result<(), SqlError> {
        if let Some(other) = self.tables[index].ddl_locked_by_other(txid) {
            self.row_locks.borrow_mut().wait_for(txid, other)?;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on \"{}\"",
                self.tables[index].def.name.as_str()
            ));
        }
        if self.tables[index].n_pending_defs as usize == MAX_PENDING_TABLE_DEFS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one transaction applies more than {} table-definition versions to one table",
                MAX_PENDING_TABLE_DEFS
            ));
        }
        let current = *self.table_def(index, txid);
        let prior = self.pending_table_def(index).copied();
        let mut composed = [None; MAX_COLUMNS];
        for (committed_column, target) in composed
            .iter_mut()
            .enumerate()
            .take(self.tables[index].def.n_columns)
        {
            let current_name = match prior {
                Some(version) if version.txid == txid => version.column_mapping[committed_column],
                _ => Some(self.tables[index].def.columns()[committed_column].name),
            };
            let Some(current_name) = current_name else {
                continue;
            };
            let Some(current_column) = current.column_index(current_name.as_str()) else {
                continue;
            };
            *target = column_mapping[current_column];
        }
        let version = PendingTableDef {
            txid,
            def,
            column_mapping: composed,
            rewrites_rows: rewrites_rows
                || prior.is_some_and(|version| version.txid == txid && version.rewrites_rows),
        };
        let slot = match self.pending_table_defs.iter().position(|entry| !entry.used) {
            Some(slot) => {
                self.pending_table_defs[slot] = PendingTableDefSlot {
                    used: true,
                    version,
                };
                slot
            }
            None => {
                let slot = self.pending_table_defs.len();
                self.pending_table_defs
                    .push(PendingTableDefSlot {
                        used: true,
                        version,
                    })
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "pending table-definition pool is exhausted"
                        )
                    })?;
                slot
            }
        };
        let position = self.tables[index].n_pending_defs as usize;
        self.tables[index].pending_def_slots[position] = slot as u32;
        self.tables[index].n_pending_defs += 1;
        self.tables[index].pending_def_txid = Some(txid);
        Ok(())
    }

    pub fn rollback_table_def(&mut self, index: usize, txid: u32) {
        if self.tables[index].pending_def_txid != Some(txid) {
            return;
        }
        let Some(position) = self.tables[index].n_pending_defs.checked_sub(1) else {
            return;
        };
        let slot = self.tables[index].pending_def_slots[position as usize] as usize;
        self.pending_table_defs[slot].used = false;
        self.tables[index].pending_def_slots[position as usize] = u32::MAX;
        self.tables[index].n_pending_defs = position;
        if position == 0 {
            self.tables[index].pending_def_txid = None;
        }
    }

    /// Promotes the latest pending definition after its WAL batch is durable.
    /// Row images are promoted separately by the transaction coordinator.
    pub fn commit_table_def(&mut self, index: usize, txid: u32) -> bool {
        let Some(pending) = self.pending_table_def(index).copied() else {
            return false;
        };
        if pending.txid != txid {
            return false;
        }
        self.set_table_def(index, pending.def, &pending.column_mapping);
        self.clear_pending_table_defs(index);
        self.rename_stored_query_dependency(
            DependencyClass::Table,
            index,
            pending.def.schema,
            pending.def.name,
        );
        pending.rewrites_rows
    }

    pub fn finish_table_def_commit(&mut self, index: usize, rewrote_rows: bool) {
        if rewrote_rows {
            self.set_spill_list(index, &[]);
        }
    }

    /// Allocates a slot for a fresh table. Shared by replay (committed) and
    /// the executor (pending); `pending` overlays the uncommitted-CREATE
    /// state so the table is invisible to other transactions until commit.
    fn alloc_table(
        &mut self,
        def: TableDef,
        pending: Option<PendingDdl>,
    ) -> Result<usize, SqlError> {
        let Some(slot) = self.tables.iter().position(Table::is_free) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many tables (limit {})",
                self.tables.len()
            ));
        };
        // A reused slot must not keep the dropped table's value indexes.
        self.release_enforcers(slot);
        self.clear_pending_table_defs(slot);
        self.clear_pending_table_statistics(slot);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Table,
            slot: slot as u16,
        });
        let ownership = self.initial_ownership(pending.map_or(0, |pending| pending.txid));
        self.catalog_seq += 1;
        let stamp = self.catalog_seq;
        let table = &mut self.tables[slot];
        table.def = def;
        table.ownership = ownership;
        table.created_at = stamp;
        table.rows.clear();
        table.live = pending.is_none();
        table.pending_ddl = pending;
        table.mark_dirty();
        table.statistics = TableStatistics::EMPTY;
        table.statistics_dirty = false;
        table.statistics_wal_dirty = false;
        // A reused slot must not inherit the dropped table's sequences or
        // spilled rows.
        table.serial_last = [0; MAX_COLUMNS];
        table.serial_dirty = false;
        table.spill_ssts = [None; MAX_SPILL_SSTS];
        table.n_spill_ssts = 0;
        table.n_tombstones = 0;
        table.tombstones_overflow = false;
        let schema = table.def.schema;
        let name = table.def.name;
        self.rename_stored_query_dependency(DependencyClass::Table, slot, schema, name);
        Ok(slot)
    }

    /// Committed create (journal replay): the table is immediately part of the
    /// durable image.
    /// Rebinds every persisted user-defined column type from its stable name to
    /// the current catalog slot. Slots are runtime identities and may change
    /// across restart; scalar enums and enum/domain arrays therefore never
    /// trust the slot encoded in the table definition.
    fn bind_user_type_columns(&self, def: &mut TableDef) -> Result<(), SqlError> {
        for i in 0..def.n_columns {
            let col = &def.columns[i];
            match col.ctype {
                ColType::Enum(_) | ColType::Array(ArrElem::Enum(_)) => {
                    let UserTypeName { schema, name } = col.user_type.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "reloaded enum column has no type identity"
                        )
                    })?;
                    let slot = self
                        .enum_slot(schema.as_str(), name.as_str(), 0)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "enum type \"{}.{}\" for a reloaded column does not exist",
                                schema.as_str(),
                                name.as_str()
                            )
                        })?;
                    def.columns[i].ctype = if matches!(col.ctype, ColType::Array(_)) {
                        ColType::Array(ArrElem::Enum(slot as u16))
                    } else {
                        ColType::Enum(slot as u16)
                    };
                }
                ColType::Array(ArrElem::Domain { .. }) => {
                    let UserTypeName { schema, name } = col.user_type.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "reloaded domain-array column has no type identity"
                        )
                    })?;
                    let slot = self
                        .domain_slot(schema.as_str(), name.as_str(), 0)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "domain type \"{}.{}\" for a reloaded column does not exist",
                                schema.as_str(),
                                name.as_str()
                            )
                        })?;
                    let domain = self.domain(slot);
                    let element = ArrElem::domain(slot as u16, domain.base).ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "arrays of domain {} require a scalar base type",
                            name.as_str()
                        )
                    })?;
                    def.columns[i].ctype = ColType::Array(element);
                }
                _ if col.user_type.is_some_and(|identity| {
                    self.domain_slot(identity.schema.as_str(), identity.name.as_str(), 0)
                        .is_some()
                }) =>
                {
                    let identity = col.user_type.expect("domain identity checked above");
                    let slot = self
                        .domain_slot(identity.schema.as_str(), identity.name.as_str(), 0)
                        .expect("domain identity checked above");
                    // Scalar domains execute as their base representation, but
                    // their durable identity is the domain.  Resolve it before
                    // the `Composite`/`Enum` cases below so a domain named
                    // independently from its composite base cannot be
                    // mistaken for that base during replay.
                    def.columns[i].ctype = self.domain(slot).base;
                }
                ColType::Composite(_) | ColType::Array(ArrElem::Composite(_)) => {
                    let UserTypeName { schema, name } = col.user_type.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "reloaded composite column has no type identity"
                        )
                    })?;
                    let slot = self
                        .composite_slot(schema.as_str(), name.as_str(), 0)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "composite type \"{}.{}\" for a reloaded column does not exist",
                                schema.as_str(),
                                name.as_str()
                            )
                        })?;
                    def.columns[i].ctype = if matches!(col.ctype, ColType::Array(_)) {
                        ColType::Array(ArrElem::Composite(slot as u16))
                    } else {
                        ColType::Composite(slot as u16)
                    };
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn create_table(&mut self, mut def: TableDef) -> Result<usize, SqlError> {
        if self
            .find_table(def.schema.as_str(), def.name.as_str())
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        // A reloaded enum column decodes as `Enum(ENUM_SLOT_UNRESOLVED)` plus the
        // enum's name (in `domain`); bind it to the live catalog slot now that
        // the enum has itself been loaded (WAL/checkpoint order guarantees it).
        self.bind_user_type_columns(&mut def)?;
        let slot = self.alloc_table(def, None)?;
        // Build the (empty) enforcers now; replay repopulates them once its rows
        // are applied (see the rebuild in Engine startup).
        self.refresh_enforcers(slot)?;
        Ok(slot)
    }

    /// Starts replaying ALTER TABLE's two-record in-place rewrite.
    pub fn begin_replay_table_rewrite(
        &mut self,
        previous_schema: &str,
        previous_name: &str,
        preserve_rows: bool,
        column_mapping: [u16; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        if self.replay_table_rewrite.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "corrupt journal contains nested table rewrite markers"
            ));
        }
        let Some(index) = self.find_table(previous_schema, previous_name) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "journal rewrites unknown table \"{}\"",
                previous_name
            ));
        };
        self.replay_table_rewrite = Some(ReplayTableRewrite {
            table: index,
            preserve_rows,
            column_mapping,
        });
        Ok(())
    }

    /// Completes a pending ALTER TABLE replay when its final definition arrives.
    /// Returns false when the definition is an ordinary CREATE TABLE.
    pub fn complete_replay_table_rewrite(&mut self, mut def: TableDef) -> Result<bool, SqlError> {
        let Some(rewrite) = self.replay_table_rewrite.take() else {
            return Ok(false);
        };
        self.bind_user_type_columns(&mut def)?;
        let mut column_mapping = [None; MAX_COLUMNS];
        for (old_column, &target_column) in rewrite.column_mapping.iter().enumerate() {
            if target_column == u16::MAX {
                continue;
            }
            let Some(target) = def.columns().get(target_column as usize) else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt journal maps table column {} past the rewritten definition",
                    target_column
                ));
            };
            column_mapping[old_column] = Some(target.name);
        }
        let index = rewrite.table;
        self.set_table_def(index, def, &column_mapping);
        if !rewrite.preserve_rows {
            self.tables[index].rows.clear();
            self.tables[index].statistics = TableStatistics::EMPTY;
            self.tables[index].statistics_wal_dirty = false;
            self.set_spill_list(index, &[]);
        }
        Ok(true)
    }

    pub fn ensure_no_pending_replay_table_rewrite(&self) -> Result<(), SqlError> {
        if self.replay_table_rewrite.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "corrupt journal ends during a table rewrite"
            ));
        }
        Ok(())
    }

    /// Transactional create: the table exists only for `txid` until commit.
    /// A name already visible to `txid` is a duplicate (42P07); a name held by
    /// another transaction's uncommitted DDL joins the shared wait graph.
    pub fn create_table_in(&mut self, def: TableDef, txid: u32) -> Result<usize, SqlError> {
        self.require_schema_create(def.schema.as_str(), txid)?;
        if self
            .find_visible(def.schema.as_str(), def.name.as_str(), txid)
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        if let Some(other) =
            self.ddl_name_locked_by_other(def.schema.as_str(), def.name.as_str(), txid)
        {
            self.row_locks.borrow_mut().wait_for(txid, other)?;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on \"{}\"",
                def.name.as_str()
            ));
        }
        let slot = self.alloc_table(
            def,
            Some(PendingDdl {
                txid,
                creating: true,
            }),
        )?;
        // Build the enforcers now (fallible pool acquire surfaces at CREATE, not
        // at commit); this transaction's inserts maintain them at commit.
        self.refresh_enforcers(slot)?;
        Ok(slot)
    }

    /// The txid of another transaction holding uncommitted DDL for `name`.
    fn ddl_name_locked_by_other(&self, schema: &str, name: &str, txid: u32) -> Option<u32> {
        self.tables
            .iter()
            .enumerate()
            .filter(|(index, table)| {
                (table.def.schema.as_str() == schema && table.def.name.as_str() == name)
                    || self.pending_table_def(*index).is_some_and(|pending| {
                        pending.def.schema.as_str() == schema && pending.def.name.as_str() == name
                    })
            })
            .find_map(|(_, table)| table.ddl_locked_by_other(txid))
    }

    /// Committed drop (journal replay): rows are retained; the slot is freed at
    /// checkpoint.
    pub fn drop_table(&mut self, index: usize) {
        let (schema, name) = (self.tables[index].def.schema, self.tables[index].def.name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.release_enforcers(index);
        self.clear_pending_table_defs(index);
        self.clear_pending_table_statistics(index);
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].mark_dirty();
        self.commit_triggers_for_table(index);
        self.commit_policies_for_table(index);
    }

    /// Transactional drop: the table stays visible to every other transaction
    /// (committed baseline) until `txid` commits.
    pub fn drop_table_in(&mut self, index: usize, txid: u32) {
        self.tables[index].pending_ddl = Some(PendingDdl {
            txid,
            creating: false,
        });
        self.tables[index].mark_dirty();
    }

    /// Promotes an uncommitted CREATE to the committed image.
    pub fn commit_create(&mut self, index: usize) {
        self.tables[index].live = true;
        self.tables[index].pending_ddl = None;
    }

    /// Applies a committed DROP: the table leaves the image and its rows are
    /// reclaimed.
    pub fn commit_drop(&mut self, index: usize) {
        let (schema, name) = (self.tables[index].def.schema, self.tables[index].def.name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.release_enforcers(index);
        self.clear_pending_table_defs(index);
        self.clear_pending_table_statistics(index);
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].rows.clear();
        self.tables[index].statistics_wal_dirty = false;
        self.commit_triggers_for_table(index);
        self.commit_policies_for_table(index);
    }

    /// Rolls back an uncommitted CREATE, freeing the slot.
    pub fn rollback_create(&mut self, index: usize) {
        self.release_enforcers(index);
        self.clear_pending_table_defs(index);
        self.clear_pending_table_statistics(index);
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].rows.clear();
        self.tables[index].statistics_wal_dirty = false;
    }

    /// Rolls back an uncommitted DROP: the table returns to the committed
    /// image unchanged.
    pub fn rollback_drop(&mut self, index: usize) {
        self.tables[index].pending_ddl = None;
    }

    /// Whether any live view exists (lets the executor skip view expansion).
    pub fn has_any_view(&self) -> bool {
        self.views
            .iter()
            .any(|view| view.ddl_state != CatalogDdlState::Absent)
    }

    /// Committed views as (name, SELECT text), for checkpoint serialization.
    pub fn live_views(&self) -> impl Iterator<Item = &ViewDef> {
        self.views
            .iter()
            .filter(|view| view.ddl_state == CatalogDdlState::Present)
    }

    /// Committed views with their slot indices, for OID assignment.
    pub fn views_with_slots(&self) -> impl Iterator<Item = (usize, &ViewDef)> {
        self.views
            .iter()
            .enumerate()
            .filter(|(_, view)| view.ddl_state == CatalogDdlState::Present)
    }

    /// Views visible to `txid`, including the transaction's own DDL.
    pub(crate) fn views_visible_to(&self, txid: u32) -> impl Iterator<Item = (usize, &ViewDef)> {
        self.views
            .iter()
            .enumerate()
            .filter(move |(_, view)| view.visible_to(txid))
    }

    pub(crate) fn view(&self, slot: usize) -> &ViewDef {
        &self.views[slot]
    }

    pub(crate) fn view_dependencies(&self, slot: usize) -> &StoredQueryDependencies {
        &self.view_dependencies[slot]
    }

    pub(crate) fn view_count(&self) -> usize {
        self.views.len()
    }

    /// Committed publications for catalog visibility and replication setup.
    pub fn live_publications(&self) -> impl Iterator<Item = &PublicationDef> {
        self.publications
            .iter()
            .filter(|publication| publication.ddl_state == CatalogDdlState::Present)
    }

    pub fn publications_with_slots(&self) -> impl Iterator<Item = (usize, &PublicationDef)> {
        self.publications
            .iter()
            .enumerate()
            .filter(|(_, publication)| publication.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn publications_with_slots_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &PublicationDef)> {
        self.publications
            .iter()
            .enumerate()
            .filter(move |(_, publication)| publication.visible_to(txid))
    }

    /// Committed publication lookup for replication protocol setup.
    pub(crate) fn publication(&self, name: &str) -> Option<&PublicationDef> {
        self.live_publications()
            .find(|publication| publication.name.as_str() == name)
    }

    pub(crate) fn publication_definition(
        &self,
        name: &str,
        txid: u32,
    ) -> Option<(usize, PublicationDefinition)> {
        self.publications
            .iter()
            .enumerate()
            .find_map(|(slot, publication)| {
                (publication.visible_to(txid) && publication.name_for(txid).as_str() == name)
                    .then_some((slot, publication.definition_for(txid)))
            })
    }

    pub(crate) fn publication_owner(&self, slot: usize, txid: u32) -> u16 {
        self.publications[slot].ownership.owner_to(txid)
    }

    pub(crate) fn restore_publication_owner(&mut self, slot: usize, owner: u16) {
        self.publications[slot].ownership = Ownership {
            owner,
            pending: None,
        };
    }

    pub(crate) fn set_publication_owner(
        &mut self,
        slot: usize,
        owner: usize,
        txid: u32,
    ) -> Result<Option<PendingOwnership>, SqlError> {
        self.ensure_publication_changeable(slot, txid)?;
        let ownership = &mut self.publications[slot].ownership;
        let prior = ownership.pending;
        ownership.pending = Some(PendingOwnership {
            txid,
            owner: owner as u16,
        });
        Ok(prior)
    }

    pub(crate) fn restore_publication_owner_pending(
        &mut self,
        slot: usize,
        prior: Option<PendingOwnership>,
    ) {
        self.publications[slot].ownership.pending = prior;
    }

    pub(crate) fn rename_publication(
        &mut self,
        slot: usize,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingPublicationName>, SqlError> {
        self.ensure_publication_changeable(slot, txid)?;
        if self
            .publications
            .iter()
            .enumerate()
            .any(|(other_slot, publication)| {
                other_slot != slot
                    && publication.visible_to(txid)
                    && publication.name_for(txid) == name
            })
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "publication \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(blocker) =
            self.publications
                .iter()
                .enumerate()
                .find_map(|(other_slot, publication)| {
                    (other_slot != slot)
                        .then_some(publication.pending_name)
                        .flatten()
                        .filter(|pending| pending.name == name && pending.txid != txid)
                        .map(|pending| pending.txid)
                })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let publication = &mut self.publications[slot];
        if let Some(pending) = publication.pending_name
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "publication \"{}\" is being renamed by another transaction",
                publication.name.as_str()
            ));
        }
        let prior = publication.pending_name;
        publication.pending_name = Some(PendingPublicationName { txid, name });
        Ok(prior)
    }

    pub(crate) fn commit_publication_rename(&mut self, slot: usize, txid: u32) {
        let publication = &mut self.publications[slot];
        if let Some(pending) = publication.pending_name
            && pending.txid == txid
        {
            publication.name = pending.name;
            publication.pending_name = None;
        }
    }

    pub(crate) fn rollback_publication_rename(
        &mut self,
        slot: usize,
        prior: Option<PendingPublicationName>,
    ) {
        self.publications[slot].pending_name = prior;
    }

    pub(crate) fn publication_selecting_schema(
        &self,
        schema: u8,
        txid: u32,
    ) -> Option<(SqlName, PublicationDefinition)> {
        self.publications.iter().find_map(|publication| {
            let definition = publication.definition_for(txid);
            (publication.visible_to(txid)
                && definition.schemas[..definition.schema_count].contains(&schema))
            .then_some((publication.name_for(txid), definition))
        })
    }

    pub(crate) fn require_publication_owner(&self, slot: usize, txid: u32) -> Result<(), SqlError> {
        if txid == 0 {
            return Ok(());
        }
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let owner = self.publications[slot].ownership.owner_to(txid) as usize;
        if self.role(role).attributes_to(txid).superuser
            || owner == role
            || self.role_can_set(role, owner, txid)
        {
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be owner of publication {}",
            self.publications[slot].name.as_str()
        ))
    }

    pub(crate) fn create_replication_slot(
        &mut self,
        name: ReplicationSlotName,
        restart_lsn: u64,
        behavior: ReplicationSlotBehavior,
    ) -> Result<usize, SqlError> {
        if self
            .replication_slots
            .iter()
            .any(|slot| slot.live && slot.name.as_str() == name.as_str())
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "replication slot \"{}\" already exists",
                name.as_str()
            ));
        }
        let Some(index) = self.replication_slots.iter().position(|slot| !slot.live) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many replication slots (limit {})",
                self.replication_slots.len()
            ));
        };
        self.replication_slots[index] = ReplicationSlotDef {
            name: name.sql_name(),
            restart_lsn,
            confirmed_flush_lsn: restart_lsn,
            behavior,
            active: false,
            live: true,
        };
        Ok(index)
    }

    pub(crate) fn replication_slot(&self, name: &str) -> Option<&ReplicationSlotDef> {
        self.replication_slots
            .iter()
            .find(|slot| slot.live && slot.name.as_str() == name)
    }

    pub(crate) fn alter_replication_slot(
        &mut self,
        name: ReplicationSlotName,
        behavior: ReplicationSlotBehavior,
    ) -> Result<(), SqlError> {
        let slot = self
            .replication_slots
            .iter_mut()
            .find(|slot| slot.live && slot.name == name.sql_name())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "replication slot \"{}\" does not exist",
                    name.as_str()
                )
            })?;
        if slot.active {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slot \"{}\" is active",
                name.as_str()
            ));
        }
        slot.behavior = behavior;
        Ok(())
    }

    pub(crate) fn replication_slots_with_slots(
        &self,
    ) -> impl Iterator<Item = (usize, &ReplicationSlotDef)> {
        self.replication_slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.live)
    }

    pub(crate) fn replication_slot_capacity(&self) -> usize {
        self.replication_slots.len()
    }

    /// The oldest durable point any logical consumer may still request.
    /// WAL segment collection must retain the segment straddling this LSN.
    pub(crate) fn oldest_replication_restart_lsn(&self) -> Option<u64> {
        self.replication_slots_with_slots()
            .map(|(_, slot)| slot.restart_lsn)
            .min()
    }

    pub(crate) fn restore_replication_slot(
        &mut self,
        name: ReplicationSlotName,
        restart_lsn: u64,
        confirmed_flush_lsn: u64,
        behavior: ReplicationSlotBehavior,
    ) -> Result<(), SqlError> {
        if confirmed_flush_lsn < restart_lsn {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "replication slot confirmed LSN precedes restart LSN"
            ));
        }
        let slot = self.create_replication_slot(name, restart_lsn, behavior)?;
        self.replication_slots[slot].confirmed_flush_lsn = confirmed_flush_lsn;
        Ok(())
    }

    pub(crate) fn drop_replication_slot(&mut self, name: &str) -> Result<(), SqlError> {
        let Some(slot) = self
            .replication_slots
            .iter_mut()
            .find(|slot| slot.live && slot.name.as_str() == name)
        else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "replication slot \"{}\" does not exist",
                name
            ));
        };
        if slot.active {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slot \"{}\" is active",
                name
            ));
        }
        *slot = ReplicationSlotDef {
            name: SqlName::EMPTY,
            restart_lsn: 0,
            confirmed_flush_lsn: 0,
            behavior: ReplicationSlotBehavior::DEFAULT,
            active: false,
            live: false,
        };
        Ok(())
    }

    pub(crate) fn prepare_replication_slot_advance(
        &self,
        name: &str,
        confirmed_flush_lsn: u64,
    ) -> Result<ReplicationSlotAdvance, SqlError> {
        let (index, slot) = self
            .replication_slots
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.live && slot.name.as_str() == name)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "replication slot \"{}\" does not exist",
                    name
                )
            })?;
        if confirmed_flush_lsn < slot.confirmed_flush_lsn {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "replication slot confirmed LSN cannot move backwards"
            ));
        }
        Ok(ReplicationSlotAdvance {
            slot: index,
            name: slot.name,
            confirmed_flush_lsn,
        })
    }

    pub(crate) fn apply_replication_slot_advance(&mut self, advance: ReplicationSlotAdvance) {
        let slot = self
            .replication_slots
            .get_mut(advance.slot)
            .filter(|slot| slot.live && slot.name == advance.name)
            .expect("validated replication slot must remain live until its WAL commit");
        slot.confirmed_flush_lsn = advance.confirmed_flush_lsn;
        slot.restart_lsn = advance.confirmed_flush_lsn;
    }

    pub(crate) fn activate_replication_slot(&mut self, name: &str) -> Result<u64, SqlError> {
        let slot = self
            .replication_slots
            .iter_mut()
            .find(|slot| slot.live && slot.name.as_str() == name)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "replication slot \"{}\" does not exist",
                    name
                )
            })?;
        if slot.active {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slot \"{}\" is active",
                name
            ));
        }
        slot.active = true;
        Ok(slot.confirmed_flush_lsn)
    }

    pub(crate) fn deactivate_replication_slot(&mut self, name: &str) {
        if let Some(slot) = self
            .replication_slots
            .iter_mut()
            .find(|slot| slot.live && slot.name.as_str() == name)
        {
            slot.active = false;
        }
    }

    pub(crate) fn subscriptions_with_slots_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &SubscriptionDef)> {
        self.subscriptions
            .iter()
            .enumerate()
            .filter(move |(_, subscription)| subscription.visible_to(txid))
    }

    pub(crate) fn subscriptions_with_slots_durable(
        &self,
    ) -> impl Iterator<Item = (usize, &SubscriptionDef)> {
        self.subscriptions
            .iter()
            .enumerate()
            .filter(|(_, subscription)| {
                subscription.ddl_state == CatalogDdlState::Present
                    || subscription.cleanup != SubscriptionCleanup::None
            })
    }

    pub(crate) fn subscription(&self, name: &str, txid: u32) -> Option<(usize, &SubscriptionDef)> {
        self.subscriptions_with_slots_visible_to(txid)
            .find(|(_, subscription)| subscription.name_for(txid).as_str() == name)
    }

    pub(crate) fn create_subscription(
        &mut self,
        spec: SubscriptionSpec<'_>,
        txid: u32,
    ) -> Result<usize, SqlError> {
        let owner = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        if spec.publications.is_empty() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "CREATE SUBSCRIPTION requires at least one publication"
            ));
        }
        if spec.publications.len() > MAX_SUBSCRIPTION_PUBLICATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many publications in subscription (limit {})",
                MAX_SUBSCRIPTION_PUBLICATIONS
            ));
        }
        if !matches!(
            (spec.slot, spec.bootstrap),
            (
                SubscriptionSlot::Absent,
                SubscriptionBootstrap::Deferred
                    | SubscriptionBootstrap::CopyWithoutSlot
                    | SubscriptionBootstrap::Ready
            ) | (
                SubscriptionSlot::External(_),
                SubscriptionBootstrap::Deferred
                    | SubscriptionBootstrap::CopyExternalSlot
                    | SubscriptionBootstrap::Refresh { .. }
                    | SubscriptionBootstrap::Ready
            ) | (
                SubscriptionSlot::Managed(_),
                SubscriptionBootstrap::CreateManagedSlot { .. }
                    | SubscriptionBootstrap::Refresh { .. }
                    | SubscriptionBootstrap::Ready
            )
        ) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "subscription slot ownership does not match its bootstrap state"
            ));
        }
        if self.subscriptions.iter().any(|subscription| {
            subscription.visible_to(txid) && subscription.name_for(txid) == spec.name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "subscription \"{}\" already exists",
                spec.name.as_str()
            ));
        }
        let Some(slot) = self.subscriptions.iter().position(|subscription| {
            subscription.ddl_state == CatalogDdlState::Absent
                && subscription.cleanup == SubscriptionCleanup::None
        }) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many subscriptions (limit {})",
                self.subscriptions.len()
            ));
        };
        let mut publications = [SqlName::EMPTY; MAX_SUBSCRIPTION_PUBLICATIONS];
        publications[..spec.publications.len()].copy_from_slice(spec.publications);
        self.catalog_seq += 1;
        self.subscriptions[slot] = SubscriptionDef {
            created_at: self.catalog_seq,
            definition_generation: 1,
            name: spec.name,
            pending_name: None,
            connection: spec.connection,
            publications,
            publication_count: spec.publications.len(),
            pending_definition: None,
            enabled: spec.enabled,
            pending_enabled: None,
            slot: spec.slot,
            behavior: spec.behavior,
            bootstrap: spec.bootstrap,
            pending_bootstrap: None,
            cleanup: SubscriptionCleanup::None,
            failure: None,
            confirmed_lsn: 0,
            ownership: Ownership {
                owner: owner as u16,
                pending: None,
            },
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub(crate) fn drop_subscription(
        &mut self,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        let Some(slot) = self.subscriptions.iter().position(|subscription| {
            subscription.visible_to(txid) && subscription.name_for(txid).as_str() == name
        }) else {
            return Ok(None);
        };
        self.subscriptions[slot].ddl_state = self.subscriptions[slot].ddl_state.drop_by(txid);
        Ok(Some(slot))
    }

    pub(crate) fn require_subscription_owner(
        &self,
        slot: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        let role = self.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let owner = self.subscriptions[slot].ownership.owner_to(txid) as usize;
        if self.role(role).attributes_to(txid).superuser || role == owner {
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be owner of subscription {}",
            self.subscriptions[slot].name.as_str()
        ))
    }

    pub(crate) fn commit_subscription_create(&mut self, slot: usize) {
        self.subscriptions[slot].ddl_state = self.subscriptions[slot].ddl_state.commit_create();
    }

    pub(crate) fn restore_subscription_stream_identity(
        &mut self,
        slot: usize,
        created_at: u64,
        definition_generation: u64,
    ) -> Result<(), SqlError> {
        if created_at == 0 || definition_generation == 0 {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "subscription stream identity must be nonzero"
            ));
        }
        let subscription = &mut self.subscriptions[slot];
        subscription.created_at = created_at;
        subscription.definition_generation = definition_generation;
        self.catalog_seq = self.catalog_seq.max(created_at);
        Ok(())
    }

    pub(crate) fn restore_subscription_owner(&mut self, slot: usize, owner: u16) {
        self.subscriptions[slot].ownership = Ownership {
            owner,
            pending: None,
        };
    }

    pub(crate) fn set_subscription_owner(
        &mut self,
        slot: usize,
        owner: usize,
        txid: u32,
    ) -> Result<Option<PendingOwnership>, SqlError> {
        self.require_subscription_owner(slot, txid)?;
        let prior = self.subscriptions[slot].ownership.pending;
        self.subscriptions[slot].ownership.pending = Some(PendingOwnership {
            txid,
            owner: owner as u16,
        });
        Ok(prior)
    }

    pub(crate) fn restore_subscription_owner_pending(
        &mut self,
        slot: usize,
        prior: Option<PendingOwnership>,
    ) {
        self.subscriptions[slot].ownership.pending = prior;
    }

    pub(crate) fn commit_subscription_owner(&mut self, slot: usize, txid: u32) {
        let ownership = &mut self.subscriptions[slot].ownership;
        if let Some(pending) = ownership.pending
            && pending.txid == txid
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn rename_subscription(
        &mut self,
        slot: usize,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingSubscriptionName>, SqlError> {
        if self
            .subscriptions
            .iter()
            .enumerate()
            .any(|(other, subscription)| {
                other != slot
                    && subscription.visible_to(txid)
                    && subscription.name_for(txid) == name
            })
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "subscription \"{}\" already exists",
                name.as_str()
            ));
        }
        let subscription = &mut self.subscriptions[slot];
        if let Some(pending) = subscription.pending_name
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "subscription \"{}\" is being renamed by another transaction",
                subscription.name.as_str()
            ));
        }
        let prior = subscription.pending_name;
        subscription.pending_name = Some(PendingSubscriptionName { txid, name });
        Ok(prior)
    }

    pub(crate) fn commit_subscription_rename(&mut self, slot: usize, txid: u32) {
        let subscription = &mut self.subscriptions[slot];
        if let Some(pending) = subscription.pending_name
            && pending.txid == txid
        {
            subscription.name = pending.name;
            subscription.pending_name = None;
            subscription.definition_generation += 1;
        }
    }

    pub(crate) fn rollback_subscription_rename(
        &mut self,
        slot: usize,
        prior: Option<PendingSubscriptionName>,
    ) {
        self.subscriptions[slot].pending_name = prior;
    }

    pub(crate) fn commit_subscription_drop(&mut self, slot: usize) {
        let created_at = self.subscriptions[slot].created_at;
        for relation in self.subscription_relations.iter_mut() {
            if relation.subscription_created_at == created_at {
                relation.ddl_state = CatalogDdlState::Absent;
            }
        }
        self.subscriptions[slot].cleanup = match self.subscriptions[slot].slot {
            SubscriptionSlot::Managed(_) => SubscriptionCleanup::DropManagedSlot,
            SubscriptionSlot::Absent | SubscriptionSlot::External(_) => SubscriptionCleanup::None,
        };
        self.subscriptions[slot].ddl_state = self.subscriptions[slot].ddl_state.commit_drop();
    }

    pub(crate) fn subscription_cleanup(
        &self,
        slot: usize,
    ) -> Option<(u64, SqlName, SubscriptionConnInfo, SqlName)> {
        let subscription = self.subscriptions.get(slot)?;
        let SubscriptionCleanup::DropManagedSlot = subscription.cleanup else {
            return None;
        };
        let SubscriptionSlot::Managed(remote_slot) = subscription.slot else {
            unreachable!("managed cleanup retains a managed slot")
        };
        Some((
            subscription.created_at,
            subscription.name,
            subscription.connection,
            remote_slot.sql_name(),
        ))
    }

    pub(crate) fn complete_subscription_cleanup(
        &mut self,
        slot: usize,
        created_at: u64,
    ) -> Result<(), SqlError> {
        let subscription = self.subscriptions.get_mut(slot).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "subscription cleanup slot is invalid"
            )
        })?;
        if subscription.ddl_state != CatalogDdlState::Absent
            || subscription.created_at != created_at
            || subscription.cleanup != SubscriptionCleanup::DropManagedSlot
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "subscription cleanup identity changed"
            ));
        }
        subscription.cleanup = SubscriptionCleanup::None;
        Ok(())
    }

    pub(crate) fn rollback_subscription_create(&mut self, slot: usize) {
        self.subscriptions[slot].ddl_state = self.subscriptions[slot].ddl_state.rollback_create();
    }

    pub(crate) fn rollback_subscription_drop(&mut self, slot: usize, txid: u32) {
        self.subscriptions[slot].ddl_state = self.subscriptions[slot].ddl_state.rollback_drop(txid);
    }

    /// Stages an enablement change only after proving no concurrent catalog
    /// operation owns this subscription.  `None` is a real no-op, not an
    /// alternative execution path: the requested visible state already holds.
    pub(crate) fn set_subscription_enabled(
        &mut self,
        slot: usize,
        enabled: bool,
        txid: u32,
    ) -> Result<SubscriptionEnabledChange, SqlError> {
        self.ensure_subscription_changeable(slot, txid)?;
        let subscription = &mut self.subscriptions[slot];
        if subscription.enabled_to(txid) == enabled {
            return Ok(SubscriptionEnabledChange::Unchanged);
        }
        let prior = subscription.pending_enabled;
        subscription.pending_enabled = Some(PendingSubscriptionEnabled { txid, enabled });
        Ok(SubscriptionEnabledChange::Changed { prior })
    }

    fn ensure_subscription_changeable(&self, slot: usize, txid: u32) -> Result<(), SqlError> {
        let subscription = &self.subscriptions[slot];
        let blocker = subscription
            .ddl_state
            .pending_txid()
            .or_else(|| subscription.pending_enabled.map(|pending| pending.txid))
            .or_else(|| subscription.pending_bootstrap.map(|pending| pending.txid))
            .or_else(|| subscription.pending_definition.map(|pending| pending.txid))
            .or_else(|| subscription.ownership.pending.map(|pending| pending.txid))
            .filter(|owner| *owner != txid);
        if let Some(blocker) = blocker {
            return Err(self.catalog_ddl_wait_error(txid, blocker, subscription.name.as_str()));
        }
        Ok(())
    }

    pub(crate) fn commit_subscription_enabled(&mut self, slot: usize, txid: u32) {
        let subscription = &mut self.subscriptions[slot];
        if let Some(pending) = subscription.pending_enabled
            && pending.txid == txid
        {
            subscription.enabled = pending.enabled;
            subscription.pending_enabled = None;
            if pending.enabled {
                subscription.failure = None;
            }
        }
    }

    pub(crate) fn restore_subscription_enabled(
        &mut self,
        slot: usize,
        prior: Option<PendingSubscriptionEnabled>,
    ) {
        self.subscriptions[slot].pending_enabled = prior;
    }

    pub(crate) fn set_subscription_bootstrap(
        &mut self,
        slot: usize,
        bootstrap: SubscriptionBootstrap,
        txid: u32,
    ) -> Result<SubscriptionBootstrapChange, SqlError> {
        self.ensure_subscription_changeable(slot, txid)?;
        let subscription = &mut self.subscriptions[slot];
        if subscription.bootstrap_to(txid) == bootstrap {
            return Ok(SubscriptionBootstrapChange::Unchanged);
        }
        let prior = subscription.pending_bootstrap;
        subscription.pending_bootstrap = Some(PendingSubscriptionBootstrap { txid, bootstrap });
        Ok(SubscriptionBootstrapChange::Changed { prior })
    }

    pub(crate) fn commit_subscription_bootstrap(&mut self, slot: usize, txid: u32) {
        let subscription = &mut self.subscriptions[slot];
        if let Some(pending) = subscription.pending_bootstrap
            && pending.txid == txid
        {
            subscription.bootstrap = pending.bootstrap;
            subscription.pending_bootstrap = None;
            subscription.failure = None;
        }
    }

    pub(crate) fn restore_subscription_bootstrap(
        &mut self,
        slot: usize,
        prior: Option<PendingSubscriptionBootstrap>,
    ) {
        self.subscriptions[slot].pending_bootstrap = prior;
    }

    pub(crate) fn set_subscription_definition(
        &mut self,
        slot: usize,
        connection: SubscriptionConnInfo,
        publications: &[SqlName],
        publisher_slot: SubscriptionSlot,
        behavior: SubscriptionBehavior,
        txid: u32,
    ) -> Result<SubscriptionDefinitionChange, SqlError> {
        self.ensure_subscription_changeable(slot, txid)?;
        if publications.is_empty() || publications.len() > MAX_SUBSCRIPTION_PUBLICATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription publication list exceeds its fixed capacity"
            ));
        }
        let subscription = &mut self.subscriptions[slot];
        let current_connection = subscription
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(subscription.connection, |pending| pending.connection);
        let (current_publications, current_count) = subscription
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or(
                (subscription.publications, subscription.publication_count),
                |pending| (pending.publications, pending.publication_count),
            );
        let (current_slot, current_behavior) = subscription
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .map_or((subscription.slot, subscription.behavior), |pending| {
                (pending.slot, pending.behavior)
            });
        if current_connection.as_str() == connection.as_str()
            && current_count == publications.len()
            && current_publications[..current_count] == *publications
            && current_slot == publisher_slot
            && current_behavior == behavior
        {
            return Ok(SubscriptionDefinitionChange {
                changed: false,
                prior: subscription.pending_definition,
            });
        }
        let mut names = [SqlName::EMPTY; MAX_SUBSCRIPTION_PUBLICATIONS];
        names[..publications.len()].copy_from_slice(publications);
        let prior = subscription.pending_definition;
        subscription.pending_definition = Some(PendingSubscriptionDefinition {
            txid,
            connection,
            publications: names,
            publication_count: publications.len(),
            slot: publisher_slot,
            behavior,
        });
        Ok(SubscriptionDefinitionChange {
            changed: true,
            prior,
        })
    }

    /// Returns the definition visible to `txid`. A definition change is one
    /// atomic catalog value: callers cannot combine a staged connection with
    /// committed publication membership.
    pub(crate) fn subscription_definition_to(
        &self,
        slot: usize,
        txid: u32,
    ) -> (
        SubscriptionConnInfo,
        [SqlName; MAX_SUBSCRIPTION_PUBLICATIONS],
        usize,
        SubscriptionSlot,
        SubscriptionBehavior,
    ) {
        self.subscriptions[slot].definition_to(txid)
    }

    pub(crate) fn commit_subscription_definition(&mut self, slot: usize, txid: u32) {
        let subscription = &mut self.subscriptions[slot];
        if let Some(pending) = subscription.pending_definition
            && pending.txid == txid
        {
            let replaces_stream = subscription.connection.as_str() != pending.connection.as_str()
                || subscription.publication_count != pending.publication_count
                || subscription.publications[..subscription.publication_count]
                    != pending.publications[..pending.publication_count]
                || subscription.slot != pending.slot
                || !subscription
                    .behavior
                    .same_publisher_stream(pending.behavior);
            subscription.connection = pending.connection;
            subscription.publications = pending.publications;
            subscription.publication_count = pending.publication_count;
            subscription.slot = pending.slot;
            subscription.behavior = pending.behavior;
            if replaces_stream {
                subscription.definition_generation = subscription
                    .definition_generation
                    .checked_add(1)
                    .expect("subscription definition generation exhausted");
            }
            subscription.pending_definition = None;
            subscription.failure = None;
            for relation in self.subscription_relations.iter_mut() {
                if relation.ddl_state == CatalogDdlState::Present
                    && relation.subscription_created_at == subscription.created_at
                    && replaces_stream
                {
                    relation.definition_generation = subscription.definition_generation;
                }
            }
        }
    }

    pub(crate) fn fail_subscription(
        &mut self,
        stream: SubscriptionStream,
        failure: SubscriptionFailure,
    ) -> Result<(), SqlError> {
        let subscription = self.subscriptions.get_mut(stream.slot()).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "subscription worker slot is invalid"
            )
        })?;
        if !subscription.visible_to(0)
            || subscription.created_at != stream.created_at()
            || subscription.definition_generation != stream.definition_generation()
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "subscription failure targets a replaced stream definition"
            ));
        }
        subscription.failure = Some(failure);
        if subscription.behavior.disable_on_error {
            subscription.enabled = false;
        }
        Ok(())
    }

    pub(crate) fn restore_subscription_definition(
        &mut self,
        slot: usize,
        prior: Option<PendingSubscriptionDefinition>,
    ) {
        self.subscriptions[slot].pending_definition = prior;
    }

    /// Validates one monotonically advancing, committed publisher position.
    /// Returning `None` makes re-delivery after a lost acknowledgement
    /// explicitly idempotent; callers must not apply that remote transaction
    /// again.
    pub(crate) fn subscription_stream(&self, slot: usize, txid: u32) -> Option<SubscriptionStream> {
        self.subscriptions
            .get(slot)
            .filter(|subscription| subscription.visible_to(txid))
            .map(|subscription| SubscriptionStream {
                slot,
                created_at: subscription.created_at,
                definition_generation: subscription.definition_generation,
                name: subscription.name,
            })
    }

    pub(crate) fn subscription_advance(
        &self,
        stream: SubscriptionStream,
        confirmed_lsn: u64,
        txid: u32,
    ) -> Result<Option<SubscriptionAdvance>, SqlError> {
        let subscription = self
            .subscriptions
            .get(stream.slot)
            .filter(|subscription| {
                subscription.visible_to(txid)
                    && subscription.created_at == stream.created_at
                    && subscription.definition_generation == stream.definition_generation
                    && subscription.name == stream.name
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "subscription stream definition changed before its remote transaction committed"
                )
            })?;
        if confirmed_lsn <= subscription.confirmed_lsn {
            return Ok(None);
        }
        Ok(Some(SubscriptionAdvance {
            stream,
            confirmed_lsn,
        }))
    }

    pub(crate) fn apply_subscription_advance(&mut self, advance: SubscriptionAdvance) {
        let subscription = self
            .subscriptions
            .get_mut(advance.stream.slot)
            .filter(|subscription| {
                subscription.ddl_state == CatalogDdlState::Present
                    && subscription.created_at == advance.stream.created_at
                    && subscription.definition_generation == advance.stream.definition_generation
                    && subscription.name == advance.stream.name
            })
            .expect("validated subscription must remain live until its WAL commit");
        subscription.confirmed_lsn = advance.confirmed_lsn;
        subscription.bootstrap = SubscriptionBootstrap::Ready;
        for relation in self.subscription_relations.iter_mut() {
            if relation.ddl_state == CatalogDdlState::Present
                && relation.subscription_created_at == advance.stream.created_at
                && relation.definition_generation == advance.stream.definition_generation
            {
                relation.state = SubscriptionRelationState::Ready;
                relation.synchronization_lsn = advance.confirmed_lsn;
            }
        }
    }

    pub(crate) fn begin_subscription_relation_refresh(
        &mut self,
        stream: SubscriptionStream,
        txid: u32,
    ) -> Result<(), SqlError> {
        self.subscriptions
            .get(stream.slot)
            .filter(|subscription| {
                subscription.visible_to(txid)
                    && subscription.created_at == stream.created_at
                    && subscription.definition_generation == stream.definition_generation
                    && subscription.name == stream.name
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "subscription stream definition changed before relation refresh"
                )
            })?;
        for relation in self.subscription_relations.iter_mut() {
            if let Some(owner) = relation.ddl_state.pending_txid()
                && owner != txid
                && relation.subscription_created_at == stream.created_at
            {
                return Err(sql_err!(
                    sqlstate::OBJECT_IN_USE,
                    "subscription relation catalog is being changed concurrently"
                ));
            }
            if relation.ddl_state == CatalogDdlState::Present
                && relation.subscription_created_at == stream.created_at
            {
                relation.ddl_state = relation.ddl_state.drop_by(txid);
            }
        }
        Ok(())
    }

    pub(crate) fn stage_subscription_relation(
        &mut self,
        stream: SubscriptionStream,
        schema: &str,
        table: &str,
        txid: u32,
    ) -> Result<usize, SqlError> {
        let table_slot = self.find_visible(schema, table, txid).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "subscription relation \"{}.{}\" does not exist locally",
                schema,
                table
            )
        })?;
        if table_slot > usize::from(u16::MAX) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription relation table identity exceeds its fixed range"
            ));
        }
        if self.subscription_relations.iter().any(|relation| {
            relation.ddl_state.visible_to(txid)
                && relation.subscription_created_at == stream.created_at
                && relation.definition_generation == stream.definition_generation
                && relation.table_slot() == table_slot
        }) {
            return Ok(table_slot);
        }
        let relation = self
            .subscription_relations
            .iter_mut()
            .find(|relation| relation.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "subscription relation catalog exhausted its startup capacity"
                )
            })?;
        *relation = SubscriptionRelation {
            subscription_created_at: stream.created_at,
            definition_generation: stream.definition_generation,
            table_slot: table_slot as u16,
            state: SubscriptionRelationState::DataCopy,
            synchronization_lsn: 0,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(table_slot)
    }

    pub(crate) fn restore_subscription_relation(
        &mut self,
        stream: SubscriptionStream,
        schema: &str,
        table: &str,
        state: SubscriptionRelationState,
        synchronization_lsn: u64,
    ) -> Result<(), SqlError> {
        let table_slot = self.stage_subscription_relation(stream, schema, table, 0)?;
        let relation = self
            .subscription_relations
            .iter_mut()
            .find(|relation| {
                relation.subscription_created_at == stream.created_at
                    && relation.definition_generation == stream.definition_generation
                    && relation.table_slot() == table_slot
            })
            .expect("staged subscription relation is present");
        relation.state = state;
        relation.synchronization_lsn = synchronization_lsn;
        self.commit_subscription_relation_refresh(0);
        Ok(())
    }

    pub(crate) fn commit_subscription_relation_refresh(&mut self, txid: u32) {
        for relation in self.subscription_relations.iter_mut() {
            match relation.ddl_state {
                CatalogDdlState::PendingCreate { txid: owner } if owner == txid => {
                    relation.ddl_state = CatalogDdlState::Present;
                }
                CatalogDdlState::PendingDrop { txid: owner } if owner == txid => {
                    relation.ddl_state = CatalogDdlState::Absent;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn rollback_subscription_relation_refresh(&mut self, txid: u32) {
        for relation in self.subscription_relations.iter_mut() {
            match relation.ddl_state {
                CatalogDdlState::PendingCreate { txid: owner } if owner == txid => {
                    relation.ddl_state = CatalogDdlState::Absent;
                }
                CatalogDdlState::PendingDrop { txid: owner } if owner == txid => {
                    relation.ddl_state = CatalogDdlState::Present;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn subscription_relations_visible_to(
        &self,
        subscription: &SubscriptionDef,
        txid: u32,
    ) -> impl Iterator<Item = &SubscriptionRelation> {
        self.subscription_relations.iter().filter(move |relation| {
            relation.ddl_state.visible_to(txid)
                && relation.subscription_created_at == subscription.created_at
                && relation.definition_generation == subscription.definition_generation
        })
    }

    pub(crate) fn subscription_relation_is_ready(
        &self,
        stream: SubscriptionStream,
        schema: &str,
        table: &str,
    ) -> bool {
        let Some(table_slot) = self.find_visible(schema, table, 0) else {
            return false;
        };
        self.subscription_relations.iter().any(|relation| {
            relation.ddl_state == CatalogDdlState::Present
                && relation.subscription_created_at == stream.created_at
                && relation.definition_generation == stream.definition_generation
                && relation.table_slot() == table_slot
                && relation.state == SubscriptionRelationState::Ready
        })
    }

    pub fn create_publication(
        &mut self,
        spec: PublicationSpec<'_>,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if spec.tables.len() > MAX_PUBLICATION_TABLES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many tables in publication (limit {})",
                MAX_PUBLICATION_TABLES
            ));
        }
        if spec.table_column_masks.len() != spec.tables.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "publication table projections do not match publication members"
            ));
        }
        if spec.table_filter_sql.len() != spec.tables.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "publication row filters do not match publication members"
            ));
        }
        if spec.schemas.len() > MAX_SCHEMAS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many schemas in publication (limit {})",
                MAX_SCHEMAS
            ));
        }
        if let Some(blocker) = self.publications.iter().find_map(|publication| {
            (publication.name_for(txid) == spec.name)
                .then_some(publication.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, spec.name.as_str()));
        }
        if self.publications.iter().any(|publication| {
            publication.visible_to(txid) && publication.name_for(txid) == spec.name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "publication \"{}\" already exists",
                spec.name.as_str()
            ));
        }
        if let Some(blocker) = self.publications.iter().find_map(|publication| {
            publication
                .pending_name
                .filter(|pending| pending.name == spec.name && pending.txid != txid)
                .map(|pending| pending.txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, spec.name.as_str()));
        }
        let Some(slot) = self
            .publications
            .iter()
            .position(|publication| publication.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many publications (limit {})",
                self.publications.len()
            ));
        };
        let mut members = [u16::MAX; MAX_PUBLICATION_TABLES];
        members[..spec.tables.len()].copy_from_slice(spec.tables);
        let mut table_column_masks = [0u64; MAX_PUBLICATION_TABLES];
        table_column_masks[..spec.table_column_masks.len()]
            .copy_from_slice(spec.table_column_masks);
        let table_filters = PublicationFilters::from_sql(spec.table_filter_sql)?;
        let mut schemas = [u8::MAX; MAX_SCHEMAS];
        schemas[..spec.schemas.len()].copy_from_slice(spec.schemas);
        self.catalog_seq += 1;
        self.publications[slot] = PublicationDef {
            created_at: self.catalog_seq,
            name: spec.name,
            pending_name: None,
            all_tables: spec.all_tables,
            tables: members,
            table_column_masks,
            table_filters,
            table_count: spec.tables.len(),
            schemas,
            schema_count: spec.schemas.len(),
            publish_insert: spec.publish_insert,
            publish_update: spec.publish_update,
            publish_delete: spec.publish_delete,
            publish_truncate: spec.publish_truncate,
            publish_via_partition_root: spec.publish_via_partition_root,
            publish_generated_columns: spec.publish_generated_columns,
            pending_definition: None,
            ownership: self.initial_ownership(txid),
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub fn drop_publication(&mut self, name: &str, txid: u32) -> Result<Option<usize>, SqlError> {
        let Some(slot) = self.publications.iter().position(|publication| {
            publication.visible_to(txid) && publication.name_for(txid).as_str() == name
        }) else {
            return Ok(None);
        };
        self.ensure_publication_changeable(slot, txid)?;
        let publication = &mut self.publications[slot];
        publication.ddl_state = publication.ddl_state.drop_by(txid);
        Ok(Some(slot))
    }

    pub fn commit_publication_create(&mut self, slot: usize) {
        self.publications[slot].ddl_state = self.publications[slot].ddl_state.commit_create();
    }
    pub(crate) fn commit_publication_owner(&mut self, slot: usize, txid: u32) {
        let ownership = &mut self.publications[slot].ownership;
        if let Some(pending) = ownership.pending
            && pending.txid == txid
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }
    pub fn commit_publication_drop(&mut self, slot: usize) {
        self.publications[slot].ddl_state = self.publications[slot].ddl_state.commit_drop();
    }

    pub(crate) fn alter_publication(
        &mut self,
        name: &str,
        definition: PublicationDefinition,
        txid: u32,
    ) -> Result<(usize, PublicationAlteration), SqlError> {
        if definition.table_count > MAX_PUBLICATION_TABLES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many tables in publication (limit {})",
                MAX_PUBLICATION_TABLES
            ));
        }
        let Some(slot) = self.publications.iter().position(|publication| {
            publication.visible_to(txid) && publication.name_for(txid).as_str() == name
        }) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name
            ));
        };
        self.ensure_publication_changeable(slot, txid)?;
        let publication = &mut self.publications[slot];
        if matches!(publication.ddl_state, CatalogDdlState::PendingCreate { txid: owner } if owner == txid)
        {
            let prior = publication.definition();
            publication.set_definition(definition);
            return Ok((slot, PublicationAlteration::Created(prior)));
        }
        let prior = publication.pending_definition;
        publication.pending_definition = Some(PendingPublicationDefinition { txid, definition });
        Ok((slot, PublicationAlteration::Committed(prior)))
    }

    fn ensure_publication_changeable(&self, slot: usize, txid: u32) -> Result<(), SqlError> {
        let publication = &self.publications[slot];
        let blocker = publication
            .ddl_state
            .pending_txid()
            .or_else(|| publication.pending_definition.map(|pending| pending.txid))
            .or_else(|| publication.pending_name.map(|pending| pending.txid))
            .or_else(|| publication.ownership.pending.map(|pending| pending.txid))
            .filter(|owner| *owner != txid);
        if let Some(blocker) = blocker {
            return Err(self.catalog_ddl_wait_error(txid, blocker, publication.name.as_str()));
        }
        Ok(())
    }

    pub(crate) fn commit_publication_alter(&mut self, slot: usize, txid: u32) {
        let publication = &mut self.publications[slot];
        if let Some(pending) = publication
            .pending_definition
            .filter(|pending| pending.txid == txid)
        {
            publication.set_definition(pending.definition);
            publication.pending_definition = None;
        }
    }

    pub(crate) fn rollback_publication_alter(&mut self, slot: usize, prior: PublicationAlteration) {
        let publication = &mut self.publications[slot];
        match prior {
            PublicationAlteration::Committed(prior) => publication.pending_definition = prior,
            PublicationAlteration::Created(prior) => publication.set_definition(prior),
        }
    }
    pub fn rollback_publication_create(&mut self, slot: usize) {
        self.publications[slot].ddl_state = self.publications[slot].ddl_state.rollback_create();
    }
    pub fn rollback_publication_drop(&mut self, slot: usize, txid: u32) {
        let publication = &mut self.publications[slot];
        publication.ddl_state = publication.ddl_state.rollback_drop(txid);
    }

    /// The stored SELECT text of a view visible to `txid`, if `name` names one
    /// (own uncommitted CREATE/DROP included; another transaction's excluded).
    pub fn find_view(&self, schema: &str, name: &str, txid: u32) -> Option<&ViewDef> {
        self.views.iter().find(|v| {
            v.visible_to(txid) && v.schema_for(txid).as_str() == schema && v.name.as_str() == name
        })
    }

    // --- Materialized-view catalog (parallel to views; data lives in a
    // same-named backing Table, so these hold only the defining query). ---

    /// Committed materialized views, for checkpoint serialization.
    pub fn live_matviews(&self) -> impl Iterator<Item = &MatviewDef> {
        self.matviews
            .iter()
            .filter(|m| m.ddl_state == CatalogDdlState::Present)
    }

    pub fn matviews_with_slots(&self) -> impl Iterator<Item = (usize, &MatviewDef)> {
        self.matviews
            .iter()
            .enumerate()
            .filter(|(_, matview)| matview.ddl_state == CatalogDdlState::Present)
    }

    /// Materialized views visible to `txid`, including the transaction's own DDL.
    pub(crate) fn matviews_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &MatviewDef)> {
        self.matviews
            .iter()
            .enumerate()
            .filter(move |(_, matview)| matview.visible_to(txid))
    }

    pub(crate) fn matview(&self, slot: usize) -> &MatviewDef {
        &self.matviews[slot]
    }

    pub(crate) fn matview_dependencies(&self, slot: usize) -> &StoredQueryDependencies {
        &self.matview_dependencies[slot]
    }

    pub(crate) fn matview_count(&self) -> usize {
        self.matviews.len()
    }

    pub fn find_matview(&self, schema: &str, name: &str, txid: u32) -> Option<&MatviewDef> {
        self.matviews
            .iter()
            .find(|m| m.visible_to(txid) && m.schema.as_str() == schema && m.name.as_str() == name)
    }

    /// The slot of a materialized view visible to `txid`, for later mutation
    /// (REFRESH marks it populated).
    pub fn matview_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.matviews.iter().position(|m| {
            m.visible_to(txid) && m.schema.as_str() == schema && m.name.as_str() == name
        })
    }

    pub fn set_matview_populated(&mut self, slot: usize, populated: bool) {
        self.matviews[slot].populated = populated;
    }

    /// Registers a materialized view as an uncommitted CREATE owned by `txid`.
    /// Unlike `create_view`, the name-vs-table collision is owned by the backing
    /// table's own creation, so no `find_table`/`or_replace` handling is needed.
    pub fn create_matview(
        &mut self,
        schema: SqlName,
        name: SqlName,
        query: StoredQueryDefinition,
        populated: bool,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.matviews.iter().find_map(|m| {
            (m.schema.as_str() == schema.as_str() && m.name.as_str() == name.as_str())
                .then_some(m.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let Some(new) = self
            .matviews
            .iter()
            .position(|m| m.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many materialized views (limit {})",
                self.matviews.len()
            ));
        };
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::MaterializedView,
            slot: new as u16,
        });
        self.catalog_seq += 1;
        self.matviews[new] = MatviewDef {
            created_at: self.catalog_seq,
            schema,
            name,
            sql: query.sql,
            creation_path: query.creation_path,
            ownership,
            populated,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        self.matview_dependencies[new] = query.dependencies;
        Ok(new)
    }

    pub fn drop_matview(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.matviews.iter().find_map(|m| {
            (m.schema.as_str() == schema && m.name.as_str() == name)
                .then_some(m.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.matviews.iter().position(|m| {
            m.visible_to(txid) && m.schema.as_str() == schema && m.name.as_str() == name
        }) else {
            return Ok(None);
        };
        self.pending_drop_matview(i, txid);
        Ok(Some(i))
    }

    fn pending_drop_matview(&mut self, slot: usize, txid: u32) {
        let m = &mut self.matviews[slot];
        m.ddl_state = m.ddl_state.drop_by(txid);
    }

    pub fn commit_matview_create(&mut self, slot: usize) {
        self.matviews[slot].ddl_state = self.matviews[slot].ddl_state.commit_create();
    }

    pub fn commit_matview_drop(&mut self, slot: usize) {
        let (schema, name) = (self.matviews[slot].schema, self.matviews[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.matviews[slot].ddl_state = self.matviews[slot].ddl_state.commit_drop();
        self.clear_extension_dependencies_for_object(AccessObject {
            class: AccessClass::MaterializedView,
            slot: slot as u16,
        });
    }

    pub fn rollback_matview_create(&mut self, slot: usize) {
        self.matviews[slot].ddl_state = self.matviews[slot].ddl_state.rollback_create();
    }

    pub fn rollback_matview_drop(&mut self, slot: usize, txid: u32) {
        let m = &mut self.matviews[slot];
        m.ddl_state = m.ddl_state.rollback_drop(txid);
    }

    // --- Sequences -------------------------------------------------------

    pub fn live_sequences(&self) -> impl Iterator<Item = &SequenceDef> {
        self.sequences
            .iter()
            .filter(|sequence| sequence.ddl_state == CatalogDdlState::Present)
    }

    pub fn sequences_with_slots(&self) -> impl Iterator<Item = (usize, &SequenceDef)> {
        self.sequences
            .iter()
            .enumerate()
            .filter(|(_, sequence)| sequence.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn sequence(&self, slot: usize) -> &SequenceDef {
        &self.sequences[slot]
    }

    pub(crate) fn sequence_count(&self) -> usize {
        self.sequences.len()
    }

    pub fn find_sequence(&self, schema: &str, name: &str, txid: u32) -> Option<&SequenceDef> {
        self.sequences.iter().find(|s| {
            let definition = s.definition_for(txid);
            s.visible_to(txid)
                && definition.schema.as_str() == schema
                && definition.name.as_str() == name
        })
    }

    pub fn sequence_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.sequences.iter().position(|s| {
            let definition = s.definition_for(txid);
            s.visible_to(txid)
                && definition.schema.as_str() == schema
                && definition.name.as_str() == name
        })
    }

    pub fn generated_sequence_slot(
        &self,
        table_schema: &str,
        table: &str,
        column: &str,
        txid: u32,
    ) -> Option<usize> {
        self.sequences.iter().position(|sequence| {
            sequence.visible_to(txid)
                && matches!(
                    sequence.generator_for,
                    Some(owner)
                        if owner.table_schema.as_str() == table_schema
                            && owner.table.as_str() == table
                            && owner.column.as_str() == column
                )
        })
    }

    /// Resolves a (possibly unqualified) sequence name to its slot: a qualifier
    /// names the schema directly, otherwise the search path is walked, matching
    /// [`Self::resolve_relation`].
    pub fn sequence_on_path(
        &self,
        qualifier: Option<&str>,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        if let Some(schema) = qualifier {
            return self.sequence_slot(schema, name, txid);
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema_name = self.schemas[*slot as usize].name;
                if let Some(found) = self.sequence_slot(schema_name.as_str(), name, txid) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Whether PostgreSQL's shared relation namespace already contains this
    /// name, including indexes and sequences.
    pub fn relation_name_taken(&self, schema: &str, name: &str, txid: u32) -> bool {
        self.relation_kind_in(schema, name, txid).is_some()
    }

    /// Registers a sequence as an uncommitted CREATE owned by `txid`. The caller
    /// has already validated options and checked the name is free.
    pub fn create_sequence(
        &mut self,
        schema: SqlName,
        name: SqlName,
        spec: SeqSpec,
        owner: Option<SequenceOwner>,
        generator_for: Option<SequenceOwner>,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.sequences.iter().find_map(|s| {
            (s.schema.as_str() == schema.as_str() && s.name.as_str() == name.as_str())
                .then_some(s.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let Some(new) = self
            .sequences
            .iter()
            .position(|sequence| sequence.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many sequences (limit {})",
                self.sequences.len()
            ));
        };
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Sequence,
            slot: new as u16,
        });
        self.catalog_seq += 1;
        self.sequences[new] = SequenceDef {
            created_at: self.catalog_seq,
            schema,
            name,
            ownership,
            data_type: spec.data_type,
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
            owner,
            generator_for,
            last_value: Cell::new(spec.start_value),
            is_called: Cell::new(false),
            dirty: Cell::new(false),
            pending_definition: None,
            pending_last_value: Cell::new(spec.start_value),
            pending_is_called: Cell::new(false),
            pending_dirty: Cell::new(false),
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(new)
    }

    pub(crate) fn sequence_for(&self, slot: usize, txid: u32) -> SequenceDef {
        self.sequences[slot].definition_for(txid)
    }

    pub(crate) fn stage_sequence_alter(
        &mut self,
        slot: usize,
        alteration: SequenceAlteration,
        txid: u32,
    ) -> Result<Option<PendingSequenceDefinition>, SqlError> {
        let sequence = &mut self.sequences[slot];
        if let Some(pending) = sequence.pending_definition
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "sequence \"{}\" is being altered by another transaction",
                sequence.name.as_str()
            ));
        }
        let prior = sequence
            .pending_definition
            .map(|pending| PendingSequenceDefinition {
                last_value: if pending.txid == txid {
                    sequence.pending_last_value.get()
                } else {
                    pending.last_value
                },
                is_called: if pending.txid == txid {
                    sequence.pending_is_called.get()
                } else {
                    pending.is_called
                },
                ..pending
            });
        let (last_value, is_called) = if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            (
                sequence.pending_last_value.get(),
                sequence.pending_is_called.get(),
            )
        } else {
            (sequence.last_value.get(), sequence.is_called.get())
        };
        let (last_value, is_called) = alteration
            .restart
            .map_or((last_value, is_called), |value| (value, false));
        sequence.pending_definition = Some(PendingSequenceDefinition {
            txid,
            schema: alteration.schema,
            spec: alteration.spec,
            owner: alteration.owner,
            generator_for: alteration.generator_for,
            last_value,
            is_called,
        });
        sequence.pending_last_value.set(last_value);
        sequence.pending_is_called.set(is_called);
        sequence.pending_dirty.set(alteration.restart.is_some());
        Ok(prior)
    }

    pub(crate) fn commit_sequence_alter(&mut self, slot: usize, txid: u32) {
        if self.sequences[slot]
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .is_some()
        {
            let old_schema = self.sequences[slot].schema;
            let name = self.sequences[slot].name;
            let last_value = self.sequences[slot].pending_last_value.get();
            let is_called = self.sequences[slot].pending_is_called.get();
            let definition = self.sequences[slot].definition_for(txid);
            self.sequences[slot] = definition;
            self.sequences[slot].last_value.set(last_value);
            self.sequences[slot].is_called.set(is_called);
            self.sequences[slot].dirty.set(false);
            self.sequences[slot].pending_dirty.set(false);
            if old_schema != self.sequences[slot].schema {
                let new_schema = self.sequences[slot].schema;
                for comment in self.comments.iter_mut() {
                    if comment.used
                        && comment.class == CommentClass::Relation
                        && comment.schema == old_schema
                        && comment.name == name
                    {
                        comment.schema = new_schema;
                    }
                }
            }
        }
    }

    pub(crate) fn rollback_sequence_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingSequenceDefinition>,
    ) {
        self.sequences[slot].pending_definition = prior;
        if let Some(prior) = prior {
            self.sequences[slot]
                .pending_last_value
                .set(prior.last_value);
            self.sequences[slot].pending_is_called.set(prior.is_called);
            self.sequences[slot].pending_dirty.set(false);
        } else {
            self.sequences[slot].pending_dirty.set(false);
        }
    }

    pub(crate) fn next_sequence_value(&self, slot: usize, txid: u32) -> Result<i64, SqlError> {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            let definition = sequence.definition_for(txid);
            return definition.next_value_with(
                &sequence.pending_last_value,
                &sequence.pending_is_called,
                Some(&sequence.pending_dirty),
            );
        }
        sequence.next_value()
    }

    pub(crate) fn set_sequence_value(
        &self,
        slot: usize,
        txid: u32,
        value: i64,
        is_called: bool,
    ) -> Result<i64, SqlError> {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            let definition = sequence.definition_for(txid);
            return definition.set_value_with(
                value,
                is_called,
                &sequence.pending_last_value,
                &sequence.pending_is_called,
                Some(&sequence.pending_dirty),
            );
        }
        sequence.set_value(value, is_called)
    }

    pub(crate) fn check_sequence_value(
        &self,
        slot: usize,
        txid: u32,
        value: i64,
    ) -> Result<(), SqlError> {
        self.sequence_for(slot, txid).check_setval(value)
    }

    pub(crate) fn sequence_value_for(&self, slot: usize, txid: u32) -> (i64, bool) {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            return (
                sequence.pending_last_value.get(),
                sequence.pending_is_called.get(),
            );
        }
        (sequence.last_value.get(), sequence.is_called.get())
    }

    pub(crate) fn sequence_value_dirty_for(&self, slot: usize, txid: u32) -> bool {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            return sequence.pending_dirty.get();
        }
        sequence.dirty.get()
    }

    pub(crate) fn clear_sequence_value_dirty(&self, slot: usize, txid: u32) {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            sequence.pending_dirty.set(false);
        } else {
            sequence.dirty.set(false);
        }
    }

    pub(crate) fn reset_sequence_value(
        &self,
        slot: usize,
        txid: u32,
        value: i64,
    ) -> SequenceValueState {
        let sequence = &self.sequences[slot];
        if sequence
            .pending_definition
            .is_some_and(|pending| pending.txid == txid)
        {
            let prior = SequenceValueState::Pending {
                last_value: sequence.pending_last_value.get(),
                is_called: sequence.pending_is_called.get(),
                dirty: sequence.pending_dirty.get(),
            };
            sequence.pending_last_value.set(value);
            sequence.pending_is_called.set(false);
            sequence.pending_dirty.set(true);
            return prior;
        }
        let prior = SequenceValueState::Committed {
            last_value: sequence.last_value.get(),
            is_called: sequence.is_called.get(),
            dirty: sequence.dirty.get(),
        };
        sequence.last_value.set(value);
        sequence.is_called.set(false);
        sequence.dirty.set(true);
        prior
    }

    pub(crate) fn restore_sequence_value(&self, slot: usize, prior: SequenceValueState) {
        let sequence = &self.sequences[slot];
        match prior {
            SequenceValueState::Committed {
                last_value,
                is_called,
                dirty,
            } => {
                sequence.last_value.set(last_value);
                sequence.is_called.set(is_called);
                sequence.dirty.set(dirty);
            }
            SequenceValueState::Pending {
                last_value,
                is_called,
                dirty,
            } => {
                sequence.pending_last_value.set(last_value);
                sequence.pending_is_called.set(is_called);
                sequence.pending_dirty.set(dirty);
            }
        }
    }

    pub fn drop_sequence(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.sequences.iter().find_map(|s| {
            (s.schema.as_str() == schema && s.name.as_str() == name)
                .then_some(s.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.sequences.iter().position(|s| {
            s.visible_to(txid) && s.schema.as_str() == schema && s.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let sequence = &mut self.sequences[i];
        sequence.ddl_state = sequence.ddl_state.drop_by(txid);
        Ok(Some(i))
    }

    pub fn commit_sequence_create(&mut self, slot: usize) {
        self.sequences[slot].ddl_state = self.sequences[slot].ddl_state.commit_create();
    }

    pub fn commit_sequence_drop(&mut self, slot: usize) {
        let (schema, name) = (self.sequences[slot].schema, self.sequences[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.sequences[slot].ddl_state = self.sequences[slot].ddl_state.commit_drop();
    }

    pub fn rollback_sequence_create(&mut self, slot: usize) {
        self.sequences[slot].ddl_state = self.sequences[slot].ddl_state.rollback_create();
    }

    pub fn rollback_sequence_drop(&mut self, slot: usize, txid: u32) {
        let sequence = &mut self.sequences[slot];
        sequence.ddl_state = sequence.ddl_state.rollback_drop(txid);
    }

    /// Applies a replayed/absolute `SequenceAdvance`: set value state directly,
    /// without marking dirty (replay must not re-journal).
    pub fn apply_sequence_advance(&mut self, schema: &str, name: &str, last: i64, is_called: bool) {
        if let Some(i) = self.sequences.iter().position(|s| {
            (s.ddl_state != CatalogDdlState::Absent)
                && s.schema.as_str() == schema
                && s.name.as_str() == name
        }) {
            self.sequences[i].last_value.set(last);
            self.sequences[i].is_called.set(is_called);
            self.sequences[i].dirty.set(false);
        }
    }

    // --- Domains (`CREATE DOMAIN`) ---------------------------------------

    /// Resolves a (possibly schema-qualified) domain type name to its
    /// definition, visible to `txid`, searching the current path when
    /// unqualified.
    pub fn find_domain(&self, type_name: &str, txid: u32) -> Option<DomainDef> {
        let (qualifier, name) = match type_name.split_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, type_name),
        };
        self.find_domain_slot(qualifier, name, txid)
            .map(|slot| self.domain_for(slot, txid))
    }

    fn find_domain_slot(&self, qualifier: Option<&str>, name: &str, txid: u32) -> Option<usize> {
        if let Some(schema) = qualifier {
            return self.domains.iter().position(|d| {
                let definition = d.definition_for(txid);
                d.visible_to(txid)
                    && definition.schema.as_str() == schema
                    && definition.name.as_str() == name
            });
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema = self.schemas[*slot as usize].name;
                if let Some(i) = self.domains.iter().position(|d| {
                    let definition = d.definition_for(txid);
                    d.visible_to(txid)
                        && definition.schema.as_str() == schema.as_str()
                        && definition.name.as_str() == name
                }) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Resolves a (possibly qualified) domain type name to its slot on the
    /// current path, visible to `txid`.
    pub fn resolve_domain_slot(&self, type_name: &str, txid: u32) -> Option<usize> {
        let (qualifier, name) = match type_name.split_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, type_name),
        };
        self.find_domain_slot(qualifier, name, txid)
    }

    /// The definition of a domain named `name` (any schema) visible to `txid` —
    /// for enforcing a column's domain constraints, where the column stores
    /// only the domain's name.
    pub fn domain_by_name(&self, name: &str, txid: u32) -> Option<DomainDef> {
        self.domains
            .iter()
            .position(|domain| {
                domain.visible_to(txid) && domain.definition_for(txid).name.as_str() == name
            })
            .map(|slot| self.domain_for(slot, txid))
    }

    /// The domain named `(schema, name)` visible to `txid`, by slot.
    pub fn domain_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.domains.iter().position(|d| {
            let definition = d.definition_for(txid);
            d.visible_to(txid)
                && definition.schema.as_str() == schema
                && definition.name.as_str() == name
        })
    }

    /// Resolves a durable reference held by a column or a child domain. SQL
    /// name lookup deliberately uses [`Self::domain_slot`] instead: an owning
    /// transaction must not keep resolving a domain's retired public name.
    pub(crate) fn domain_identity_slot(
        &self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        self.domain_slot(schema, name, txid).or_else(|| {
            self.domains.iter().position(|domain| {
                domain.visible_to(txid)
                    && domain.schema.as_str() == schema
                    && domain.name.as_str() == name
                    && domain
                        .pending_definition
                        .is_some_and(|pending| pending.txid == txid && pending.identity.is_some())
            })
        })
    }

    /// Resolves a table column's declared type for schema metadata.
    ///
    /// Domains keep their base [`ColType`] for execution, so schema metadata
    /// cannot derive its OID from `column.ctype.oid()`.
    /// Its context-specific accessors intentionally do not expose a generic
    /// OID conversion: result metadata follows the base-type contract for
    /// domains, while parameters, catalogs, and replication use this identity.
    pub fn declared_column_type(
        &self,
        column: &ColumnMeta,
        txid: u32,
    ) -> Result<DeclaredColumnType, SqlError> {
        use crate::sql::types::oid;

        match column.ctype {
            ColType::Array(ArrElem::Domain { slot, .. }) => {
                let UserTypeName { schema, name } = column.user_type.ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "domain-array column type lacks its durable identity"
                    )
                })?;
                if self.domain_identity_slot(schema.as_str(), name.as_str(), txid)
                    != Some(slot as usize)
                {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "domain-array column type does not match its durable identity"
                    ));
                }
                return Ok(DeclaredColumnType::Builtin {
                    oid: oid::domain_array_oid(slot),
                });
            }
            _ if column.user_type.is_some_and(|identity| {
                self.domain_identity_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .is_some()
            }) =>
            {
                let UserTypeName { schema, name } = column.user_type.expect("domain checked above");
                let slot = self
                    .domain_identity_slot(schema.as_str(), name.as_str(), txid)
                    .expect("domain checked above");
                return Ok(DeclaredColumnType::UserDefined {
                    oid: oid::domain_oid(slot as u16),
                    schema,
                    name,
                });
            }
            ColType::Enum(slot) => {
                let UserTypeName { schema, name } = column.user_type.ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "enum column type lacks its durable identity"
                    )
                })?;
                if self.enum_slot(schema.as_str(), name.as_str(), txid) != Some(slot as usize) {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "enum column type does not match its durable identity"
                    ));
                }
                return Ok(DeclaredColumnType::UserDefined {
                    oid: oid::enum_oid(slot),
                    schema,
                    name,
                });
            }
            ColType::Array(ArrElem::Enum(slot)) => {
                let UserTypeName { schema, name } = column.user_type.ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "enum-array column type lacks its durable identity"
                    )
                })?;
                if self.enum_slot(schema.as_str(), name.as_str(), txid) != Some(slot as usize) {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "enum-array column type does not match its durable identity"
                    ));
                }
                return Ok(DeclaredColumnType::Builtin {
                    oid: oid::enum_array_oid(slot),
                });
            }
            ColType::Composite(slot) => {
                let UserTypeName { schema, name } = column.user_type.ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "composite column type lacks its durable identity"
                    )
                })?;
                if self.composite_slot(schema.as_str(), name.as_str(), txid) != Some(slot as usize)
                {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "composite column type does not match its durable identity"
                    ));
                }
                return Ok(DeclaredColumnType::UserDefined {
                    oid: oid::composite_oid(slot),
                    schema,
                    name,
                });
            }
            ColType::Array(ArrElem::Composite(slot)) => {
                let UserTypeName { schema, name } = column.user_type.ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "composite-array column type lacks its durable identity"
                    )
                })?;
                if self.composite_slot(schema.as_str(), name.as_str(), txid) != Some(slot as usize)
                {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "composite-array column type does not match its durable identity"
                    ));
                }
                return Ok(DeclaredColumnType::Builtin {
                    oid: oid::composite_array_oid(slot),
                });
            }
            _ => {}
        }

        let Some(UserTypeName { schema, name }) = column.user_type else {
            return Ok(DeclaredColumnType::Builtin {
                oid: column.ctype.oid(),
            });
        };
        if let Some(slot) = self.domain_identity_slot(schema.as_str(), name.as_str(), txid) {
            return Ok(DeclaredColumnType::UserDefined {
                oid: oid::domain_oid(slot as u16),
                schema,
                name,
            });
        }
        if let Some(slot) = self.enum_slot(schema.as_str(), name.as_str(), txid) {
            return Ok(DeclaredColumnType::UserDefined {
                oid: oid::enum_oid(slot as u16),
                schema,
                name,
            });
        }
        if let Some(slot) = self.composite_slot(schema.as_str(), name.as_str(), txid) {
            return Ok(DeclaredColumnType::UserDefined {
                oid: oid::composite_oid(slot as u16),
                schema,
                name,
            });
        }
        Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "declared column type \"{}.{}\" does not exist",
            schema.as_str(),
            name.as_str()
        ))
    }

    pub fn domain(&self, slot: usize) -> &DomainDef {
        &self.domains[slot]
    }

    pub(crate) fn domain_count(&self) -> usize {
        self.domains.len()
    }

    /// Committed domains carrying their slot indices, for the checkpoint and
    /// `pg_type`.
    pub fn live_domains(&self) -> impl Iterator<Item = (usize, &DomainDef)> {
        self.domains
            .iter()
            .enumerate()
            .filter(|(_, d)| d.ddl_state == CatalogDdlState::Present)
    }

    /// Whether any table column (in any table) is declared with this domain —
    /// the dependency that makes `DROP DOMAIN ... RESTRICT` fail.
    pub fn domain_in_use(&self, schema: &str, name: &str) -> Option<(SqlName, SqlName)> {
        for table in self.tables.iter().filter(|t| t.live) {
            for col in table.def.columns() {
                if col.user_type.is_some_and(|identity| {
                    identity.name.as_str() == name && identity.schema.as_str() == schema
                }) {
                    return Some((table.def.name, col.name));
                }
            }
        }
        None
    }

    /// Registers a domain as an uncommitted CREATE owned by `txid` (or, for
    /// replay/checkpoint with `txid == 0`, committed directly). The caller has
    /// validated the base type and constraints and checked the name is free.
    pub fn create_domain(
        &mut self,
        schema: SqlName,
        name: SqlName,
        spec: DomainSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.create_domain_with_binding(schema, name, spec, txid, false)
    }

    /// Manifest loading is the sole phase allowed to defer a named base-type
    /// slot until every catalog record has been parsed.
    pub(crate) fn create_domain_from_manifest(
        &mut self,
        schema: SqlName,
        name: SqlName,
        spec: DomainSpec,
    ) -> Result<usize, SqlError> {
        self.create_domain_with_binding(schema, name, spec, 0, true)
    }

    fn create_domain_with_binding(
        &mut self,
        schema: SqlName,
        name: SqlName,
        mut spec: DomainSpec,
        txid: u32,
        defer_manifest_binding: bool,
    ) -> Result<usize, SqlError> {
        if let Some(identity) = spec.base_user_type {
            spec.base = match spec.base {
                ColType::Enum(slot)
                    if slot != ColType::ENUM_SLOT_UNRESOLVED
                        && (slot as usize) < self.enums.len()
                        && self.enum_for(slot as usize, txid).visible_to(txid) =>
                {
                    // WAL preserves the catalog slot. Its spelling can be stale
                    // after an older domain image is replayed over a moved type.
                    let definition = self.enum_for(slot as usize, txid);
                    spec.base_user_type = Some(UserTypeName {
                        schema: definition.schema,
                        name: definition.name,
                    });
                    ColType::Enum(slot)
                }
                ColType::Enum(_) => {
                    match self.enum_slot(identity.schema.as_str(), identity.name.as_str(), txid) {
                        Some(slot) => ColType::Enum(slot as u16),
                        None if defer_manifest_binding => {
                            ColType::Enum(ColType::ENUM_SLOT_UNRESOLVED)
                        }
                        None => {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "domain base enum \"{}.{}\" does not exist",
                                identity.schema.as_str(),
                                identity.name.as_str()
                            ));
                        }
                    }
                }
                ColType::Composite(slot)
                    if slot != ColType::COMPOSITE_SLOT_UNRESOLVED
                        && (slot as usize) < self.composites.len()
                        && self.composite_for(slot as usize, txid).visible_to(txid) =>
                {
                    // See the enum case: slot identity is durable; names are a
                    // rebindable catalog projection.
                    let definition = self.composite_for(slot as usize, txid);
                    spec.base_user_type = Some(UserTypeName {
                        schema: definition.schema,
                        name: definition.name,
                    });
                    ColType::Composite(slot)
                }
                ColType::Composite(_) => match self.composite_slot(
                    identity.schema.as_str(),
                    identity.name.as_str(),
                    txid,
                ) {
                    Some(slot) => ColType::Composite(slot as u16),
                    None if defer_manifest_binding => {
                        ColType::Composite(ColType::COMPOSITE_SLOT_UNRESOLVED)
                    }
                    None => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "domain base composite \"{}.{}\" does not exist",
                            identity.schema.as_str(),
                            identity.name.as_str()
                        ));
                    }
                },
                _ => {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "domain base identity names a non-user-defined type"
                    ));
                }
            };
        }
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.domains.iter().find_map(|d| {
            (d.schema == schema && d.name == name
                || d.pending_definition
                    .and_then(|pending| pending.identity)
                    .is_some_and(|identity| identity.schema == schema && identity.name == name))
            .then(|| {
                d.pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| d.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if let Some(blocker) = self.enums.iter().find_map(|e| {
            (e.schema == schema && e.name == name
                || e.pending_definition
                    .is_some_and(|pending| pending.schema == schema && pending.name == name))
            .then(|| {
                e.pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| e.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if let Some(blocker) = self.composites.iter().find_map(|composite| {
            (composite.schema == schema && composite.name == name
                || composite
                    .pending_definition
                    .is_some_and(|pending| pending.schema == schema && pending.name == name))
            .then(|| {
                composite
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| composite.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.domains.iter().any(|domain| {
            domain.visible_to(txid)
                && domain.definition_for(txid).schema == schema
                && domain.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        if self.enums.iter().any(|e| {
            e.visible_to(txid)
                && e.definition_for(txid).schema == schema
                && e.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        if self.composites.iter().any(|composite| {
            composite.visible_to(txid)
                && composite.definition_for(txid).schema == schema
                && composite.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        let Some(new) = self
            .domains
            .iter()
            .position(|d| d.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many domains (limit {})",
                self.domains.len()
            ));
        };
        self.catalog_seq += 1;
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Domain,
            slot: new as u16,
        });
        self.domains[new] = DomainDef {
            created_at: self.catalog_seq,
            schema,
            name,
            ownership,
            base_domain: spec.base_domain,
            base_user_type: spec.base_user_type,
            base: spec.base,
            base_type_mod: spec.base_type_mod,
            not_null: spec.not_null,
            default_expr: spec.default_expr,
            checks: spec.checks,
            n_checks: spec.n_checks,
            pending_definition: None,
            ddl_state: if txid == 0 {
                CatalogDdlState::Present
            } else {
                CatalogDdlState::PendingCreate { txid }
            },
        };
        Ok(new)
    }

    pub(crate) fn domain_for(&self, slot: usize, txid: u32) -> DomainDef {
        self.domains[slot].definition_for(txid)
    }

    /// Finishes manifest-time user-type binding once every catalog definition
    /// has been installed. Startup never exposes the interim unresolved slot.
    pub(crate) fn rebind_domain_base_types(&mut self) -> Result<(), SqlError> {
        fn bind(
            storage: &mut Storage,
            slot: usize,
            state: &mut [u8; MAX_DOMAINS],
        ) -> Result<(), SqlError> {
            match state[slot] {
                2 => return Ok(()),
                1 => {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "recovered domain base chain contains a cycle"
                    ));
                }
                _ => state[slot] = 1,
            }
            let domain = storage.domains[slot];
            if let Some(parent) = domain.base_domain {
                let parent_slot = storage
                    .domain_slot(parent.schema.as_str(), parent.name.as_str(), 0)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "domain base \"{}.{}\" does not exist",
                            parent.schema.as_str(),
                            parent.name.as_str()
                        )
                    })?;
                bind(storage, parent_slot, state)?;
                storage.domains[slot].base = storage.domains[parent_slot].base;
            } else if let Some(identity) = domain.base_user_type {
                storage.domains[slot].base = match domain.base {
                    ColType::Enum(_) => ColType::Enum(
                        storage
                            .enum_slot(identity.schema.as_str(), identity.name.as_str(), 0)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "domain base enum \"{}.{}\" does not exist",
                                    identity.schema.as_str(),
                                    identity.name.as_str()
                                )
                            })? as u16,
                    ),
                    ColType::Composite(_) => ColType::Composite(
                        storage
                            .composite_slot(identity.schema.as_str(), identity.name.as_str(), 0)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "domain base composite \"{}.{}\" does not exist",
                                    identity.schema.as_str(),
                                    identity.name.as_str()
                                )
                            })? as u16,
                    ),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "domain base identity names a non-user-defined type"
                        ));
                    }
                };
            }
            state[slot] = 2;
            Ok(())
        }

        let mut state = [0u8; MAX_DOMAINS];
        for slot in 0..self.domains.len() {
            let domain = self.domains[slot];
            if domain.ddl_state != CatalogDdlState::Present {
                continue;
            }
            bind(self, slot, &mut state)?;
        }
        Ok(())
    }

    /// Rebind persisted declarations after their complete catalog exists.
    /// Checkpoint type codes deliberately omit runtime slots; a user-type name
    /// is the durable witness that selects the current enum, composite, or
    /// domain representation.
    fn rebind_declared_user_type(
        &self,
        ctype: ColType,
        identity: UserTypeName,
    ) -> Result<ColType, SqlError> {
        if let Some(slot) = self.domain_slot(identity.schema.as_str(), identity.name.as_str(), 0) {
            let domain = self.domain(slot);
            return match ctype {
                ColType::Array(ArrElem::Domain { .. }) => ArrElem::domain(slot as u16, domain.base)
                    .map(ColType::Array)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "arrays of domain {} require a scalar base type",
                            identity.name.as_str()
                        )
                    }),
                _ => Ok(domain.base),
            };
        }
        if let Some(slot) = self.enum_slot(identity.schema.as_str(), identity.name.as_str(), 0) {
            return Ok(if matches!(ctype, ColType::Array(_)) {
                ColType::Array(ArrElem::Enum(slot as u16))
            } else {
                ColType::Enum(slot as u16)
            });
        }
        if let Some(slot) = self.composite_slot(identity.schema.as_str(), identity.name.as_str(), 0)
        {
            return Ok(if matches!(ctype, ColType::Array(_)) {
                ColType::Array(ArrElem::Composite(slot as u16))
            } else {
                ColType::Composite(slot as u16)
            });
        }
        Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "declared type \"{}.{}\" does not exist",
            identity.schema.as_str(),
            identity.name.as_str()
        ))
    }

    /// Completes checkpoint-time binding for every table and composite field.
    /// No recovered declaration is exposed with an unresolved user-type slot.
    pub(crate) fn rebind_user_type_declarations(&mut self) -> Result<(), SqlError> {
        for table in 0..self.tables.len() {
            if !self.tables[table].live {
                continue;
            }
            let mut definition = self.tables[table].def;
            self.bind_user_type_columns(&mut definition)?;
            self.tables[table].def = definition;
        }
        for composite in 0..self.composites.len() {
            if self.composites[composite].ddl_state != CatalogDdlState::Present {
                continue;
            }
            let n_fields = self.composites[composite].n_fields;
            for field in 0..n_fields {
                let definition = self.composites[composite].fields[field];
                if definition.dropped {
                    continue;
                }
                let Some(identity) = definition.user_type else {
                    continue;
                };
                let rebound = self.rebind_declared_user_type(definition.ctype, identity)?;
                self.composites[composite].fields[field].ctype = rebound;
            }
        }
        Ok(())
    }

    /// Rebind routine declaration representations from their durable type
    /// names after a manifest rebuild.  Routine slots are deliberately never
    /// used as identities: a dropped type may free its slot before recovery.
    pub(crate) fn rebind_routine_types(&mut self) -> Result<(), SqlError> {
        fn rebind(
            storage: &Storage,
            ctype: ColType,
            identity: UserTypeName,
        ) -> Result<ColType, SqlError> {
            if polymorphic_type(ctype, Some(identity)).is_some() {
                return Ok(ctype);
            }
            if let Some(slot) =
                storage.domain_slot(identity.schema.as_str(), identity.name.as_str(), 0)
            {
                let domain = storage.domain(slot);
                return match ctype {
                    ColType::Array(_) => {
                        crate::sql::types::ArrElem::domain(slot as u16, domain.base)
                            .map(ColType::Array)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::PROTOCOL_VIOLATION,
                                    "routine domain array has invalid element type"
                                )
                            })
                    }
                    _ => Ok(domain.base),
                };
            }
            if let Some(slot) =
                storage.enum_slot(identity.schema.as_str(), identity.name.as_str(), 0)
            {
                return Ok(match ctype {
                    ColType::Array(_) => {
                        ColType::Array(crate::sql::types::ArrElem::Enum(slot as u16))
                    }
                    _ => ColType::Enum(slot as u16),
                });
            }
            if let Some(slot) =
                storage.composite_slot(identity.schema.as_str(), identity.name.as_str(), 0)
            {
                return Ok(match ctype {
                    ColType::Array(_) => {
                        ColType::Array(crate::sql::types::ArrElem::Composite(slot as u16))
                    }
                    _ => ColType::Composite(slot as u16),
                });
            }
            Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "routine type \"{}.{}\" does not exist",
                identity.schema.as_str(),
                identity.name.as_str()
            ))
        }
        for slot in 0..self.routines.len() {
            if self.routines[slot].ddl_state != CatalogDdlState::Present {
                continue;
            }
            let mut routine = self.routines[slot];
            for argument in &mut routine.arguments[..routine.argument_count] {
                if let Some(identity) = argument.user_type {
                    argument.ctype = rebind(self, argument.ctype, identity)?;
                }
            }
            for parameter in &mut routine.parameters[..routine.parameter_count] {
                if let Some(identity) = parameter.user_type {
                    parameter.ctype = rebind(self, parameter.ctype, identity)?;
                }
            }
            for column in &mut routine.result_columns[..routine.result_column_count] {
                if let Some(identity) = column.user_type {
                    column.ctype = rebind(self, column.ctype, identity)?;
                }
            }
            match &mut routine.kind {
                RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                    if let Some(identity) = result.user_type {
                        result.ctype = rebind(self, result.ctype, identity)?;
                    }
                }
                RoutineKind::Aggregate(aggregate) => {
                    for result in [&mut aggregate.state_type, &mut aggregate.result_type] {
                        if let Some(identity) = result.user_type {
                            result.ctype = rebind(self, result.ctype, identity)?;
                        }
                    }
                    if let Some(moving) = &mut aggregate.moving
                        && let Some(identity) = moving.state_type.user_type
                    {
                        moving.state_type.ctype = rebind(self, moving.state_type.ctype, identity)?;
                    }
                }
                RoutineKind::RecordFunction { .. }
                | RoutineKind::TableFunction
                | RoutineKind::Trigger
                | RoutineKind::Procedure => {}
            }
            self.routines[slot] = routine;
        }
        Ok(())
    }

    pub(crate) fn stage_domain_alter(
        &mut self,
        slot: usize,
        spec: DomainSpec,
        txid: u32,
    ) -> Result<Option<PendingDomainDefinition>, SqlError> {
        let domain = &mut self.domains[slot];
        if let Some(pending) = domain.pending_definition
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "domain \"{}\" is being altered by another transaction",
                domain.name.as_str()
            ));
        }
        let prior = domain.pending_definition;
        domain.pending_definition = Some(PendingDomainDefinition {
            txid,
            spec,
            identity: prior.and_then(|pending| pending.identity),
        });
        Ok(prior)
    }

    pub(crate) fn stage_domain_identity(
        &mut self,
        slot: usize,
        schema: SqlName,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingDomainDefinition>, SqlError> {
        let current = self.domain_for(slot, txid);
        if current.schema == schema && current.name == name {
            return Ok(self.domains[slot].pending_definition);
        }
        if let Some(blocker) = self
            .domains
            .iter()
            .enumerate()
            .find_map(|(other_slot, domain)| {
                let definition = domain.definition_for(txid);
                (other_slot != slot && definition.schema == schema && definition.name == name)
                    .then(|| {
                        domain
                            .pending_definition
                            .map(|pending| pending.txid)
                            .or_else(|| domain.ddl_state.pending_txid())
                    })
                    .flatten()
                    .filter(|&owner| owner != txid)
            })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.domains.iter().enumerate().any(|(other_slot, domain)| {
            other_slot != slot
                && domain.visible_to(txid)
                && domain.definition_for(txid).schema == schema
                && domain.definition_for(txid).name == name
        }) || self.enums.iter().any(|enumeration| {
            enumeration.visible_to(txid)
                && enumeration.schema == schema
                && enumeration.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        let domain = &mut self.domains[slot];
        if let Some(pending) = domain.pending_definition
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "domain \"{}\" is being altered by another transaction",
                domain.name.as_str()
            ));
        }
        let prior = domain.pending_definition;
        let spec = prior.map_or(
            DomainSpec {
                base_domain: domain.base_domain,
                base_user_type: domain.base_user_type,
                base: domain.base,
                base_type_mod: domain.base_type_mod,
                not_null: domain.not_null,
                default_expr: domain.default_expr,
                checks: domain.checks,
                n_checks: domain.n_checks,
            },
            |pending| pending.spec,
        );
        domain.pending_definition = Some(PendingDomainDefinition {
            txid,
            spec,
            identity: Some(PendingDomainIdentity { schema, name }),
        });
        Ok(prior)
    }

    pub(crate) fn commit_domain_alter(&mut self, slot: usize, txid: u32) {
        if self.domains[slot]
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .is_some()
        {
            let definition = self.domains[slot].definition_for(txid);
            if definition.schema != self.domains[slot].schema
                || definition.name != self.domains[slot].name
            {
                self.rename_domain_references(slot, definition.schema, definition.name);
            }
            self.domains[slot] = definition;
        }
    }

    pub(crate) fn rollback_domain_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingDomainDefinition>,
    ) {
        self.domains[slot].pending_definition = prior;
    }

    pub fn restore_domain(&mut self, slot: usize, prior: DomainDef) {
        self.domains[slot] = prior;
    }

    fn rename_domain_references(&mut self, slot: usize, schema: SqlName, name: SqlName) {
        let old_schema = self.domains[slot].schema;
        let old_name = self.domains[slot].name;
        self.domains[slot].schema = schema;
        self.domains[slot].name = name;
        for table in self
            .tables
            .iter_mut()
            .filter(|table| table.live || table.pending_ddl.is_some())
        {
            let mut changed = false;
            for column in table.def.columns[..table.def.n_columns].iter_mut() {
                let uses_domain = column.user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
                    || matches!(
                        column.ctype,
                        ColType::Array(ArrElem::Domain { slot: domain_slot, .. })
                            if domain_slot as usize == slot
                    );
                if uses_domain {
                    column.user_type = Some(UserTypeName { schema, name });
                    changed = true;
                }
            }
            if changed {
                table.mark_dirty();
            }
        }
        for domain in self.domains.iter_mut() {
            if domain.base_domain
                == Some(UserTypeName {
                    schema: old_schema,
                    name: old_name,
                })
            {
                domain.base_domain = Some(UserTypeName { schema, name });
            }
        }
        for comment in self.comments.iter_mut() {
            if comment.used
                && comment.class == CommentClass::Type
                && comment.schema == old_schema
                && comment.name == old_name
            {
                comment.schema = schema;
                comment.name = name;
            }
        }
        self.move_routine_type_references(old_schema, old_name, schema, name, |_| true);
        self.rename_stored_query_dependency(DependencyClass::Domain, slot, schema, name);
    }

    pub fn drop_domain(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.domains.iter().find_map(|d| {
            (d.schema.as_str() == schema && d.name.as_str() == name)
                .then_some(d.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.domains.iter().position(|d| {
            d.visible_to(txid) && d.schema.as_str() == schema && d.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let d = &mut self.domains[i];
        d.ddl_state = d.ddl_state.drop_by(txid);
        Ok(Some(i))
    }

    pub fn commit_domain_create(&mut self, slot: usize) {
        self.domains[slot].ddl_state = self.domains[slot].ddl_state.commit_create();
    }

    pub fn commit_domain_drop(&mut self, slot: usize) {
        let (schema, name) = (self.domains[slot].schema, self.domains[slot].name);
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.domains[slot].ddl_state = self.domains[slot].ddl_state.commit_drop();
    }

    pub fn rollback_domain_create(&mut self, slot: usize) {
        self.domains[slot].ddl_state = self.domains[slot].ddl_state.rollback_create();
    }

    pub fn rollback_domain_drop(&mut self, slot: usize, txid: u32) {
        let d = &mut self.domains[slot];
        d.ddl_state = d.ddl_state.rollback_drop(txid);
    }

    // --- Enum types (CREATE TYPE ... AS ENUM) ---

    /// The slot of a (possibly schema-qualified) enum type name, visible to
    /// `txid`, searching the current path when unqualified.
    fn find_enum_slot(&self, qualifier: Option<&str>, name: &str, txid: u32) -> Option<usize> {
        if let Some(schema) = qualifier {
            return self.enums.iter().position(|e| {
                e.visible_to(txid)
                    && e.definition_for(txid).schema.as_str() == schema
                    && e.definition_for(txid).name.as_str() == name
            });
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema = self.schemas[*slot as usize].name;
                if let Some(i) = self.enums.iter().position(|e| {
                    e.visible_to(txid)
                        && e.definition_for(txid).schema.as_str() == schema.as_str()
                        && e.definition_for(txid).name.as_str() == name
                }) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Resolves a (possibly qualified) enum type name to its slot on the current
    /// path, visible to `txid`.
    pub fn resolve_enum_slot(&self, type_name: &str, txid: u32) -> Option<usize> {
        let (qualifier, name) = match type_name.split_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, type_name),
        };
        self.find_enum_slot(qualifier, name, txid)
    }

    /// The definition of an enum named `name` (any schema) visible to `txid` —
    /// for resolving a column whose stored type identity is only the enum name.
    pub fn enum_by_name(&self, name: &str, txid: u32) -> Option<&EnumDef> {
        self.enums
            .iter()
            .find(|e| e.visible_to(txid) && e.definition_for(txid).name.as_str() == name)
    }

    /// The slot of an enum named `name` (any schema) visible to `txid`.
    pub fn enum_slot_by_name(&self, name: &str, txid: u32) -> Option<usize> {
        self.enums
            .iter()
            .position(|e| e.visible_to(txid) && e.definition_for(txid).name.as_str() == name)
    }

    /// The enum named `(schema, name)` visible to `txid`, by slot.
    pub fn enum_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.enums.iter().position(|e| {
            e.visible_to(txid)
                && e.definition_for(txid).schema.as_str() == schema
                && e.definition_for(txid).name.as_str() == name
        })
    }

    pub(crate) fn enum_for(&self, slot: usize, txid: u32) -> EnumDef {
        self.enums[slot].definition_for(txid)
    }

    pub(crate) fn enum_count(&self) -> usize {
        self.enums.len()
    }

    /// Committed enums carrying their slot indices, for the checkpoint,
    /// `pg_type` and `pg_enum`.
    pub fn live_enums(&self) -> impl Iterator<Item = (usize, &EnumDef)> {
        self.enums
            .iter()
            .enumerate()
            .filter(|(_, e)| e.ddl_state == CatalogDdlState::Present)
    }

    /// Whether any table column (in any table) is declared with this enum —
    /// the dependency that makes `DROP TYPE ... RESTRICT` fail.
    pub fn enum_in_use(&self, slot: usize) -> Option<(SqlName, SqlName)> {
        for table in self.tables.iter().filter(|t| t.live) {
            for col in table.def.columns() {
                if matches!(col.ctype, ColType::Enum(s) if s as usize == slot)
                    || matches!(
                        col.ctype,
                        ColType::Array(ArrElem::Enum(s)) if s as usize == slot
                    )
                {
                    return Some((table.def.name, col.name));
                }
            }
        }
        None
    }

    /// Registers an enum as an uncommitted CREATE owned by `txid` (or, for
    /// replay/checkpoint with `txid == 0`, committed directly). The caller has
    /// validated the labels and checked the name is free.
    pub fn create_enum(
        &mut self,
        schema: SqlName,
        name: SqlName,
        spec: EnumSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.enums.iter().find_map(|e| {
            let same_name = e.schema == schema && e.name == name
                || e.pending_definition
                    .is_some_and(|pending| pending.schema == schema && pending.name == name);
            same_name
                .then(|| {
                    e.pending_definition
                        .map(|pending| pending.txid)
                        .or_else(|| e.ddl_state.pending_txid())
                })
                .flatten()
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.enums.iter().any(|e| {
            e.visible_to(txid)
                && e.definition_for(txid).schema == schema
                && e.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(blocker) = self.domains.iter().find_map(|d| {
            (d.schema == schema && d.name == name
                || d.pending_definition
                    .and_then(|pending| pending.identity)
                    .is_some_and(|identity| identity.schema == schema && identity.name == name))
            .then_some(d.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.domains.iter().any(|d| {
            d.visible_to(txid)
                && d.definition_for(txid).schema == schema
                && d.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        if self.composites.iter().any(|composite| {
            composite.visible_to(txid)
                && composite.definition_for(txid).schema == schema
                && composite.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        let Some(new) = self
            .enums
            .iter()
            .position(|e| e.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many enum types (limit {})",
                self.enums.len()
            ));
        };
        self.catalog_seq += 1;
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Enum,
            slot: new as u16,
        });
        self.enums[new] = EnumDef {
            created_at: self.catalog_seq,
            schema,
            name,
            ownership,
            members: spec.members,
            n_members: spec.n_members,
            pending_definition: None,
            ddl_state: if txid == 0 {
                CatalogDdlState::Present
            } else {
                CatalogDdlState::PendingCreate { txid }
            },
        };
        Ok(new)
    }

    pub(crate) fn stage_enum_alter(
        &mut self,
        slot: usize,
        definition: EnumDef,
        txid: u32,
    ) -> Result<Option<PendingEnumDefinition>, SqlError> {
        if let Some(blocker) = self
            .enums
            .iter()
            .enumerate()
            .find_map(|(other_slot, other)| {
                (other_slot != slot
                    && (other.schema == definition.schema && other.name == definition.name
                        || other.pending_definition.is_some_and(|pending| {
                            pending.schema == definition.schema && pending.name == definition.name
                        })))
                .then(|| {
                    other
                        .pending_definition
                        .map(|pending| pending.txid)
                        .or_else(|| other.ddl_state.pending_txid())
                })
                .flatten()
                .filter(|&owner| owner != txid)
            })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if let Some(blocker) = self.domains.iter().find_map(|domain| {
            (domain.schema == definition.schema && domain.name == definition.name
                || domain
                    .pending_definition
                    .and_then(|pending| pending.identity)
                    .is_some_and(|identity| {
                        identity.schema == definition.schema && identity.name == definition.name
                    }))
            .then_some(domain.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if self.enums.iter().enumerate().any(|(other_slot, other)| {
            other_slot != slot
                && other.visible_to(txid)
                && other.definition_for(txid).schema == definition.schema
                && other.definition_for(txid).name == definition.name
        }) || self.domains.iter().any(|domain| {
            domain.visible_to(txid)
                && domain.definition_for(txid).schema == definition.schema
                && domain.definition_for(txid).name == definition.name
        }) || self.composites.iter().any(|composite| {
            composite.visible_to(txid)
                && composite.definition_for(txid).schema == definition.schema
                && composite.definition_for(txid).name == definition.name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                definition.name.as_str()
            ));
        }
        let enumeration = &mut self.enums[slot];
        if let Some(pending) = enumeration.pending_definition
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "type \"{}\" is being altered by another transaction",
                enumeration.name.as_str()
            ));
        }
        let prior = enumeration.pending_definition;
        enumeration.pending_definition = Some(PendingEnumDefinition {
            txid,
            schema: definition.schema,
            name: definition.name,
            members: definition.members,
            n_members: definition.n_members,
        });
        Ok(prior)
    }

    pub(crate) fn commit_enum_alter(&mut self, slot: usize, txid: u32) {
        if self.enums[slot]
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .is_some()
        {
            let definition = self.enums[slot].definition_for(txid);
            let previous = self.enums[slot];
            if definition.schema != previous.schema || definition.name != previous.name {
                self.move_enum_references(
                    slot,
                    previous.schema,
                    previous.name,
                    definition.schema,
                    definition.name,
                );
            }
            self.enums[slot] = definition;
        }
    }

    pub(crate) fn rollback_enum_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingEnumDefinition>,
    ) {
        self.enums[slot].pending_definition = prior;
    }

    /// Moves an enum and every persisted reference to its type identity. Runtime
    /// slots and value sort keys stay stable; comments are name-keyed and move
    /// with the type just as PostgreSQL keeps the same `pg_type` OID.
    fn move_enum_references(
        &mut self,
        slot: usize,
        old_schema: SqlName,
        old_name: SqlName,
        new_schema: SqlName,
        new_name: SqlName,
    ) {
        for table in self
            .tables
            .iter_mut()
            .filter(|table| table.live || table.pending_ddl.is_some())
        {
            let mut changed = false;
            for column in table.def.columns[..table.def.n_columns].iter_mut() {
                let uses_enum = matches!(column.ctype, ColType::Enum(s) if s as usize == slot)
                    || matches!(
                        column.ctype,
                        ColType::Array(ArrElem::Enum(s)) if s as usize == slot
                    );
                if uses_enum {
                    if let Some(identity) = &mut column.user_type {
                        identity.schema = new_schema;
                        identity.name = new_name;
                    }
                    changed = true;
                }
            }
            if changed {
                table.mark_dirty();
            }
        }
        for composite in self
            .composites
            .iter_mut()
            .filter(|composite| composite.ddl_state != CatalogDdlState::Absent)
        {
            for field in composite.fields.iter_mut().take(composite.n_fields) {
                if matches!(field.ctype, ColType::Enum(candidate) | ColType::Array(ArrElem::Enum(candidate)) if candidate as usize == slot)
                    && let Some(identity) = &mut field.user_type
                {
                    identity.schema = new_schema;
                    identity.name = new_name;
                }
            }
            if let Some(pending) = &mut composite.pending_definition {
                for field in pending.fields.iter_mut().take(pending.n_fields) {
                    if matches!(field.ctype, ColType::Enum(candidate) | ColType::Array(ArrElem::Enum(candidate)) if candidate as usize == slot)
                        && let Some(identity) = &mut field.user_type
                    {
                        identity.schema = new_schema;
                        identity.name = new_name;
                    }
                }
            }
        }
        for domain in self
            .domains
            .iter_mut()
            .filter(|domain| domain.ddl_state != CatalogDdlState::Absent)
        {
            if matches!(domain.base, ColType::Enum(candidate) if candidate as usize == slot)
                && domain.base_user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
            {
                domain.base_user_type = Some(UserTypeName {
                    schema: new_schema,
                    name: new_name,
                });
            }
        }
        for comment in self.comments.iter_mut() {
            if comment.used
                && comment.class == CommentClass::Type
                && comment.schema == old_schema
                && comment.name == old_name
            {
                comment.schema = new_schema;
                comment.name = new_name;
            }
        }
        self.move_routine_type_references(old_schema, old_name, new_schema, new_name, |ctype| {
            matches!(ctype, ColType::Enum(candidate) | ColType::Array(ArrElem::Enum(candidate)) if candidate as usize == slot)
        });
        self.rename_stored_query_dependency(DependencyClass::Enum, slot, new_schema, new_name);
    }

    pub fn drop_enum(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.enums.iter().find_map(|e| {
            let same_name = e.schema.as_str() == schema
                && (e.name.as_str() == name
                    || e.pending_definition
                        .is_some_and(|pending| pending.name.as_str() == name));
            same_name
                .then(|| {
                    e.pending_definition
                        .map(|pending| pending.txid)
                        .or_else(|| e.ddl_state.pending_txid())
                })
                .flatten()
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.enums.iter().position(|e| {
            e.visible_to(txid)
                && e.schema.as_str() == schema
                && e.definition_for(txid).name.as_str() == name
        }) else {
            return Ok(None);
        };
        let e = &mut self.enums[i];
        e.ddl_state = e.ddl_state.drop_by(txid);
        Ok(Some(i))
    }

    pub fn commit_enum_create(&mut self, slot: usize) {
        self.enums[slot].ddl_state = self.enums[slot].ddl_state.commit_create();
    }

    pub fn commit_enum_drop(&mut self, slot: usize) {
        let (schema, name) = (self.enums[slot].schema, self.enums[slot].name);
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.enums[slot].ddl_state = self.enums[slot].ddl_state.commit_drop();
    }

    pub fn rollback_enum_create(&mut self, slot: usize) {
        self.enums[slot].ddl_state = self.enums[slot].ddl_state.rollback_create();
    }

    pub fn rollback_enum_drop(&mut self, slot: usize, txid: u32) {
        let e = &mut self.enums[slot];
        e.ddl_state = e.ddl_state.rollback_drop(txid);
    }

    // --- Named composite types (CREATE TYPE ... AS (...)) ---

    pub fn resolve_composite_slot(&self, type_name: &str, txid: u32) -> Option<usize> {
        let (qualifier, name) = type_name
            .split_once('.')
            .map_or((None, type_name), |(s, n)| (Some(s), n));
        self.composites.iter().enumerate().find_map(|(slot, definition)| {
            (definition.visible_to(txid)
                && definition.definition_for(txid).name.as_str() == name
                && qualifier.map_or_else(|| self.path.entries().iter().any(|entry| matches!(entry, PathEntry::Schema(schema_slot) if self.schemas[*schema_slot as usize].name == definition.definition_for(txid).schema)), |schema| definition.definition_for(txid).schema.as_str() == schema))
                .then_some(slot)
        })
    }

    pub fn composite_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.composites.iter().position(|definition| {
            definition.visible_to(txid)
                && definition.definition_for(txid).schema.as_str() == schema
                && definition.definition_for(txid).name.as_str() == name
        })
    }

    pub fn composite(&self, slot: usize) -> &CompositeDef {
        &self.composites[slot]
    }

    pub(crate) fn composite_for(&self, slot: usize, txid: u32) -> CompositeDef {
        self.composites[slot].definition_for(txid)
    }

    pub fn live_composites(&self) -> impl Iterator<Item = (usize, &CompositeDef)> {
        self.composites
            .iter()
            .enumerate()
            .filter(|(_, definition)| definition.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn composites_with_slots_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, CompositeDef)> + '_ {
        self.composites
            .iter()
            .enumerate()
            .filter(move |(_, definition)| definition.visible_to(txid))
            .map(move |(slot, definition)| (slot, definition.definition_for(txid)))
    }

    pub(crate) fn stage_composite_alter(
        &mut self,
        slot: usize,
        definition: CompositeDef,
        txid: u32,
    ) -> Result<Option<PendingCompositeDefinition>, SqlError> {
        if let Some(blocker) = self.enums.iter().find_map(|enumeration| {
            (enumeration.schema == definition.schema && enumeration.name == definition.name
                || enumeration.pending_definition.is_some_and(|pending| {
                    pending.schema == definition.schema && pending.name == definition.name
                }))
            .then(|| {
                enumeration
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| enumeration.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if let Some(blocker) = self.domains.iter().find_map(|domain| {
            (domain.schema == definition.schema && domain.name == definition.name
                || domain
                    .pending_definition
                    .and_then(|pending| pending.identity)
                    .is_some_and(|identity| {
                        identity.schema == definition.schema && identity.name == definition.name
                    }))
            .then_some(domain.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if let Some(blocker) = self
            .composites
            .iter()
            .enumerate()
            .find_map(|(other_slot, other)| {
                (other_slot != slot
                    && (other.schema == definition.schema && other.name == definition.name
                        || other.pending_definition.is_some_and(|pending| {
                            pending.schema == definition.schema && pending.name == definition.name
                        })))
                .then(|| {
                    other
                        .pending_definition
                        .map(|pending| pending.txid)
                        .or_else(|| other.ddl_state.pending_txid())
                })
                .flatten()
                .filter(|&owner| owner != txid)
            })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if self.enums.iter().any(|enumeration| {
            enumeration.visible_to(txid)
                && enumeration.definition_for(txid).schema == definition.schema
                && enumeration.definition_for(txid).name == definition.name
        }) || self.domains.iter().any(|domain| {
            domain.visible_to(txid)
                && domain.definition_for(txid).schema == definition.schema
                && domain.definition_for(txid).name == definition.name
        }) || self
            .composites
            .iter()
            .enumerate()
            .any(|(other_slot, other)| {
                other_slot != slot
                    && other.visible_to(txid)
                    && other.definition_for(txid).schema == definition.schema
                    && other.definition_for(txid).name == definition.name
            })
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                definition.name.as_str()
            ));
        }
        let name = self.composites[slot].name;
        let composite = &mut self.composites[slot];
        if let Some(pending) = composite.pending_definition
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, name.as_str()));
        }
        let prior = composite.pending_definition;
        composite.pending_definition = Some(PendingCompositeDefinition {
            txid,
            schema: definition.schema,
            name: definition.name,
            fields: definition.fields,
            n_fields: definition.n_fields,
        });
        Ok(prior)
    }

    pub(crate) fn commit_composite_alter(&mut self, slot: usize, txid: u32) {
        let renamed = {
            let composite = &mut self.composites[slot];
            if let Some(pending) = composite.pending_definition
                && pending.txid == txid
            {
                let prior_schema = composite.schema;
                let prior_name = composite.name;
                composite.schema = pending.schema;
                composite.name = pending.name;
                composite.fields = pending.fields;
                composite.n_fields = pending.n_fields;
                composite.pending_definition = None;
                (prior_schema != composite.schema || prior_name != composite.name).then_some((
                    prior_schema,
                    prior_name,
                    composite.schema,
                    composite.name,
                ))
            } else {
                None
            }
        };
        if let Some((prior_schema, prior_name, new_schema, new_name)) = renamed {
            self.move_composite_references(slot, prior_schema, prior_name, new_schema, new_name);
        }
    }

    /// A composite slot is durable identity, while persisted definitions retain
    /// the SQL identity needed to rebind that slot after recovery. Move every
    /// such identity in the same catalog commit that changes its schema or name.
    fn move_composite_references(
        &mut self,
        slot: usize,
        old_schema: SqlName,
        old_name: SqlName,
        new_schema: SqlName,
        new_name: SqlName,
    ) {
        for table in self
            .tables
            .iter_mut()
            .filter(|table| table.live || table.pending_ddl.is_some())
        {
            let mut changed = false;
            for column in table.def.columns[..table.def.n_columns].iter_mut() {
                if matches!(
                    column.ctype,
                    ColType::Composite(candidate)
                        | ColType::Array(ArrElem::Composite(candidate))
                        if candidate as usize == slot
                ) && column.user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
                {
                    column.user_type = Some(UserTypeName {
                        schema: new_schema,
                        name: new_name,
                    });
                    changed = true;
                }
            }
            if changed {
                table.mark_dirty();
            }
        }
        for composite in self
            .composites
            .iter_mut()
            .filter(|composite| composite.ddl_state != CatalogDdlState::Absent)
        {
            for field in composite.fields.iter_mut().take(composite.n_fields) {
                if matches!(
                    field.ctype,
                    ColType::Composite(candidate)
                        | ColType::Array(ArrElem::Composite(candidate))
                        if candidate as usize == slot
                ) && field.user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
                {
                    field.user_type = Some(UserTypeName {
                        schema: new_schema,
                        name: new_name,
                    });
                }
            }
            if let Some(pending) = &mut composite.pending_definition {
                for field in pending.fields.iter_mut().take(pending.n_fields) {
                    if matches!(
                        field.ctype,
                        ColType::Composite(candidate)
                            | ColType::Array(ArrElem::Composite(candidate))
                            if candidate as usize == slot
                    ) && field.user_type
                        == Some(UserTypeName {
                            schema: old_schema,
                            name: old_name,
                        })
                    {
                        field.user_type = Some(UserTypeName {
                            schema: new_schema,
                            name: new_name,
                        });
                    }
                }
            }
        }
        for domain in self
            .domains
            .iter_mut()
            .filter(|domain| domain.ddl_state != CatalogDdlState::Absent)
        {
            if matches!(domain.base, ColType::Composite(candidate) if candidate as usize == slot)
                && domain.base_user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
            {
                domain.base_user_type = Some(UserTypeName {
                    schema: new_schema,
                    name: new_name,
                });
            }
        }
        for comment in self.comments.iter_mut() {
            if comment.used
                && comment.class == CommentClass::Type
                && comment.schema == old_schema
                && comment.name == old_name
            {
                comment.schema = new_schema;
                comment.name = new_name;
            }
        }
        self.move_routine_type_references(old_schema, old_name, new_schema, new_name, |ctype| {
            matches!(ctype, ColType::Composite(candidate) | ColType::Array(ArrElem::Composite(candidate)) if candidate as usize == slot)
        });
        self.rename_stored_query_dependency(DependencyClass::Composite, slot, new_schema, new_name);
    }

    fn move_routine_type_references(
        &mut self,
        old_schema: SqlName,
        old_name: SqlName,
        new_schema: SqlName,
        new_name: SqlName,
        uses: impl Fn(ColType) -> bool,
    ) {
        let move_argument = |argument: &mut RoutineArgumentDef| {
            if uses(argument.ctype)
                && argument.user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
            {
                argument.user_type = Some(UserTypeName {
                    schema: new_schema,
                    name: new_name,
                });
            }
        };
        let move_result = |result: &mut RoutineResult| {
            if uses(result.ctype)
                && result.user_type
                    == Some(UserTypeName {
                        schema: old_schema,
                        name: old_name,
                    })
            {
                result.user_type = Some(UserTypeName {
                    schema: new_schema,
                    name: new_name,
                });
            }
        };
        for routine in self.routines.iter_mut() {
            for argument in routine.arguments.iter_mut().take(routine.argument_count) {
                move_argument(argument);
            }
            for column in routine
                .result_columns
                .iter_mut()
                .take(routine.result_column_count)
            {
                move_argument(column);
            }
            for parameter in routine.parameters.iter_mut().take(routine.parameter_count) {
                let mut argument = RoutineArgumentDef {
                    name: parameter.name,
                    ctype: parameter.ctype,
                    user_type: parameter.user_type,
                };
                move_argument(&mut argument);
                parameter.user_type = argument.user_type;
            }
            match &mut routine.kind {
                RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                    move_result(result)
                }
                RoutineKind::Aggregate(aggregate) => {
                    move_result(&mut aggregate.state_type);
                    move_result(&mut aggregate.result_type);
                    if let Some(moving) = &mut aggregate.moving {
                        move_result(&mut moving.state_type);
                    }
                }
                _ => {}
            }
            if let Some(pending) = &mut routine.pending_definition {
                for argument in pending.arguments.iter_mut().take(pending.argument_count) {
                    move_argument(argument);
                }
                for column in pending
                    .result_columns
                    .iter_mut()
                    .take(pending.result_column_count)
                {
                    move_argument(column);
                }
                for parameter in pending.parameters.iter_mut().take(pending.parameter_count) {
                    let mut argument = RoutineArgumentDef {
                        name: parameter.name,
                        ctype: parameter.ctype,
                        user_type: parameter.user_type,
                    };
                    move_argument(&mut argument);
                    parameter.user_type = argument.user_type;
                }
                match &mut pending.kind {
                    RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                        move_result(result)
                    }
                    RoutineKind::Aggregate(aggregate) => {
                        move_result(&mut aggregate.state_type);
                        move_result(&mut aggregate.result_type);
                        if let Some(moving) = &mut aggregate.moving {
                            move_result(&mut moving.state_type);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn rollback_composite_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingCompositeDefinition>,
    ) {
        self.composites[slot].pending_definition = prior;
    }

    pub fn create_composite(
        &mut self,
        schema: SqlName,
        name: SqlName,
        mut spec: CompositeSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.domains.iter().find_map(|domain| {
            (domain.schema == schema && domain.name == name
                || domain
                    .pending_definition
                    .and_then(|pending| pending.identity)
                    .is_some_and(|identity| identity.schema == schema && identity.name == name))
            .then(|| {
                domain
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| domain.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if let Some(blocker) = self.enums.iter().find_map(|enumeration| {
            (enumeration.schema == schema && enumeration.name == name
                || enumeration
                    .pending_definition
                    .is_some_and(|pending| pending.schema == schema && pending.name == name))
            .then(|| {
                enumeration
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| enumeration.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if let Some(blocker) = self.composites.iter().find_map(|composite| {
            (composite.schema == schema && composite.name == name
                || composite
                    .pending_definition
                    .is_some_and(|pending| pending.schema == schema && pending.name == name))
            .then(|| {
                composite
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| composite.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        for field in spec.fields.iter_mut().take(spec.n_fields) {
            let Some(identity) = field.user_type else {
                continue;
            };
            let rebound = match field.ctype {
                ColType::Enum(_) | ColType::Array(ArrElem::Enum(_)) => self
                    .enum_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .map(|slot| {
                        if matches!(field.ctype, ColType::Array(_)) {
                            ColType::Array(ArrElem::Enum(slot as u16))
                        } else {
                            ColType::Enum(slot as u16)
                        }
                    }),
                ColType::Composite(_) | ColType::Array(ArrElem::Composite(_)) => self
                    .composite_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .map(|slot| {
                        if matches!(field.ctype, ColType::Array(_)) {
                            ColType::Array(ArrElem::Composite(slot as u16))
                        } else {
                            ColType::Composite(slot as u16)
                        }
                    }),
                _ => None,
            };
            if let Some(ctype) = rebound {
                field.ctype = ctype;
            }
        }
        let exists = self
            .domain_slot(schema.as_str(), name.as_str(), txid)
            .is_some()
            || self
                .enum_slot(schema.as_str(), name.as_str(), txid)
                .is_some()
            || self
                .composite_slot(schema.as_str(), name.as_str(), txid)
                .is_some();
        if exists {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        let Some(slot) = self
            .composites
            .iter()
            .position(|definition| definition.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many composite types (limit {})",
                self.composites.len()
            ));
        };
        self.create_composite_at(slot, schema, name, spec, txid)
    }

    /// Replays a durable composite catalog identity. A WAL record names the
    /// slot explicitly, so restart cannot rebind existing rows to whichever
    /// free slot happens to be allocated first.
    pub(crate) fn create_composite_at(
        &mut self,
        slot: usize,
        schema: SqlName,
        name: SqlName,
        mut spec: CompositeSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if slot >= self.composites.len()
            || self.composites[slot].ddl_state != CatalogDdlState::Absent
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "journal composite catalog identity is unavailable"
            ));
        }
        for field in spec.fields.iter_mut().take(spec.n_fields) {
            let Some(identity) = field.user_type else {
                continue;
            };
            if let Some(ctype) = match field.ctype {
                ColType::Enum(_) | ColType::Array(ArrElem::Enum(_)) => self
                    .enum_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .map(|type_slot| {
                        if matches!(field.ctype, ColType::Array(_)) {
                            ColType::Array(ArrElem::Enum(type_slot as u16))
                        } else {
                            ColType::Enum(type_slot as u16)
                        }
                    }),
                ColType::Composite(_) | ColType::Array(ArrElem::Composite(_)) => self
                    .composite_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .map(|type_slot| {
                        if matches!(field.ctype, ColType::Array(_)) {
                            ColType::Array(ArrElem::Composite(type_slot as u16))
                        } else {
                            ColType::Composite(type_slot as u16)
                        }
                    }),
                _ => None,
            } {
                field.ctype = ctype;
            }
        }
        self.catalog_seq += 1;
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Composite,
            slot: slot as u16,
        });
        self.composites[slot] = CompositeDef {
            created_at: self.catalog_seq,
            schema,
            name,
            ownership: self.initial_ownership(txid),
            fields: spec.fields,
            n_fields: spec.n_fields,
            pending_definition: None,
            ddl_state: if txid == 0 {
                CatalogDdlState::Present
            } else {
                CatalogDdlState::PendingCreate { txid }
            },
        };
        Ok(slot)
    }

    pub fn commit_composite_create(&mut self, slot: usize) {
        self.composites[slot].ddl_state = self.composites[slot].ddl_state.commit_create();
    }

    pub fn drop_composite(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.composites.iter().find_map(|definition| {
            (definition.schema.as_str() == schema
                && (definition.name.as_str() == name
                    || definition
                        .pending_definition
                        .is_some_and(|pending| pending.name.as_str() == name)))
            .then(|| {
                definition
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| definition.ddl_state.pending_txid())
            })
            .flatten()
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(slot) = self.composites.iter().position(|definition| {
            definition.visible_to(txid)
                && definition.schema.as_str() == schema
                && definition.definition_for(txid).name.as_str() == name
        }) else {
            return Ok(None);
        };
        self.composites[slot].ddl_state = self.composites[slot].ddl_state.drop_by(txid);
        Ok(Some(slot))
    }

    pub fn commit_composite_drop(&mut self, slot: usize) {
        let definition = self.composites[slot];
        self.drop_object_comments(
            CommentClass::Type,
            definition.schema.as_str(),
            definition.name.as_str(),
        );
        self.composites[slot].ddl_state = self.composites[slot].ddl_state.commit_drop();
    }

    pub fn rollback_composite_create(&mut self, slot: usize) {
        self.composites[slot].ddl_state = self.composites[slot].ddl_state.rollback_create();
    }

    pub fn rollback_composite_drop(&mut self, slot: usize, txid: u32) {
        self.composites[slot].ddl_state = self.composites[slot].ddl_state.rollback_drop(txid);
    }

    /// Registers a view as an uncommitted CREATE owned by `txid` (other
    /// transactions keep seeing the committed catalog until commit).
    /// `or_replace` marks an existing visible view pending-dropped. Returns
    /// `(new_slot, replaced_old_slot)`. Errors if the name is taken by a
    /// table, by a view visible to `txid` (without `or_replace`), or by
    /// another transaction's uncommitted view DDL.
    pub fn create_view(
        &mut self,
        schema: SqlName,
        name: SqlName,
        query: StoredQueryDefinition,
        security: ViewSecurity,
        or_replace: bool,
        txid: u32,
    ) -> Result<(usize, Option<usize>), SqlError> {
        self.require_schema_create(schema.as_str(), txid)?;
        if self.find_table(schema.as_str(), name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(blocker) = self.views.iter().find_map(|v| {
            (v.schema_for(txid).as_str() == schema.as_str() && v.name.as_str() == name.as_str())
                .then_some(v.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let existing = self.views.iter().position(|v| {
            v.visible_to(txid)
                && v.schema_for(txid).as_str() == schema.as_str()
                && v.name.as_str() == name.as_str()
        });
        if existing.is_some() && !or_replace {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                name.as_str()
            ));
        }
        let Some(new) = self
            .views
            .iter()
            .position(|v| v.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many views (limit {})",
                self.views.len()
            ));
        };
        if let Some(old) = existing {
            self.pending_drop_view(old, txid);
        }
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::View,
            slot: new as u16,
        });
        self.catalog_seq += 1;
        self.views[new] = ViewDef {
            created_at: self.catalog_seq,
            schema,
            name,
            sql: query.sql,
            creation_path: query.creation_path,
            security,
            ownership,
            pending_schema: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        self.view_dependencies[new] = query.dependencies;
        Ok((new, existing))
    }

    /// Marks the view visible to `txid` pending-dropped; returns its slot (for
    /// undo). None if absent. Errors if another transaction's uncommitted DDL
    /// holds the name.
    pub fn drop_view(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.views.iter().find_map(|v| {
            (v.schema_for(txid).as_str() == schema && v.name.as_str() == name)
                .then_some(v.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.views.iter().position(|v| {
            v.visible_to(txid) && v.schema_for(txid).as_str() == schema && v.name.as_str() == name
        }) else {
            return Ok(None);
        };
        self.pending_drop_view(i, txid);
        Ok(Some(i))
    }

    /// Overlays a pending DROP on a slot: the owner's own pending-create
    /// simply evaporates (never committed, nothing to keep).
    fn pending_drop_view(&mut self, slot: usize, txid: u32) {
        let v = &mut self.views[slot];
        v.ddl_state = v.ddl_state.drop_by(txid);
    }

    /// Promotes an uncommitted CREATE VIEW into the committed catalog.
    pub fn commit_view_create(&mut self, slot: usize) {
        let schema = self.views[slot].schema;
        let name = self.views[slot].name;
        if let Some(old_slot) = self.views.iter().enumerate().find_map(|(old_slot, view)| {
            (old_slot != slot
                && view.ddl_state == CatalogDdlState::Present
                && view.schema == schema
                && view.name == name)
                .then_some(old_slot)
        }) {
            self.replace_stored_query_dependency_slot(
                DependencyClass::View,
                old_slot,
                slot,
                schema,
                name,
            );
        }
        self.views[slot].ddl_state = self.views[slot].ddl_state.commit_create();
    }

    pub(crate) fn stage_view_schema(
        &mut self,
        slot: usize,
        schema: SqlName,
        txid: u32,
    ) -> Result<Option<PendingObjectSchema>, SqlError> {
        let prior = self.views[slot].pending_schema;
        if prior.is_some_and(|pending| pending.txid != txid) {
            return Err(self.catalog_ddl_wait_error(
                txid,
                prior.expect("checked Some").txid,
                self.views[slot].name.as_str(),
            ));
        }
        self.views[slot].pending_schema = Some(PendingObjectSchema { txid, schema });
        Ok(prior)
    }

    pub(crate) fn commit_view_schema(&mut self, slot: usize, txid: u32) {
        let Some(pending) = self.views[slot]
            .pending_schema
            .filter(|pending| pending.txid == txid)
        else {
            return;
        };
        let old_schema = self.views[slot].schema;
        let name = self.views[slot].name;
        self.views[slot].schema = pending.schema;
        self.views[slot].pending_schema = None;
        for comment in self.comments.iter_mut() {
            if comment.used
                && matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                && comment.schema == old_schema
                && comment.name == name
            {
                comment.schema = pending.schema;
            }
        }
    }

    pub(crate) fn rollback_view_schema(&mut self, slot: usize, prior: Option<PendingObjectSchema>) {
        self.views[slot].pending_schema = prior;
    }

    /// Promotes an uncommitted DROP VIEW into the committed catalog.
    pub fn commit_view_drop(&mut self, slot: usize) {
        let (schema, name) = (self.views[slot].schema, self.views[slot].name);
        // CREATE OR REPLACE installs the replacement before retiring this
        // slot. Comments belong to the logical same-named object and survive;
        // an ordinary DROP has no replacement and removes them.
        let replaced = self.views.iter().enumerate().any(|(other, view)| {
            other != slot
                && view.ddl_state == CatalogDdlState::Present
                && view.schema == schema
                && view.name == name
        });
        if !replaced {
            self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
            self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        }
        self.views[slot].ddl_state = self.views[slot].ddl_state.commit_drop();
    }

    /// Discards an uncommitted CREATE VIEW (rollback): the slot is freed.
    pub fn rollback_view_create(&mut self, slot: usize) {
        self.views[slot].ddl_state = self.views[slot].ddl_state.rollback_create();
    }

    /// Discards an uncommitted DROP VIEW (rollback). A committed view becomes
    /// visible again; a same-transaction pending-create (create + drop, then
    /// the drop rolled back to a savepoint) reverts to pending-create.
    pub fn rollback_view_drop(&mut self, slot: usize, txid: u32) {
        let view = &mut self.views[slot];
        view.ddl_state = view.ddl_state.rollback_drop(txid);
    }

    pub(crate) fn routine_count(&self) -> usize {
        self.routines.len()
    }

    pub(crate) fn routine(&self, slot: usize) -> &RoutineDef {
        &self.routines[slot]
    }

    /// Returns the routine definition visible to `txid`.  Replacement keeps
    /// the catalog slot and object identifier stable while its new definition
    /// is private until commit.
    pub(crate) fn routine_for(&self, slot: usize, txid: u32) -> RoutineDef {
        self.routines[slot].definition_for(txid)
    }

    pub(crate) fn routine_slot_by_oid(&self, oid: i32, txid: u32) -> Option<usize> {
        self.routines
            .iter()
            .position(|routine| routine.visible_to(txid) && routine_oid(routine) == oid)
    }

    /// Resolves a scalar call from its evaluated values and their SQL type
    /// identities. Domains deliberately use their base representation at
    /// runtime, so their OID belongs at this boundary rather than being
    /// inferred from an erased datum.
    pub(crate) fn routine_slot_for_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_for_call_syntax_oids(name, argument_type_oids, false, txid)
    }

    pub(crate) fn routine_slot_for_call_syntax_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<usize> {
        if argument_type_oids.len() > MAX_ROUTINE_ARGUMENTS {
            return None;
        }
        if !explicit_variadic
            && let Some(slot) = self.routine_slot_on_path_oids(
                name,
                argument_type_oids,
                txid,
                RoutineCallKind::Scalar,
            )
        {
            return Some(slot);
        }
        self.variadic_routine_slot_on_path_oids(
            name,
            argument_type_oids,
            explicit_variadic,
            txid,
            RoutineCallKind::Scalar,
        )
    }

    pub(crate) fn routine_slot_for_function_call_syntax_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<usize> {
        if !explicit_variadic
            && let Some(slot) =
                self.routine_slot_for_function_call_oids(name, argument_type_oids, txid)
        {
            return Some(slot);
        }
        self.routine_slot_for_call_syntax_oids(name, argument_type_oids, explicit_variadic, txid)
            .or_else(|| {
                self.variadic_routine_slot_on_path_oids(
                    name,
                    argument_type_oids,
                    explicit_variadic,
                    txid,
                    RoutineCallKind::Set,
                )
            })
    }

    fn variadic_routine_slot_on_path_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        let resolve = |schema: &str, routine_name: &str| {
            let mut candidates = self
                .routines
                .iter()
                .enumerate()
                .filter_map(|(slot, routine)| {
                    let definition = routine.definition_for(txid);
                    if !routine.visible_to(txid)
                        || !kind.accepts(definition.kind)
                        || definition.schema_for(txid).as_str() != schema
                        || definition.name_for(txid).as_str() != routine_name
                    {
                        return None;
                    }
                    let variadic_index = definition.arguments().len().checked_sub(1)?;
                    let variadic_parameter = definition.parameter_for_input(variadic_index)?;
                    let RoutineParameterMode::Variadic { .. } = variadic_parameter.mode else {
                        return None;
                    };
                    let ColType::Array(element) = variadic_parameter.ctype else {
                        return None;
                    };
                    let fixed = &definition.arguments()[..variadic_index];
                    if argument_type_oids.len() < fixed.len() + usize::from(!explicit_variadic)
                        || explicit_variadic && argument_type_oids.len() != fixed.len() + 1
                        || !fixed.iter().zip(argument_type_oids).all(|(argument, oid)| {
                            self.routine_argument_oid(argument, txid)
                                .is_some_and(|expected| {
                                    self.routine_implicit_cast(*oid, expected, txid)
                                })
                        })
                    {
                        return None;
                    }
                    let expected = if explicit_variadic {
                        self.routine_argument_oid(&definition.arguments()[variadic_index], txid)?
                    } else {
                        element.element_oid()
                    };
                    argument_type_oids[variadic_index..]
                        .iter()
                        .all(|oid| self.routine_implicit_cast(*oid, expected, txid))
                        .then_some(slot)
                });
            let first = candidates.next();
            if first.is_some() && candidates.next().is_none() {
                first
            } else {
                None
            }
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            resolve(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    pub(crate) fn routine_slot_for_named_call_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        if argument_type_oids.len() > MAX_ROUTINE_ARGUMENTS
            || argument_names.len() != argument_type_oids.len()
        {
            return None;
        }
        let resolve = |schema: &str, routine_name: &str| {
            self.routine_slot_in_named_oids(
                schema,
                routine_name,
                argument_names,
                argument_type_oids,
                txid,
                RoutineCallKind::Scalar,
            )
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            resolve(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    pub(crate) fn routine_slot_for_named_function_call_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        if let Some(slot) =
            self.routine_slot_for_named_call_oids(name, argument_names, argument_type_oids, txid)
        {
            return Some(slot);
        }
        let resolve = |schema: &str, routine_name: &str| {
            self.routine_slot_in_named_oids(
                schema,
                routine_name,
                argument_names,
                argument_type_oids,
                txid,
                RoutineCallKind::Set,
            )
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            resolve(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    pub(crate) fn routine_for_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<RoutineDef> {
        self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Scalar)
            .and_then(|slot| self.routine_for_bound_call(slot, argument_type_oids, txid))
    }

    pub(crate) fn aggregate_for_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<(usize, RoutineDef, AggregateRoutine)> {
        let slot = self.routine_slot_on_path_oids(
            name,
            argument_type_oids,
            txid,
            RoutineCallKind::Aggregate,
        )?;
        let routine = self.routine_for_bound_call(slot, argument_type_oids, txid)?;
        let RoutineKind::Aggregate(aggregate) = routine.kind else {
            unreachable!("aggregate call resolution returned a non-aggregate")
        };
        Some((slot, routine, aggregate))
    }

    pub(crate) fn has_aggregate_candidate(&self, name: &str, arity: usize, txid: u32) -> bool {
        self.has_routine_candidate(name, arity, txid, |routine| {
            matches!(routine.kind, RoutineKind::Aggregate(_))
        })
    }

    pub(crate) fn function_for_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<RoutineDef> {
        self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Scalar)
            .or_else(|| {
                self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Set)
            })
            .and_then(|slot| self.routine_for_bound_call(slot, argument_type_oids, txid))
    }

    pub(crate) fn function_for_call_syntax_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<RoutineDef> {
        if !explicit_variadic
            && let Some(routine) = self.function_for_call_oids(name, argument_type_oids, txid)
        {
            return Some(routine);
        }
        let slot = self
            .variadic_routine_slot_on_path_oids(
                name,
                argument_type_oids,
                explicit_variadic,
                txid,
                RoutineCallKind::Scalar,
            )
            .or_else(|| {
                self.variadic_routine_slot_on_path_oids(
                    name,
                    argument_type_oids,
                    explicit_variadic,
                    txid,
                    RoutineCallKind::Set,
                )
            })?;
        let routine = self.routine_for(slot, txid);
        let mut declared_oids = [crate::sql::types::oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
        for (index, argument) in routine.arguments().iter().enumerate() {
            declared_oids[index] = self.routine_argument_oid(argument, txid)?;
        }
        self.routine_for_bound_call(slot, &declared_oids[..routine.argument_count], txid)
    }

    pub(crate) fn function_for_named_call_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<RoutineDef> {
        let slot = self.routine_slot_for_named_function_call_oids(
            name,
            argument_names,
            argument_type_oids,
            txid,
        )?;
        let routine = self.routine_for(slot, txid);
        let mapping =
            routine.call_input_mapping(argument_names, argument_type_oids.len(), false)?;
        let mut completed = [crate::sql::types::oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
        for (input_index, argument) in routine.arguments().iter().enumerate() {
            completed[input_index] = self.routine_argument_oid(argument, txid)?;
        }
        for (call_index, oid) in argument_type_oids.iter().enumerate() {
            completed[usize::from(mapping[call_index])] = *oid;
        }
        self.routine_for_bound_call(slot, &completed[..routine.argument_count], txid)
    }

    /// Resolves the declared input OIDs for a function call that still has
    /// untyped protocol parameters. Only a unique overload may supply the
    /// contract; an unknown argument is never guessed from one of several
    /// candidates.
    pub(crate) fn function_call_parameter_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        explicit_variadic: bool,
        actual_oids: &[i32],
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        if actual_oids.len() > MAX_ROUTINE_ARGUMENTS
            || (!argument_names.is_empty() && argument_names.len() != actual_oids.len())
        {
            return None;
        }
        let resolve = |schema: &str, routine_name: &str| {
            let mut found = None;
            for (slot, stored) in self.routines.iter().enumerate() {
                let routine = stored.definition_for(txid);
                if !stored.visible_to(txid)
                    || !matches!(
                        routine.kind,
                        RoutineKind::Function { .. }
                            | RoutineKind::SetFunction { .. }
                            | RoutineKind::RecordFunction { .. }
                            | RoutineKind::TableFunction
                    )
                    || routine.schema_for(txid).as_str() != schema
                    || routine.name_for(txid).as_str() != routine_name
                {
                    continue;
                }
                let Some(mapping) = routine.call_input_mapping(
                    argument_names,
                    actual_oids.len(),
                    explicit_variadic,
                ) else {
                    continue;
                };
                let variadic_input =
                    routine
                        .arguments()
                        .iter()
                        .enumerate()
                        .find_map(|(input_index, _)| {
                            matches!(
                                routine.parameter_for_input(input_index)?.mode,
                                RoutineParameterMode::Variadic { .. }
                            )
                            .then_some(input_index)
                        });
                let mut expected = [crate::sql::types::oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
                let mut matches = true;
                for (call_index, actual_oid) in actual_oids.iter().copied().enumerate() {
                    let input_index = usize::from(mapping[call_index]);
                    let argument = routine.arguments()[input_index];
                    let expected_oid = if !explicit_variadic && variadic_input == Some(input_index)
                    {
                        match argument.ctype {
                            ColType::Array(element) => Some(element.element_oid()),
                            _ => None,
                        }
                    } else {
                        self.routine_argument_oid(&argument, txid)
                    };
                    let Some(expected_oid) = expected_oid else {
                        matches = false;
                        break;
                    };
                    expected[call_index] = if argument.polymorphic_type().is_some() {
                        actual_oid
                    } else {
                        expected_oid
                    };
                    if actual_oid != crate::sql::types::oid::UNKNOWN
                        && argument.polymorphic_type().is_none()
                        && !self.routine_implicit_cast(actual_oid, expected_oid, txid)
                    {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
                if found.replace((slot, expected)).is_some() {
                    return None;
                }
            }
            found.map(|(_, expected)| expected)
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        for entry in self.path.entries() {
            let PathEntry::Schema(schema_slot) = entry else {
                continue;
            };
            let schema = self.schemas[*schema_slot as usize].name;
            if let Some(expected) = resolve(schema.as_str(), name) {
                return Some(expected);
            }
        }
        None
    }

    pub(crate) fn routine_for_bound_call(
        &self,
        slot: usize,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<RoutineDef> {
        let mut routine = self.routine_for(slot, txid);
        if routine.accepts_input_arity(argument_type_oids.len())
            && routine.arguments()[..argument_type_oids.len()]
                .iter()
                .zip(argument_type_oids)
                .all(|(argument, oid)| {
                    self.routine_argument_oid(argument, txid)
                        .is_some_and(|expected| self.routine_implicit_cast(*oid, expected, txid))
                })
        {
            return Some(routine);
        }
        let binding = self.polymorphic_call_binding(
            &routine.arguments()[..argument_type_oids.len()],
            argument_type_oids,
            txid,
        )?;
        for argument in routine.arguments.iter_mut().take(routine.argument_count) {
            let Some(kind) = argument.polymorphic_type() else {
                continue;
            };
            let concrete = self.routine_result_for_oid(binding.concrete_oid(kind)?, txid)?;
            argument.ctype = concrete.ctype;
            argument.user_type = concrete.user_type;
        }
        for parameter in routine.parameters.iter_mut().take(routine.parameter_count) {
            let Some(kind) = polymorphic_type(parameter.ctype, parameter.user_type) else {
                continue;
            };
            let concrete = self.routine_result_for_oid(binding.concrete_oid(kind)?, txid)?;
            parameter.ctype = concrete.ctype;
            parameter.user_type = concrete.user_type;
        }
        for column in routine
            .result_columns
            .iter_mut()
            .take(routine.result_column_count)
        {
            let Some(kind) = column.polymorphic_type() else {
                continue;
            };
            let concrete = self.routine_result_for_oid(binding.concrete_oid(kind)?, txid)?;
            column.ctype = concrete.ctype;
            column.user_type = concrete.user_type;
        }
        let resolve = |result: &mut RoutineResult| -> Option<()> {
            let Some(kind) = result.polymorphic_type() else {
                return Some(());
            };
            *result = self.routine_result_for_oid(binding.concrete_oid(kind)?, txid)?;
            Some(())
        };
        match &mut routine.kind {
            RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                resolve(result)?;
            }
            RoutineKind::Aggregate(aggregate) => {
                resolve(&mut aggregate.state_type)?;
                resolve(&mut aggregate.result_type)?;
                if let Some(moving) = &mut aggregate.moving {
                    resolve(&mut moving.state_type)?;
                }
            }
            RoutineKind::RecordFunction { .. }
            | RoutineKind::TableFunction
            | RoutineKind::Trigger
            | RoutineKind::Procedure => {}
        }
        Some(routine)
    }

    pub(crate) fn routine_slot_for_function_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Scalar)
            .or_else(|| {
                self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Set)
            })
    }

    pub(crate) fn has_set_routine_candidate(&self, name: &str, arity: usize, txid: u32) -> bool {
        self.has_routine_candidate(name, arity, txid, |routine| routine.kind.is_set_returning())
    }

    pub(crate) fn has_function_routine_candidate(
        &self,
        name: &str,
        arity: usize,
        txid: u32,
    ) -> bool {
        self.has_routine_candidate(name, arity, txid, |routine| {
            matches!(
                routine.kind,
                RoutineKind::Function { .. }
                    | RoutineKind::SetFunction { .. }
                    | RoutineKind::RecordFunction { .. }
                    | RoutineKind::TableFunction
            )
        })
    }

    pub(crate) fn has_volatile_function_routine_candidate(
        &self,
        name: &str,
        arity: usize,
        txid: u32,
    ) -> bool {
        self.has_routine_candidate(name, arity, txid, |routine| {
            matches!(
                routine.kind,
                RoutineKind::Function { .. }
                    | RoutineKind::SetFunction { .. }
                    | RoutineKind::RecordFunction { .. }
                    | RoutineKind::TableFunction
            ) && routine.attributes.volatility == RoutineVolatility::Volatile
        })
    }

    fn has_routine_candidate(
        &self,
        name: &str,
        arity: usize,
        txid: u32,
        accepts: impl Fn(RoutineDef) -> bool,
    ) -> bool {
        let matches = |schema: &str, routine_name: &str| {
            self.routines.iter().any(|routine| {
                let definition = routine.definition_for(txid);
                routine.visible_to(txid)
                    && accepts(definition)
                    && definition.accepts_input_arity(arity)
                    && definition.schema_for(txid).as_str() == schema
                    && definition.name_for(txid).as_str() == routine_name
            })
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return matches(schema, routine_name);
        }
        self.path.entries().iter().any(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return false;
            };
            matches(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    /// Resolves a set-returning call from declared argument identities.  This
    /// is distinct from the datum representation because a domain's runtime
    /// representation is its base type.
    pub(crate) fn routine_slot_for_table_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        if argument_type_oids.len() > MAX_ROUTINE_ARGUMENTS {
            return None;
        }
        self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Set)
    }

    pub(crate) fn routine_function_result_oid(
        &self,
        routine: &RoutineDef,
        txid: u32,
    ) -> Option<i32> {
        let result = match routine.kind {
            RoutineKind::Function { result } | RoutineKind::SetFunction { result } => result,
            RoutineKind::RecordFunction { .. } | RoutineKind::TableFunction => {
                return Some(crate::sql::types::oid::RECORD);
            }
            RoutineKind::Trigger | RoutineKind::Procedure | RoutineKind::Aggregate(_) => {
                return None;
            }
        };
        self.routine_type_oid(result.ctype, result.user_type, txid)
    }

    pub(crate) fn procedure_slot_for_call_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_on_path_oids(name, argument_type_oids, txid, RoutineCallKind::Procedure)
    }

    pub(crate) fn procedure_slot_for_call_syntax_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<usize> {
        if !explicit_variadic
            && let Some(slot) = self.procedure_slot_for_call_oids(name, argument_type_oids, txid)
        {
            return Some(slot);
        }
        self.variadic_routine_slot_on_path_oids(
            name,
            argument_type_oids,
            explicit_variadic,
            txid,
            RoutineCallKind::Procedure,
        )
    }

    pub(crate) fn procedure_slot_for_named_call_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<usize> {
        let resolve = |schema: &str, routine_name: &str| {
            self.routine_slot_in_named_oids(
                schema,
                routine_name,
                argument_names,
                argument_type_oids,
                txid,
                RoutineCallKind::Procedure,
            )
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            resolve(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    pub(crate) fn procedure_slot_for_call_shape(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_count: usize,
        txid: u32,
    ) -> Option<usize> {
        let resolve = |schema: &str, routine_name: &str| {
            let mut candidates = self
                .routines
                .iter()
                .enumerate()
                .filter_map(|(slot, routine)| {
                    let definition = routine.definition_for(txid);
                    (routine.visible_to(txid)
                        && definition.schema_for(txid).as_str() == schema
                        && definition.name_for(txid).as_str() == routine_name
                        && definition
                            .procedure_call_mapping(argument_names, argument_count)
                            .is_some())
                    .then_some(slot)
                });
            let first = candidates.next();
            if first.is_some() && candidates.next().is_none() {
                first
            } else {
                None
            }
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            resolve(self.schemas[*slot as usize].name.as_str(), name)
        })
    }

    /// Resolves protocol parameter OIDs for a CALL before its Bind values
    /// exist. OUT placeholders and input arguments both derive their type from
    /// the unique procedure declaration.
    pub(crate) fn procedure_call_parameter_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        explicit_variadic: bool,
        actual_oids: &[i32],
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        if actual_oids.len() > MAX_ROUTINE_ARGUMENTS
            || (!argument_names.is_empty() && argument_names.len() != actual_oids.len())
        {
            return None;
        }
        let resolve = |schema: &str, routine_name: &str| {
            let mut found = None;
            for stored in self.routines.iter() {
                let routine = stored.definition_for(txid);
                if !stored.visible_to(txid)
                    || !matches!(routine.kind, RoutineKind::Procedure)
                    || routine.schema_for(txid).as_str() != schema
                    || routine.name_for(txid).as_str() != routine_name
                {
                    continue;
                }
                let output_call = routine
                    .parameters()
                    .iter()
                    .any(|parameter| parameter.mode.is_output());
                let input_mapping = if output_call {
                    let Some(mapping) =
                        routine.procedure_call_mapping(argument_names, actual_oids.len())
                    else {
                        continue;
                    };
                    mapping
                } else {
                    let Some(mapping) = routine.call_input_mapping(
                        argument_names,
                        actual_oids.len(),
                        explicit_variadic,
                    ) else {
                        continue;
                    };
                    mapping
                };
                let variadic_input =
                    routine
                        .arguments()
                        .iter()
                        .enumerate()
                        .find_map(|(input_index, _)| {
                            matches!(
                                routine.parameter_for_input(input_index)?.mode,
                                RoutineParameterMode::Variadic { .. }
                            )
                            .then_some(input_index)
                        });
                let mut expected = [crate::sql::types::oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
                let mut matches = true;
                for (call_index, actual_oid) in actual_oids.iter().copied().enumerate() {
                    let (polymorphic, expected_oid) =
                        if output_call && input_mapping[call_index] == u8::MAX {
                            let parameter_index =
                                match argument_names.get(call_index).copied().flatten() {
                                    Some(name) => {
                                        routine.parameters().iter().position(|parameter| {
                                            parameter.name.as_str().eq_ignore_ascii_case(name)
                                        })?
                                    }
                                    None => call_index,
                                };
                            let parameter = *routine.parameters().get(parameter_index)?;
                            let expected_oid =
                                self.routine_type_oid(parameter.ctype, parameter.user_type, txid)?;
                            (
                                polymorphic_type(parameter.ctype, parameter.user_type),
                                expected_oid,
                            )
                        } else {
                            let input_index = usize::from(input_mapping[call_index]);
                            let argument = routine.arguments()[input_index];
                            let expected_oid =
                                if !explicit_variadic && variadic_input == Some(input_index) {
                                    match argument.ctype {
                                        ColType::Array(element) => element.element_oid(),
                                        _ => return None,
                                    }
                                } else {
                                    self.routine_argument_oid(&argument, txid)?
                                };
                            (argument.polymorphic_type(), expected_oid)
                        };
                    expected[call_index] = if polymorphic.is_some() {
                        actual_oid
                    } else {
                        expected_oid
                    };
                    if actual_oid != crate::sql::types::oid::UNKNOWN
                        && polymorphic.is_none()
                        && !self.routine_implicit_cast(actual_oid, expected_oid, txid)
                    {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
                if found.replace(expected).is_some() {
                    return None;
                }
            }
            found
        };
        if let Some((schema, routine_name)) = name.split_once('.') {
            return resolve(schema, routine_name);
        }
        for entry in self.path.entries() {
            let PathEntry::Schema(schema_slot) = entry else {
                continue;
            };
            let schema = self.schemas[*schema_slot as usize].name;
            if let Some(expected) = resolve(schema.as_str(), name) {
                return Some(expected);
            }
        }
        None
    }

    pub(crate) fn trigger_slot_for_call(&self, name: &str, txid: u32) -> Option<usize> {
        self.routine_slot_on_path(name, &[], txid, RoutineCallKind::Trigger)
    }

    fn routine_slot_on_path(
        &self,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        if let Some((schema, name)) = name.split_once('.') {
            return self.routine_slot_in(schema, name, argument_types, txid, kind);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            self.routine_slot_in(
                self.schemas[*slot as usize].name.as_str(),
                name,
                argument_types,
                txid,
                kind,
            )
        })
    }

    fn routine_slot_on_path_oids(
        &self,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        if let Some((schema, name)) = name.split_once('.') {
            return self.routine_slot_in_oids(schema, name, argument_type_oids, txid, kind);
        }
        self.path.entries().iter().find_map(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return None;
            };
            self.routine_slot_in_oids(
                self.schemas[*slot as usize].name.as_str(),
                name,
                argument_type_oids,
                txid,
                kind,
            )
        })
    }

    fn routine_slot_in(
        &self,
        schema: &str,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        self.routines.iter().position(|routine| {
            let definition = routine.definition_for(txid);
            routine.visible_to(txid)
                && kind.accepts(definition.kind)
                && definition.schema_for(txid).as_str() == schema
                && definition.name_for(txid).as_str() == name
                && definition.accepts_input_arity(argument_types.len())
                && definition.arguments()[..argument_types.len()]
                    .iter()
                    .zip(argument_types)
                    .all(|(parameter, value)| parameter.ctype == *value)
        })
    }

    fn routine_slot_in_oids(
        &self,
        schema: &str,
        name: &str,
        argument_type_oids: &[i32],
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        let exact =
            self.routines.iter().position(|routine| {
                let definition = routine.definition_for(txid);
                routine.visible_to(txid)
                    && kind.accepts(definition.kind)
                    && definition.schema_for(txid).as_str() == schema
                    && definition.name_for(txid).as_str() == name
                    && definition.argument_count == argument_type_oids.len()
                    && definition.arguments().iter().zip(argument_type_oids).all(
                        |(argument, oid)| self.routine_argument_oid(argument, txid) == Some(*oid),
                    )
            });
        if exact.is_some() {
            return exact;
        }
        let concrete = self
            .routines
            .iter()
            .enumerate()
            .filter_map(|(slot, routine)| {
                let definition = routine.definition_for(txid);
                (routine.visible_to(txid)
                    && kind.accepts(definition.kind)
                    && definition.schema_for(txid).as_str() == schema
                    && definition.name_for(txid).as_str() == name
                    && definition.accepts_input_arity(argument_type_oids.len())
                    && definition.arguments()[..argument_type_oids.len()]
                        .iter()
                        .zip(argument_type_oids)
                        .all(|(argument, oid)| {
                            self.routine_argument_oid(argument, txid)
                                .is_some_and(|expected| {
                                    self.routine_implicit_cast(*oid, expected, txid)
                                })
                        }))
                .then_some(slot)
            });
        let mut concrete = concrete;
        let first = concrete.next();
        if first.is_some() && concrete.next().is_none() {
            return first;
        }
        let polymorphic = self
            .routines
            .iter()
            .enumerate()
            .filter_map(|(slot, routine)| {
                let definition = routine.definition_for(txid);
                (routine.visible_to(txid)
                    && kind.accepts(definition.kind)
                    && definition.schema_for(txid).as_str() == schema
                    && definition.name_for(txid).as_str() == name
                    && definition.accepts_input_arity(argument_type_oids.len())
                    && self
                        .polymorphic_call_binding(
                            &definition.arguments()[..argument_type_oids.len()],
                            argument_type_oids,
                            txid,
                        )
                        .is_some())
                .then_some(slot)
            });
        let mut polymorphic = polymorphic;
        let first = polymorphic.next();
        if first.is_some() && polymorphic.next().is_none() {
            first
        } else {
            None
        }
    }

    fn routine_slot_in_named_oids(
        &self,
        schema: &str,
        name: &str,
        argument_names: &[Option<&str>],
        argument_type_oids: &[i32],
        txid: u32,
        call_kind: RoutineCallKind,
    ) -> Option<usize> {
        let matches = |routine: &RoutineDef, implicit: bool, polymorphic: bool| {
            let definition = routine.definition_for(txid);
            if !routine.visible_to(txid)
                || !call_kind.accepts(definition.kind)
                || definition.schema_for(txid).as_str() != schema
                || definition.name_for(txid).as_str() != name
            {
                return false;
            }
            let Some(mapping) =
                definition.call_input_mapping(argument_names, argument_type_oids.len(), false)
            else {
                return false;
            };
            let mut binding = PolymorphicBinding::EMPTY;
            let mut saw_polymorphic = false;
            for (call_index, actual_oid) in argument_type_oids.iter().enumerate() {
                let argument = definition.arguments[usize::from(mapping[call_index])];
                if let Some(kind) = argument.polymorphic_type() {
                    if !polymorphic || binding.bind(kind, *actual_oid).is_none() {
                        return false;
                    }
                    saw_polymorphic = true;
                } else {
                    let Some(expected) = self.routine_argument_oid(&argument, txid) else {
                        return false;
                    };
                    if *actual_oid != expected
                        && (!implicit || !self.routine_implicit_cast(*actual_oid, expected, txid))
                    {
                        return false;
                    }
                }
            }
            polymorphic == saw_polymorphic
        };
        for (implicit, polymorphic) in [(false, false), (true, false), (true, true)] {
            let mut candidates = self
                .routines
                .iter()
                .enumerate()
                .filter_map(|(slot, routine)| {
                    matches(routine, implicit, polymorphic).then_some(slot)
                });
            let first = candidates.next();
            if first.is_some() && candidates.next().is_none() {
                return first;
            }
        }
        None
    }

    fn polymorphic_call_binding(
        &self,
        arguments: &[RoutineArgumentDef],
        argument_type_oids: &[i32],
        txid: u32,
    ) -> Option<PolymorphicBinding> {
        let mut binding = PolymorphicBinding::EMPTY;
        let mut saw_polymorphic = false;
        for (argument, actual_oid) in arguments.iter().zip(argument_type_oids) {
            let Some(polymorphic) = argument.polymorphic_type() else {
                if !self
                    .routine_argument_oid(argument, txid)
                    .is_some_and(|expected| self.routine_implicit_cast(*actual_oid, expected, txid))
                {
                    return None;
                }
                continue;
            };
            saw_polymorphic = true;
            binding.bind(polymorphic, *actual_oid)?;
        }
        saw_polymorphic.then_some(binding)
    }

    fn routine_argument_oid(&self, argument: &RoutineArgumentDef, txid: u32) -> Option<i32> {
        if let Some(polymorphic) = argument.polymorphic_type() {
            return Some(polymorphic.oid());
        }
        self.routine_type_oid(argument.ctype, argument.user_type, txid)
    }

    pub(crate) fn routine_type_oid(
        &self,
        ctype: ColType,
        user_type: Option<UserTypeName>,
        txid: u32,
    ) -> Option<i32> {
        use crate::sql::types::{ArrElem, oid};

        if let Some(polymorphic) = polymorphic_type(ctype, user_type) {
            return Some(polymorphic.oid());
        }

        match ctype {
            ColType::Enum(slot) => return Some(oid::enum_oid(slot)),
            ColType::Composite(slot) => return Some(oid::composite_oid(slot)),
            ColType::Array(ArrElem::Enum(slot)) => return Some(oid::enum_array_oid(slot)),
            ColType::Array(ArrElem::Composite(slot)) => {
                return Some(oid::composite_array_oid(slot));
            }
            ColType::Array(ArrElem::Domain { slot, .. }) => {
                return Some(oid::domain_array_oid(slot));
            }
            _ => {}
        }
        let Some(identity) = user_type else {
            return Some(ctype.oid());
        };
        let array = matches!(ctype, ColType::Array(_));
        self.user_type_identity_oid(identity, array, txid)
    }

    pub(crate) fn routine_result_for_oid(&self, type_oid: i32, txid: u32) -> Option<RoutineResult> {
        use crate::sql::types::{ArrElem, ColType, oid};

        let identity = |schema: SqlName, name: SqlName| Some(UserTypeName { schema, name });
        if (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + MAX_DOMAINS as i32).contains(&type_oid) {
            let slot = usize::try_from(type_oid - oid::FIRST_DOMAIN).ok()?;
            let domain = self.domain_for(slot, txid);
            return domain.visible_to(txid).then_some(RoutineResult {
                ctype: domain.base,
                user_type: identity(domain.schema, domain.name),
            });
        }
        if (oid::FIRST_DOMAIN_ARRAY..oid::FIRST_DOMAIN_ARRAY + MAX_DOMAINS as i32)
            .contains(&type_oid)
        {
            let slot = usize::try_from(type_oid - oid::FIRST_DOMAIN_ARRAY).ok()?;
            let domain = self.domain_for(slot, txid);
            return domain.visible_to(txid).then_some(RoutineResult {
                ctype: ColType::Array(ArrElem::domain(slot as u16, domain.base)?),
                user_type: identity(domain.schema, domain.name),
            });
        }
        if (oid::FIRST_ENUM..oid::FIRST_ENUM + MAX_ENUMS as i32).contains(&type_oid) {
            let slot = usize::try_from(type_oid - oid::FIRST_ENUM).ok()?;
            let enumeration = self.enum_for(slot, txid);
            return enumeration.visible_to(txid).then_some(RoutineResult {
                ctype: ColType::Enum(slot as u16),
                user_type: identity(enumeration.schema, enumeration.name),
            });
        }
        if (oid::FIRST_ENUM_ARRAY..oid::FIRST_ENUM_ARRAY + MAX_ENUMS as i32).contains(&type_oid) {
            let slot = usize::try_from(type_oid - oid::FIRST_ENUM_ARRAY).ok()?;
            let enumeration = self.enum_for(slot, txid);
            return enumeration.visible_to(txid).then_some(RoutineResult {
                ctype: ColType::Array(ArrElem::Enum(slot as u16)),
                user_type: identity(enumeration.schema, enumeration.name),
            });
        }
        if (oid::FIRST_COMPOSITE..oid::FIRST_COMPOSITE + MAX_COMPOSITES as i32).contains(&type_oid)
        {
            let slot = usize::try_from(type_oid - oid::FIRST_COMPOSITE).ok()?;
            let composite = self.composite_for(slot, txid);
            return composite.visible_to(txid).then_some(RoutineResult {
                ctype: ColType::Composite(slot as u16),
                user_type: identity(composite.schema, composite.name),
            });
        }
        if (oid::FIRST_COMPOSITE_ARRAY..oid::FIRST_COMPOSITE_ARRAY + MAX_COMPOSITES as i32)
            .contains(&type_oid)
        {
            let slot = usize::try_from(type_oid - oid::FIRST_COMPOSITE_ARRAY).ok()?;
            let composite = self.composite_for(slot, txid);
            return composite.visible_to(txid).then_some(RoutineResult {
                ctype: ColType::Array(ArrElem::Composite(slot as u16)),
                user_type: identity(composite.schema, composite.name),
            });
        }
        ColType::from_oid(type_oid).map(RoutineResult::builtin)
    }

    /// Whether PostgreSQL permits `actual_oid` to enter a routine parameter
    /// declared as `expected_oid` without an explicit cast. Routine overload
    /// resolution consumes this relation; execution then performs the cast at
    /// the declared-parameter boundary.
    pub(crate) fn routine_implicit_cast(
        &self,
        actual_oid: i32,
        expected_oid: i32,
        txid: u32,
    ) -> bool {
        use crate::sql::types::{ColType, oid};

        if actual_oid == expected_oid || actual_oid == oid::UNKNOWN {
            return true;
        }

        // A domain is implicitly treated as its base type when passed out of
        // the domain, but PostgreSQL does not implicitly manufacture a value
        // of a different domain at routine lookup.
        let expected_is_domain =
            (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + MAX_DOMAINS as i32).contains(&expected_oid);
        if expected_is_domain {
            return false;
        }
        let actual = match self.routine_result_for_oid(actual_oid, txid) {
            Some(actual) => actual,
            None => return false,
        };
        let expected = match self.routine_result_for_oid(expected_oid, txid) {
            Some(expected) => expected,
            None => return false,
        };
        if actual.user_type.is_some() && actual.ctype == expected.ctype {
            return true;
        }
        if let (ColType::Array(actual), ColType::Array(expected)) = (actual.ctype, expected.ctype) {
            return self.routine_implicit_cast(actual.element_oid(), expected.element_oid(), txid);
        }

        matches!(
            (actual_oid, expected_oid),
            // Numeric widening casts from pg_cast.castcontext = 'i'.
            (oid::INT2, oid::INT4 | oid::INT8 | oid::NUMERIC | oid::FLOAT4 | oid::FLOAT8)
                | (oid::INT4, oid::INT8 | oid::NUMERIC | oid::FLOAT4 | oid::FLOAT8)
                | (oid::INT8, oid::NUMERIC | oid::FLOAT4 | oid::FLOAT8)
                | (oid::NUMERIC, oid::FLOAT4 | oid::FLOAT8)
                | (oid::FLOAT4, oid::FLOAT8)
                // PostgreSQL string-category binary and function casts.
                | (oid::BPCHAR, oid::VARCHAR | oid::NAME | oid::TEXT)
                | (oid::VARCHAR, oid::BPCHAR | oid::NAME | oid::TEXT | oid::REGCLASS)
                | (oid::TEXT, oid::BPCHAR | oid::VARCHAR | oid::NAME | oid::REGCLASS)
                | (oid::NAME, oid::TEXT)
                | (oid::BIT, oid::VARBIT)
                | (oid::VARBIT, oid::BIT)
                // Date/time and network casts accepted implicitly by PG18.
                | (oid::DATE, oid::TIMESTAMP | oid::TIMESTAMPTZ)
                | (oid::TIMESTAMP, oid::TIMESTAMPTZ)
                | (oid::TIME, oid::TIMETZ | oid::INTERVAL)
                | (oid::CIDR, oid::INET)
                | (oid::MACADDR, oid::MACADDR8)
                | (oid::MACADDR8, oid::MACADDR)
                // OID alias types are binary-coercible in both directions;
                // integer casts to OID aliases follow PostgreSQL's pg_cast.
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::OID)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGPROC)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGPROCEDURE)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGOPER)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGOPERATOR)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGCLASS)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGTYPE)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGNAMESPACE)
                | (oid::INT2 | oid::INT4 | oid::INT8, oid::REGROLE)
                | (oid::OID, oid::REGPROC | oid::REGPROCEDURE | oid::REGOPER | oid::REGOPERATOR | oid::REGCLASS | oid::REGTYPE | oid::REGNAMESPACE | oid::REGROLE)
                | (oid::REGPROC | oid::REGPROCEDURE | oid::REGOPER | oid::REGOPERATOR | oid::REGCLASS | oid::REGTYPE | oid::REGNAMESPACE | oid::REGROLE, oid::OID)
                | (oid::REGPROC, oid::REGPROCEDURE)
                | (oid::REGPROCEDURE, oid::REGPROC)
                | (oid::REGOPER, oid::REGOPERATOR)
                | (oid::REGOPERATOR, oid::REGOPER)
        )
    }

    pub(crate) fn user_type_identity_oid(
        &self,
        identity: UserTypeName,
        array: bool,
        txid: u32,
    ) -> Option<i32> {
        use crate::sql::types::oid;

        if let Some(slot) = self.domain_slot(identity.schema.as_str(), identity.name.as_str(), txid)
        {
            return Some(if array {
                oid::domain_array_oid(slot as u16)
            } else {
                oid::domain_oid(slot as u16)
            });
        }
        if let Some(slot) = self.enum_slot(identity.schema.as_str(), identity.name.as_str(), txid) {
            return Some(if array {
                oid::enum_array_oid(slot as u16)
            } else {
                oid::enum_oid(slot as u16)
            });
        }
        self.composite_slot(identity.schema.as_str(), identity.name.as_str(), txid)
            .map(|slot| {
                if array {
                    oid::composite_array_oid(slot as u16)
                } else {
                    oid::composite_oid(slot as u16)
                }
            })
    }

    pub(crate) fn routine_slot_by_signature(
        &self,
        schema: &str,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
    ) -> Option<usize> {
        self.routines.iter().position(|routine| {
            let definition = routine.definition_for(txid);
            routine.visible_to(txid)
                && definition.schema_for(txid).as_str() == schema
                && definition.name_for(txid).as_str() == name
                && definition.argument_count == argument_types.len()
                && definition
                    .arguments()
                    .iter()
                    .zip(argument_types)
                    .all(|(argument, ctype)| argument.ctype == *ctype)
        })
    }

    pub(crate) fn routine_slot_by_declared_signature(
        &self,
        schema: &str,
        name: &str,
        arguments: &[RoutineArgumentDef],
        txid: u32,
    ) -> Option<usize> {
        self.routines.iter().position(|routine| {
            let definition = routine.definition_for(txid);
            routine.visible_to(txid)
                && definition.schema_for(txid).as_str() == schema
                && definition.name_for(txid).as_str() == name
                && definition.argument_count == arguments.len()
                && definition
                    .arguments()
                    .iter()
                    .zip(arguments)
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    })
        })
    }

    /// Resolves PostgreSQL's omitted-argument `DROP FUNCTION name` form.  It
    /// is valid only when exactly one visible routine has that identity.
    pub(crate) fn routine_slot_by_name_unambiguous(
        &self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, ()> {
        let mut found = None;
        for (slot, routine) in self.routines.iter().enumerate() {
            if !routine.visible_to(txid)
                || routine.schema_for(txid).as_str() != schema
                || routine.name_for(txid).as_str() != name
            {
                continue;
            }
            if found.replace(slot).is_some() {
                return Err(());
            }
        }
        Ok(found)
    }

    pub(crate) fn alter_routine_identity(
        &mut self,
        slot: usize,
        schema: SqlName,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingRoutineIdentity>, SqlError> {
        let routine = self.routines[slot];
        if let Some(pending) = routine.pending_identity
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, routine.name.as_str()));
        }
        if self.routines.iter().enumerate().any(|(other, candidate)| {
            other != slot
                && candidate.visible_to(txid)
                && candidate.schema_for(txid) == schema
                && candidate.name_for(txid) == name
                && candidate.argument_count == routine.argument_count
                && candidate
                    .arguments()
                    .iter()
                    .zip(routine.arguments())
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    })
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_FUNCTION,
                "routine \"{}\" already exists with same argument types",
                name.as_str()
            ));
        }
        if let Some(blocker) = self
            .routines
            .iter()
            .enumerate()
            .find_map(|(other, candidate)| {
                (other != slot)
                    .then_some(candidate.pending_identity)
                    .flatten()
                    .filter(|pending| {
                        pending.txid != txid
                            && pending.schema == schema
                            && pending.name == name
                            && candidate.argument_count == routine.argument_count
                            && candidate.arguments().iter().zip(routine.arguments()).all(
                                |(left, right)| {
                                    left.ctype == right.ctype && left.user_type == right.user_type
                                },
                            )
                    })
                    .map(|pending| pending.txid)
            })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let prior = self.routines[slot].pending_identity;
        self.routines[slot].pending_identity = Some(PendingRoutineIdentity { txid, schema, name });
        Ok(prior)
    }

    pub(crate) fn commit_routine_identity(&mut self, slot: usize, txid: u32) {
        let changed = {
            let routine = &mut self.routines[slot];
            if let Some(pending) = routine.pending_identity
                && pending.txid == txid
            {
                routine.schema = pending.schema;
                routine.name = pending.name;
                routine.pending_identity = None;
                Some((pending.schema, pending.name))
            } else {
                None
            }
        };
        if let Some((schema, name)) = changed {
            self.rename_stored_query_dependency(DependencyClass::Routine, slot, schema, name);
        }
    }

    pub(crate) fn restore_routine_identity(
        &mut self,
        slot: usize,
        prior: Option<PendingRoutineIdentity>,
    ) {
        self.routines[slot].pending_identity = prior;
    }

    pub(crate) const fn routine_access_object(slot: usize) -> AccessObject {
        AccessObject {
            class: AccessClass::Routine,
            slot: slot as u16,
        }
    }

    pub(crate) fn routine_dependencies_for(
        &self,
        slot: usize,
        txid: u32,
    ) -> &StoredQueryDependencies {
        if let Some(pending) = self.routines[slot]
            .pending_definition
            .filter(|pending| pending.txid == txid)
        {
            return &self.pending_routine_dependencies[pending.dependency_slot as usize]
                .dependencies;
        }
        &self.routine_dependencies[slot]
    }

    fn allocate_pending_routine_dependencies(
        &mut self,
        routine: usize,
        txid: u32,
        dependencies: StoredQueryDependencies,
    ) -> Result<u32, SqlError> {
        let Some(slot) = self
            .pending_routine_dependencies
            .iter()
            .position(|pending| !pending.used)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many pending routine definitions"
            ));
        };
        self.pending_routine_dependencies[slot] = PendingRoutineDependencies {
            used: true,
            txid,
            routine: routine as u16,
            dependencies,
        };
        Ok(slot as u32)
    }

    pub(crate) fn create_routine(
        &mut self,
        spec: RoutineSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        let RoutineSpec {
            identity,
            schema,
            name,
            arguments,
            argument_count,
            parameters,
            parameter_count,
            kind,
            result_columns,
            result_column_count,
            language,
            attributes,
            configs,
            config_count,
            body_kind,
            body,
            creation_path,
            dependencies,
        } = spec;
        if config_count > configs.len()
            || configs[..config_count]
                .iter()
                .enumerate()
                .any(|(index, config)| {
                    configs[..index]
                        .iter()
                        .any(|prior| prior.name == config.name)
                })
        {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "routine configuration contract is invalid"
            ));
        }
        if parameter_count > parameters.len()
            || parameters[..parameter_count]
                .iter()
                .filter(|parameter| parameter.mode.is_input())
                .count()
                != argument_count
        {
            return Err(sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "routine parameter contract does not match its input signature"
            ));
        }
        let output_parameter_count = parameters[..parameter_count]
            .iter()
            .filter(|parameter| parameter.mode.is_output())
            .count();
        let mut saw_default = false;
        let mut saw_variadic = false;
        for parameter in &parameters[..parameter_count] {
            if saw_variadic && !matches!(parameter.mode, RoutineParameterMode::Out) {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "VARIADIC parameter must be the last input parameter"
                ));
            }
            if parameter.mode.is_input() {
                if parameter.mode.default().is_some() {
                    saw_default = true;
                } else if saw_default {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "input parameters after one with a default value must also have defaults"
                    ));
                }
            }
            saw_variadic |= matches!(parameter.mode, RoutineParameterMode::Variadic { .. });
        }
        match kind {
            RoutineKind::TableFunction => {
                if result_column_count == 0 || result_column_count > result_columns.len() {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "record-returning function must have between one and {} result columns",
                        result_columns.len()
                    ));
                }
                if result_columns[..result_column_count]
                    .iter()
                    .enumerate()
                    .any(|(index, column)| {
                        result_columns[..index]
                            .iter()
                            .any(|prior| prior.name == column.name)
                    })
                {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "routine result column names must be distinct"
                    ));
                }
            }
            RoutineKind::RecordFunction { .. }
                if result_column_count < 2
                    || result_column_count != output_parameter_count
                    || result_column_count > result_columns.len() =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "record function result contract does not match its output parameters"
                ));
            }
            RoutineKind::RecordFunction { .. } => {}
            RoutineKind::Function { .. } | RoutineKind::SetFunction { .. }
                if result_column_count != 0
                    && (result_column_count != 1 || output_parameter_count != 1) =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "scalar function result contract does not match its output parameter"
                ));
            }
            RoutineKind::Function { .. } | RoutineKind::SetFunction { .. } => {}
            RoutineKind::Trigger | RoutineKind::Procedure | RoutineKind::Aggregate(_)
                if result_column_count != 0 =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "routine kind cannot define result columns"
                ));
            }
            RoutineKind::Trigger | RoutineKind::Procedure | RoutineKind::Aggregate(_) => {}
        }
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.routines.iter().find_map(|routine| {
            (routine.schema_for(txid) == schema
                && routine.name_for(txid) == name
                && routine.argument_count == argument_count
                && routine.arguments()[..argument_count]
                    .iter()
                    .zip(&arguments[..argument_count])
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    }))
            .then_some(routine.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.routines.iter().any(|routine| {
            routine.visible_to(txid)
                && routine.schema_for(txid) == schema
                && routine.name_for(txid) == name
                && routine.argument_count == argument_count
                && routine.arguments()[..argument_count]
                    .iter()
                    .zip(&arguments[..argument_count])
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    })
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_FUNCTION,
                "function \"{}\" already exists with same argument types",
                name.as_str()
            ));
        }
        let Some(slot) = self
            .routines
            .iter()
            .position(|routine| routine.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many routines (limit {})",
                self.routines.len()
            ));
        };
        self.clear_object_acl_entries(Self::routine_access_object(slot));
        let (created_at, ownership) = match identity {
            RoutineIdentity::Allocate => {
                self.catalog_seq += 1;
                (self.catalog_seq, self.initial_ownership(txid))
            }
            RoutineIdentity::Preserve {
                created_at,
                ownership,
            } => {
                self.catalog_seq = self.catalog_seq.max(created_at);
                (created_at, ownership)
            }
        };
        self.routines[slot] = RoutineDef {
            created_at,
            schema,
            name,
            pending_identity: None,
            pending_definition: None,
            arguments,
            argument_count,
            parameters,
            parameter_count,
            kind,
            result_columns,
            result_column_count,
            language,
            attributes,
            configs,
            config_count,
            body_kind,
            body,
            creation_path,
            ownership,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        self.routine_dependencies[slot] = dependencies;
        Ok(slot)
    }

    pub(crate) fn commit_routine_create(&mut self, slot: usize, txid: u32) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.commit_create();
        let ownership = &mut self.routines[slot].ownership;
        if let Some(pending) = ownership.pending
            && pending.txid == txid
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn rollback_routine_create(&mut self, slot: usize) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.rollback_create();
        self.routine_dependencies[slot] = StoredQueryDependencies::EMPTY;
    }

    pub(crate) fn replace_routine(
        &mut self,
        slot: usize,
        mut definition: PendingRoutineDefinition,
        dependencies: StoredQueryDependencies,
    ) -> Result<Option<PendingRoutineDefinition>, SqlError> {
        let name = self.routines[slot].name;
        if let Some(pending) = self.routines[slot].pending_definition
            && pending.txid != definition.txid
        {
            return Err(self.catalog_ddl_wait_error(definition.txid, pending.txid, name.as_str()));
        }
        definition.dependency_slot =
            self.allocate_pending_routine_dependencies(slot, definition.txid, dependencies)?;
        let routine = &mut self.routines[slot];
        let prior = routine.pending_definition;
        routine.pending_definition = Some(definition);
        Ok(prior)
    }

    pub(crate) fn commit_routine_replace(&mut self, slot: usize, txid: u32) {
        if let Some(pending) = self.routines[slot].pending_definition
            && pending.txid == txid
        {
            self.routine_dependencies[slot] =
                self.pending_routine_dependencies[pending.dependency_slot as usize].dependencies;
            let routine = &mut self.routines[slot];
            routine.arguments = pending.arguments;
            routine.argument_count = pending.argument_count;
            routine.parameters = pending.parameters;
            routine.parameter_count = pending.parameter_count;
            routine.kind = pending.kind;
            routine.result_columns = pending.result_columns;
            routine.result_column_count = pending.result_column_count;
            routine.language = pending.language;
            routine.attributes = pending.attributes;
            routine.configs = pending.configs;
            routine.config_count = pending.config_count;
            routine.body_kind = pending.body_kind;
            routine.body = pending.body;
            routine.creation_path = pending.creation_path;
            routine.pending_definition = None;
            for dependency in self.pending_routine_dependencies.iter_mut() {
                if dependency.used
                    && dependency.txid == txid
                    && usize::from(dependency.routine) == slot
                {
                    *dependency = PendingRoutineDependencies::EMPTY;
                }
            }
        }
    }

    pub(crate) fn rollback_routine_replace(
        &mut self,
        slot: usize,
        prior: Option<PendingRoutineDefinition>,
    ) {
        if let Some(current) = self.routines[slot].pending_definition {
            self.pending_routine_dependencies[current.dependency_slot as usize] =
                PendingRoutineDependencies::EMPTY;
        }
        self.routines[slot].pending_definition = prior;
    }

    pub(crate) fn drop_routine(&mut self, slot: usize, txid: u32) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn commit_routine_drop(&mut self, slot: usize) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.commit_drop();
        let object = Self::routine_access_object(slot);
        self.clear_object_acl_entries(object);
        self.clear_extension_dependencies_for_object(object);
        self.routine_dependencies[slot] = StoredQueryDependencies::EMPTY;
        for dependency in self.pending_routine_dependencies.iter_mut() {
            if dependency.used && usize::from(dependency.routine) == slot {
                *dependency = PendingRoutineDependencies::EMPTY;
            }
        }
    }

    pub(crate) fn rollback_routine_drop(&mut self, slot: usize, txid: u32) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn policy(&self, slot: usize) -> &PolicyDef {
        &self.policies[slot]
    }

    pub(crate) fn policy_count(&self) -> usize {
        self.policies.len()
    }

    pub(crate) fn policies_for_table(
        &self,
        table: usize,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &PolicyDef)> {
        self.policies.iter().enumerate().filter(move |(_, policy)| {
            policy.visible_to(txid) && usize::from(policy.table) == table
        })
    }

    pub(crate) fn policies_with_slots_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &PolicyDef)> {
        self.policies
            .iter()
            .enumerate()
            .filter(move |(_, policy)| policy.visible_to(txid))
    }

    /// Whether this role is subject to the table's row-security policies.
    /// Superusers and BYPASSRLS roles always bypass; owners bypass unless the
    /// table is forced. Disabled row security never consults policies.
    pub(crate) fn row_security_applies(&self, table: usize, role: usize, txid: u32) -> bool {
        let state = self.table_def(table, txid).row_level_security;
        if !state.enabled {
            return false;
        }
        let attributes = self.role(role).attributes_to(txid);
        if attributes.superuser || attributes.bypass_row_level_security {
            return false;
        }
        state.forced || self.object_owner(self.table_access_object(table, txid), txid) != role
    }

    pub(crate) fn policy_slot_on(&self, table: usize, name: &str, txid: u32) -> Option<usize> {
        self.policies_for_table(table, txid)
            .find_map(|(slot, policy)| (policy.name.as_str() == name).then_some(slot))
    }

    pub(crate) fn create_policy(&mut self, spec: PolicySpec, txid: u32) -> Result<usize, SqlError> {
        if spec.table >= self.tables.len()
            || spec.definition.roles.entries().is_empty()
            || (matches!(spec.command, PolicyCommandKind::Insert)
                && spec.definition.using.is_some())
            || (matches!(
                spec.command,
                PolicyCommandKind::Select | PolicyCommandKind::Delete
            ) && spec.definition.with_check.is_some())
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid row-security policy definition"
            ));
        }
        if self
            .policy_slot_on(spec.table, spec.name.as_str(), txid)
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "policy \"{}\" for table already exists",
                spec.name.as_str()
            ));
        }
        if self.policies_for_table(spec.table, txid).count() == MAX_POLICIES_PER_TABLE {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table has too many policies (limit {})",
                MAX_POLICIES_PER_TABLE
            ));
        }
        let Some(slot) = self
            .policies
            .iter()
            .position(|policy| policy.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row-security policy catalog is full (limit {})",
                self.policies.len()
            ));
        };
        self.catalog_seq += 1;
        self.policies[slot] = PolicyDef {
            created_at: self.catalog_seq,
            name: spec.name,
            table: u16::try_from(spec.table).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table slot exceeds policy catalog capacity"
                )
            })?,
            command: spec.command,
            permissive: spec.permissive,
            definition: spec.definition,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub(crate) fn alter_policy(
        &mut self,
        slot: usize,
        definition: PolicyDefinition,
        txid: u32,
    ) -> Result<Option<PendingPolicyDefinition>, SqlError> {
        if matches!(self.policies[slot].command, PolicyCommandKind::Insert)
            && definition.using.is_some()
            || matches!(
                self.policies[slot].command,
                PolicyCommandKind::Select | PolicyCommandKind::Delete
            ) && definition.with_check.is_some()
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "policy expression is not valid for its command"
            ));
        }
        if let Some(pending) = self.policies[slot].pending_definition
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.policies[slot].name.as_str(),
            ));
        }
        let prior = self.policies[slot].pending_definition;
        self.policies[slot].pending_definition = Some(PendingPolicyDefinition { txid, definition });
        Ok(prior)
    }

    pub(crate) fn drop_policy(&mut self, slot: usize, txid: u32) {
        self.policies[slot].ddl_state = self.policies[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn commit_policy_create(&mut self, slot: usize) {
        self.policies[slot].ddl_state = self.policies[slot].ddl_state.commit_create();
    }

    pub(crate) fn rollback_policy_create(&mut self, slot: usize) {
        self.policies[slot].ddl_state = self.policies[slot].ddl_state.rollback_create();
    }

    pub(crate) fn commit_policy_alter(&mut self, slot: usize, txid: u32) {
        let policy = &mut self.policies[slot];
        if let Some(pending) = policy.pending_definition
            && pending.txid == txid
        {
            policy.definition = pending.definition;
            policy.pending_definition = None;
        }
    }

    pub(crate) fn rollback_policy_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingPolicyDefinition>,
    ) {
        self.policies[slot].pending_definition = prior;
    }

    pub(crate) fn commit_policy_drop(&mut self, slot: usize) {
        self.policies[slot].ddl_state = self.policies[slot].ddl_state.commit_drop();
        self.policies[slot].pending_definition = None;
    }

    pub(crate) fn rollback_policy_drop(&mut self, slot: usize, txid: u32) {
        self.policies[slot].ddl_state = self.policies[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn commit_policies_for_table(&mut self, table: usize) {
        for policy in self.policies.iter_mut() {
            if policy.ddl_state != CatalogDdlState::Absent && usize::from(policy.table) == table {
                policy.ddl_state = CatalogDdlState::Absent;
                policy.pending_definition = None;
            }
        }
    }

    pub(crate) fn replay_set_policy(&mut self, spec: PolicySpec) -> Result<(), SqlError> {
        if let Some(slot) = self.policy_slot_on(spec.table, spec.name.as_str(), 0) {
            let policy = &mut self.policies[slot];
            policy.command = spec.command;
            policy.permissive = spec.permissive;
            policy.definition = spec.definition;
            policy.pending_definition = None;
            return Ok(());
        }
        let slot = self.create_policy(spec, 0)?;
        self.commit_policy_create(slot);
        Ok(())
    }

    pub(crate) fn replay_drop_policy(&mut self, table: usize, name: &str) {
        if let Some(slot) = self.policy_slot_on(table, name, 0) {
            self.drop_policy(slot, 0);
            self.commit_policy_drop(slot);
        }
    }

    pub(crate) fn restore_policy(
        &mut self,
        created_at: u64,
        spec: PolicySpec,
    ) -> Result<(), SqlError> {
        self.replay_set_policy(spec)?;
        let slot = self
            .policy_slot_on(spec.table, spec.name.as_str(), 0)
            .expect("restored policy is installed");
        self.policies[slot].created_at = created_at;
        self.catalog_seq = self.catalog_seq.max(created_at);
        Ok(())
    }

    pub(crate) fn triggers_for_table(
        &self,
        table: usize,
        txid: u32,
    ) -> impl Iterator<Item = (usize, TriggerDef)> + '_ {
        self.triggers
            .iter()
            .enumerate()
            .filter(move |(_, trigger)| {
                trigger.visible_to(txid) && trigger.target == TriggerTarget::Table(table as u16)
            })
            .map(move |(slot, trigger)| (slot, trigger.effective_to(txid)))
    }

    pub(crate) fn triggers_for_view(
        &self,
        view: usize,
        txid: u32,
    ) -> impl Iterator<Item = (usize, TriggerDef)> + '_ {
        self.triggers
            .iter()
            .enumerate()
            .filter(move |(_, trigger)| {
                trigger.visible_to(txid) && trigger.target == TriggerTarget::View(view as u16)
            })
            .map(move |(slot, trigger)| (slot, trigger.effective_to(txid)))
    }

    pub(crate) fn triggers_for_target(
        &self,
        target: TriggerTarget,
        txid: u32,
    ) -> impl Iterator<Item = (usize, TriggerDef)> + '_ {
        self.triggers_with_slots_visible_to(txid)
            .filter(move |(_, trigger)| trigger.target == target)
    }

    pub(crate) fn triggers_with_slots_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, TriggerDef)> + '_ {
        self.triggers
            .iter()
            .enumerate()
            .filter(move |(_, trigger)| trigger.visible_to(txid))
            .map(move |(slot, trigger)| (slot, trigger.effective_to(txid)))
    }

    pub(crate) fn trigger(&self, slot: usize) -> &TriggerDef {
        &self.triggers[slot]
    }

    pub(crate) fn trigger_to(&self, slot: usize, txid: u32) -> TriggerDef {
        self.triggers[slot].effective_to(txid)
    }

    pub(crate) fn trigger_slot_on(
        &self,
        target: TriggerTarget,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        self.triggers_for_target(target, txid)
            .find_map(|(slot, trigger)| (trigger.name_to(txid).as_str() == name).then_some(slot))
    }

    pub(crate) fn trigger_slot_inherited_by(
        &self,
        table: usize,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        self.triggers_with_slots_visible_to(txid)
            .find_map(|(slot, trigger)| {
                let TriggerTarget::Table(parent) = trigger.target else {
                    return None;
                };
                (matches!(trigger.level, crate::sql::ast::TriggerLevel::Row)
                    && trigger.name_to(txid).as_str() == name
                    && self.partition_descends_from(table, usize::from(parent), txid))
                .then_some(slot)
            })
    }

    pub(crate) fn partition_trigger_enabled_to(
        &self,
        trigger: usize,
        table: usize,
        txid: u32,
    ) -> TriggerEnabled {
        self.partition_trigger_states
            .iter()
            .find(|state| state.trigger as usize == trigger && state.table as usize == table)
            .and_then(|state| state.enabled_to(txid))
            .unwrap_or_else(|| self.trigger_to(trigger, txid).enabled_to(txid))
    }

    pub(crate) fn stage_partition_trigger_enabled(
        &mut self,
        trigger: usize,
        table: usize,
        enabled: TriggerEnabled,
        txid: u32,
    ) -> Result<Option<(usize, PartitionTriggerState)>, SqlError> {
        let existing = self.partition_trigger_states.iter().position(|state| {
            state.trigger as usize == trigger
                && state.table as usize == table
                && (state.present || state.pending.is_some())
        });
        let slot = existing.or_else(|| {
            self.partition_trigger_states
                .iter()
                .position(|state| !state.present && state.pending.is_none())
        });
        let Some(slot) = slot else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "partition trigger state exceeds the startup catalog capacity"
            ));
        };
        let prior = self.partition_trigger_states[slot];
        if let Some(pending) = prior.pending
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.trigger(trigger).name.as_str(),
            ));
        }
        if prior.enabled_to(txid) == Some(enabled) {
            return Ok(None);
        }
        self.partition_trigger_states[slot] = PartitionTriggerState {
            trigger: trigger as u16,
            table: table as u16,
            enabled: prior.enabled,
            present: prior.present,
            pending: Some(PendingPartitionTriggerState { txid, enabled }),
        };
        Ok(Some((slot, prior)))
    }

    pub(crate) fn commit_partition_trigger_state(&mut self, slot: usize, txid: u32) {
        let state = &mut self.partition_trigger_states[slot];
        if let Some(pending) = state.pending
            && pending.txid == txid
        {
            state.enabled = pending.enabled;
            state.present = true;
            state.pending = None;
        }
    }

    pub(crate) fn rollback_partition_trigger_state(
        &mut self,
        slot: usize,
        prior: PartitionTriggerState,
    ) {
        self.partition_trigger_states[slot] = prior;
    }

    pub(crate) fn restore_partition_trigger_state(
        &mut self,
        trigger: usize,
        table: usize,
        enabled: TriggerEnabled,
    ) -> Result<(), SqlError> {
        if self.partition_trigger_states.iter().any(|state| {
            state.present && state.trigger as usize == trigger && state.table as usize == table
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "duplicate partition trigger state in checkpoint"
            ));
        }
        let Some(slot) = self
            .partition_trigger_states
            .iter()
            .position(|state| !state.present && state.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many partition trigger states in checkpoint"
            ));
        };
        self.partition_trigger_states[slot] = PartitionTriggerState {
            trigger: trigger as u16,
            table: table as u16,
            enabled,
            present: true,
            pending: None,
        };
        Ok(())
    }

    pub(crate) fn partition_trigger_states(
        &self,
    ) -> impl Iterator<Item = (usize, usize, TriggerEnabled)> + '_ {
        self.partition_trigger_states.iter().filter_map(|state| {
            state.present.then_some((
                usize::from(state.trigger),
                usize::from(state.table),
                state.enabled,
            ))
        })
    }

    pub(crate) const fn trigger_access_object(slot: usize) -> AccessObject {
        AccessObject {
            class: AccessClass::Trigger,
            slot: slot as u16,
        }
    }

    pub(crate) fn create_trigger(
        &mut self,
        spec: TriggerSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if !spec.is_valid() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid trigger definition"
            ));
        }
        if self
            .triggers_with_slots_visible_to(txid)
            .find_map(|(slot, trigger)| {
                (trigger.target == spec.target && trigger.name_to(txid) == spec.name)
                    .then_some(slot)
            })
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "trigger \"{}\" for relation already exists",
                spec.name.as_str()
            ));
        }
        let Some(slot) = self
            .triggers
            .iter()
            .position(|trigger| trigger.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many triggers (limit {})",
                self.triggers.len()
            ));
        };
        self.catalog_seq += 1;
        self.triggers[slot] = TriggerDef {
            created_at: self.catalog_seq,
            name: spec.name,
            target: spec.target,
            kind: spec.kind,
            function: u16::try_from(spec.function).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "routine slot exceeds trigger capacity"
                )
            })?,
            timing: spec.timing,
            level: spec.level,
            events: spec.events,
            update_columns: spec.update_columns,
            transition_tables: spec.transition_tables,
            when: spec.when,
            arguments: spec.arguments,
            enabled: TriggerEnabled::Origin,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub(crate) fn drop_trigger(&mut self, slot: usize, txid: u32) {
        self.triggers[slot].ddl_state = self.triggers[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn alter_trigger(
        &mut self,
        slot: usize,
        name: SqlName,
        enabled: TriggerEnabled,
        txid: u32,
    ) -> Result<TriggerAlter, SqlError> {
        let blocker = self.triggers[slot].pending_definition;
        if let Some(pending) = blocker
            && pending.txid != txid
        {
            let name = self.triggers[slot].name;
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, name.as_str()));
        }
        let trigger = &mut self.triggers[slot];
        let mut definition = trigger.definition_to(txid);
        if definition.name == name && definition.enabled == enabled {
            return Ok(TriggerAlter::Unchanged);
        }
        let old_name = definition.name;
        let comment_subid = trigger.target.comment_subid();
        let prior = trigger.pending_definition;
        definition.name = name;
        definition.enabled = enabled;
        trigger.pending_definition = Some(PendingTriggerDefinition { txid, definition });
        if old_name != name {
            self.stage_trigger_comment_rename(old_name, name, comment_subid, txid);
        }
        Ok(TriggerAlter::Changed { prior })
    }

    pub(crate) fn replace_trigger(
        &mut self,
        slot: usize,
        spec: TriggerSpec,
        txid: u32,
    ) -> Result<TriggerAlter, SqlError> {
        if !spec.is_valid() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid replacement trigger definition"
            ));
        }
        let trigger = &self.triggers[slot];
        if let Some(pending) = trigger.pending_definition
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, trigger.name.as_str()));
        }
        if trigger.target != spec.target || !matches!(trigger.kind, TriggerKind::Ordinary) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "trigger \"{}\" for relation is a constraint trigger",
                spec.name.as_str()
            ));
        }
        let function = u16::try_from(spec.function).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine slot exceeds trigger capacity"
            )
        })?;
        let prior = trigger.pending_definition;
        self.triggers[slot].pending_definition = Some(PendingTriggerDefinition {
            txid,
            definition: TriggerDefinition {
                name: spec.name,
                kind: spec.kind,
                function,
                timing: spec.timing,
                level: spec.level,
                events: spec.events,
                update_columns: spec.update_columns,
                transition_tables: spec.transition_tables,
                when: spec.when,
                arguments: spec.arguments,
                enabled: TriggerEnabled::Origin,
            },
        });
        Ok(TriggerAlter::Changed { prior })
    }

    pub(crate) fn commit_trigger_alter(&mut self, slot: usize, txid: u32) {
        let trigger = &mut self.triggers[slot];
        if let Some(pending) = trigger.pending_definition
            && pending.txid == txid
        {
            let old_name = trigger.name;
            let new_name = pending.definition.name;
            let subid = trigger.target.comment_subid();
            trigger.apply_definition(pending.definition);
            trigger.pending_definition = None;
            for comment in self.comments.iter_mut().filter(|comment| {
                comment.used
                    && comment.class == CommentClass::Trigger
                    && comment.subid == subid
                    && ((old_name != new_name && comment.name == old_name)
                        || comment
                            .pending_identity
                            .is_some_and(|identity| identity.txid == txid))
            }) {
                if old_name != new_name {
                    comment.name = new_name;
                }
                comment.pending_identity = None;
            }
        }
    }

    pub(crate) fn rollback_trigger_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingTriggerDefinition>,
    ) {
        let current = self.triggers[slot].pending_definition;
        let committed_name = self.triggers[slot].name;
        let restored_name = prior
            .map(|pending| pending.definition.name)
            .unwrap_or(committed_name);
        let subid = self.triggers[slot].target.comment_subid();
        if let Some(current) = current {
            for comment in self.comments.iter_mut().filter(|comment| {
                comment.used
                    && comment.class == CommentClass::Trigger
                    && comment.subid == subid
                    && comment
                        .pending_identity
                        .is_some_and(|identity| identity.txid == current.txid)
            }) {
                comment.pending_identity =
                    (restored_name != comment.name).then_some(PendingCommentIdentity {
                        txid: current.txid,
                        name: restored_name,
                    });
            }
        }
        self.triggers[slot].pending_definition = prior;
    }

    pub(crate) fn commit_trigger_create(&mut self, slot: usize) {
        self.triggers[slot].ddl_state = self.triggers[slot].ddl_state.commit_create();
    }

    pub(crate) fn restore_trigger(
        &mut self,
        created_at: u64,
        spec: TriggerSpec,
        enabled: TriggerEnabled,
    ) -> Result<usize, SqlError> {
        if !spec.is_valid() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid trigger definition in checkpoint"
            ));
        }
        if self
            .triggers_with_slots_visible_to(0)
            .find_map(|(slot, trigger)| {
                (trigger.target == spec.target && trigger.name_to(0) == spec.name).then_some(slot)
            })
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "duplicate trigger in checkpoint"
            ));
        }
        let Some(slot) = self
            .triggers
            .iter()
            .position(|trigger| trigger.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many triggers in checkpoint"
            ));
        };
        self.catalog_seq = self.catalog_seq.max(created_at);
        self.triggers[slot] = TriggerDef {
            created_at,
            name: spec.name,
            target: spec.target,
            kind: spec.kind,
            function: u16::try_from(spec.function).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "routine slot exceeds trigger capacity"
                )
            })?,
            timing: spec.timing,
            level: spec.level,
            events: spec.events,
            update_columns: spec.update_columns,
            transition_tables: spec.transition_tables,
            when: spec.when,
            arguments: spec.arguments,
            enabled,
            pending_definition: None,
            ddl_state: CatalogDdlState::Present,
        };
        Ok(slot)
    }

    pub(crate) fn rollback_trigger_create(&mut self, slot: usize) {
        self.triggers[slot].ddl_state = self.triggers[slot].ddl_state.rollback_create();
    }

    pub(crate) fn commit_trigger_drop(&mut self, slot: usize) {
        let trigger = self.triggers[slot];
        self.triggers[slot].ddl_state = self.triggers[slot].ddl_state.commit_drop();
        self.clear_trigger_dependents(slot, trigger);
    }

    fn clear_trigger_dependents(&mut self, slot: usize, trigger: TriggerDef) {
        self.clear_extension_dependencies_for_object(Self::trigger_access_object(slot));
        for state in self.partition_trigger_states.iter_mut() {
            if usize::from(state.trigger) == slot {
                *state = PartitionTriggerState::EMPTY;
            }
        }
        for comment_slot in 0..self.comments.len() {
            let comment = &self.comments[comment_slot];
            if comment.used
                && comment.class == CommentClass::Trigger
                && comment.name == trigger.name
                && comment.subid == trigger.target.comment_subid()
            {
                self.comments[comment_slot].live = None;
                self.reap_comment(comment_slot);
            }
        }
    }

    fn drop_trigger_comments_for_target(&mut self, target: TriggerTarget) {
        let subid = target.comment_subid();
        for slot in 0..self.comments.len() {
            let comment = &self.comments[slot];
            if comment.used && comment.class == CommentClass::Trigger && comment.subid == subid {
                self.comments[slot].live = None;
                self.reap_comment(slot);
            }
        }
    }

    /// Triggers are internal relation dependents. A committed table drop
    /// retires them in the same catalog transition.
    pub(crate) fn commit_triggers_for_table(&mut self, table: usize) {
        let target = TriggerTarget::Table(table as u16);
        self.drop_trigger_comments_for_target(target);
        for slot in 0..self.triggers.len() {
            let trigger = self.triggers[slot];
            if trigger.ddl_state != CatalogDdlState::Absent && trigger.target == target {
                self.triggers[slot].ddl_state = CatalogDdlState::Absent;
                self.triggers[slot].pending_definition = None;
                self.clear_trigger_dependents(slot, trigger);
            }
        }
    }

    pub(crate) fn rollback_trigger_drop(&mut self, slot: usize, txid: u32) {
        self.triggers[slot].ddl_state = self.triggers[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn replay_create_routine(
        &mut self,
        mut definition: RoutineDef,
        dependencies: StoredQueryDependencies,
    ) -> Result<(), SqlError> {
        definition.ownership = definition.ownership.committed();
        if let Some(slot) = self.routine_slot_by_declared_signature(
            definition.schema.as_str(),
            definition.name.as_str(),
            definition.arguments(),
            0,
        ) {
            // CREATE OR REPLACE keeps its routine object identifier.  WAL uses
            // the complete post-change definition for both creates and
            // replacements, so the matching durable identity selects an
            // in-place replay rather than a drop/reallocate cycle.
            if self.routines[slot].created_at == definition.created_at {
                self.routines[slot].arguments = definition.arguments;
                self.routines[slot].argument_count = definition.argument_count;
                self.routines[slot].parameters = definition.parameters;
                self.routines[slot].parameter_count = definition.parameter_count;
                self.routines[slot].kind = definition.kind;
                self.routines[slot].result_columns = definition.result_columns;
                self.routines[slot].result_column_count = definition.result_column_count;
                self.routines[slot].language = definition.language;
                self.routines[slot].attributes = definition.attributes;
                self.routines[slot].configs = definition.configs;
                self.routines[slot].config_count = definition.config_count;
                self.routines[slot].body_kind = definition.body_kind;
                self.routines[slot].body = definition.body;
                self.routines[slot].creation_path = definition.creation_path;
                self.routine_dependencies[slot] = dependencies;
                self.routines[slot].ownership = definition.ownership;
                self.routines[slot].pending_definition = None;
                return Ok(());
            }
            self.drop_routine(slot, 0);
            self.commit_routine_drop(slot);
        }
        let slot = self.create_routine(
            RoutineSpec {
                identity: RoutineIdentity::Preserve {
                    created_at: definition.created_at,
                    ownership: definition.ownership,
                },
                schema: definition.schema,
                name: definition.name,
                arguments: definition.arguments,
                argument_count: definition.argument_count,
                parameters: definition.parameters,
                parameter_count: definition.parameter_count,
                kind: definition.kind,
                result_columns: definition.result_columns,
                result_column_count: definition.result_column_count,
                language: definition.language,
                attributes: definition.attributes,
                configs: definition.configs,
                config_count: definition.config_count,
                body_kind: definition.body_kind,
                body: definition.body,
                creation_path: definition.creation_path,
                dependencies,
            },
            0,
        )?;
        self.commit_routine_create(slot, 0);
        Ok(())
    }

    pub(crate) fn extended_statistics_count(&self) -> usize {
        self.extended_statistics.len()
    }

    pub(crate) fn extended_statistics(&self, slot: usize) -> &ExtendedStatisticsDef {
        &self.extended_statistics[slot]
    }

    pub(crate) fn extended_statistics_visible(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ExtendedStatisticsDef)> {
        self.extended_statistics
            .iter()
            .enumerate()
            .filter(move |(_, statistics)| statistics.visible_to(txid))
    }

    pub(crate) fn extended_statistics_for_table(
        &self,
        table: usize,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ExtendedStatisticsDef)> {
        self.extended_statistics_visible(txid)
            .filter(move |(_, statistics)| usize::from(statistics.table) == table)
    }

    pub(crate) fn extended_statistics_slot(
        &self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        self.extended_statistics.iter().position(|statistics| {
            let definition = statistics.definition_for(txid);
            statistics.visible_to(txid)
                && definition.schema.as_str() == schema
                && definition.name.as_str() == name
        })
    }

    pub(crate) fn extended_statistics_slot_on_path(
        &self,
        schema: Option<&str>,
        name: &str,
        txid: u32,
    ) -> Option<usize> {
        if let Some(schema) = schema {
            return self.extended_statistics_slot(schema, name, txid);
        }
        self.path.entries().iter().find_map(|entry| match entry {
            PathEntry::Schema(slot) => self.extended_statistics_slot(
                self.schemas[*slot as usize].name.as_str(),
                name,
                txid,
            ),
            PathEntry::Catalog => None,
        })
    }

    pub(crate) fn create_extended_statistics(
        &mut self,
        spec: ExtendedStatisticsSpec,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.require_schema_create(spec.schema.as_str(), txid)?;
        if usize::from(spec.table) >= self.tables.len()
            || !self.tables[usize::from(spec.table)].visible_to(txid)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "statistics relation does not exist"
            ));
        }
        if spec.n_keys == 0 || usize::from(spec.n_keys) > MAX_EXTENDED_STATISTICS_KEYS {
            return Err(sql_err!(
                sqlstate::INVALID_OBJECT_DEFINITION,
                "invalid extended statistics key count"
            ));
        }
        if let Some(blocker) = self.extended_statistics.iter().find_map(|statistics| {
            let definition = statistics.definition_for(txid);
            (definition.schema == spec.schema && definition.name == spec.name)
                .then_some(statistics.ddl_state.pending_txid()?)
                .filter(|owner| *owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, spec.name.as_str()));
        }
        if self
            .extended_statistics_slot(spec.schema.as_str(), spec.name.as_str(), txid)
            .is_some()
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "statistics object \"{}\" already exists",
                spec.name.as_str()
            ));
        }
        if self
            .extended_statistics_for_table(usize::from(spec.table), txid)
            .count()
            == MAX_EXTENDED_STATISTICS_PER_TABLE
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "relation has too many statistics objects (limit {})",
                MAX_EXTENDED_STATISTICS_PER_TABLE
            ));
        }
        let Some(slot) = self
            .extended_statistics
            .iter()
            .position(|statistics| statistics.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many statistics objects (limit {})",
                self.extended_statistics.len()
            ));
        };
        let created_at = if spec.created_at == 0 {
            self.catalog_seq = self.catalog_seq.saturating_add(1);
            self.catalog_seq
        } else {
            self.catalog_seq = self.catalog_seq.max(spec.created_at);
            spec.created_at
        };
        self.extended_statistics[slot] = ExtendedStatisticsDef {
            created_at,
            table: spec.table,
            mutable: ExtendedStatisticsMutableDefinition {
                schema: spec.schema,
                name: spec.name,
                target: spec.target,
            },
            pending_definition: None,
            pending_keys: None,
            ownership: self.initial_ownership(txid),
            keys: spec.keys,
            n_keys: spec.n_keys,
            kinds: spec.kinds,
            expression_only: spec.expression_only,
            data: ExtendedStatisticsData::EMPTY,
            pending_data_slots: [u32::MAX; MAX_PENDING_TABLE_DEFS],
            n_pending_data: 0,
            pending_data_txid: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    pub(crate) fn alter_extended_statistics(
        &mut self,
        slot: usize,
        definition: ExtendedStatisticsMutableDefinition,
        txid: u32,
    ) -> Result<Option<PendingExtendedStatisticsDefinition>, SqlError> {
        if let Some(other) = self.extended_statistics_slot(
            definition.schema.as_str(),
            definition.name.as_str(),
            txid,
        ) && other != slot
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "statistics object \"{}\" already exists",
                definition.name.as_str()
            ));
        }
        let statistics = &mut self.extended_statistics[slot];
        if let Some(pending) = statistics.pending_definition
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "statistics object \"{}\" is being altered by another transaction",
                statistics.mutable.name.as_str()
            ));
        }
        let prior = statistics.pending_definition;
        statistics.pending_definition =
            Some(PendingExtendedStatisticsDefinition { txid, definition });
        Ok(prior)
    }

    pub(crate) fn alter_extended_statistics_keys(
        &mut self,
        slot: usize,
        keys: [ExtendedStatisticsKey; MAX_EXTENDED_STATISTICS_KEYS],
        txid: u32,
    ) -> Result<Option<PendingExtendedStatisticsKeys>, SqlError> {
        let statistics = &mut self.extended_statistics[slot];
        if let Some(pending) = statistics.pending_keys
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "statistics object \"{}\" is being altered by another transaction",
                statistics.mutable.name.as_str()
            ));
        }
        let prior = statistics.pending_keys;
        statistics.pending_keys = Some(PendingExtendedStatisticsKeys { txid, keys });
        Ok(prior)
    }

    pub(crate) fn commit_extended_statistics_create(&mut self, slot: usize) {
        self.extended_statistics[slot].ddl_state =
            self.extended_statistics[slot].ddl_state.commit_create();
    }

    pub(crate) fn rollback_extended_statistics_create(&mut self, slot: usize) {
        self.clear_pending_extended_statistics_data(slot);
        self.extended_statistics[slot].pending_keys = None;
        self.extended_statistics[slot].ddl_state =
            self.extended_statistics[slot].ddl_state.rollback_create();
    }

    pub(crate) fn commit_extended_statistics_alter(&mut self, slot: usize, txid: u32) {
        let statistics = &mut self.extended_statistics[slot];
        if let Some(pending) = statistics.pending_definition
            && pending.txid == txid
        {
            statistics.mutable = pending.definition;
            statistics.pending_definition = None;
        }
        if let Some(pending) = statistics.pending_keys
            && pending.txid == txid
        {
            statistics.keys = pending.keys;
            statistics.pending_keys = None;
        }
    }

    pub(crate) fn rollback_extended_statistics_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingExtendedStatisticsDefinition>,
    ) {
        self.extended_statistics[slot].pending_definition = prior;
    }

    pub(crate) fn rollback_extended_statistics_keys(
        &mut self,
        slot: usize,
        prior: Option<PendingExtendedStatisticsKeys>,
    ) {
        self.extended_statistics[slot].pending_keys = prior;
    }

    pub(crate) fn drop_extended_statistics(&mut self, slot: usize, txid: u32) {
        self.extended_statistics[slot].ddl_state =
            self.extended_statistics[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn commit_extended_statistics_drop(&mut self, slot: usize) {
        self.clear_pending_extended_statistics_data(slot);
        self.extended_statistics[slot].data = ExtendedStatisticsData::EMPTY;
        self.extended_statistics[slot].pending_definition = None;
        self.extended_statistics[slot].pending_keys = None;
        self.extended_statistics[slot].ddl_state =
            self.extended_statistics[slot].ddl_state.commit_drop();
    }

    pub(crate) fn rollback_extended_statistics_drop(&mut self, slot: usize, txid: u32) {
        self.extended_statistics[slot].ddl_state =
            self.extended_statistics[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn extended_statistics_data(
        &self,
        slot: usize,
        txid: u32,
    ) -> ExtendedStatisticsData {
        let statistics = &self.extended_statistics[slot];
        if statistics.pending_data_txid == Some(txid)
            && let Some(position) = statistics.n_pending_data.checked_sub(1)
        {
            let pending = statistics.pending_data_slots[position as usize] as usize;
            return self.pending_extended_statistics_data[pending].data;
        }
        statistics.data
    }

    pub(crate) fn pending_extended_statistics_data_for(
        &self,
        slot: usize,
        txid: u32,
    ) -> Option<ExtendedStatisticsData> {
        (self.extended_statistics[slot].pending_data_txid == Some(txid))
            .then(|| self.extended_statistics_data(slot, txid))
    }

    pub(crate) fn write_extended_statistics_data(
        &mut self,
        slot: usize,
        txid: u32,
        data: ExtendedStatisticsData,
    ) -> Result<(), SqlError> {
        let statistics = &self.extended_statistics[slot];
        if let Some(owner) = statistics.pending_data_txid
            && owner != txid
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "could not serialize ANALYZE of statistics object \"{}\"",
                statistics.definition_for(txid).name.as_str()
            ));
        }
        if statistics.n_pending_data as usize == MAX_PENDING_TABLE_DEFS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one transaction analyzes statistics object more than {} times",
                MAX_PENDING_TABLE_DEFS
            ));
        }
        let pending = match self
            .pending_extended_statistics_data
            .iter()
            .position(|entry| !entry.used)
        {
            Some(pending) => {
                self.pending_extended_statistics_data[pending] =
                    PendingExtendedStatisticsDataSlot { used: true, data };
                pending
            }
            None => {
                let pending = self.pending_extended_statistics_data.len();
                self.pending_extended_statistics_data
                    .push(PendingExtendedStatisticsDataSlot { used: true, data })
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "pending extended-statistics pool is exhausted"
                        )
                    })?;
                pending
            }
        };
        let statistics = &mut self.extended_statistics[slot];
        let position = statistics.n_pending_data as usize;
        statistics.pending_data_slots[position] = pending as u32;
        statistics.n_pending_data += 1;
        statistics.pending_data_txid = Some(txid);
        Ok(())
    }

    pub(crate) fn rollback_extended_statistics_data(&mut self, slot: usize, txid: u32) {
        let statistics = &mut self.extended_statistics[slot];
        if statistics.pending_data_txid != Some(txid) {
            return;
        }
        let Some(position) = statistics.n_pending_data.checked_sub(1) else {
            return;
        };
        let pending = statistics.pending_data_slots[position as usize] as usize;
        self.pending_extended_statistics_data[pending].used = false;
        statistics.pending_data_slots[position as usize] = u32::MAX;
        statistics.n_pending_data = position;
        if position == 0 {
            statistics.pending_data_txid = None;
        }
    }

    fn clear_pending_extended_statistics_data(&mut self, slot: usize) {
        let statistics = &mut self.extended_statistics[slot];
        for position in 0..statistics.n_pending_data as usize {
            let pending = statistics.pending_data_slots[position] as usize;
            self.pending_extended_statistics_data[pending].used = false;
            statistics.pending_data_slots[position] = u32::MAX;
        }
        statistics.n_pending_data = 0;
        statistics.pending_data_txid = None;
    }

    pub(crate) fn commit_extended_statistics_data(&mut self, slot: usize, txid: u32) {
        let data = self.extended_statistics_data(slot, txid);
        if self.extended_statistics[slot].pending_data_txid != Some(txid) {
            return;
        }
        self.extended_statistics[slot].data = data;
        self.clear_pending_extended_statistics_data(slot);
    }

    pub(crate) fn install_extended_statistics_data(
        &mut self,
        slot: usize,
        data: ExtendedStatisticsData,
    ) {
        self.clear_pending_extended_statistics_data(slot);
        self.extended_statistics[slot].data = data;
    }

    pub(crate) fn replay_extended_statistics(
        &mut self,
        spec: ExtendedStatisticsSpec,
    ) -> Result<usize, SqlError> {
        if let Some(slot) = self.extended_statistics.iter().position(|statistics| {
            statistics.ddl_state != CatalogDdlState::Absent
                && statistics.created_at == spec.created_at
        }) {
            if self
                .extended_statistics
                .iter()
                .enumerate()
                .any(|(other, statistics)| {
                    other != slot
                        && statistics.ddl_state != CatalogDdlState::Absent
                        && statistics.mutable.schema == spec.schema
                        && statistics.mutable.name == spec.name
                })
            {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "journal replays duplicate statistics object \"{}\"",
                    spec.name.as_str()
                ));
            }
            let statistics = &mut self.extended_statistics[slot];
            statistics.table = spec.table;
            statistics.mutable = ExtendedStatisticsMutableDefinition {
                schema: spec.schema,
                name: spec.name,
                target: spec.target,
            };
            statistics.pending_definition = None;
            statistics.pending_keys = None;
            statistics.keys = spec.keys;
            statistics.n_keys = spec.n_keys;
            statistics.kinds = spec.kinds;
            statistics.expression_only = spec.expression_only;
            statistics.ddl_state = CatalogDdlState::Present;
            return Ok(slot);
        }
        let slot = self.create_extended_statistics(spec, 0)?;
        self.commit_extended_statistics_create(slot);
        Ok(slot)
    }

    pub(crate) fn replay_drop_extended_statistics(
        &mut self,
        schema: &str,
        name: &str,
    ) -> Result<(), SqlError> {
        let Some(slot) = self.extended_statistics_slot(schema, name, 0) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "journal drops unknown statistics object \"{}\"",
                name
            ));
        };
        self.drop_extended_statistics(slot, 0);
        self.commit_extended_statistics_drop(slot);
        Ok(())
    }

    pub fn index_exists(&self, schema: &str, name: &str, txid: u32) -> bool {
        self.index_slot(schema, name, txid).is_some()
    }

    pub(crate) fn index_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.indexes.iter().position(|index| {
            index.visible_to(txid)
                && index.schema.as_str() == schema
                && index.name_for(txid).as_str() == name
        })
    }

    /// The transaction-visible index definition. Returning the copy keeps a
    /// caller from retaining a catalog borrow across cache reconstruction.
    pub fn index_definition(&self, schema: &str, name: &str, txid: u32) -> Option<IndexDef> {
        self.index_slot(schema, name, txid)
            .map(|slot| self.indexes[slot])
    }

    pub(crate) fn rename_index(
        &mut self,
        slot: usize,
        name: SqlName,
        txid: u32,
    ) -> Result<Option<PendingIndexName>, SqlError> {
        let index = self.indexes[slot];
        if !index.visible_to(txid) {
            return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "index does not exist"));
        }
        if let Some(blocker) = self
            .indexes
            .iter()
            .enumerate()
            .find_map(|(other, candidate)| {
                (other != slot
                    && candidate.schema == index.schema
                    && candidate
                        .pending_name
                        .is_some_and(|pending| pending.name == name && pending.txid != txid))
                .then_some(candidate.pending_name?.txid)
            })
        {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self.indexes.iter().enumerate().any(|(other, candidate)| {
            other != slot
                && candidate.visible_to(txid)
                && candidate.schema == index.schema
                && candidate.name_for(txid) == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                name.as_str()
            ));
        }
        let index = &mut self.indexes[slot];
        if let Some(pending) = index.pending_name
            && pending.txid != txid
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "index \"{}\" is being renamed by another transaction",
                index.name.as_str()
            ));
        }
        let prior = index.pending_name;
        index.pending_name = Some(PendingIndexName { txid, name });
        Ok(prior)
    }

    pub(crate) fn commit_index_rename(&mut self, slot: usize, txid: u32) {
        let (schema, old_name, new_name) = {
            let index = &mut self.indexes[slot];
            let Some(pending) = index.pending_name else {
                return;
            };
            if pending.txid != txid {
                return;
            }
            let old_name = index.name;
            index.name = pending.name;
            index.pending_name = None;
            (index.schema, old_name, pending.name)
        };
        for comment in self.comments.iter_mut() {
            if comment.used
                && comment.class == CommentClass::Relation
                && comment.schema == schema
                && comment.name == old_name
            {
                comment.name = new_name;
            }
        }
        if let Some(table) = self.index_table_slot(slot) {
            self.tables[table].mark_dirty();
        }
    }

    pub(crate) fn rollback_index_rename(&mut self, slot: usize, prior: Option<PendingIndexName>) {
        self.indexes[slot].pending_name = prior;
    }

    /// Registers an index as an uncommitted CREATE owned by `def.pending`'s
    /// transaction; returns its slot. Errors on a duplicate visible name or
    /// another transaction's uncommitted DDL on the name.
    pub fn create_index(&mut self, def: IndexDef, txid: u32) -> Result<usize, SqlError> {
        self.require_schema_create(def.schema.as_str(), txid)?;
        if let Some(blocker) = self.indexes.iter().find_map(|index| {
            (index.schema.as_str() == def.schema.as_str()
                && index.name_for(txid).as_str() == def.name.as_str())
            .then_some(index.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, def.name.as_str()));
        }
        if self.relation_name_taken(def.schema.as_str(), def.name.as_str(), txid) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        let Some(i) = self
            .indexes
            .iter()
            .position(|x| x.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many indexes (limit {})",
                self.indexes.len()
            ));
        };
        let ownership = self.initial_ownership(txid);
        self.clear_object_acl_entries(AccessObject {
            class: AccessClass::Index,
            slot: i as u16,
        });
        let created_at = if def.created_at == 0 {
            self.catalog_seq = self.catalog_seq.saturating_add(1);
            self.catalog_seq
        } else {
            self.catalog_seq = self.catalog_seq.max(def.created_at);
            def.created_at
        };
        self.indexes[i] = IndexDef {
            created_at,
            ownership,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
            ..def
        };
        Ok(i)
    }

    pub(crate) fn alter_index_definition(
        &mut self,
        slot: usize,
        definition: IndexMutableDefinition,
        txid: u32,
    ) -> Result<Option<PendingIndexDefinition>, SqlError> {
        let pending = self.indexes[slot].pending_definition;
        if let Some(pending) = pending
            && pending.txid != txid
        {
            return Err(self.catalog_ddl_wait_error(
                txid,
                pending.txid,
                self.indexes[slot].name.as_str(),
            ));
        }
        let index = &mut self.indexes[slot];
        let prior = index.pending_definition;
        index.pending_definition = Some(PendingIndexDefinition { txid, definition });
        Ok(prior)
    }

    pub(crate) fn commit_index_definition(&mut self, slot: usize, txid: u32) {
        let index = &mut self.indexes[slot];
        if let Some(pending) = index.pending_definition
            && pending.txid == txid
        {
            index.mutable = pending.definition;
            index.pending_definition = None;
        }
    }

    pub(crate) fn rollback_index_definition(
        &mut self,
        slot: usize,
        prior: Option<PendingIndexDefinition>,
    ) {
        self.indexes[slot].pending_definition = prior;
    }

    /// Marks every index visible to `txid` on a table pending-dropped
    /// (PostgreSQL drops a table's indexes when the table itself is dropped).
    /// Commit finalizes via [`Self::commit_indexes_for`]; rollback reverts via
    /// [`Self::rollback_indexes_for`].
    pub fn drop_indexes_for(&mut self, schema: &str, table: &str, txid: u32) {
        for i in 0..self.indexes.len() {
            if self.indexes[i].visible_to(txid)
                && self.indexes[i].schema.as_str() == schema
                && self.indexes[i].table.as_str() == table
            {
                self.pending_drop_index(i, txid);
            }
        }
    }

    /// Promotes this transaction's pending index drops on a table (cascaded
    /// from its DROP TABLE) into the committed catalog.
    pub fn commit_indexes_for(&mut self, schema: &str, table: &str, txid: u32) {
        for x in self.indexes.iter_mut() {
            if x.schema.as_str() == schema
                && x.table.as_str() == table
                && x.ddl_state == (CatalogDdlState::PendingDrop { txid })
            {
                x.ddl_state = x.ddl_state.commit_drop();
            }
        }
    }

    /// Discards this transaction's pending index drops on a table (a rolled
    /// back DROP TABLE): committed indexes become visible again.
    pub fn rollback_indexes_for(&mut self, schema: &str, table: &str, txid: u32) {
        for x in self.indexes.iter_mut() {
            if x.schema.as_str() == schema
                && x.table.as_str() == table
                && x.ddl_state == (CatalogDdlState::PendingDrop { txid })
            {
                x.ddl_state = x.ddl_state.rollback_drop(txid);
            }
        }
    }

    /// Marks the index visible to `txid` pending-dropped; returns its slot
    /// (for undo). None if absent. Errors if another transaction's uncommitted
    /// DDL holds the name.
    pub fn drop_index(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if let Some(blocker) = self.indexes.iter().find_map(|index| {
            (index.schema.as_str() == schema && index.name_for(txid).as_str() == name)
                .then_some(index.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.index_slot(schema, name, txid) else {
            return Ok(None);
        };
        self.pending_drop_index(i, txid);
        Ok(Some(i))
    }

    /// Overlays a pending DROP on a slot: the owner's own pending-create
    /// simply evaporates.
    fn pending_drop_index(&mut self, slot: usize, txid: u32) {
        let x = &mut self.indexes[slot];
        x.ddl_state = x.ddl_state.drop_by(txid);
    }

    /// Promotes an uncommitted CREATE INDEX into the committed catalog.
    pub fn commit_index_create(&mut self, slot: usize) {
        self.indexes[slot].ddl_state = self.indexes[slot].ddl_state.commit_create();
        if let Some(table) = self.index_table_slot(slot) {
            self.tables[table].mark_dirty();
        }
    }

    /// Promotes an uncommitted DROP INDEX into the committed catalog.
    pub fn commit_index_drop(&mut self, slot: usize) {
        let (schema, name) = (self.indexes[slot].schema, self.indexes[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.indexes[slot].ddl_state = self.indexes[slot].ddl_state.commit_drop();
        self.clear_extension_dependencies_for_object(AccessObject {
            class: AccessClass::Index,
            slot: slot as u16,
        });
        if let Some(table) = self.index_table_slot(slot) {
            self.tables[table].mark_dirty();
        }
    }

    /// The committed table named by an index slot. The definition stays in
    /// the reusable catalog slot after DROP, so this also resolves the table
    /// while finalizing a drop.
    pub fn index_table_slot(&self, slot: usize) -> Option<usize> {
        self.index_table_slot_to(slot, 0)
    }

    /// The transaction-visible table named by an index. DDL execution must
    /// use this form so a just-created table is not mistaken for corruption.
    pub(crate) fn index_table_slot_to(&self, slot: usize, txid: u32) -> Option<usize> {
        let index = self.indexes.get(slot)?;
        self.find_visible(index.schema.as_str(), index.table.as_str(), txid)
    }

    /// Discards an uncommitted CREATE INDEX (rollback): the slot is freed.
    pub fn rollback_index_create(&mut self, slot: usize) {
        self.indexes[slot].ddl_state = self.indexes[slot].ddl_state.rollback_create();
    }

    /// Discards an uncommitted DROP INDEX (rollback); a same-transaction
    /// pending-create reverts to pending-create.
    pub fn rollback_index_drop(&mut self, slot: usize, txid: u32) {
        let x = &mut self.indexes[slot];
        x.ddl_state = x.ddl_state.rollback_drop(txid);
    }

    /// Unique indexes visible to `txid` over the named table (for constraint
    /// enforcement — an uncommitted CREATE UNIQUE INDEX binds its owner).
    /// Every index on `table` that `txid` can see, including one it created in
    /// its own still-open transaction.
    pub fn indexes_for<'a>(
        &'a self,
        schema: &'a str,
        table: &'a str,
        txid: u32,
    ) -> impl Iterator<Item = &'a IndexDef> {
        let committed_binding = self
            .find_visible(schema, table, txid)
            .map(|slot| (self.tables[slot].def.schema, self.tables[slot].def.name));
        self.indexes.iter().filter(move |x| {
            x.visible_to(txid)
                && ((x.schema.as_str() == schema && x.table.as_str() == table)
                    || committed_binding.is_some_and(|(old_schema, old_table)| {
                        x.schema == old_schema && x.table == old_table
                    }))
        })
    }

    pub fn unique_indexes_for<'a>(
        &'a self,
        schema: &'a str,
        table: &'a str,
        txid: u32,
    ) -> impl Iterator<Item = &'a IndexDef> {
        self.indexes_for(schema, table, txid).filter(|x| x.unique)
    }

    /// All committed indexes, for checkpoint serialization.
    pub fn live_indexes(&self) -> impl Iterator<Item = &IndexDef> {
        self.indexes
            .iter()
            .filter(|x| x.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn live_indexes_with_slots(&self) -> impl Iterator<Item = (usize, &IndexDef)> {
        self.indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| index.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn index_count(&self) -> usize {
        self.indexes.len()
    }

    pub(crate) fn index_visible_to(&self, slot: usize, txid: u32) -> Option<IndexDef> {
        self.indexes
            .get(slot)
            .copied()
            .filter(|index| index.visible_to(txid))
    }

    pub(crate) fn tablespace_slot(&self, name: &str, txid: u32) -> Option<usize> {
        self.tablespaces.iter().position(|tablespace| {
            tablespace.visible_to(txid) && tablespace.name_for(txid).as_str() == name
        })
    }

    pub(crate) fn tablespace_by_id(&self, id: u16, txid: u32) -> Option<TablespaceDef> {
        let slot = usize::from(id.checked_sub(2)?);
        self.tablespaces
            .get(slot)
            .copied()
            .filter(|tablespace| tablespace.visible_to(txid))
    }

    pub(crate) fn tablespace_name(&self, id: u16, txid: u32) -> Option<SqlName> {
        match id {
            0 => SqlName::parse("pg_default").ok(),
            1 => SqlName::parse("pg_global").ok(),
            _ => self
                .tablespace_by_id(id, txid)
                .map(|tablespace| tablespace.name_for(txid)),
        }
    }

    pub(crate) fn tablespace_id(&self, name: &str, txid: u32) -> Option<u16> {
        if name.eq_ignore_ascii_case("pg_default") {
            return Some(0);
        }
        if name.eq_ignore_ascii_case("pg_global") {
            return Some(1);
        }
        self.tablespace_slot(name, txid)
            .and_then(|slot| u16::try_from(slot + 2).ok())
    }

    pub(crate) fn tablespaces_visible_to(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &TablespaceDef)> {
        self.tablespaces
            .iter()
            .enumerate()
            .filter(move |(_, tablespace)| tablespace.visible_to(txid))
    }

    pub(crate) fn create_tablespace(
        &mut self,
        created_at: u64,
        name: SqlName,
        location: StackStr<TABLESPACE_LOCATION_MAX>,
        options: TablespaceOptions,
        owner: u16,
        txid: u32,
    ) -> Result<usize, SqlError> {
        self.validate_tablespace_identity(name, owner, txid)?;
        let slot = self
            .tablespaces
            .iter()
            .position(|tablespace| tablespace.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "tablespace catalog capacity exhausted"
                )
            })?;
        self.install_tablespace(
            slot,
            TablespaceImage {
                created_at,
                name,
                location,
                options,
                owner,
            },
            txid,
        );
        Ok(slot)
    }

    pub(crate) fn restore_tablespace(
        &mut self,
        slot: usize,
        created_at: u64,
        name: SqlName,
        location: StackStr<TABLESPACE_LOCATION_MAX>,
        options: TablespaceOptions,
        owner: u16,
    ) -> Result<(), SqlError> {
        self.validate_tablespace_identity(name, owner, 0)?;
        let Some(target) = self.tablespaces.get(slot) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "tablespace slot is out of range"
            ));
        };
        if target.ddl_state != CatalogDdlState::Absent {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "tablespace slot {} is already occupied",
                slot
            ));
        }
        self.install_tablespace(
            slot,
            TablespaceImage {
                created_at,
                name,
                location,
                options,
                owner,
            },
            0,
        );
        Ok(())
    }

    fn validate_tablespace_identity(
        &self,
        name: SqlName,
        owner: u16,
        txid: u32,
    ) -> Result<(), SqlError> {
        if self.tablespace_id(name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "tablespace \"{}\" already exists",
                name.as_str()
            ));
        }
        if self
            .roles
            .get(usize::from(owner))
            .is_none_or(|role| !role.visible_to(txid))
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "tablespace owner does not exist"
            ));
        }
        Ok(())
    }

    fn install_tablespace(&mut self, slot: usize, image: TablespaceImage, txid: u32) {
        let created_at = if image.created_at == 0 {
            self.catalog_seq = self.catalog_seq.saturating_add(1);
            self.catalog_seq
        } else {
            self.catalog_seq = self.catalog_seq.max(image.created_at);
            image.created_at
        };
        self.tablespaces[slot] = TablespaceDef {
            created_at,
            name: image.name,
            location: image.location,
            options: image.options,
            ownership: Ownership {
                owner: 0,
                pending: Some(PendingOwnership {
                    txid,
                    owner: image.owner,
                }),
            },
            pending: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
    }

    pub(crate) fn alter_tablespace_definition(
        &mut self,
        slot: usize,
        name: SqlName,
        options: TablespaceOptions,
        txid: u32,
    ) -> Result<Option<PendingTablespaceDefinition>, SqlError> {
        if self
            .tablespaces
            .iter()
            .enumerate()
            .any(|(other, tablespace)| {
                other != slot && tablespace.visible_to(txid) && tablespace.name_for(txid) == name
            })
            || ((name.as_str().eq_ignore_ascii_case("pg_default")
                || name.as_str().eq_ignore_ascii_case("pg_global"))
                && self.tablespaces[slot].name_for(txid) != name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "tablespace \"{}\" already exists",
                name.as_str()
            ));
        }
        let prior = self.tablespaces[slot].pending;
        if prior.is_some_and(|pending| pending.txid != txid) {
            return Err(self.catalog_ddl_wait_error(
                txid,
                prior.unwrap().txid,
                self.tablespaces[slot].name.as_str(),
            ));
        }
        self.tablespaces[slot].pending = Some(PendingTablespaceDefinition {
            txid,
            name,
            options,
        });
        Ok(prior)
    }

    pub(crate) fn drop_tablespace(&mut self, slot: usize, txid: u32) -> Result<(), SqlError> {
        let id = u16::try_from(slot + 2).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "tablespace slot is out of range"
            )
        })?;
        if self
            .indexes
            .iter()
            .any(|index| index.visible_to(txid) && index.mutable_for(txid).tablespace == id)
        {
            return Err(sql_err!(
                sqlstate::OBJECT_IN_USE,
                "tablespace \"{}\" is not empty",
                self.tablespaces[slot].name_for(txid).as_str()
            ));
        }
        self.tablespaces[slot].ddl_state = self.tablespaces[slot].ddl_state.drop_by(txid);
        Ok(())
    }

    pub(crate) fn commit_tablespace_create(&mut self, slot: usize) {
        self.tablespaces[slot].ddl_state = self.tablespaces[slot].ddl_state.commit_create();
        self.tablespaces[slot].ownership = self.tablespaces[slot].ownership.committed();
    }

    pub(crate) fn commit_tablespace_alter(&mut self, slot: usize, txid: u32) {
        if let Some(pending) = self.tablespaces[slot].pending
            && pending.txid == txid
        {
            let old_name = self.tablespaces[slot].name;
            self.tablespaces[slot].name = pending.name;
            self.tablespaces[slot].options = pending.options;
            self.tablespaces[slot].pending = None;
            for comment in self.comments.iter_mut() {
                if comment.used
                    && comment.class == CommentClass::Tablespace
                    && comment.name == old_name
                {
                    comment.name = pending.name;
                }
            }
        }
        if self.tablespaces[slot]
            .ownership
            .pending
            .is_some_and(|pending| pending.txid == txid)
        {
            self.tablespaces[slot].ownership = self.tablespaces[slot].ownership.committed();
        }
    }

    pub(crate) fn commit_tablespace_drop(&mut self, slot: usize) {
        let name = self.tablespaces[slot].name;
        self.drop_object_comments(CommentClass::Tablespace, "", name.as_str());
        self.tablespaces[slot].ddl_state = self.tablespaces[slot].ddl_state.commit_drop();
    }

    pub(crate) fn rollback_tablespace_create(&mut self, slot: usize) {
        self.tablespaces[slot].ddl_state = self.tablespaces[slot].ddl_state.rollback_create();
    }

    pub(crate) fn rollback_tablespace_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingTablespaceDefinition>,
    ) {
        self.tablespaces[slot].pending = prior;
    }

    pub(crate) fn rollback_tablespace_drop(&mut self, slot: usize, txid: u32) {
        self.tablespaces[slot].ddl_state = self.tablespaces[slot].ddl_state.rollback_drop(txid);
    }

    /// A definition-only schema move (ALTER TABLE ... SET SCHEMA): the table
    /// and its indexes change schema, and every inbound foreign key follows —
    /// deterministically, so WAL replay reproduces it from the names alone.
    pub fn move_table_schema(&mut self, index: usize, new_schema: SqlName) {
        let old_schema = self.tables[index].def.schema;
        let name = self.tables[index].def.name;
        self.tables[index].def.schema = new_schema;
        self.tables[index].mark_dirty();
        for matview in self.matviews.iter_mut() {
            if matview.ddl_state == CatalogDdlState::Present
                && matview.schema == old_schema
                && matview.name == name
            {
                matview.schema = new_schema;
            }
        }
        for x in self.indexes.iter_mut() {
            if x.ddl_state == CatalogDdlState::Present
                && x.schema.as_str() == old_schema.as_str()
                && x.table.as_str() == name.as_str()
            {
                x.schema = new_schema;
            }
        }
        for sequence in self.sequences.iter_mut() {
            if sequence.ddl_state != CatalogDdlState::Present {
                continue;
            }
            let moves_with_table = matches!(
                sequence.owner,
                Some(owner)
                    if owner.table_schema == old_schema && owner.table == name
            );
            if moves_with_table {
                sequence.schema = new_schema;
                let owner = sequence.owner.as_mut().expect("matched Some owner");
                owner.table_schema = new_schema;
            }
            if matches!(
                sequence.generator_for,
                Some(generator)
                    if generator.table_schema == old_schema && generator.table == name
            ) {
                let generator = sequence
                    .generator_for
                    .as_mut()
                    .expect("matched Some generator");
                generator.table_schema = new_schema;
            }
        }
        for t in self.tables.iter_mut() {
            if !t.live {
                continue;
            }
            let mut changed = false;
            for f in 0..t.def.n_fkeys {
                let fk = &mut t.def.fkeys[f];
                if fk.parent_schema.as_str() == old_schema.as_str()
                    && fk.parent.as_str() == name.as_str()
                {
                    fk.parent_schema = new_schema;
                    changed = true;
                }
            }
            if changed {
                t.mark_dirty();
            }
        }
        for comment in self.comments.iter_mut() {
            if comment.used
                && matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                && comment.schema == old_schema
                && comment.name == name
            {
                comment.schema = new_schema;
            }
        }
        self.rename_stored_query_dependency(DependencyClass::Table, index, new_schema, name);
    }

    /// Removes one foreign key from a table's definition by constraint name
    /// (DROP SCHEMA CASCADE severing an inbound reference), returning it for
    /// transactional undo.
    pub fn drop_fk(&mut self, index: usize, fk_name: &str) -> Option<ForeignKey> {
        let def = &mut self.tables[index].def;
        let at = (0..def.n_fkeys).find(|&f| def.fkeys[f].name.as_str() == fk_name)?;
        let removed = def.fkeys[at];
        for f in at..def.n_fkeys - 1 {
            def.fkeys[f] = def.fkeys[f + 1];
        }
        def.n_fkeys -= 1;
        self.tables[index].mark_dirty();
        Some(removed)
    }

    /// Replaces a table's definition in place (ALTER TABLE).
    pub fn set_table_def(
        &mut self,
        index: usize,
        def: TableDef,
        column_mapping: &[Option<SqlName>; MAX_COLUMNS],
    ) {
        let old = self.tables[index].def;
        if old.schema != def.schema {
            self.move_table_schema(index, def.schema);
        }
        let current = self.tables[index].def;
        if current.name != def.name {
            for index_def in self.indexes.iter_mut() {
                if index_def.schema == current.schema && index_def.table == current.name {
                    index_def.table = def.name;
                }
            }
            for table in self.tables.iter_mut() {
                let mut changed = false;
                for foreign_key in &mut table.def.fkeys[..table.def.n_fkeys] {
                    if foreign_key.parent_schema == current.schema
                        && foreign_key.parent == current.name
                    {
                        foreign_key.parent = def.name;
                        changed = true;
                    }
                }
                if changed {
                    table.mark_dirty();
                }
            }
            for comment in self.comments.iter_mut() {
                if comment.used
                    && matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                    && comment.schema == current.schema
                    && comment.name == current.name
                {
                    comment.name = def.name;
                }
            }
        }
        for sequence in self.sequences.iter_mut() {
            sequence.owner =
                rebind_sequence_column(sequence.owner, &current, &def, column_mapping, false);
            sequence.generator_for = rebind_sequence_column(
                sequence.generator_for,
                &current,
                &def,
                column_mapping,
                true,
            );
        }
        for index_def in self.indexes.iter_mut() {
            if index_def.ddl_state != CatalogDdlState::Present
                || index_def.schema != old.schema
                || index_def.table != old.name
            {
                continue;
            }
            for column in &mut index_def.columns[..index_def.n_cols] {
                let Some(target_name) = column_mapping
                    .get(*column as usize)
                    .and_then(|target| *target)
                else {
                    continue;
                };
                if let Some(target_column) = def.column_index(target_name.as_str()) {
                    *column = target_column as u16;
                }
            }
            index_def.schema = def.schema;
            index_def.table = def.name;
        }
        self.tables[index].def = def;
        self.tables[index].mark_dirty();
    }

    pub fn next_rowid(&mut self) -> u64 {
        let id = self.next_rowid;
        self.next_rowid += 1;
        id
    }

    pub fn peek_next_rowid(&self) -> u64 {
        self.next_rowid
    }

    pub fn bump_lsn(&mut self) -> u64 {
        self.lsn += 1;
        self.lsn
    }

    pub fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The current command read snapshot (see [`Storage::read_snapshot`] field).
    pub fn read_snapshot(&self) -> u32 {
        self.read_snapshot
    }

    pub fn commit_snapshot(&self) -> u64 {
        self.commit_snapshot
    }

    pub fn set_commit_snapshot(&mut self, snapshot: u64) {
        self.commit_snapshot = snapshot;
    }

    pub fn register_snapshot(&mut self, txid: u32, snapshot: u64) -> Result<(), SqlError> {
        if let Some((_, existing)) = self
            .active_snapshots
            .iter_mut()
            .find(|(owner, _)| *owner == txid)
        {
            *existing = snapshot;
            return Ok(());
        }
        self.active_snapshots.push((txid, snapshot)).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "more than {} active historical snapshots",
                self.active_snapshots.capacity()
            )
        })
    }

    pub fn release_snapshot(&mut self, txid: u32) {
        let Some(index) = self
            .active_snapshots
            .iter()
            .position(|(owner, _)| *owner == txid)
        else {
            return;
        };
        self.active_snapshots.swap_remove(index);
        let oldest = self.oldest_snapshot();
        for table in self.tables.iter_mut() {
            for (_, state) in table.rows.iter_mut() {
                state.history.prune(oldest);
            }
        }
    }

    pub fn oldest_snapshot(&self) -> Option<u64> {
        self.active_snapshots
            .iter()
            .map(|(_, snapshot)| *snapshot)
            .min()
    }

    pub fn has_active_snapshots(&self) -> bool {
        !self.active_snapshots.is_empty()
    }

    pub fn lock_table(
        &self,
        txid: u32,
        table: usize,
        mode: crate::sql::ast::TableLockMode,
        nowait: bool,
    ) -> Result<(), SqlError> {
        use crate::sql::ast::TableLockMode;

        fn mode_bit(mode: TableLockMode) -> u8 {
            1 << mode as u8
        }

        fn modes_conflict(left: u8, right: u8) -> bool {
            for left_index in 0..8 {
                if left & (1 << left_index) == 0 {
                    continue;
                }
                for right_index in 0..8 {
                    if right & (1 << right_index) != 0 && mode_conflicts(left_index, right_index) {
                        return true;
                    }
                }
            }
            false
        }

        fn mode_conflicts(left: u8, right: u8) -> bool {
            use TableLockMode::*;
            let left = match left {
                0 => AccessShare,
                1 => RowShare,
                2 => RowExclusive,
                3 => ShareUpdateExclusive,
                4 => Share,
                5 => ShareRowExclusive,
                6 => Exclusive,
                _ => AccessExclusive,
            };
            let right = match right {
                0 => AccessShare,
                1 => RowShare,
                2 => RowExclusive,
                3 => ShareUpdateExclusive,
                4 => Share,
                5 => ShareRowExclusive,
                6 => Exclusive,
                _ => AccessExclusive,
            };
            matches!(
                (left, right),
                (AccessShare, AccessExclusive)
                    | (RowShare, Exclusive | AccessExclusive)
                    | (
                        RowExclusive,
                        Share | ShareRowExclusive | Exclusive | AccessExclusive
                    )
                    | (
                        ShareUpdateExclusive,
                        ShareUpdateExclusive
                            | Share
                            | ShareRowExclusive
                            | Exclusive
                            | AccessExclusive
                    )
                    | (
                        Share,
                        RowExclusive
                            | ShareUpdateExclusive
                            | ShareRowExclusive
                            | Exclusive
                            | AccessExclusive
                    )
                    | (
                        ShareRowExclusive,
                        RowExclusive
                            | ShareUpdateExclusive
                            | Share
                            | ShareRowExclusive
                            | Exclusive
                            | AccessExclusive
                    )
                    | (
                        Exclusive,
                        RowShare
                            | RowExclusive
                            | ShareUpdateExclusive
                            | Share
                            | ShareRowExclusive
                            | Exclusive
                            | AccessExclusive
                    )
                    | (AccessExclusive, _)
            ) || matches!(right, AccessExclusive)
        }

        let mut table_locks = self.table_locks.borrow_mut();
        let requested = mode_bit(mode);
        let own_index = table_locks
            .iter()
            .position(|lock| lock.owner == txid && lock.table == table as u32);
        if own_index.is_some_and(|index| table_locks[index].mask() & requested != 0) {
            return Ok(());
        }
        let combined = own_index
            .map(|index| table_locks[index].mask() | requested)
            .unwrap_or(requested);
        if let Some(blocker) = table_locks.iter().find(|lock| {
            lock.owner != txid
                && lock.table == table as u32
                && modes_conflict(combined, lock.mask())
        }) {
            if nowait {
                return Err(sql_err!(
                    sqlstate::LOCK_NOT_AVAILABLE,
                    "could not obtain lock on relation"
                ));
            }
            self.row_locks.borrow_mut().wait_for(txid, blocker.owner)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a relation lock"
            ));
        }
        let sequence = self.next_lock_sequence();
        if let Some(index) = own_index {
            table_locks[index].modes[mode as usize] = sequence;
            return Ok(());
        }
        let mut modes = [0; 8];
        modes[mode as usize] = sequence;
        table_locks
            .push(TableLock {
                owner: txid,
                table: table as u32,
                modes,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table-lock registry is full ({} locks)",
                    table_locks.capacity()
                )
            })
    }

    fn next_lock_sequence(&self) -> u64 {
        let next = self.lock_sequence.get().wrapping_add(1).max(1);
        self.lock_sequence.set(next);
        next
    }

    pub(crate) fn lock_mark(&self) -> u64 {
        self.lock_sequence.get()
    }

    pub(crate) fn rollback_locks_to(&self, txid: u32, mark: u64) {
        let mut table_locks = self.table_locks.borrow_mut();
        let mut table_changed = false;
        let mut index = 0usize;
        while index < table_locks.len() {
            if table_locks[index].owner != txid {
                index += 1;
                continue;
            }
            for acquired_at in &mut table_locks[index].modes {
                if *acquired_at > mark {
                    *acquired_at = 0;
                    table_changed = true;
                }
            }
            if table_locks[index].mask() == 0 {
                table_locks.swap_remove(index);
            } else {
                index += 1;
            }
        }
        drop(table_locks);
        let mut row_locks = self.row_locks.borrow_mut();
        row_locks.rollback_to(txid, mark);
        if table_changed {
            row_locks.resource_released(txid);
        }
    }

    pub fn release_table_locks(&self, txid: u32) {
        let mut table_locks = self.table_locks.borrow_mut();
        let mut index = 0usize;
        while index < table_locks.len() {
            if table_locks[index].owner == txid {
                table_locks.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    pub(crate) fn acquire_row_lock(
        &self,
        table: usize,
        rowid: u64,
        txid: u32,
        strength: crate::sql::ast::LockStrength,
        wait: crate::sql::ast::LockWait,
    ) -> Result<crate::sql::lock::LockDecision, SqlError> {
        let sequence = self.next_lock_sequence();
        self.row_locks
            .borrow_mut()
            .acquire(table, rowid, txid, strength, wait, sequence)
    }

    pub(crate) fn release_row_locks(&self, txid: u32) {
        self.row_locks.borrow_mut().release(txid);
    }

    pub(crate) fn lock_generation(&self) -> u64 {
        self.row_locks.borrow().generation()
    }

    pub(crate) fn begin_serializable(&self, txid: u32) -> Result<(), SqlError> {
        let mut snapshots = self.serializable_snapshots.borrow_mut();
        if snapshots.iter().any(|entry| entry.0 == txid) {
            return Ok(());
        }
        for (table, definition) in self.tables.iter().enumerate() {
            snapshots
                .push((txid, table as u32, definition.generation, false))
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "serializable snapshot registry is full ({} table snapshots)",
                        snapshots.capacity()
                    )
                })?;
        }
        Ok(())
    }

    pub(crate) fn record_serializable_read(&self, txid: u32, table: usize) {
        if let Some(entry) = self
            .serializable_snapshots
            .borrow_mut()
            .iter_mut()
            .find(|entry| entry.0 == txid && entry.1 == table as u32)
        {
            entry.3 = true;
        }
    }

    pub(crate) fn validate_serializable(&self, txid: u32) -> Result<(), SqlError> {
        for &(owner, table, generation, read) in self.serializable_snapshots.borrow().iter() {
            if owner == txid && read && self.tables[table as usize].generation != generation {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "could not serialize access due to read/write dependencies among transactions"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn release_serializable(&self, txid: u32) {
        let mut snapshots = self.serializable_snapshots.borrow_mut();
        let mut index = 0usize;
        while index < snapshots.len() {
            if snapshots[index].0 == txid {
                snapshots.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    pub fn has_access_share_locks(&self) -> bool {
        !self.table_locks.borrow().is_empty()
    }

    pub(crate) fn schema_lock_blocker(&self, txid: u32) -> Option<u32> {
        let table_locks = self.table_locks.borrow();
        self.active_snapshots
            .iter()
            .map(|(owner, _)| *owner)
            .chain(table_locks.iter().map(|lock| lock.owner))
            .find(|owner| *owner != txid)
    }

    pub(crate) fn wait_for_transaction(&self, waiter: u32, blocker: u32) -> Result<(), SqlError> {
        self.row_locks.borrow_mut().wait_for(waiter, blocker)
    }

    fn catalog_ddl_wait_error(&self, waiter: u32, blocker: u32, name: &str) -> SqlError {
        match self.wait_for_transaction(waiter, blocker) {
            Err(error) => error,
            Ok(()) => sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent DDL on \"{}\"",
                name
            ),
        }
    }

    /// Lowers reads to a command snapshot (a data-modifying `WITH` statement) or
    /// restores full own-write visibility ([`SNAPSHOT_ALL`]). Reset to
    /// `SNAPSHOT_ALL` at the start of every statement, so a snapshot never leaks.
    pub fn set_read_snapshot(&mut self, snapshot: u32) {
        self.read_snapshot = snapshot;
    }

    /// Recovery: pins the LSN to a replayed record's.
    pub fn set_lsn(&mut self, lsn: u64) {
        self.lsn = lsn;
    }

    /// Recovery: ensures freshly assigned rowids stay above replayed ones.
    pub fn observe_rowid(&mut self, rowid: u64) {
        self.next_rowid = self.next_rowid.max(rowid + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_image_obeys_an_lsn_snapshot() {
        let location = RowLoc { offset: 12, len: 4 };
        let state = RowState::committed_only_at(location, 42);
        assert_eq!(state.visible_at_lsn(7, SNAPSHOT_ALL, 41), None);
        assert_eq!(
            state.visible_at_lsn(7, SNAPSHOT_ALL, 42),
            Some(RowHome::Heap(location))
        );
        assert_eq!(
            state.visible_at_lsn(7, SNAPSHOT_ALL, 99),
            Some(RowHome::Heap(location))
        );

        // A transaction's own pending image remains governed by command ID:
        // committed-snapshot filtering applies only when no visible own image
        // overlays it.
        let mut pending = state;
        pending
            .pending
            .push(PendingChange {
                txid: 7,
                cid: 3,
                loc: Some(RowLoc { offset: 20, len: 4 }),
            })
            .unwrap();
        assert_eq!(
            pending.visible_at_lsn(7, 4, 41),
            Some(RowHome::Heap(RowLoc { offset: 20, len: 4 }))
        );
        assert_eq!(pending.visible_at_lsn(8, 4, 41), None);
    }

    #[test]
    fn comment_class_codec_rejects_unknown_values() {
        for class in [
            CommentClass::Relation,
            CommentClass::Schema,
            CommentClass::Type,
            CommentClass::Tablespace,
            CommentClass::Extension,
            CommentClass::Trigger,
        ] {
            assert_eq!(CommentClass::from_u8(class.to_u8()), Some(class));
        }
        assert_eq!(CommentClass::from_u8(6), None);
        assert_eq!(CommentClass::from_u8(u8::MAX), None);
    }

    #[test]
    fn access_class_codec_covers_every_durable_catalog_class() {
        for class in [
            AccessClass::Table,
            AccessClass::View,
            AccessClass::MaterializedView,
            AccessClass::Sequence,
            AccessClass::Schema,
            AccessClass::Domain,
            AccessClass::Enum,
            AccessClass::Index,
            AccessClass::Routine,
            AccessClass::Composite,
            AccessClass::Statistics,
            AccessClass::Tablespace,
            AccessClass::Extension,
            AccessClass::Trigger,
        ] {
            assert_eq!(AccessClass::from_u8(class as u8), Some(class));
        }
        assert_eq!(AccessClass::from_u8(u8::MAX), None);
    }

    fn test_config() -> Config {
        let mut c = Config::default_dev();
        c.memtable_bytes = 1 << 16;
        c.max_connections = 2;
        c.max_tables = 4;
        c.table_rows = 128;
        c.txn_rows = 128;
        c.value_index_rows = 512;
        c.max_value_indexes = 8;
        c.max_extension_scripts = 8;
        c.extension_script_bytes = 1 << 16;
        c
    }

    fn test_budget(config: &Config) -> Budget {
        Budget::new(config.memtable_bytes + Storage::extra_budget_bytes(config) + (1 << 20))
    }

    fn make_def(name: &str, columns: &[(&str, ColType, bool)]) -> TableDef {
        let mut def = TableDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse(name).unwrap(),
            columns: [ColumnMeta {
                name: SqlName::parse("").unwrap(),
                ctype: ColType::Bool,
                type_mod: -1,
                collation: Collation::None,
                not_null: NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type: None,
            }; MAX_COLUMNS],
            n_columns: columns.len(),
            ..TableDef::empty()
        };
        for (i, (n, t, nn)) in columns.iter().enumerate() {
            def.columns[i] = ColumnMeta {
                name: SqlName::parse(n).unwrap(),
                ctype: *t,
                type_mod: -1,
                collation: if t.is_collatable() {
                    Collation::Default
                } else {
                    Collation::None
                },
                not_null: NotNullOrigin::local(*nn),
                unique: false,
                primary: false,
                auto_increment: false,
                default: ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type: None,
            };
        }
        def
    }

    #[test]
    fn stored_query_dependency_arrays_live_outside_catalog_definitions() {
        assert!(size_of::<ViewDef>() < size_of::<StoredQueryDependencies>());
        assert!(size_of::<MatviewDef>() < size_of::<StoredQueryDependencies>());
    }

    #[test]
    fn user_type_identity_is_atomic() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget).unwrap();
        let mut column = ColumnMeta::EMPTY;
        column.ctype = ColType::Enum(0);
        assert_eq!(
            storage
                .declared_column_type(&column, 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROTOCOL_VIOLATION
        );
        column.user_type = Some(UserTypeName {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("missing_type").unwrap(),
        });
        assert_eq!(
            storage
                .declared_column_type(&column, 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROTOCOL_VIOLATION
        );
    }

    #[test]
    fn create_find_drop_reuse() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut s = Storage::new(&config, &mut budget).unwrap();
        let def = make_def("t1", &[("id", ColType::Int4, true)]);
        let index = s.create_table(def).unwrap();
        assert_eq!(s.find_table("public", "t1"), Some(index));
        assert_eq!(
            s.create_table(def).unwrap_err().sqlstate,
            sqlstate::DUPLICATE_TABLE
        );
        s.drop_table(index);
        assert_eq!(s.find_table("public", "t1"), None);
        // Slot is reusable; capacity is enforced.
        for i in 0..4u32 {
            let name = crate::stack_format!(8, "x{}", i);
            s.create_table(make_def(name.as_str(), &[("a", ColType::Bool, false)]))
                .unwrap();
        }
        let err = s
            .create_table(make_def("overflow", &[("a", ColType::Bool, false)]))
            .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
    }

    #[test]
    fn published_generation_cleanup_keeps_newer_table_writes_dirty() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let slot = storage
            .create_table(make_def(
                "checkpoint_generation",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let captured = storage.table(slot).generation;
        storage.table_mut(slot).mark_dirty();
        storage.clear_dirty_through(&[captured]);
        assert!(storage.table(slot).dirty);
        let current = storage.table(slot).generation;
        storage.clear_dirty_through(&[current]);
        assert!(!storage.table(slot).dirty);
    }

    #[test]
    fn replay_table_rewrite_marker_must_be_paired_exactly_once() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage
            .create_table(make_def("t", &[("id", ColType::Int4, true)]))
            .unwrap();
        let mut column_mapping = [u16::MAX; MAX_COLUMNS];
        column_mapping[0] = 0;

        storage
            .begin_replay_table_rewrite("public", "t", false, column_mapping)
            .unwrap();
        assert_eq!(
            storage
                .begin_replay_table_rewrite("public", "t", false, column_mapping)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert_eq!(
            storage
                .ensure_no_pending_replay_table_rewrite()
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert!(
            storage
                .complete_replay_table_rewrite(make_def("t", &[("id", ColType::Int4, true)]))
                .unwrap()
        );
        storage.ensure_no_pending_replay_table_rewrite().unwrap();
    }

    #[test]
    fn catalog_ddl_state_has_one_visibility_rule() {
        assert!(!CatalogDdlState::Absent.visible_to(1));
        assert!(CatalogDdlState::Present.visible_to(1));
        assert!(CatalogDdlState::PendingCreate { txid: 1 }.visible_to(1));
        assert!(!CatalogDdlState::PendingCreate { txid: 1 }.visible_to(2));
        assert!(!CatalogDdlState::PendingDrop { txid: 1 }.visible_to(1));
        assert!(CatalogDdlState::PendingDrop { txid: 1 }.visible_to(2));
    }

    #[test]
    fn catalog_ddl_state_transitions_preserve_the_committed_baseline() {
        assert_eq!(
            CatalogDdlState::PendingCreate { txid: 1 }.commit_create(),
            CatalogDdlState::Present
        );
        assert_eq!(
            CatalogDdlState::PendingDrop { txid: 1 }.commit_drop(),
            CatalogDdlState::Absent
        );
        assert_eq!(
            CatalogDdlState::PendingCreate { txid: 1 }.rollback_create(),
            CatalogDdlState::Absent
        );
        assert_eq!(
            CatalogDdlState::PendingDrop { txid: 1 }.rollback_drop(1),
            CatalogDdlState::Present
        );
        let created_then_dropped = CatalogDdlState::PendingCreate { txid: 1 }.drop_by(1);
        assert_eq!(
            created_then_dropped,
            CatalogDdlState::PendingCreateDrop { txid: 1 }
        );
        assert_eq!(
            created_then_dropped.rollback_drop(1),
            CatalogDdlState::PendingCreate { txid: 1 }
        );
        assert_eq!(
            created_then_dropped.commit_create().commit_drop(),
            CatalogDdlState::Absent
        );
        assert_eq!(
            CatalogDdlState::Present.drop_by(1),
            CatalogDdlState::PendingDrop { txid: 1 }
        );
    }

    #[test]
    fn heap_append_and_full() {
        let mut config = test_config();
        config.memtable_bytes = 64;
        let mut budget = test_budget(&config);
        let mut s = Storage::new(&config, &mut budget).unwrap();
        let (loc, slice) = s.heap.append(10).unwrap();
        slice.copy_from_slice(b"0123456789");
        assert_eq!(s.heap.get(loc), b"0123456789");
        let err = s.heap.append(60).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
    }

    #[test]
    fn heap_compaction_preserves_rows_of_a_pending_create() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let slot = storage
            .create_table_in(make_def("pending", &[("id", ColType::Int4, true)]), 7)
            .unwrap();
        let _ = storage.heap.append(8).unwrap();
        let (location, bytes) = storage.heap.append(7).unwrap();
        bytes.copy_from_slice(b"pending");
        let mut state = RowState {
            committed: None,
            committed_lsn: 0,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        };
        state
            .pending
            .push(PendingChange {
                txid: 7,
                cid: 1,
                loc: Some(location),
            })
            .unwrap();
        storage.table_mut(slot).rows.insert(1, state).unwrap();
        let mut scratch = FixedVec::new(&mut budget, "compact", 8).unwrap();

        storage.compact_heap(&mut scratch).unwrap();

        let compacted = storage
            .table(slot)
            .rows
            .get(&1)
            .unwrap()
            .pending
            .last()
            .unwrap()
            .loc
            .unwrap();
        assert_eq!(compacted.offset, 0);
        assert_eq!(storage.heap.get(compacted), b"pending");
        let (_, replacement) = storage.heap.append(7).unwrap();
        replacement.copy_from_slice(b"replace");
        assert_eq!(storage.heap.get(compacted), b"pending");
    }

    #[test]
    fn name_length_limit() {
        let long = "x".repeat(64);
        assert!(SqlName::parse(&long).is_err());
        let ok = "y".repeat(63);
        assert_eq!(SqlName::parse(&ok).unwrap().as_str(), ok);
    }

    #[test]
    fn replication_slots_are_bounded_and_have_resume_positions() {
        let mut config = test_config();
        config.max_replication_slots = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage
            .create_replication_slot(
                ReplicationSlotName::parse("changes").unwrap(),
                42,
                ReplicationSlotBehavior::DEFAULT,
            )
            .unwrap();
        let slot = storage.replication_slot("changes").unwrap();
        assert_eq!(slot.restart_lsn, 42);
        assert_eq!(slot.confirmed_flush_lsn, 42);
        assert!(!slot.active);
        assert_eq!(
            storage
                .create_replication_slot(
                    ReplicationSlotName::parse("other").unwrap(),
                    43,
                    ReplicationSlotBehavior::DEFAULT,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        let advance = storage
            .prepare_replication_slot_advance("changes", 47)
            .unwrap();
        storage.apply_replication_slot_advance(advance);
        let slot = storage.replication_slot("changes").unwrap();
        assert_eq!(slot.restart_lsn, 47);
        assert_eq!(slot.confirmed_flush_lsn, 47);
        assert_eq!(storage.oldest_replication_restart_lsn(), Some(47));
        storage.drop_replication_slot("changes").unwrap();
        assert!(storage.replication_slot("changes").is_none());
    }

    #[test]
    fn column_default_encodes_one_execution_state() {
        let expression = StackStr::from_str("7");
        assert!(matches!(
            ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), Some(expression), false),
            Some(ColumnDefault::Constant { .. })
        ));
        assert!(matches!(
            ColumnDefault::from_parts(None, Some(expression), false),
            Some(ColumnDefault::Expression(_))
        ));
        assert!(matches!(
            ColumnDefault::from_parts(None, Some(expression), true),
            Some(ColumnDefault::Generated(_))
        ));
        assert!(ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), None, true).is_none());
        assert!(ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), None, false).is_none());
        assert!(
            ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), Some(expression), true).is_none()
        );
    }

    #[test]
    fn publication_filters_share_one_bounded_catalog_payload() {
        let source = [StackStr::from_str("id > 0"), StackStr::from_str("id < 10")];
        let filters = PublicationFilters::from_sql(&source).unwrap();
        assert_eq!(filters.get(0), "id > 0");
        assert_eq!(filters.get(1), "id < 10");

        let member =
            StackStr::from_str("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
        let too_large = [member; PUBLICATION_FILTER_STORAGE_BYTES / PUBLICATION_FILTER_SQL_MAX + 1];
        assert_eq!(
            PublicationFilters::from_sql(&too_large)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
    }
}
