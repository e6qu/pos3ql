//! Table storage: the in-memory write path of the LSM.
//!
//! Row bytes live in one fixed heap (the memtable); each table maps rowid →
//! location. Updates write a new copy and repoint the map — superseded
//! bytes are reclaimed when the memtable flushes to object storage (later
//! phase). All capacities are fixed at startup.

pub(crate) mod rowenc;

use core::hash::{Hash, Hasher};

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::util::StackStr;

pub(crate) use rowenc::MAX_COLUMNS;

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
    Text { len: u8, bytes: [u8; MAX_DEFAULT_TEXT] },
    Numeric { sign: u8, weight: i16, dscale: u16, nbytes: u8, digits: [u8; MAX_DEFAULT_TEXT] },
}

pub(crate) const MAX_DEFAULT_TEXT: usize = 48;

impl OwnedDatum {
    pub fn from_datum(d: &crate::sql::types::Datum) -> Result<Self, SqlError> {
        use crate::sql::types::Datum;
        Ok(match d {
            Datum::Record(_) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot store a composite (record) value in a column"
                ))
            }
            Datum::Null => Self::Null,
            Datum::Bool(b) => Self::Bool(*b),
            Datum::Int4(v) => Self::Int4(*v),
            Datum::Int2(v) => Self::Int4(*v as i32),
            Datum::Int8(v) => Self::Int8(*v),
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
                ))
            }
            Datum::Numeric(n) => {
                if n.digits.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "numeric default too large"));
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
                Self::Text { len: s.len() as u8, bytes }
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
            Self::Numeric { sign, weight, dscale, nbytes, digits } => {
                Datum::Numeric(crate::sql::numeric::Numeric {
                    sign: match sign {
                        0 => crate::sql::numeric::Sign::Pos,
                        1 => crate::sql::numeric::Sign::Neg,
                        _ => crate::sql::numeric::Sign::NaN,
                    },
                    weight: *weight,
                    dscale: *dscale,
                    digits: &digits[..*nbytes as usize],
                })
            }
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
    /// DEFAULT value (constants only).
    pub default_value: Option<OwnedDatum>,
}

impl ColumnMeta {
    pub const EMPTY: Self = ColumnMeta {
        name: SqlName::EMPTY,
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
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

/// A row's visibility state: the committed image plus at most one
/// uncommitted change owned by a single transaction (single-threaded
/// execution means at most one writer holds a row at a time; a second
/// writer fails fast instead of blocking).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowState {
    pub committed: Option<RowHome>,
    pub pending: Option<PendingChange>,
}

/// Where a committed row's bytes live: the RAM heap, or spilled to the
/// table's checkpoint SST in the block store (fetched back through the cache
/// tiers on read). The rows *map* stays in RAM either way — it is the
/// authoritative index — so spilling moves bytes, never visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowHome {
    Heap(RowLoc),
    Spilled { len: u32, sst: u8 },
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
pub struct PendingChange {
    pub txid: u32,
    /// `None` = pending delete.
    pub loc: Option<RowLoc>,
}

impl RowState {
    pub fn committed_only(loc: RowLoc) -> Self {
        Self {
            committed: Some(RowHome::Heap(loc)),
            pending: None,
        }
    }

    pub fn committed_spilled(len: u32, sst: u8) -> Self {
        Self {
            committed: Some(RowHome::Spilled { len, sst }),
            pending: None,
        }
    }

    /// What transaction `txid` sees: its own pending change, else the
    /// committed image. `None` = row invisible.
    pub fn visible_to(&self, txid: u32) -> Option<RowHome> {
        match self.pending {
            Some(p) if p.txid == txid => p.loc.map(RowHome::Heap),
            _ => self.committed,
        }
    }

    /// Whether another transaction has an uncommitted change here.
    pub fn locked_by_other(&self, txid: u32) -> Option<u32> {
        match self.pending {
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
                "memtable is full ({} bytes); with object storage on, rows spill at the next checkpoint — retry, raise memtable_bytes, or enable s3",
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
}

/// The most delta SSTs a table accumulates before a checkpoint merges them
/// back into one — the write-amplification / read-fan-out tradeoff.
pub(crate) const MAX_SPILL_SSTS: usize = 8;

/// Deletes remembered between checkpoints; past this the next checkpoint
/// rewrites the table fully rather than lose one.
pub(crate) const MAX_TOMBSTONES: usize = 1024;

/// An uncommitted catalog change to one table, owned by one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingDdl {
    pub txid: u32,
    /// `true` = pending CREATE (committed baseline: absent), `false` = pending
    /// DROP (committed baseline: present).
    pub creating: bool,
}

impl Table {
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
    /// any — that transaction has the catalog slot for this name locked.
    pub fn ddl_locked_by_other(&self, txid: u32) -> Option<u32> {
        match self.pending_ddl {
            Some(p) if p.txid != txid => Some(p.txid),
            _ => None,
        }
    }

