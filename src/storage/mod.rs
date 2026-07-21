//! Table storage: the in-memory write path of the LSM.
//!
//! Row bytes live in one fixed heap (the memtable); each table maps rowid →
//! location. Updates write a new copy and repoint the map — superseded
//! bytes are reclaimed when the memtable flushes to object storage (later
//! phase). All capacities are fixed at startup.

pub mod rowenc;

use core::hash::{Hash, Hasher};

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::util::StackStr;

pub use rowenc::MAX_COLUMNS;

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
                "42622",
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

pub const MAX_DEFAULT_TEXT: usize = 48;

impl OwnedDatum {
    pub fn from_datum(d: &crate::sql::types::Datum) -> Result<Self, SqlError> {
        use crate::sql::types::Datum;
        Ok(match d {
            Datum::Record(_) => {
                return Err(sql_err!(
                    "0A000",
                    "cannot store a composite (record) value in a column"
                ))
            }
            Datum::Null => Self::Null,
            Datum::Bool(b) => Self::Bool(*b),
            Datum::Int4(v) => Self::Int4(*v),
            Datum::Int8(v) => Self::Int8(*v),
            Datum::Float8(v) => Self::Float8(*v),
            Datum::Date(_)
            | Datum::Timestamp(_)
            | Datum::Timestamptz(_)
            | Datum::Time(_)
            | Datum::Interval(_)
            | Datum::Json { .. }
            | Datum::Array { .. }
            | Datum::Range { .. }
            | Datum::Multirange { .. }
            | Datum::Bit { .. }
            | Datum::Uuid(_)
            | Datum::Bytea(_) => {
                return Err(sql_err!(
                    "0A000",
                    "defaults of this type are not supported yet (store as text)"
                ))
            }
            Datum::Numeric(n) => {
                if n.digits.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!("54000", "numeric default too large"));
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
            Datum::Text(s) => {
                if s.len() > MAX_DEFAULT_TEXT {
                    return Err(sql_err!(
                        "54000",
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
pub const MAX_UNIQUES: usize = 8;
/// Maximum number of CHECK constraints per table.
pub const MAX_CHECKS: usize = 8;
/// Maximum stored length of a CHECK predicate's source text.
pub const CHECK_SQL_MAX: usize = 512;
/// Maximum number of FOREIGN KEY constraints per table.
pub const MAX_FKEYS: usize = 8;

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
    pub parent: SqlName,
    pub parent_cols: [u16; MAX_INDEX_COLS],
    pub n_parent_cols: usize,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

impl ForeignKey {
    pub const EMPTY: Self = ForeignKey {
        name: SqlName::EMPTY,
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
    pub committed: Option<RowLoc>,
    pub pending: Option<PendingChange>,
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
            committed: Some(loc),
            pending: None,
        }
    }

    /// What transaction `txid` sees: its own pending change, else the
    /// committed image. `None` = row invisible.
    pub fn visible_to(&self, txid: u32) -> Option<RowLoc> {
        match self.pending {
            Some(p) if p.txid == txid => p.loc,
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
                "memtable is full ({} bytes); flush to object storage is not implemented yet",
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
}

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
pub const VIEW_SQL_MAX: usize = 2048;

/// A named view: its output is its stored SELECT text, expanded as a derived
/// table at query time.
#[derive(Clone)]
pub struct ViewDef {
    pub name: SqlName,
    pub sql: StackStr<VIEW_SQL_MAX>,
    pub live: bool,
    /// An uncommitted CREATE/DROP owned by one transaction (catalog MVCC,
    /// mirroring `Table::pending_ddl`): other transactions see `live`; the
    /// owner sees the pending existence.
    pub pending: Option<PendingDdl>,
}

impl ViewDef {
    /// Whether `txid` sees this view exist.
    pub fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }
}

/// Maximum columns in an index key.
pub const MAX_INDEX_COLS: usize = 8;

/// A named index over a table's columns. Our engine does full scans, so an
/// index never accelerates a query; it exists as a durable catalog object and,
/// when `unique`, enforces a uniqueness constraint on its column tuple.
#[derive(Clone, Copy)]
pub struct IndexDef {
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

pub struct Storage {
    pub heap: RowHeap,
    tables: FixedVec<Table>,
    views: FixedVec<ViewDef>,
    indexes: FixedVec<IndexDef>,
    next_rowid: u64,
    /// Log sequence number of the latest write; becomes the WAL position.
    lsn: u64,
}

impl Storage {
    /// Bytes drawn beyond the row heap itself, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        config.max_tables
            * (size_of::<Table>()
                + FixedMap::<u64, RowState>::budget_bytes(config.table_rows)
                + size_of::<ViewDef>()
                + size_of::<IndexDef>())
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
                    live: false,
                    pending_ddl: None,
                    dirty: false,
                })
                .expect("sized to max_tables");
        }
        let mut views = FixedVec::new(budget, "views", config.max_tables)?;
        for _ in 0..config.max_tables {
            views
                .push(ViewDef {
                    name: SqlName::parse("").expect("empty name fits"),
                    sql: StackStr::new(),
                    live: false,
                    pending: None,
                })
                .expect("sized to max_tables");
        }
        let mut indexes = FixedVec::new(budget, "indexes", config.max_tables)?;
        for _ in 0..config.max_tables {
            indexes
                .push(IndexDef {
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
            next_rowid: 1,
            lsn: 0,
        })
    }

    /// Live tables with their slot indices.
    pub fn live_tables(&self) -> impl Iterator<Item = (usize, &Table)> {
        self.tables
            .iter()
            .enumerate()
            .filter(|(_, t)| t.live)
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
                if let Some(loc) = state.committed {
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
                state.committed = Some(new_loc);
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
                    "40001",
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
    pub fn commit_row(&mut self, table_index: usize, rowid: u64, txid: u32) {
        let table = &mut self.tables[table_index];
        let Some(state) = table.rows.get_mut(&rowid) else {
            return;
        };
        match state.pending {
            Some(p) if p.txid == txid => {
                state.committed = p.loc;
                state.pending = None;
                if state.committed.is_none() {
                    table.rows.remove(&rowid);
                }
                table.dirty = true;
            }
            _ => {}
        }
    }

    /// Committed-catalog lookup (ignores uncommitted DDL): used by journal
    /// replay and any context that operates on the durable image.
    pub fn find_table(&self, name: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.live && t.def.name.as_str() == name)
    }

    /// Transaction-scoped lookup: `txid` sees its own uncommitted CREATE/DROP
    /// and every committed table, but not another transaction's uncommitted
    /// DDL.
    pub fn find_visible(&self, name: &str, txid: u32) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.visible_to(txid) && t.def.name.as_str() == name)
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
        let table = &mut self.tables[slot];
        table.def = def;
        table.rows.clear();
        table.live = pending.is_none();
        table.pending_ddl = pending;
        table.dirty = true;
        Ok(slot)
    }

    /// Committed create (journal replay): the table is immediately part of the
    /// durable image.
    pub fn create_table(&mut self, def: TableDef) -> Result<usize, SqlError> {
        if self.find_table(def.name.as_str()).is_some() {
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
        if self.find_visible(def.name.as_str(), txid).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        if let Some(other) = self.ddl_name_locked_by_other(def.name.as_str(), txid) {
            let _ = other;
            return Err(sql_err!(
                "40001",
                "could not serialize access due to concurrent DDL on \"{}\"",
                def.name.as_str()
            ));
        }
        self.alloc_table(def, Some(PendingDdl { txid, creating: true }))
    }

    /// The txid of another transaction holding uncommitted DDL for `name`.
    fn ddl_name_locked_by_other(&self, name: &str, txid: u32) -> Option<u32> {
        self.tables
            .iter()
            .filter(|t| t.def.name.as_str() == name)
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
    pub fn live_views(&self) -> impl Iterator<Item = (&str, &str)> {
        self.views
            .iter()
            .filter(|v| v.live)
            .map(|v| (v.name.as_str(), v.sql.as_str()))
    }

    /// The stored SELECT text of a view visible to `txid`, if `name` names one
    /// (own uncommitted CREATE/DROP included; another transaction's excluded).
    pub fn find_view(&self, name: &str, txid: u32) -> Option<&str> {
        self.views
            .iter()
            .find(|v| v.visible_to(txid) && v.name.as_str() == name)
            .map(|v| v.sql.as_str())
    }

    /// Registers a view as an uncommitted CREATE owned by `txid` (other
    /// transactions keep seeing the committed catalog until commit).
    /// `or_replace` marks an existing visible view pending-dropped. Returns
    /// `(new_slot, replaced_old_slot)`. Errors if the name is taken by a
    /// table, by a view visible to `txid` (without `or_replace`), or by
    /// another transaction's uncommitted view DDL.
    pub fn create_view(
        &mut self,
        name: SqlName,
        sql: StackStr<VIEW_SQL_MAX>,
        or_replace: bool,
        txid: u32,
    ) -> Result<(usize, Option<usize>), SqlError> {
        if self.find_table(name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                name.as_str()
            ));
        }
        // Another transaction's uncommitted CREATE/DROP holds the name; a
        // fail-fast conflict replaces PostgreSQL's lock wait.
        if self.views.iter().any(|v| {
            v.name.as_str() == name.as_str()
                && matches!(v.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                "40001",
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
        }
        let existing = self
            .views
            .iter()
            .position(|v| v.visible_to(txid) && v.name.as_str() == name.as_str());
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
        self.views[new] = ViewDef {
            name,
            sql,
            live: false,
            pending: Some(PendingDdl { txid, creating: true }),
        };
        Ok((new, existing))
    }

    /// Marks the view visible to `txid` pending-dropped; returns its slot (for
    /// undo). None if absent. Errors if another transaction's uncommitted DDL
    /// holds the name.
    pub fn drop_view(&mut self, name: &str, txid: u32) -> Result<Option<usize>, SqlError> {
        if self.views.iter().any(|v| {
            v.name.as_str() == name && matches!(v.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                "40001",
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self
            .views
            .iter()
            .position(|v| v.visible_to(txid) && v.name.as_str() == name)
        else {
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

    pub fn index_exists(&self, name: &str, txid: u32) -> bool {
        self.indexes
            .iter()
            .any(|x| x.visible_to(txid) && x.name.as_str() == name)
    }

    /// Registers an index as an uncommitted CREATE owned by `def.pending`'s
    /// transaction; returns its slot. Errors on a duplicate visible name or
    /// another transaction's uncommitted DDL on the name.
    pub fn create_index(&mut self, def: IndexDef, txid: u32) -> Result<usize, SqlError> {
        if self.indexes.iter().any(|x| {
            x.name.as_str() == def.name.as_str()
                && matches!(x.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                "40001",
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                def.name.as_str()
            ));
        }
        if self.index_exists(def.name.as_str(), txid) {
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
    pub fn drop_indexes_for(&mut self, table: &str, txid: u32) {
        for i in 0..self.indexes.len() {
            if self.indexes[i].visible_to(txid) && self.indexes[i].table.as_str() == table {
                self.pending_drop_index(i, txid);
            }
        }
    }

    /// Promotes this transaction's pending index drops on a table (cascaded
    /// from its DROP TABLE) into the committed catalog.
    pub fn commit_indexes_for(&mut self, table: &str, txid: u32) {
        for x in self.indexes.iter_mut() {
            if x.table.as_str() == table
                && matches!(x.pending, Some(p) if p.txid == txid && !p.creating)
            {
                x.live = false;
                x.pending = None;
            }
        }
    }

    /// Discards this transaction's pending index drops on a table (a rolled
    /// back DROP TABLE): committed indexes become visible again.
    pub fn rollback_indexes_for(&mut self, table: &str, txid: u32) {
        for x in self.indexes.iter_mut() {
            if x.table.as_str() == table
                && matches!(x.pending, Some(p) if p.txid == txid && !p.creating)
            {
                x.pending = None;
            }
        }
    }

    /// Marks the index visible to `txid` pending-dropped; returns its slot
    /// (for undo). None if absent. Errors if another transaction's uncommitted
    /// DDL holds the name.
    pub fn drop_index(&mut self, name: &str, txid: u32) -> Result<Option<usize>, SqlError> {
        if self.indexes.iter().any(|x| {
            x.name.as_str() == name && matches!(x.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                "40001",
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self
            .indexes
            .iter()
            .position(|x| x.visible_to(txid) && x.name.as_str() == name)
        else {
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
    pub fn unique_indexes_for(&self, table: &str, txid: u32) -> impl Iterator<Item = &IndexDef> {
        self.indexes
            .iter()
            .filter(move |x| x.visible_to(txid) && x.unique && x.table.as_str() == table)
    }

    /// All committed indexes, for checkpoint serialization.
    pub fn live_indexes(&self) -> impl Iterator<Item = &IndexDef> {
        self.indexes.iter().filter(|x| x.live)
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
        assert_eq!(s.find_table("t1"), Some(index));
        assert_eq!(
            s.create_table(def).unwrap_err().sqlstate,
            sqlstate::DUPLICATE_TABLE
        );
        s.drop_table(index);
        assert_eq!(s.find_table("t1"), None);
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
