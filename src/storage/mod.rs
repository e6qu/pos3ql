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
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::mem::value_index::ValueIndexPool;
use crate::sql::eval::{SqlError, hash_key, sqlstate};
use crate::sql::types::{ArrElem, ColType, Datum};
use crate::sql_err;
use crate::store::BlockStore;
use crate::util::StackStr;

pub(crate) use rowenc::MAX_COLUMNS;

/// Maximum explicit relation membership entries in one publication.  WAL
/// encodes this count in one byte, so the capacity derives from that boundary
/// instead of permitting an unencodable 256th member.
pub(crate) const MAX_PUBLICATION_TABLES: usize = u8::MAX as usize;

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
    Int8(i64),
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

pub(crate) const MAX_DEFAULT_TEXT: usize = 48;

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
    /// A pre-source journal entry. New DDL never constructs this variant;
    /// keeping it explicit lets recovery preserve an older durable default
    /// without admitting incomplete state into new catalog writes.
    LegacyConstant(OwnedDatum),
    Expression(StackStr<DEFAULT_EXPR_MAX>),
    Generated(StackStr<DEFAULT_EXPR_MAX>),
}

impl ColumnDefault {
    pub const NONE: Self = Self::None;

    pub const fn expression(&self) -> Option<&StackStr<DEFAULT_EXPR_MAX>> {
        match self {
            Self::None | Self::LegacyConstant(_) => None,
            Self::Constant { expression, .. }
            | Self::Expression(expression)
            | Self::Generated(expression) => Some(expression),
        }
    }