    /// Whether the slot is free for a fresh CREATE: no committed table, no
    /// pending DDL, and no retained rows.
    fn is_free(&self) -> bool {
        !self.live && self.pending_ddl.is_none() && self.rows.is_empty()
    }
}

/// Maximum length of a stored view definition (the SELECT text).
pub(crate) const VIEW_SQL_MAX: usize = 2048;

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
    pub live: bool,
    /// An uncommitted CREATE/DROP owned by one transaction (catalog MVCC,
    /// mirroring `Table::pending_ddl`): other transactions see `live`; the
    /// owner sees the pending existence.
    pub pending: Option<PendingDdl>,
}

impl ViewDef {
    /// Whether `txid` sees this view exist.
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }
}

/// Maximum columns in an index key.
pub(crate) const MAX_INDEX_COLS: usize = 8;

/// A named index over a table's columns. Our engine does full scans, so an
/// index never accelerates a query; it exists as a durable catalog object and,
/// when `unique`, enforces a uniqueness constraint on its column tuple.
#[derive(Clone, Copy)]
pub struct IndexDef {
    /// The schema of both the index and its table (an index always lives in
    /// its table's schema).
    pub schema: SqlName,
    pub name: SqlName,
    pub table: SqlName,
    pub columns: [u16; MAX_INDEX_COLS],
    pub n_cols: usize,
    pub unique: bool,
    pub live: bool,
    /// An uncommitted CREATE/DROP owned by one transaction (catalog MVCC,
    /// mirroring `Table::pending_ddl`).
    pub pending: Option<PendingDdl>,
}

impl IndexDef {
    /// Whether `txid` sees this index exist.
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }
}

/// How many schemas may exist at once, including the built-in "public".
pub(crate) const MAX_SCHEMAS: usize = 32;

/// A named schema (namespace for tables, views and indexes). Catalog MVCC
/// mirrors `Table`: `live` is the committed image, `pending` an uncommitted
/// CREATE/DROP owned by one transaction.
#[derive(Clone, Copy)]
pub struct SchemaDef {
    pub name: SqlName,
    pub live: bool,
    pub pending: Option<PendingDdl>,
}

impl SchemaDef {
    /// Whether `txid` sees this schema exist.
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
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
        PathContext { entries, n: 2, explicit_catalog: false }
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

pub struct Storage {
    pub heap: RowHeap,
    tables: FixedVec<Table>,
    views: FixedVec<ViewDef>,
    indexes: FixedVec<IndexDef>,
    schemas: FixedVec<SchemaDef>,
    /// The running statement's effective search path (see [`PathContext`]).
    path: PathContext,
    /// Monotonic stamp for `created_at` fields.
    catalog_seq: u64,
    next_rowid: u64,
    /// Log sequence number of the latest write; becomes the WAL position.
    lsn: u64,
    /// The read path for spilled rows: the tiered block stack shared with the
    /// checkpointer, plus owned reader scratch. `None` without object storage
    /// — then rows never spill and the heap-full error stands.
    spill: Option<SpillReader>,
}

/// Fetches spilled rows back through the cache tiers. The buffers are owned
/// and startup-reserved; the stack is shared with the checkpointer through a
/// `RefCell` (single-threaded engine, short borrows).
pub(crate) struct SpillReader {
    blocks: std::rc::Rc<std::cell::RefCell<crate::store::TieredStore<crate::store::OwnedObjectStore>>>,
    /// Two scratch sets so one consume-in-place fetch may nest inside another
    /// (a validation scan holding one row while checking it against the
    /// rest). Deeper nesting is a loud error, not a deadlock.
    scratch: [std::cell::RefCell<SpillScratch>; 2],
}

/// The reader's owned block buffers (index, data, chain assembly).
struct SpillScratch {
    index_buf: Box<[u8]>,
    data_buf: Box<[u8]>,
    assembly_buf: Box<[u8]>,
}

impl SpillReader {
    /// Startup-only: reserves the reader scratch from the budget.
    pub(crate) fn new(
        budget: &mut Budget,
        blocks: std::rc::Rc<std::cell::RefCell<crate::store::TieredStore<crate::store::OwnedObjectStore>>>,
    ) -> Result<Self, BudgetError> {
        budget.draw(
            2 * (2 * crate::store::MAX_PAYLOAD + crate::store::MAX_ASSEMBLED),
            "spill reader",
        )?;
        let fresh = || {
            std::cell::RefCell::new(SpillScratch {
                index_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                data_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                assembly_buf: vec![0u8; crate::store::MAX_ASSEMBLED].into_boxed_slice(),
            })
        };
        Ok(Self { blocks, scratch: [fresh(), fresh()] })
    }
}

impl Storage {
    /// Bytes drawn beyond the row heap itself, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        config.max_tables
            * (size_of::<Table>()
                + FixedMap::<u64, RowState>::budget_bytes(config.table_rows)
                + size_of::<ViewDef>()
                + size_of::<IndexDef>())
            + MAX_SCHEMAS * size_of::<SchemaDef>()
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let heap = RowHeap::new(budget, config.memtable_bytes)?;
        let mut tables = FixedVec::new(budget, "tables", config.max_tables)?;
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
                            default_value: None,
                        }; MAX_COLUMNS],
                        n_columns: 0,
                        ..TableDef::empty()
                    },
                    rows: FixedMap::new(budget, "table_rows", config.table_rows)?,
                    created_at: 0,
                    live: false,
                    pending_ddl: None,
                    dirty: false,
                    serial_last: [0; MAX_COLUMNS],
                    serial_dirty: false,
                    spill_ssts: [None; MAX_SPILL_SSTS],
                    n_spill_ssts: 0,
                    tombstones: [0; MAX_TOMBSTONES],
                    n_tombstones: 0,
                    tombstones_overflow: false,
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
                    live: false,
                    pending: None,
                })
                .expect("sized to max_tables");
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
                    live: i == 0,
                    pending: None,
                })
                .expect("sized to MAX_SCHEMAS");
        }
        let mut indexes = FixedVec::new(budget, "indexes", config.max_tables)?;
        for _ in 0..config.max_tables {
            indexes
                .push(IndexDef {
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    table: SqlName::parse("").expect("empty name fits"),
                    columns: [0; MAX_INDEX_COLS],
                    n_cols: 0,
                    unique: false,
                    live: false,
                    pending: None,
                })
                .expect("sized to max_tables");
        }
        Ok(Self {
            heap,
            tables,
            views,
            indexes,
            schemas,
            path: PathContext::public_only(),
            catalog_seq: 0,
            next_rowid: 1,
            lsn: 0,
            spill: None,
        })
    }

    /// Committed-catalog schema lookup (ignores uncommitted DDL): journal
    /// replay and the durable image.
    pub fn find_schema(&self, name: &str) -> Option<usize> {
        self.schemas
            .iter()
            .position(|n| n.live && n.name.as_str() == name)
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

    /// Committed schemas with their slot indices, for checkpoint and catalog
    /// output.
    pub fn live_schemas(&self) -> impl Iterator<Item = (usize, &SchemaDef)> {
        self.schemas
            .iter()
            .enumerate()
            .filter(|(_, n)| n.live)
    }

    /// Schemas visible to `txid`, for catalog output inside a transaction.
    pub fn visible_schemas(&self, txid: u32) -> impl Iterator<Item = (usize, &SchemaDef)> {
        self.schemas
            .iter()
            .enumerate()
            .filter(move |(_, n)| n.visible_to(txid))
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
        self.alloc_schema(name, None)
    }

    /// Transactional create: the schema exists only for `txid` until commit.
    pub fn create_schema_in(&mut self, name: SqlName, txid: u32) -> Result<usize, SqlError> {
        if self.find_schema_visible(name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_SCHEMA,
                "schema \"{}\" already exists",
                name.as_str()
            ));
        }
        if self.schemas.iter().any(|n| {
            n.name.as_str() == name.as_str() && matches!(n.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent DDL on schema \"{}\"",
                name.as_str()
            ));
        }
        self.alloc_schema(name, Some(PendingDdl { txid, creating: true }))
    }

    fn alloc_schema(&mut self, name: SqlName, pending: Option<PendingDdl>) -> Result<usize, SqlError> {
        let Some(slot) = self
            .schemas
            .iter()
            .position(|n| !n.live && n.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many schemas (limit {})",
                self.schemas.len()
            ));
        };
        self.schemas[slot] = SchemaDef { name, live: pending.is_none(), pending };
        Ok(slot)
    }

    /// Committed drop (journal replay).
    pub fn drop_schema(&mut self, slot: usize) {
        self.schemas[slot].live = false;
        self.schemas[slot].pending = None;
    }

    /// Transactional drop: the schema stays visible to other transactions
    /// until `txid` commits. The owner's own pending-create evaporates.
    pub fn drop_schema_in(&mut self, slot: usize, txid: u32) {
        let n = &mut self.schemas[slot];
        if matches!(n.pending, Some(p) if p.txid == txid && p.creating) {
            n.live = false;
            n.pending = None;
        } else {
            n.pending = Some(PendingDdl { txid, creating: false });
        }
    }

    /// Promotes an uncommitted CREATE SCHEMA into the committed catalog.
    pub fn commit_schema_create(&mut self, slot: usize) {
        self.schemas[slot].live = true;
        self.schemas[slot].pending = None;
    }

    /// Applies a committed DROP SCHEMA.
    pub fn commit_schema_drop(&mut self, slot: usize) {
        self.schemas[slot].live = false;
        self.schemas[slot].pending = None;
    }

    /// Rolls back an uncommitted CREATE SCHEMA, freeing the slot.
    pub fn rollback_schema_create(&mut self, slot: usize) {
        self.schemas[slot].live = false;
        self.schemas[slot].pending = None;
    }

    /// Rolls back an uncommitted DROP SCHEMA: it returns to the committed
    /// image unchanged.
    pub fn rollback_schema_drop(&mut self, slot: usize) {
        self.schemas[slot].pending = None;
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
            return PathContext { entries: shifted, n: n + 1, explicit_catalog: false };
        }
        PathContext { entries, n, explicit_catalog: true }
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
        if self.path.explicit_catalog
            && self.path.entries().first() == Some(&PathEntry::Catalog)
        {
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
        self.tables
            .iter()
            .enumerate()
            .filter(|(_, t)| t.live)
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

    /// The bytes of a visible row, wherever they live: a heap row borrows the
    /// heap directly; a spilled row is fetched through the cache tiers into
    /// `arena`. The two lifetimes unify, so call sites keep their shapes.
    pub fn row_bytes<'a>(
        &'a self,
        table_slot: usize,
        rowid: u64,
        home: RowHome,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<&'a [u8], SqlError> {
        match home {
            RowHome::Heap(loc) => Ok(self.heap.get(loc)),
            RowHome::Spilled { len, sst } => {
                let Some(spill) = &self.spill else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but no spill reader is attached"
                    ));
                };
                let Some(handle) = self
                    .tables[table_slot]
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
                let out = arena
                    .alloc_slice_with(len as usize, |_| 0u8)
                    .map_err(|_| sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "spilled rows exceed the statement arena; raise work_arena_bytes"
                    ))?;
                // Both borrows are per-fetch; the copy into the arena ends
                // them before returning.
                let Some(mut scratch) =
                    spill.scratch.iter().find_map(|c| c.try_borrow_mut().ok())
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled-row fetches nested deeper than the reader supports"
                    ));
                };
                let mut blocks = spill.blocks.borrow_mut();
                let SpillScratch { index_buf, data_buf, assembly_buf } = &mut *scratch;
                let mut reader =
                    crate::store::SstReader::over(index_buf, data_buf, assembly_buf);
                let got = reader
                    .get(&mut *blocks, &handle, rowid, out)
                    .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?;
                match got {
                    Some(n) if n == len as usize => Ok(&out[..n]),
                    Some(_) => Err(sql_err!(sqlstate::INTERNAL_ERROR, "spilled row length mismatch")),
                    None => Err(sql_err!(sqlstate::INTERNAL_ERROR, "spilled row missing from its SST")),
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
            RowHome::Spilled { len, sst } => {
                let Some(spill) = &self.spill else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "row is spilled but no spill reader is attached"
                    ));
                };
                let Some(handle) = self
                    .tables[table_slot]
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
                let Some(mut scratch) =
                    spill.scratch.iter().find_map(|c| c.try_borrow_mut().ok())
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "spilled-row fetches nested deeper than the reader supports"
                    ));
                };
                let SpillScratch { index_buf, data_buf, assembly_buf } = &mut *scratch;
                // The assembly buffer doubles as the row destination: `get`
                // assembles a chained row into the caller buffer directly, so
                // the two uses never overlap.
                let row_buf = &mut assembly_buf[..len as usize];
                let got = {
                    let mut blocks = spill.blocks.borrow_mut();
                    let mut reader =
                        crate::store::SstReader::over(index_buf, data_buf, &mut []);
                    reader
                        .get(&mut *blocks, &handle, rowid, row_buf)
                        .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?
                };
                match got {
                    Some(n) if n == len as usize => f(&row_buf[..n]),
                    Some(_) => Err(sql_err!(sqlstate::INTERNAL_ERROR, "spilled row length mismatch")),
                    None => Err(sql_err!(sqlstate::INTERNAL_ERROR, "spilled row missing from its SST")),
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
                    state.committed = Some(RowHome::Spilled { len: loc.len, sst: newest });
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
            if let Some(RowHome::Spilled { len, .. }) = state.committed {
                state.committed = Some(RowHome::Spilled { len, sst: 0 });
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
            if let Some(RowHome::Spilled { len, sst }) = state.committed {
                let sst = if sst < at {
                    sst
                } else if sst == at || sst == at + 1 {
                    at
                } else {
                    sst - removed
                };
                state.committed = Some(RowHome::Spilled { len, sst });
            }
        }
    }

    /// A delta flush: the new SST (heap rows + tombstones) joins the list;
    /// existing spilled entries keep their slots. Clears the flushed
    /// tombstones. The caller guarantees the list has room.
    pub(crate) fn append_spill(&mut self, slot: usize, handle: crate::store::SstHandle) {
        let table = &mut self.tables[slot];
        assert!(table.n_spill_ssts < MAX_SPILL_SSTS, "delta flush into a full list");
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
    }

    /// What the next checkpoint should do for this table: a delta flush (the
    /// spill list has room and every remembered tombstone fits), or a full
    /// rewrite.
    pub(crate) fn delta_eligible(&self, slot: usize) -> bool {
        let t = &self.tables[slot];
        t.n_spill_ssts > 0 && t.n_spill_ssts < MAX_SPILL_SSTS && !t.tombstones_overflow
    }

    pub(crate) fn tombstones(&self, slot: usize) -> &[u64] {
        let t = &self.tables[slot];
        &t.tombstones[..t.n_tombstones]
    }

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

    /// Clears all per-table dirty flags (after a successful checkpoint).
    pub fn clear_dirty(&mut self) {
        for t in self.tables.iter_mut() {
            t.dirty = false;
        }
    }

    /// Rewrites the row heap so it contains only live row images
    /// (committed and pending alike), in ascending offset order, repointing
    /// every table's map. Reclaims the garbage left by updates and deletes;
    /// runs at checkpoint. `scratch` must hold every live image.
    pub fn compact_heap(
        &mut self,
        scratch: &mut FixedVec<(u32, u64, bool, RowLoc)>,
    ) -> Result<(), SqlError> {
        scratch.clear();
        for (index, table) in self.tables.iter().enumerate() {
            if !table.live {
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
                        .push((index as u32, rowid, false, loc))
                        .map_err(overflow)?;
                }
                if let Some(PendingChange { loc: Some(loc), .. }) = state.pending {
                    scratch
                        .push((index as u32, rowid, true, loc))
                        .map_err(overflow)?;
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
            let (table_index, rowid, is_pending, loc) = scratch[i];
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
            if is_pending {
                let p = state.pending.as_mut().expect("pending image existed");
                p.loc = Some(new_loc);
            } else {
                state.committed = Some(RowHome::Heap(new_loc));
            }
            write_at += len;
        }
        self.heap.used = write_at;
        Ok(())
    }

    /// Records an uncommitted change to a row. Returns whether this is the
    /// transaction's first touch of the row (the caller then remembers it
    /// for commit/rollback). Fails fast when another transaction holds an
    /// uncommitted change (SQLSTATE 40001).
    pub fn write_pending(
        &mut self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        loc: Option<RowLoc>,
    ) -> Result<Option<Option<RowLoc>>, SqlError> {
        let table = &mut self.tables[table_index];
        if let Some(state) = table.rows.get_mut(&rowid) {
            if let Some(other) = state.locked_by_other(txid) {
                let _ = other;
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent update"
                ));
            }
            let prior = state.pending.map(|p| p.loc);
            state.pending = Some(PendingChange { txid, loc });
            return Ok(prior);
        }
        if table.rows.len() == table.rows.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table row limit reached ({} rows in memtable)",
                table.rows.capacity()
            ));
        }
        table
            .rows
            .insert(
                rowid,
                RowState {
                    committed: None,
                    pending: Some(PendingChange { txid, loc }),
                },
            )
            .expect("capacity checked above");
        Ok(None)
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
        if let Some(p) = state.pending
            && p.txid != txid
        {
            return;
        }
        match prior {
            None => {
                state.pending = None;
                if state.committed.is_none() {
                    table.rows.remove(&rowid);
                }
            }
            Some(loc) => state.pending = Some(PendingChange { txid, loc }),
        }
    }

    /// Promotes a row's pending change to committed. The WAL record must
    /// already be durable.
    /// Removes a committed row outright (journal replay of a DELETE),
    /// recording the tombstone a later delta checkpoint needs.
    pub fn remove_committed(&mut self, table_index: usize, rowid: u64) {
        let table = &mut self.tables[table_index];
        if table.rows.remove(&rowid).is_some() {
            Self::record_tombstone(table, rowid);
            table.dirty = true;
        }
    }

    pub fn commit_row(&mut self, table_index: usize, rowid: u64, txid: u32) {
        let table = &mut self.tables[table_index];
        let Some(state) = table.rows.get_mut(&rowid) else {
            return;
        };
        match state.pending {
            Some(p) if p.txid == txid => {
                state.committed = p.loc.map(RowHome::Heap);
                state.pending = None;
                if state.committed.is_none() {
                    table.rows.remove(&rowid);
                    // A rowid that ever reached an SST — even if its latest
                    // version was heap-resident — must tombstone, or a cold
                    // start resurrects the SST's version.
                    Self::record_tombstone(table, rowid);
                }
                table.dirty = true;
            }
            _ => {}
        }
    }

    /// Committed-catalog lookup (ignores uncommitted DDL): used by journal
    /// replay and any context that operates on the durable image.
    pub fn find_table(&self, schema: &str, name: &str) -> Option<usize> {
        self.tables.iter().position(|t| {
            t.live && t.def.schema.as_str() == schema && t.def.name.as_str() == name
        })
    }

    /// Transaction-scoped lookup: `txid` sees its own uncommitted CREATE/DROP
    /// and every committed table, but not another transaction's uncommitted
    /// DDL.
    pub fn find_visible(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.tables.iter().position(|t| {
            t.visible_to(txid)
                && t.def.schema.as_str() == schema
                && t.def.name.as_str() == name
        })
    }

    pub fn table(&self, index: usize) -> &Table {
        &self.tables[index]
    }

    pub fn table_mut(&mut self, index: usize) -> &mut Table {
        &mut self.tables[index]
    }

    /// Allocates a slot for a fresh table. Shared by replay (committed) and
    /// the executor (pending); `pending` overlays the uncommitted-CREATE
    /// state so the table is invisible to other transactions until commit.
    fn alloc_table(&mut self, def: TableDef, pending: Option<PendingDdl>) -> Result<usize, SqlError> {
        let Some(slot) = self.tables.iter().position(Table::is_free) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many tables (limit {})",
                self.tables.len()
            ));
        };
        self.catalog_seq += 1;
        let stamp = self.catalog_seq;
        let table = &mut self.tables[slot];
        table.def = def;
        table.created_at = stamp;
        table.rows.clear();
        table.live = pending.is_none();
        table.pending_ddl = pending;
        table.dirty = true;
        // A reused slot must not inherit the dropped table's sequences or
        // spilled rows.
        table.serial_last = [0; MAX_COLUMNS];
        table.serial_dirty = false;
        table.spill_ssts = [None; MAX_SPILL_SSTS];
        table.n_spill_ssts = 0;
        table.n_tombstones = 0;
        table.tombstones_overflow = false;
        Ok(slot)
    }

    /// Committed create (journal replay): the table is immediately part of the
    /// durable image.
    pub fn create_table(&mut self, def: TableDef) -> Result<usize, SqlError> {
        if self.find_table(def.schema.as_str(), def.name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        self.alloc_table(def, None)
    }

    /// Transactional create: the table exists only for `txid` until commit.
    /// A name already visible to `txid` is a duplicate (42P07); a name held by
    /// another transaction's uncommitted DDL is a conflict (40001).
    pub fn create_table_in(&mut self, def: TableDef, txid: u32) -> Result<usize, SqlError> {
        if self.find_visible(def.schema.as_str(), def.name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        if let Some(other) =
            self.ddl_name_locked_by_other(def.schema.as_str(), def.name.as_str(), txid)
        {
            let _ = other;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent DDL on \"{}\"",
                def.name.as_str()
            ));
        }
        self.alloc_table(def, Some(PendingDdl { txid, creating: true }))
    }

    /// The txid of another transaction holding uncommitted DDL for `name`.
    fn ddl_name_locked_by_other(&self, schema: &str, name: &str, txid: u32) -> Option<u32> {
        self.tables
            .iter()
            .filter(|t| t.def.schema.as_str() == schema && t.def.name.as_str() == name)
            .find_map(|t| t.ddl_locked_by_other(txid))
    }

    /// Committed drop (journal replay): rows are retained; the slot is freed at
    /// checkpoint.
    pub fn drop_table(&mut self, index: usize) {
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].dirty = true;
    }

    /// Transactional drop: the table stays visible to every other transaction
    /// (committed baseline) until `txid` commits.
    pub fn drop_table_in(&mut self, index: usize, txid: u32) {
        self.tables[index].pending_ddl = Some(PendingDdl { txid, creating: false });
        self.tables[index].dirty = true;
    }

    /// Promotes an uncommitted CREATE to the committed image.
    pub fn commit_create(&mut self, index: usize) {
        self.tables[index].live = true;
        self.tables[index].pending_ddl = None;
    }

    /// Applies a committed DROP: the table leaves the image and its rows are
    /// reclaimed.
    pub fn commit_drop(&mut self, index: usize) {
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].rows.clear();
    }

    /// Rolls back an uncommitted CREATE, freeing the slot.
    pub fn rollback_create(&mut self, index: usize) {
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].rows.clear();
    }

    /// Rolls back an uncommitted DROP: the table returns to the committed
    /// image unchanged.
    pub fn rollback_drop(&mut self, index: usize) {
        self.tables[index].pending_ddl = None;
    }


    /// Whether any live view exists (lets the executor skip view expansion).
    pub fn has_any_view(&self) -> bool {
        self.views.iter().any(|v| v.live || v.pending.is_some())
    }

    /// Committed views as (name, SELECT text), for checkpoint serialization.
    pub fn live_views(&self) -> impl Iterator<Item = &ViewDef> {
        self.views.iter().filter(|v| v.live)
    }

    pub(crate) fn view(&self, slot: usize) -> &ViewDef {
        &self.views[slot]
    }

    pub(crate) fn view_count(&self) -> usize {
        self.views.len()
    }

    /// The stored SELECT text of a view visible to `txid`, if `name` names one
    /// (own uncommitted CREATE/DROP included; another transaction's excluded).
    pub fn find_view(&self, schema: &str, name: &str, txid: u32) -> Option<&ViewDef> {
        self.views.iter().find(|v| {
            v.visible_to(txid) && v.schema.as_str() == schema && v.name.as_str() == name
        })
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
        sql: StackStr<VIEW_SQL_MAX>,
        creation_path: StackStr<128>,
        or_replace: bool,
        txid: u32,
    ) -> Result<(usize, Option<usize>), SqlError> {
        if self.find_table(schema.as_str(), name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                name.as_str()
            ));
        }
        // Another transaction's uncommitted CREATE/DROP holds the name; a
        // fail-fast conflict replaces PostgreSQL's lock wait.
        if self.views.iter().any(|v| {
            v.schema.as_str() == schema.as_str()
                && v.name.as_str() == name.as_str()
                && matches!(v.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
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
            .position(|v| !v.live && v.pending.is_none())
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
        self.catalog_seq += 1;
        self.views[new] = ViewDef {
            created_at: self.catalog_seq,
            schema,
            name,
            sql,
            creation_path,
            live: false,
            pending: Some(PendingDdl { txid, creating: true }),
        };
        Ok((new, existing))
    }

    /// Marks the view visible to `txid` pending-dropped; returns its slot (for
    /// undo). None if absent. Errors if another transaction's uncommitted DDL
    /// holds the name.
    pub fn drop_view(&mut self, schema: &str, name: &str, txid: u32) -> Result<Option<usize>, SqlError> {
        if self.views.iter().any(|v| {
            v.schema.as_str() == schema
                && v.name.as_str() == name
                && matches!(v.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
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
        if matches!(v.pending, Some(p) if p.txid == txid && p.creating) {
            v.live = false;
            v.pending = None;
        } else {
            v.pending = Some(PendingDdl { txid, creating: false });
        }
    }

    /// Promotes an uncommitted CREATE VIEW into the committed catalog.
    pub fn commit_view_create(&mut self, slot: usize) {
        self.views[slot].live = true;
        self.views[slot].pending = None;
    }

    /// Promotes an uncommitted DROP VIEW into the committed catalog.
    pub fn commit_view_drop(&mut self, slot: usize) {
        self.views[slot].live = false;
        self.views[slot].pending = None;
    }

    /// Discards an uncommitted CREATE VIEW (rollback): the slot is freed.
    pub fn rollback_view_create(&mut self, slot: usize) {
        self.views[slot].live = false;
        self.views[slot].pending = None;
    }

    /// Discards an uncommitted DROP VIEW (rollback). A committed view becomes
    /// visible again; a same-transaction pending-create (create + drop, then
    /// the drop rolled back to a savepoint) reverts to pending-create.
    pub fn rollback_view_drop(&mut self, slot: usize, txid: u32) {
        let v = &mut self.views[slot];
        if v.live {
            v.pending = None;
        } else {
            v.pending = Some(PendingDdl { txid, creating: true });
        }
    }

    pub fn index_exists(&self, schema: &str, name: &str, txid: u32) -> bool {
        self.indexes.iter().any(|x| {
            x.visible_to(txid) && x.schema.as_str() == schema && x.name.as_str() == name
        })
    }

    /// Registers an index as an uncommitted CREATE owned by `def.pending`'s
    /// transaction; returns its slot. Errors on a duplicate visible name or
    /// another transaction's uncommitted DDL on the name.
    pub fn create_index(&mut self, def: IndexDef, txid: u32) -> Result<usize, SqlError> {
        if self.indexes.iter().any(|x| {
            x.schema.as_str() == def.schema.as_str()
                && x.name.as_str() == def.name.as_str()
                && matches!(x.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                def.name.as_str()
            ));
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
            .position(|x| !x.live && x.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many indexes (limit {})",
                self.indexes.len()
            ));
        };
        self.indexes[i] = IndexDef {
            live: false,
            pending: Some(PendingDdl { txid, creating: true }),
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
                && matches!(x.pending, Some(p) if p.txid == txid && !p.creating)
            {
                x.live = false;
                x.pending = None;
            }
        }
    }

    /// Discards this transaction's pending index drops on a table (a rolled
    /// back DROP TABLE): committed indexes become visible again.
    pub fn rollback_indexes_for(&mut self, schema: &str, table: &str, txid: u32) {
        for x in self.indexes.iter_mut() {
            if x.schema.as_str() == schema
                && x.table.as_str() == table
                && matches!(x.pending, Some(p) if p.txid == txid && !p.creating)
            {
                x.pending = None;
            }
        }
    }

    /// Marks the index visible to `txid` pending-dropped; returns its slot
    /// (for undo). None if absent. Errors if another transaction's uncommitted
    /// DDL holds the name.
    pub fn drop_index(&mut self, schema: &str, name: &str, txid: u32) -> Result<Option<usize>, SqlError> {
        if self.indexes.iter().any(|x| {
            x.schema.as_str() == schema
                && x.name.as_str() == name
                && matches!(x.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self.indexes.iter().position(|x| {
            x.visible_to(txid) && x.schema.as_str() == schema && x.name.as_str() == name
        }) else {
            return Ok(None);
        };
        self.pending_drop_index(i, txid);
        Ok(Some(i))
    }

    /// Overlays a pending DROP on a slot: the owner's own pending-create
    /// simply evaporates.
    fn pending_drop_index(&mut self, slot: usize, txid: u32) {
        let x = &mut self.indexes[slot];
        if matches!(x.pending, Some(p) if p.txid == txid && p.creating) {
            x.live = false;
            x.pending = None;
        } else {
            x.pending = Some(PendingDdl { txid, creating: false });
        }
    }

    /// Promotes an uncommitted CREATE INDEX into the committed catalog.
    pub fn commit_index_create(&mut self, slot: usize) {
        self.indexes[slot].live = true;
        self.indexes[slot].pending = None;
    }

    /// Promotes an uncommitted DROP INDEX into the committed catalog.
    pub fn commit_index_drop(&mut self, slot: usize) {
        self.indexes[slot].live = false;
        self.indexes[slot].pending = None;
    }

    /// Discards an uncommitted CREATE INDEX (rollback): the slot is freed.
    pub fn rollback_index_create(&mut self, slot: usize) {
        self.indexes[slot].live = false;
        self.indexes[slot].pending = None;
    }

    /// Discards an uncommitted DROP INDEX (rollback); a same-transaction
    /// pending-create reverts to pending-create.
    pub fn rollback_index_drop(&mut self, slot: usize, txid: u32) {
        let x = &mut self.indexes[slot];
        if x.live {
            x.pending = None;
        } else {
            x.pending = Some(PendingDdl { txid, creating: true });
        }
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
        self.indexes.iter().filter(move |x| {
            x.visible_to(txid) && x.schema.as_str() == schema && x.table.as_str() == table
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
        self.indexes.iter().filter(|x| x.live)
    }

    /// A definition-only schema move (ALTER TABLE ... SET SCHEMA): the table
    /// and its indexes change schema, and every inbound foreign key follows —
    /// deterministically, so WAL replay reproduces it from the names alone.
    pub fn move_table_schema(&mut self, index: usize, new_schema: SqlName) {
        let old_schema = self.tables[index].def.schema;
        let name = self.tables[index].def.name;
        self.tables[index].def.schema = new_schema;
        self.tables[index].dirty = true;
        for x in self.indexes.iter_mut() {
            if x.live
                && x.schema.as_str() == old_schema.as_str()
                && x.table.as_str() == name.as_str()
            {
                x.schema = new_schema;
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
                t.dirty = true;
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
        self.tables[index].dirty = true;
        Some(removed)
    }

    /// Restores a foreign key removed by [`Self::drop_fk`] (rollback).
    pub fn restore_fk(&mut self, index: usize, fk: ForeignKey) {
        let def = &mut self.tables[index].def;
        if def.n_fkeys < MAX_FKEYS {
            def.fkeys[def.n_fkeys] = fk;
            def.n_fkeys += 1;
            self.tables[index].dirty = true;
        }
    }

    /// Replaces a table's definition in place (ALTER TABLE).
    pub fn set_table_def(&mut self, index: usize, def: TableDef) {
        self.tables[index].def = def;
        self.tables[index].dirty = true;
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

    fn test_config() -> Config {
        let mut c = Config::default_dev();
        c.memtable_bytes = 1 << 16;
        c.max_tables = 4;
        c.table_rows = 128;
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
                default_value: None,
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
                default_value: None,
            };
        }
        def
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
    fn name_length_limit() {
        let long = "x".repeat(64);
        assert!(SqlName::parse(&long).is_err());
        let ok = "y".repeat(63);
        assert_eq!(SqlName::parse(&ok).unwrap().as_str(), ok);
    }
}