    pub const fn constant(&self) -> Option<&OwnedDatum> {
        match self {
            Self::Constant { value, .. } | Self::LegacyConstant(value) => Some(value),
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
            (Some(value), None, false) => Some(Self::LegacyConstant(value)),
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
            Datum::Record(_) => {
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
            Datum::Null => Self::Null,
            Datum::Bool(b) => Self::Bool(*b),
            Datum::Int4(v) => Self::Int4(*v),
            Datum::Int2(v) => Self::Int4(*v as i32),
            Datum::Int8(v) => Self::Int8(*v),
            // Widened like int2→int4; the column re-coerces the default back to
            // real (f64→f32 is lossless for a value that was already f32).
            Datum::Float4(v) => Self::Float8(f64::from(*v)),
            Datum::Float8(v) => Self::Float8(*v),
            Datum::Date(_)
            | Datum::Timestamp(_)
            | Datum::Timestamptz(_)
            | Datum::Time(_)
            | Datum::Timetz(..)
            | Datum::Interval(_)
            | Datum::Json { .. }
            | Datum::Array { .. }
            | Datum::Range { .. }
            | Datum::Multirange { .. }
            | Datum::Bit { .. }
            | Datum::Uuid(_)
            | Datum::Bytea(_) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "defaults of this type are not supported yet (store as text)"
                ));
            }
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
            Self::Int8(v) => Datum::Int8(*v),
            Self::Float8(v) => Datum::Float8(*v),
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
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnMeta {
    pub name: SqlName,
    pub ctype: ColType,
    /// PostgreSQL atttypmod: -1 = none. varchar(n)/char(n) encode `n + 4`;
    /// numeric(p,s) encodes `((p<<16)|s) + 4`. Enforced during coercion.
    pub type_mod: i32,
    pub not_null: bool,
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
        not_null: false,
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

/// A multi-column UNIQUE or PRIMARY KEY constraint. Single-column PK/UNIQUE
/// declared inline on a column stays on that column's flags; this covers the
/// multi-column table-level form.
#[derive(Debug, Clone, Copy)]
pub struct UniqueKey {
    pub name: SqlName,
    pub columns: [u16; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub is_primary: bool,
}

impl UniqueKey {
    pub const EMPTY: Self = UniqueKey {
        name: SqlName::EMPTY,
        columns: [0u16; MAX_INDEX_COLS],
        n_cols: 0,
        is_primary: false,
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
}

impl CheckConstraint {
    pub const EMPTY: Self = CheckConstraint {
        name: SqlName::EMPTY,
        expression: StackStr::new(),
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
}

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
    /// absent but the entry shadows an older SST row. Zero means the image
    /// predates LSN-versioned row metadata (legacy checkpoint/cold start) and
    /// is therefore visible to every later snapshot.
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
        /// Exact immutable version to fetch. Legacy rowid-only SSTs use zero.
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
    /// a durable commit-LSN snapshot. Object-resident fallback belongs to
    /// `Storage::visible_row_home_at`, the engine-wide visibility choke point.
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

/// Distinctness information for one composite value-index key. NULL-bearing
/// rows are counted separately because SQL equality cannot match them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MultiColumnStatistics {
    pub(crate) valid: bool,
    pub(crate) columns: [u16; MAX_INDEX_COLS],
    pub(crate) n_columns: u8,
    pub(crate) non_null_rows: u64,
    pub(crate) distinct_values: u64,
}

impl MultiColumnStatistics {
    pub(crate) const EMPTY: Self = Self {
        valid: false,
        columns: [0; MAX_INDEX_COLS],
        n_columns: 0,
        non_null_rows: 0,
        distinct_values: 0,
    };

    pub(crate) fn covers(&self, columns: &[usize]) -> bool {
        self.valid
            && self.n_columns as usize == columns.len()
            && self.columns[..columns.len()]
                .iter()
                .all(|column| columns.contains(&usize::from(*column)))
    }
}

/// Table cardinality and width statistics used by the storage-aware planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableStatistics {
    pub(crate) valid: bool,
    pub(crate) rows: u64,
    pub(crate) average_row_width: u32,
    pub(crate) analyzed_generation: u64,
    pub(crate) columns: [ColumnStatistics; MAX_COLUMNS],
    pub(crate) multi_columns: [MultiColumnStatistics; MAX_MULTICOLUMN_STATISTICS],
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
        multi_columns: [MultiColumnStatistics::EMPTY; MAX_MULTICOLUMN_STATISTICS],
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

/// Composite statistics are collected only for the bounded composite keys the
/// planner can actually seek through the provider-neutral value-index path.
pub(crate) const MAX_MULTICOLUMN_STATISTICS: usize = MAX_VALUE_ENFORCERS;

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
}

impl DependencyClass {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Table),
            2 => Some(Self::View),
            3 => Some(Self::Domain),
            4 => Some(Self::Enum),
            5 => Some(Self::Sequence),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredQueryDependency {
    pub class: DependencyClass,
    pub slot: u16,
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

impl StoredQueryDependency {
    pub(crate) const EMPTY: Self = Self {
        class: DependencyClass::Table,
        slot: 0,
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

impl StoredQueryDependencies {
    pub(crate) const EMPTY: Self = Self {
        entries: [StoredQueryDependency::EMPTY; MAX_STORED_QUERY_DEPENDENCIES],
        len: 0,
    };

    pub fn entries(&self) -> &[StoredQueryDependency] {
        &self.entries[..self.len as usize]
    }

    pub fn push(&mut self, dependency: StoredQueryDependency) -> Result<(), SqlError> {
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

    pub fn serialized_push(
        &mut self,
        class: DependencyClass,
        schema: SqlName,
        name: SqlName,
        referenced_schema: SqlName,
        referenced_name: SqlName,
    ) -> Result<(), SqlError> {
        if self.len as usize == MAX_STORED_QUERY_DEPENDENCIES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query depends on more than {} catalog objects",
                MAX_STORED_QUERY_DEPENDENCIES
            ));
        }
        self.entries[self.len as usize] = StoredQueryDependency {
            class,
            slot: u16::MAX,
            referenced_columns: 0,
            schema,
            name,
            referenced_schema,
            referenced_name,
        };
        self.len += 1;
        Ok(())
    }

    pub fn serialized_push_with_columns(
        &mut self,
        class: DependencyClass,
        schema: SqlName,
        name: SqlName,
        referenced_schema: SqlName,
        referenced_name: SqlName,
        referenced_columns: u64,
    ) -> Result<(), SqlError> {
        self.serialized_push(class, schema, name, referenced_schema, referenced_name)?;
        self.entries[self.len as usize - 1].referenced_columns = referenced_columns;
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
    pub ownership: Ownership,
    ddl_state: CatalogDdlState,
}

impl ViewDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }
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
    pub table_count: usize,
    pub schemas: [u8; MAX_SCHEMAS],
    pub schema_count: usize,
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
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
    pub table_count: usize,
    pub schemas: [u8; MAX_SCHEMAS],
    pub schema_count: usize,
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
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
    pub schemas: &'a [u8],
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
}

/// Durable state required to resume a logical replication consumer. A slot is
/// database-scoped and deliberately carries only the pgoutput-compatible
/// fields; physical XLOG slots are outside pos3ql's object-native design.
#[derive(Clone, Copy)]
pub(crate) struct ReplicationSlotDef {
    pub name: SqlName,
    pub restart_lsn: u64,
    pub confirmed_flush_lsn: u64,
    pub active: bool,
    pub live: bool,
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
            table_count: self.table_count,
            schemas: self.schemas,
            schema_count: self.schema_count,
            publish_insert: self.publish_insert,
            publish_update: self.publish_update,
            publish_delete: self.publish_delete,
            publish_truncate: self.publish_truncate,
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
        self.table_count = definition.table_count;
        self.schemas = definition.schemas;
        self.schema_count = definition.schema_count;
        self.publish_insert = definition.publish_insert;
        self.publish_update = definition.publish_update;
        self.publish_delete = definition.publish_delete;
        self.publish_truncate = definition.publish_truncate;
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
/// User-defined routine OIDs occupy a stable, disjoint catalog range.
pub(crate) const ROUTINE_OID_BASE: i32 = 100_000;

pub(crate) fn routine_oid(routine: &RoutineDef) -> i32 {
    ROUTINE_OID_BASE
        .checked_add(i32::try_from(routine.created_at).expect("routine OID range exhausted"))
        .expect("routine OID range exhausted")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutineArgumentDef {
    pub name: SqlName,
    pub ctype: ColType,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineSpec {
    pub identity: RoutineIdentity,
    pub schema: SqlName,
    pub name: SqlName,
    pub arguments: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub argument_count: usize,
    pub kind: RoutineKind,
    pub result_columns: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub result_column_count: usize,
    pub body: StackStr<ROUTINE_SQL_MAX>,
}

/// A routine's invocation contract. Keeping a function result inside the
/// function variant makes a procedure with a fabricated scalar result
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineKind {
    Function { result: ColType },
    SetFunction { result: ColType },
    TableFunction,
    Procedure,
}

impl RoutineKind {
    pub(crate) const fn function_result(self) -> Option<ColType> {
        match self {
            Self::Function { result } | Self::SetFunction { result } => Some(result),
            Self::TableFunction => Some(ColType::Record),
            Self::Procedure => None,
        }
    }

    pub(crate) const fn is_set_returning(self) -> bool {
        matches!(self, Self::SetFunction { .. } | Self::TableFunction)
    }

    pub(crate) const fn catalog_kind(self) -> &'static str {
        match self {
            Self::Function { .. } | Self::SetFunction { .. } | Self::TableFunction => "f",
            Self::Procedure => "p",
        }
    }

    pub(crate) const fn wire_code(self) -> u8 {
        match self {
            Self::Function { .. } => 0,
            Self::SetFunction { .. } => 2,
            Self::TableFunction => 3,
            Self::Procedure => 1,
        }
    }

    pub(crate) const fn from_wire_code(code: u8, result: ColType) -> Option<Self> {
        match code {
            0 => Some(Self::Function { result }),
            1 => Some(Self::Procedure),
            2 => Some(Self::SetFunction { result }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum RoutineCallKind {
    Scalar,
    Set,
    Procedure,
}

impl RoutineCallKind {
    const fn accepts(self, kind: RoutineKind) -> bool {
        match self {
            Self::Scalar => kind.function_result().is_some() && !kind.is_set_returning(),
            Self::Set => kind.is_set_returning(),
            Self::Procedure => matches!(kind, RoutineKind::Procedure),
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
    pub arguments: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub argument_count: usize,
    pub kind: RoutineKind,
    pub(crate) result_columns: [RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
    pub(crate) result_column_count: usize,
    pub body: StackStr<ROUTINE_SQL_MAX>,
    pub ownership: Ownership,
    pub ddl_state: CatalogDdlState,
}

impl RoutineDef {
    pub(crate) const EMPTY: Self = Self {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        pending_identity: None,
        arguments: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
        argument_count: 0,
        kind: RoutineKind::Function {
            result: ColType::Text,
        },
        result_columns: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
        result_column_count: 0,
        body: StackStr::new(),
        ownership: Ownership::BOOTSTRAP,
        ddl_state: CatalogDdlState::Absent,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn arguments(&self) -> &[RoutineArgumentDef] {
        &self.arguments[..self.argument_count]
    }

    pub(crate) fn table_columns(&self) -> Option<&[RoutineArgumentDef]> {
        matches!(self.kind, RoutineKind::TableFunction)
            .then_some(&self.result_columns[..self.result_column_count])
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

/// A named btree index over a table's columns.
#[derive(Clone, Copy)]
pub struct IndexDef {
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
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingIndexName {
    pub txid: u32,
    pub name: SqlName,
}

/// How many schemas may exist at once, including the built-in "public".
pub(crate) const MAX_SCHEMAS: usize = 32;

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
/// covers built-in and user-defined rows of `pg_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommentClass {
    Relation,
    Schema,
    Type,
}

impl CommentClass {
    pub fn to_u8(self) -> u8 {
        match self {
            CommentClass::Relation => 0,
            CommentClass::Schema => 1,
            CommentClass::Type => 2,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => CommentClass::Relation,
            1 => CommentClass::Schema,
            2 => CommentClass::Type,
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
        }
    }

    fn matches(&self, class: CommentClass, schema: &str, name: &str, subid: u32) -> bool {
        self.used
            && self.class == class
            && self.subid == subid
            && self.name.as_str() == name
            && self.schema.as_str() == schema
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
    column_mapping: [u16; MAX_COLUMNS],
}

pub struct Storage {
    pub heap: RowHeap,
    tables: FixedVec<Table>,
    pending_table_defs: FixedVec<PendingTableDefSlot>,
    pending_table_statistics: FixedVec<PendingTableStatisticsSlot>,
    views: FixedVec<ViewDef>,
    routines: FixedVec<RoutineDef>,
    publications: FixedVec<PublicationDef>,
    replication_slots: FixedVec<ReplicationSlotDef>,
    view_dependencies: FixedVec<StoredQueryDependencies>,
    matviews: FixedVec<MatviewDef>,
    matview_dependencies: FixedVec<StoredQueryDependencies>,
    sequences: FixedVec<SequenceDef>,
    domains: FixedVec<DomainDef>,
    enums: FixedVec<EnumDef>,
    indexes: FixedVec<IndexDef>,
    schemas: FixedVec<SchemaDef>,
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
    pax_value_cursors: [usize; MAX_COLUMNS],
    head_pax_values: [Option<(usize, usize)>; MAX_COLUMNS],
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
    }

    /// Bytes drawn beyond the row heap itself, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        config.max_tables
            * (size_of::<Table>()
                + FixedMap::<u64, RowState>::budget_bytes(config.table_rows)
                + size_of::<ViewDef>()
                + size_of::<RoutineDef>()
                + size_of::<StoredQueryDependencies>()
                + size_of::<MatviewDef>()
                + size_of::<StoredQueryDependencies>()
                + size_of::<IndexDef>())
            + config.max_replication_slots * size_of::<ReplicationSlotDef>()
            + config.max_tables * MAX_PENDING_TABLE_DEFS * size_of::<PendingTableDefSlot>()
            + config.max_tables * MAX_PENDING_TABLE_DEFS * size_of::<PendingTableStatisticsSlot>()
            + MAX_SCHEMAS * size_of::<SchemaDef>()
            + MAX_ROLES * size_of::<RoleDef>()
            + MAX_ROLE_MEMBERSHIPS * size_of::<RoleMembership>()
            + MAX_ACL_ENTRIES * size_of::<AclEntry>()
            + MAX_DEFAULT_ACL_ENTRIES * size_of::<DefaultAclEntry>()
            + MAX_SEQUENCES * size_of::<SequenceDef>()
            + MAX_DOMAINS * size_of::<DomainDef>()
            + MAX_ENUMS * size_of::<EnumDef>()
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
                            not_null: false,
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
                    ownership: Ownership::BOOTSTRAP,
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
                    table_count: 0,
                    schemas: [u8::MAX; MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: true,
                    publish_delete: true,
                    publish_truncate: true,
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
                    active: false,
                    live: false,
                })
                .expect("sized to max_replication_slots");
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
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    pending_name: None,
                    table: SqlName::parse("").expect("empty name fits"),
                    ownership: Ownership::BOOTSTRAP,
                    columns: [0; MAX_INDEX_COLS],
                    expressions: [None; MAX_INDEX_COLS],
                    include_columns: [0; MAX_INDEX_COLS],
                    descending: [false; MAX_INDEX_COLS],
                    nulls_first: [false; MAX_INDEX_COLS],
                    n_cols: 0,
                    n_include_cols: 0,
                    nulls_not_distinct: false,
                    predicate: None,
                    unique: false,
                    ddl_state: CatalogDdlState::Absent,
                })
                .expect("sized to max_tables");
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
            publications,
            replication_slots,
            view_dependencies,
            matviews,
            matview_dependencies,
            sequences,
            domains,
            enums,
            indexes,
            schemas,
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
        })
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
                    && view.schema.as_str() == schema
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
                (definition.schema, definition.name)
            }
            AccessClass::MaterializedView => {
                let definition = &self.matviews[slot];
                (definition.schema, definition.name)
            }
            AccessClass::Sequence => {
                let definition = &self.sequences[slot];
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
                    AccessClass::Domain | AccessClass::Enum | AccessClass::Routine
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
            .find(|c| c.matches(class, schema, name, subid))
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
            c.visible_text(txid)
                .map(|t| (c.class, c.schema.as_str(), c.name.as_str(), c.subid, t))
        })
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
            .position(|c| c.matches(class, schema.as_str(), name.as_str(), subid))
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
        if c.live.is_none() && c.pending.is_none() {
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
        let attributes = self.role(role).attributes_to(txid);
        if !attributes.superuser && !attributes.create_database {
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

    fn relation_in(&self, schema: &str, name: &str, txid: u32) -> Option<ResolvedRelation> {
        if let Some(t) = self.find_visible(schema, name, txid) {
            return Some(ResolvedRelation::Table(t));
        }
        self.views
            .iter()
            .position(|v| {
                v.visible_to(txid) && v.schema.as_str() == schema && v.name.as_str() == name
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
            pax_value_cursors: [0; MAX_COLUMNS],
            head_pax_values: [None; MAX_COLUMNS],
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
            pax_value_cursors: [0; MAX_COLUMNS],
            head_pax_values: [None; MAX_COLUMNS],
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
                    cursor.pax_value_cursors = [0; MAX_COLUMNS];
                    cursor.head_pax_values = [None; MAX_COLUMNS];
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
                    == Some(crate::store::BlockType::SstDataPaxV1)
                {
                    let layout = cursor.pax_layout.as_ref().ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX block has no validated layout"
                        )
                    })?;
                    let mut schema = [ColType::Bool; MAX_COLUMNS];
                    table.def.schema(&mut schema);
                    // PAX payload slices borrow the resident block, while the
                    // executor can retain a batch row after this cursor moves
                    // on. Pack only demanded physical values into statement
                    // storage, then decode that one stable row.
                    let header_len = 2 + layout.columns().div_ceil(8);
                    let mut full_len = header_len;
                    let mut packed_len = header_len;
                    for column in 0..layout.columns() {
                        let Some((start, end)) = cursor.head_pax_values[column] else {
                            continue;
                        };
                        let value_len = end.checked_sub(start).ok_or_else(|| {
                            sql_err!(sqlstate::INTERNAL_ERROR, "PAX value span is inverted")
                        })?;
                        full_len = full_len.checked_add(value_len).ok_or_else(|| {
                            sql_err!(sqlstate::INTERNAL_ERROR, "PAX row length overflows")
                        })?;
                        if decoded_columns.is_none_or(|columns| columns[column]) {
                            packed_len = packed_len.checked_add(value_len).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "PAX packed row length overflows"
                                )
                            })?;
                        }
                    }
                    if full_len != len as usize {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX selected row length does not match its cursor header"
                        ));
                    }
                    let encoded = arena.alloc_slice_with(packed_len, |_| 0u8).map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "spilled PAX rows exceed the statement arena; raise work_arena_bytes"
                        )
                    })?;
                    encoded[..2].copy_from_slice(&(layout.columns() as u16).to_le_bytes());
                    encoded[2..2 + layout.columns().div_ceil(8)].fill(0);
                    let values = arena
                        .alloc_slice_with(layout.columns(), |_| Datum::Null)
                        .map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "spilled PAX values exceed the statement arena; raise work_arena_bytes"
                            )
                        })?;
                    let mut copied = header_len;
                    for column in 0..layout.columns() {
                        let Some((start, end)) = cursor.head_pax_values[column] else {
                            encoded[2 + column / 8] |= 1 << (column % 8);
                            continue;
                        };
                        if decoded_columns.is_some_and(|columns| !columns[column]) {
                            encoded[2 + column / 8] |= 1 << (column % 8);
                            continue;
                        }
                        copied = copied.checked_add(end - start).ok_or_else(|| {
                            sql_err!(sqlstate::INTERNAL_ERROR, "PAX packed row length overflows")
                        })?;
                        encoded[copied - (end - start)..copied].copy_from_slice(
                            &context.member_raw_blocks[member as usize][start..end],
                        );
                    }
                    if copied != packed_len {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX packed row length does not match selected value spans"
                        ));
                    }
                    rowenc::decode(encoded, &schema[..layout.columns()], values)?;
                    SpilledRowRepresentation::Values(&*values)
                } else if cursor.loaded_type == Some(crate::store::BlockType::SstDataPaxV2) {
                    let layout = cursor.pax_layout.ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX descriptor has no validated layout"
                        )
                    })?;
                    if !layout.external_columns() {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "PAX descriptor does not name external columns"
                        ));
                    }
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
                    let handle = table.spill_ssts[member as usize].expect("cursor member exists");
                    let (copied_key, copied_tombstone, copied) = {
                        let mut blocks = spill.blocks.borrow_mut();
                        crate::store::copy_block_entry_at(
                            &mut *blocks,
                            &context.member_blocks[member as usize][..cursor.loaded_len],
                            cursor.head_offset,
                            handle.versioned,
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
                    handle.versioned,
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
                let loaded_len = if matches!(
                    loaded_type,
                    crate::store::BlockType::SstDataPaxV1 | crate::store::BlockType::SstDataPaxV2
                ) {
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
                cursor.pax_layout = matches!(
                    loaded_type,
                    crate::store::BlockType::SstDataPaxV1 | crate::store::BlockType::SstDataPaxV2
                )
                .then(|| crate::store::pax_layout(&context.member_raw_blocks[member][..raw_len]))
                .transpose()
                .map_err(spill_read_error)?;
                if cursor.pax_layout.is_some() && cursor.head.is_some() {
                    cursor.raw_row = resume_raw_row;
                }
                cursor.pax_value_cursors = cursor
                    .pax_layout
                    .as_ref()
                    .map_or([0; MAX_COLUMNS], crate::store::PaxLayout::column_starts);
                cursor.head_pax_values = [None; MAX_COLUMNS];
                cursor.loaded_type = Some(loaded_type);
                cursor.loaded = Some(cursor.ordinal);
                cursor.offset = 0;
            }
            if matches!(
                cursor.loaded_type,
                Some(crate::store::BlockType::SstDataPaxV1)
                    | Some(crate::store::BlockType::SstDataPaxV2)
            ) {
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
                if !layout.external_columns() {
                    layout
                        .advance_row_values(
                            &context.member_raw_blocks[member][..cursor.raw_len],
                            row,
                            &mut cursor.pax_value_cursors,
                            &mut cursor.head_pax_values,
                        )
                        .map_err(spill_read_error)?;
                }
                let len = if tombstone {
                    0
                } else if layout.external_columns() {
                    layout
                        .row_len(&context.member_raw_blocks[member][..cursor.raw_len], row)
                        .map_err(spill_read_error)?
                } else {
                    let column_count = layout.columns();
                    let row_len = cursor
                        .head_pax_values
                        .iter()
                        .filter_map(|value| *value)
                        .take(column_count)
                        .try_fold(2 + column_count.div_ceil(8), |total, (start, end)| {
                            total.checked_add(end - start)
                        })
                        .ok_or_else(|| {
                            sql_err!(sqlstate::INTERNAL_ERROR, "PAX row length overflows")
                        })?;
                    u32::try_from(row_len).map_err(|_| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "PAX row exceeds SST entry size")
                    })?
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
                handle.versioned,
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
        let mut multi_columns = [[0u16; MAX_INDEX_COLS]; MAX_MULTICOLUMN_STATISTICS];
        let mut multi_widths = [0usize; MAX_MULTICOLUMN_STATISTICS];
        let mut multi_refresh = [false; MAX_MULTICOLUMN_STATISTICS];
        let mut multi_non_nulls = [0u64; MAX_MULTICOLUMN_STATISTICS];
        let mut multi_registers = [[0u8; REGISTERS]; MAX_MULTICOLUMN_STATISTICS];
        let mut n_multi = 0usize;
        for binding in 0..self.tables[table_slot].n_enforcers {
            let enforcer = self.tables[table_slot].enforcers[binding].expect("enforcer exists");
            if enforcer.n_cols < 2 {
                continue;
            }
            let refresh = selected_columns.is_empty()
                || enforcer
                    .columns()
                    .iter()
                    .all(|column| selected[*column as usize]);
            let slot = n_multi;
            multi_columns[slot][..enforcer.n_cols].copy_from_slice(enforcer.columns());
            multi_widths[slot] = enforcer.n_cols;
            multi_refresh[slot] = refresh;
            n_multi += 1;
        }
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
                for multi in 0..n_multi {
                    if !multi_refresh[multi]
                        || multi_columns[multi][..multi_widths[multi]]
                            .iter()
                            .any(|column| values[*column as usize].is_null())
                    {
                        continue;
                    }
                    multi_non_nulls[multi] = multi_non_nulls[multi].saturating_add(1);
                    add_distinct(
                        &mut multi_registers[multi],
                        hash_key(&values, &multi_columns[multi][..multi_widths[multi]]),
                    );
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
        for multi in 0..n_multi {
            if !multi_refresh[multi] {
                continue;
            }
            let distinct_values =
                distinct_estimate(&multi_registers[multi]).min(multi_non_nulls[multi]);
            statistics.multi_columns[multi] = MultiColumnStatistics {
                valid: rows != 0,
                columns: multi_columns[multi],
                n_columns: multi_widths[multi] as u8,
                non_null_rows: multi_non_nulls[multi],
                distinct_values,
            };
        }
        for multi in n_multi..statistics.multi_columns.len() {
            statistics.multi_columns[multi] = MultiColumnStatistics::EMPTY;
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
                out[n_out] = (i, hash_key(&values, columns));
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
            Ok((len, hash_key(&values, enforcer.columns())))
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
        // A dropped or reshaped composite key must not leave a planner-visible
        // joint statistic behind. Pending CREATE INDEX ownership is private,
        // so only reconcile committed/startup cache shapes here.
        if txid.is_none() {
            let mut changed = false;
            for statistics in &mut self.tables[table_index].statistics.multi_columns {
                if statistics.valid
                    && !want[..n_want].iter().any(|(columns, n_columns)| {
                        *n_columns == statistics.n_columns as usize
                            && columns[..*n_columns]
                                == statistics.columns[..statistics.n_columns as usize]
                    })
                {
                    *statistics = MultiColumnStatistics::EMPTY;
                    changed = true;
                }
            }
            if changed {
                self.tables[table_index].statistics_dirty = true;
                self.tables[table_index].statistics_wal_dirty = true;
            }
        }
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
        self.tables[index].rows.clear();
        self.tables[index].statistics = TableStatistics::EMPTY;
        self.tables[index].statistics_wal_dirty = false;
        self.set_spill_list(index, &[]);
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
        name: SqlName,
        restart_lsn: u64,
    ) -> Result<usize, SqlError> {
        if self
            .replication_slots
            .iter()
            .any(|slot| slot.live && slot.name == name)
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
            name,
            restart_lsn,
            confirmed_flush_lsn: restart_lsn,
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
        name: SqlName,
        restart_lsn: u64,
        confirmed_flush_lsn: u64,
    ) -> Result<(), SqlError> {
        if confirmed_flush_lsn < restart_lsn {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "replication slot confirmed LSN precedes restart LSN"
            ));
        }
        let slot = self.create_replication_slot(name, restart_lsn)?;
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
        let mut schemas = [u8::MAX; MAX_SCHEMAS];
        schemas[..spec.schemas.len()].copy_from_slice(spec.schemas);
        self.catalog_seq += 1;
        self.publications[slot] = PublicationDef {
            created_at: self.catalog_seq,
            name: spec.name,
            pending_name: None,
            all_tables: spec.all_tables,
            tables: members,
            table_count: spec.tables.len(),
            schemas,
            schema_count: spec.schemas.len(),
            publish_insert: spec.publish_insert,
            publish_update: spec.publish_update,
            publish_delete: spec.publish_delete,
            publish_truncate: spec.publish_truncate,
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
        self.views
            .iter()
            .find(|v| v.visible_to(txid) && v.schema.as_str() == schema && v.name.as_str() == name)
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
        self.sequences
            .iter()
            .find(|s| s.visible_to(txid) && s.schema.as_str() == schema && s.name.as_str() == name)
    }

    pub fn sequence_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.sequences.iter().position(|s| {
            s.visible_to(txid) && s.schema.as_str() == schema && s.name.as_str() == name
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

    /// Whether a relation of this name is visible to `txid` in `schema` — a
    /// table (including a matview's backing table), view, or sequence. Sequences
    /// share PostgreSQL's relation namespace, so CREATE SEQUENCE collides with
    /// any of them (42P07).
    pub fn relation_name_taken(&self, schema: &str, name: &str, txid: u32) -> bool {
        self.relation_in(schema, name, txid).is_some()
            || self.find_sequence(schema, name, txid).is_some()
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
        spec: SeqSpec,
        owner: Option<SequenceOwner>,
        generator_for: Option<SequenceOwner>,
        restart: Option<i64>,
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
        let (last_value, is_called) =
            restart.map_or((last_value, is_called), |value| (value, false));
        sequence.pending_definition = Some(PendingSequenceDefinition {
            txid,
            spec,
            owner,
            generator_for,
            last_value,
            is_called,
        });
        sequence.pending_last_value.set(last_value);
        sequence.pending_is_called.set(is_called);
        sequence.pending_dirty.set(restart.is_some());
        Ok(prior)
    }

    pub(crate) fn commit_sequence_alter(&mut self, slot: usize, txid: u32) {
        if self.sequences[slot]
            .pending_definition
            .filter(|pending| pending.txid == txid)
            .is_some()
        {
            let last_value = self.sequences[slot].pending_last_value.get();
            let is_called = self.sequences[slot].pending_is_called.get();
            let definition = self.sequences[slot].definition_for(txid);
            self.sequences[slot] = definition;
            self.sequences[slot].last_value.set(last_value);
            self.sequences[slot].is_called.set(is_called);
            self.sequences[slot].dirty.set(false);
            self.sequences[slot].pending_dirty.set(false);
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
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.domains.iter().find_map(|d| {
            (d.schema.as_str() == schema.as_str() && d.name.as_str() == name.as_str())
                .then_some(d.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if let Some(blocker) = self.enums.iter().find_map(|e| {
            (e.schema == schema
                && (e.name == name
                    || e.pending_definition
                        .is_some_and(|pending| pending.name == name)))
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
            e.visible_to(txid) && e.schema == schema && e.definition_for(txid).name == name
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
                    && e.schema.as_str() == schema
                    && e.definition_for(txid).name.as_str() == name
            });
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema = self.schemas[*slot as usize].name;
                if let Some(i) = self.enums.iter().position(|e| {
                    e.visible_to(txid)
                        && e.schema.as_str() == schema.as_str()
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
                && e.schema.as_str() == schema
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
            let same_name = e.schema == schema
                && (e.name == name
                    || e.pending_definition
                        .is_some_and(|pending| pending.name == name));
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
            e.visible_to(txid) && e.schema == schema && e.definition_for(txid).name == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        if let Some(blocker) = self.domains.iter().find_map(|d| {
            (d.schema == schema && d.name == name)
                .then_some(d.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if self
            .domains
            .iter()
            .any(|d| d.visible_to(txid) && d.schema == schema && d.name == name)
        {
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
        let schema = self.enums[slot].schema;
        if let Some(blocker) = self
            .enums
            .iter()
            .enumerate()
            .find_map(|(other_slot, other)| {
                (other_slot != slot
                    && other.schema == schema
                    && (other.name == definition.name
                        || other
                            .pending_definition
                            .is_some_and(|pending| pending.name == definition.name)))
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
            (domain.schema == schema && domain.name == definition.name)
                .then_some(domain.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, definition.name.as_str()));
        }
        if self.enums.iter().enumerate().any(|(other_slot, other)| {
            other_slot != slot
                && other.visible_to(txid)
                && other.schema == schema
                && other.definition_for(txid).name == definition.name
        }) || self.domains.iter().any(|domain| {
            domain.visible_to(txid) && domain.schema == schema && domain.name == definition.name
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
            if definition.name != self.enums[slot].name {
                self.rename_enum_references(slot, definition.name);
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

    /// Renames an enum and every persisted reference to its type name. Runtime
    /// slots and value sort keys stay stable; comments are name-keyed and move
    /// with the type just as PostgreSQL keeps the same `pg_type` OID.
    fn rename_enum_references(&mut self, slot: usize, new_name: SqlName) {
        let old_name = self.enums[slot].name;
        let schema = self.enums[slot].schema;
        self.enums[slot].name = new_name;
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
                        identity.name = new_name;
                    }
                    changed = true;
                }
            }
            if changed {
                table.mark_dirty();
            }
        }
        for comment in self.comments.iter_mut() {
            if comment.used
                && comment.class == CommentClass::Type
                && comment.schema == schema
                && comment.name == old_name
            {
                comment.name = new_name;
            }
        }
        self.rename_stored_query_dependency(DependencyClass::Enum, slot, schema, new_name);
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
            (v.schema.as_str() == schema.as_str() && v.name.as_str() == name.as_str())
                .then_some(v.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        let existing = self.views.iter().position(|v| {
            v.visible_to(txid)
                && v.schema.as_str() == schema.as_str()
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
            ownership,
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
            (v.schema.as_str() == schema && v.name.as_str() == name)
                .then_some(v.ddl_state.pending_txid()?)
                .filter(|&owner| owner != txid)
        }) {
            return Err(self.catalog_ddl_wait_error(txid, blocker, name));
        }
        let Some(i) = self.views.iter().position(|v| {
            v.visible_to(txid) && v.schema.as_str() == schema && v.name.as_str() == name
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

    pub(crate) fn routine_slot_by_oid(&self, oid: i32, txid: u32) -> Option<usize> {
        self.routines
            .iter()
            .position(|routine| routine.visible_to(txid) && routine_oid(routine) == oid)
    }

    pub(crate) fn routine_slot_for_call(
        &self,
        name: &str,
        arguments: &[crate::sql::types::Datum<'_>],
        txid: u32,
    ) -> Option<usize> {
        let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
        if arguments.len() > argument_types.len() {
            return None;
        }
        for (slot, argument) in arguments.iter().enumerate() {
            argument_types[slot] = crate::sql::exec::coltype_of_oid_pub(argument.type_oid())?;
        }
        self.routine_slot_for_call_types(name, &argument_types[..arguments.len()], txid)
    }

    pub(crate) fn routine_for_call_types(
        &self,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
    ) -> Option<&RoutineDef> {
        self.routine_slot_for_call_types(name, argument_types, txid)
            .map(|slot| &self.routines[slot])
    }

    pub(crate) fn routine_slot_for_call_types(
        &self,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_on_path(name, argument_types, txid, RoutineCallKind::Scalar)
    }

    pub(crate) fn has_scalar_routine_on_path(
        &self,
        name: &str,
        argument_count: usize,
        txid: u32,
    ) -> bool {
        if let Some((schema, name)) = name.split_once('.') {
            return self.routine_name_in(
                schema,
                name,
                argument_count,
                txid,
                RoutineCallKind::Scalar,
            );
        }
        self.path.entries().iter().any(|entry| {
            let PathEntry::Schema(slot) = entry else {
                return false;
            };
            self.routine_name_in(
                self.schemas[*slot as usize].name.as_str(),
                name,
                argument_count,
                txid,
                RoutineCallKind::Scalar,
            )
        })
    }

    pub(crate) fn routine_slot_for_table_call_types(
        &self,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_on_path(name, argument_types, txid, RoutineCallKind::Set)
    }

    pub(crate) fn procedure_slot_for_call_types(
        &self,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
    ) -> Option<usize> {
        self.routine_slot_on_path(name, argument_types, txid, RoutineCallKind::Procedure)
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

    fn routine_slot_in(
        &self,
        schema: &str,
        name: &str,
        argument_types: &[ColType],
        txid: u32,
        kind: RoutineCallKind,
    ) -> Option<usize> {
        self.routines.iter().position(|routine| {
            routine.visible_to(txid)
                && kind.accepts(routine.kind)
                && routine.schema_for(txid).as_str() == schema
                && routine.name_for(txid).as_str() == name
                && routine.argument_count == argument_types.len()
                && routine
                    .arguments()
                    .iter()
                    .zip(argument_types)
                    .all(|(parameter, value)| parameter.ctype == *value)
        })
    }

    fn routine_name_in(
        &self,
        schema: &str,
        name: &str,
        argument_count: usize,
        txid: u32,
        kind: RoutineCallKind,
    ) -> bool {
        self.routines.iter().any(|routine| {
            routine.visible_to(txid)
                && kind.accepts(routine.kind)
                && routine.schema_for(txid).as_str() == schema
                && routine.name_for(txid).as_str() == name
                && routine.argument_count == argument_count
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
            routine.visible_to(txid)
                && routine.schema_for(txid).as_str() == schema
                && routine.name_for(txid).as_str() == name
                && routine.argument_count == argument_types.len()
                && routine
                    .arguments()
                    .iter()
                    .zip(argument_types)
                    .all(|(argument, ctype)| argument.ctype == *ctype)
        })
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
                    .all(|(left, right)| left.ctype == right.ctype)
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
                            && candidate
                                .arguments()
                                .iter()
                                .zip(routine.arguments())
                                .all(|(left, right)| left.ctype == right.ctype)
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
        let routine = &mut self.routines[slot];
        if let Some(pending) = routine.pending_identity
            && pending.txid == txid
        {
            routine.schema = pending.schema;
            routine.name = pending.name;
            routine.pending_identity = None;
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
            kind,
            result_columns,
            result_column_count,
            body,
        } = spec;
        match kind {
            RoutineKind::TableFunction => {
                if result_column_count == 0 || result_column_count > result_columns.len() {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "table function must have between one and {} result columns",
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
                        "table function result column names must be distinct"
                    ));
                }
            }
            RoutineKind::Function { .. }
            | RoutineKind::SetFunction { .. }
            | RoutineKind::Procedure
                if result_column_count != 0 =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "only table functions may define result columns"
                ));
            }
            RoutineKind::Function { .. }
            | RoutineKind::SetFunction { .. }
            | RoutineKind::Procedure => {}
        }
        self.require_schema_create(schema.as_str(), txid)?;
        if let Some(blocker) = self.routines.iter().find_map(|routine| {
            (routine.schema_for(txid) == schema
                && routine.name_for(txid) == name
                && routine.argument_count == argument_count
                && routine.arguments()[..argument_count]
                    .iter()
                    .zip(&arguments[..argument_count])
                    .all(|(left, right)| left.ctype == right.ctype))
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
                    .all(|(left, right)| left.ctype == right.ctype)
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
            arguments,
            argument_count,
            kind,
            result_columns,
            result_column_count,
            body,
            ownership,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
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
    }

    pub(crate) fn drop_routine(&mut self, slot: usize, txid: u32) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.drop_by(txid);
    }

    pub(crate) fn commit_routine_drop(&mut self, slot: usize) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.commit_drop();
        self.clear_object_acl_entries(Self::routine_access_object(slot));
    }

    pub(crate) fn rollback_routine_drop(&mut self, slot: usize, txid: u32) {
        self.routines[slot].ddl_state = self.routines[slot].ddl_state.rollback_drop(txid);
    }

    pub(crate) fn replay_create_routine(
        &mut self,
        mut definition: RoutineDef,
    ) -> Result<(), SqlError> {
        definition.ownership = definition.ownership.committed();
        let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
        for (slot, argument) in definition.arguments().iter().enumerate() {
            argument_types[slot] = argument.ctype;
        }
        if let Some(slot) = self.routine_slot_by_signature(
            definition.schema.as_str(),
            definition.name.as_str(),
            &argument_types[..definition.argument_count],
            0,
        ) {
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
                kind: definition.kind,
                result_columns: definition.result_columns,
                result_column_count: definition.result_column_count,
                body: definition.body,
            },
            0,
        )?;
        self.commit_routine_create(slot, 0);
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
        if self.index_exists(def.schema.as_str(), def.name.as_str(), txid) {
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
        self.indexes[i] = IndexDef {
            ownership,
            ddl_state: CatalogDdlState::PendingCreate { txid },
            ..def
        };
        Ok(i)
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
        if let Some(table) = self.index_table_slot(slot) {
            self.tables[table].mark_dirty();
        }
    }

    /// The committed table named by an index slot. The definition stays in
    /// the reusable catalog slot after DROP, so this also resolves the table
    /// while finalizing a drop.
    pub fn index_table_slot(&self, slot: usize) -> Option<usize> {
        let index = self.indexes.get(slot)?;
        self.find_table(index.schema.as_str(), index.table.as_str())
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

    /// A definition-only schema move (ALTER TABLE ... SET SCHEMA): the table
    /// and its indexes change schema, and every inbound foreign key follows —
    /// deterministically, so WAL replay reproduces it from the names alone.
    pub fn move_table_schema(&mut self, index: usize, new_schema: SqlName) {
        let old_schema = self.tables[index].def.schema;
        let name = self.tables[index].def.name;
        self.tables[index].def.schema = new_schema;
        self.tables[index].mark_dirty();
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
        ] {
            assert_eq!(CommentClass::from_u8(class.to_u8()), Some(class));
        }
        assert_eq!(CommentClass::from_u8(3), None);
        assert_eq!(CommentClass::from_u8(u8::MAX), None);
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
        c
    }

    fn make_def(name: &str, columns: &[(&str, ColType, bool)]) -> TableDef {
        let mut def = TableDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse(name).unwrap(),
            columns: [ColumnMeta {
                name: SqlName::parse("").unwrap(),
                ctype: ColType::Bool,
                type_mod: -1,
                not_null: false,
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
                not_null: *nn,
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
        let mut budget = Budget::new(1 << 22);
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
        let mut budget = Budget::new(1 << 22);
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
        let mut budget = Budget::new(8 << 20);
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
        let mut budget = Budget::new(1 << 22);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage
            .create_table(make_def("t", &[("id", ColType::Int4, true)]))
            .unwrap();
        let mut column_mapping = [u16::MAX; MAX_COLUMNS];
        column_mapping[0] = 0;

        storage
            .begin_replay_table_rewrite("public", "t", column_mapping)
            .unwrap();
        assert_eq!(
            storage
                .begin_replay_table_rewrite("public", "t", column_mapping)
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
        let mut budget = Budget::new(1 << 22);
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
        let mut budget = Budget::new(1 << 22);
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
        let mut budget = Budget::new(1 << 22);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage
            .create_replication_slot(SqlName::parse("changes").unwrap(), 42)
            .unwrap();
        let slot = storage.replication_slot("changes").unwrap();
        assert_eq!(slot.restart_lsn, 42);
        assert_eq!(slot.confirmed_flush_lsn, 42);
        assert!(!slot.active);
        assert_eq!(
            storage
                .create_replication_slot(SqlName::parse("other").unwrap(), 43)
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
        assert!(
            ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), Some(expression), true).is_none()
        );
    }
}
