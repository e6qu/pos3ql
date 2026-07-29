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
    /// A DEFAULT that folds to a constant (a literal-only expression), stored as
    /// its owned value for a fast insert. Non-constant defaults live in
    /// `default_expr` instead; the two are mutually exclusive.
    pub default_value: Option<OwnedDatum>,
    /// Either a non-constant DEFAULT — anything with a function call (`now()`,
    /// `nextval(...)`, `gen_random_uuid()`, …) — or, when `is_generated`, the
    /// `GENERATED ALWAYS AS (expr) STORED` expression. Kept as raw source text
    /// and re-parsed + evaluated per row (against the row's other columns, for a
    /// generated column). Also the source for `pg_get_expr` / `\d`.
    pub default_expr: Option<StackStr<DEFAULT_EXPR_MAX>>,
    /// When set, `default_expr` is a `STORED` generation expression (attgenerated
    /// `'s'`), computed from the row rather than defaulted — the two never
    /// coexist on one column.
    pub is_generated: bool,
    /// A `GENERATED ... AS IDENTITY` column (also `auto_increment`): distinguishes
    /// it from a bare `serial` for `pg_attribute.attidentity`.
    pub is_identity: bool,
    /// `GENERATED ALWAYS AS IDENTITY` (reject explicit inserts) vs `BY DEFAULT`.
    pub identity_always: bool,
    /// The auto-increment step: 1 for `serial`, or the identity `INCREMENT BY`.
    pub auto_increment_step: i64,
    /// When the column was declared with a user-defined type, its stable name
    /// and schema. Runtime enum/domain slots are rebound from this identity
    /// after restart; storing both parts prevents same-named types in different
    /// schemas from aliasing each other.
    pub domain: Option<SqlName>,
    pub user_type_schema: Option<SqlName>,
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
        default_value: None,
        default_expr: None,
        is_generated: false,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        domain: None,
        user_type_schema: None,
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
    /// The value indexes accelerating this table's uniqueness/foreign-key
    /// probes, one per constraint, rebuilt from `def` whenever the definition or
    /// index set changes and maintained per committed row otherwise.
    pub(crate) enforcers: [Option<Enforcer>; MAX_UNIQUE_ENFORCERS],
    pub(crate) n_enforcers: usize,
}

/// The most delta SSTs a table accumulates before a checkpoint merges them
/// back into one — the write-amplification / read-fan-out tradeoff.
pub(crate) const MAX_SPILL_SSTS: usize = 8;

/// Deletes remembered between checkpoints; past this the next checkpoint
/// rewrites the table fully rather than lose one.
pub(crate) const MAX_TOMBSTONES: usize = 1024;

/// The most value indexes one table can carry: one per single-column
/// UNIQUE/PRIMARY KEY flag, per multi-column key, and per UNIQUE index.
/// Exceeding it at DDL is a loud error.
pub(crate) const MAX_UNIQUE_ENFORCERS: usize = 16;

/// A table's binding of one uniqueness constraint to its value index: the key
/// columns it covers and the pool slot holding the `value_hash → rowid` map for
/// the committed rows. A uniqueness probe finds the enforcer whose columns match
/// and seeks the index instead of scanning every row.
#[derive(Clone, Copy)]
pub(crate) struct Enforcer {
    slot: u32,
    columns: [u16; MAX_INDEX_COLS],
    n_cols: usize,
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
    pub schema: SqlName,
    pub name: SqlName,
    pub referenced_schema: SqlName,
    pub referenced_name: SqlName,
}

impl StoredQueryDependency {
    pub const EMPTY: Self = Self {
        class: DependencyClass::Table,
        slot: 0,
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
    pub const EMPTY: Self = Self {
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
            schema,
            name,
            referenced_schema,
            referenced_name,
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
    /// False after `WITH NO DATA` until the first REFRESH.
    pub populated: bool,
    pub live: bool,
    pub pending: Option<PendingDdl>,
}

impl MatviewDef {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
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
    /// Immediate parent when this domain was declared over another domain.
    /// The value representation is flattened to `base`, but the parent chain
    /// remains explicit so every inherited NOT NULL/CHECK is enforced.
    pub base_domain: Option<SqlName>,
    pub base_domain_schema: Option<SqlName>,
    pub base: ColType,
    /// The base type's atttypmod (e.g. `varchar(5)` → 9), applied to a value
    /// before the domain's own constraints.
    pub base_type_mod: i32,
    pub not_null: bool,
    pub default_expr: Option<StackStr<DEFAULT_EXPR_MAX>>,
    pub checks: [CheckConstraint; MAX_DOMAIN_CHECKS],
    pub n_checks: usize,
    pub live: bool,
    pub pending: Option<PendingDdl>,
}

impl DomainDef {
    pub(crate) const EMPTY: Self = DomainDef {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        base_domain: None,
        base_domain_schema: None,
        base: ColType::Bool,
        base_type_mod: -1,
        not_null: false,
        default_expr: None,
        checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
        n_checks: 0,
        live: false,
        pending: None,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }

    pub fn checks(&self) -> &[CheckConstraint] {
        &self.checks[..self.n_checks]
    }
}

/// The validated parameters of a `CREATE DOMAIN` / `ALTER DOMAIN`, computed by
/// the executor and handed to storage (apart from the `live`/`pending` state).
#[derive(Clone, Copy)]
pub struct DomainSpec {
    pub base_domain: Option<SqlName>,
    pub base_domain_schema: Option<SqlName>,
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
    pub members: [EnumMember; MAX_ENUM_LABELS],
    pub n_members: usize,
    pub live: bool,
    pub pending: Option<PendingDdl>,
}

impl EnumDef {
    pub(crate) const EMPTY: Self = EnumDef {
        created_at: 0,
        schema: SqlName::EMPTY,
        name: SqlName::EMPTY,
        members: [EnumMember::EMPTY; MAX_ENUM_LABELS],
        n_members: 0,
        live: false,
        pending: None,
    };

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
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
/// transactional catalog state, mirroring [`ViewDef`]; its *value* state
/// (`last_value`/`is_called`/`dirty`) is deliberately **not** — a `nextval`
/// advance survives `ROLLBACK`, exactly as PostgreSQL leaves gaps. Those three
/// fields are [`Cell`]s so `nextval`/`setval` can advance the generator through
/// a shared `&Storage` borrow (the pure expression evaluator never holds
/// `&mut`), allocation-free.
#[derive(Clone)]
pub struct SequenceDef {
    pub created_at: u64,
    pub schema: SqlName,
    pub name: SqlName,
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
    pub live: bool,
    pub pending: Option<PendingDdl>,
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
#[derive(Clone, Copy)]
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
        match self.pending {
            Some(p) if p.txid == txid => p.creating,
            _ => self.live,
        }
    }

    /// Advances the generator and returns the next value, or the 2200H overflow
    /// error when a non-cycling sequence runs off its bound. Mutates value state
    /// through the `Cell` fields (a `&Storage` borrow is all the caller holds).
    pub fn next_value(&self) -> Result<i64, SqlError> {
        if !self.is_called.get() {
            // First call after CREATE/RESTART yields the start value unchanged.
            self.is_called.set(true);
            self.dirty.set(true);
            return Ok(self.last_value.get());
        }
        let current = self.last_value.get();
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
        self.last_value.set(value);
        self.dirty.set(true);
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

pub struct Storage {
    pub heap: RowHeap,
    tables: FixedVec<Table>,
    pending_table_defs: FixedVec<PendingTableDefSlot>,
    views: FixedVec<ViewDef>,
    view_dependencies: FixedVec<StoredQueryDependencies>,
    matviews: FixedVec<MatviewDef>,
    matview_dependencies: FixedVec<StoredQueryDependencies>,
    sequences: FixedVec<SequenceDef>,
    domains: FixedVec<DomainDef>,
    enums: FixedVec<EnumDef>,
    indexes: FixedVec<IndexDef>,
    schemas: FixedVec<SchemaDef>,
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
    /// ACCESS SHARE table locks held until transaction end. The current
    /// execution core fails conflicting DDL fast instead of parking it, but
    /// the lock is real and cross-connection rather than accepted-and-ignored.
    table_locks: FixedVec<(u32, u32)>,
    /// Log sequence number of the latest write; becomes the WAL position.
    lsn: u64,
    /// The read path for spilled rows: the tiered block stack shared with the
    /// checkpointer, plus owned reader scratch. `None` without object storage
    /// — then rows never spill and the heap-full error stands.
    spill: Option<SpillReader>,
    /// Startup-allocated value indexes shared by every table's enforcers. Held
    /// in an `Option` so a rebuild can take it out for the duration of a row
    /// walk (which borrows the rest of `self`) and put it back.
    value_indexes: Option<ValueIndexPool>,
    /// The logical cap on a constrained table's committed rows: an enforcer's
    /// index holds this many (plus a one-transaction headroom the physical slot
    /// array carries), and an insert past it is a loud error. This is the price
    /// of an in-RAM value index under unbounded spill.
    value_index_cap: usize,
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
    scan_contexts: [std::cell::RefCell<ScanContext>; SCAN_CONTEXTS],
}

/// How many row-state walks may be live at once: a full join
/// ([`crate::sql::query::MAX_JOIN_TABLES`] deep), plus a constraint or
/// validation scan running inside the innermost callback, with headroom.
const SCAN_CONTEXTS: usize = 12;

/// One merged walk's working memory: the current data block per member and
/// a shared buffer for index-block navigation on block advances.
struct ScanContext {
    member_blocks: [Box<[u8]>; MAX_SPILL_SSTS],
    index_buf: Box<[u8]>,
}

/// One member's cursor position inside a merged walk.
#[derive(Clone, Copy)]
struct MemberCursor {
    /// Which data block the cursor stands in (ordinal in the sparse index).
    ordinal: usize,
    /// Byte offset of the next entry inside that block.
    offset: usize,
    /// Which ordinal the context's resident buffer currently holds, if any,
    /// and how many bytes of it are the block (the buffer is oversized).
    loaded: Option<usize>,
    loaded_len: usize,
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

/// The reader's owned block buffers (index, data, chain assembly, and the
/// staging bounce a compressed data block decompresses through).
struct SpillScratch {
    index_buf: Box<[u8]>,
    data_buf: Box<[u8]>,
    assembly_buf: Box<[u8]>,
    bounce_buf: Box<[u8]>,
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
            2 * (3 * crate::store::MAX_PAYLOAD + crate::store::MAX_ASSEMBLED),
            "spill reader",
        )?;
        budget.draw(
            SCAN_CONTEXTS * (MAX_SPILL_SSTS + 1) * crate::store::MAX_PAYLOAD,
            "row-state walk contexts",
        )?;
        let fresh = || {
            std::cell::RefCell::new(SpillScratch {
                index_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                data_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
                assembly_buf: vec![0u8; crate::store::MAX_ASSEMBLED].into_boxed_slice(),
                bounce_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            })
        };
        let context = || {
            std::cell::RefCell::new(ScanContext {
                member_blocks: core::array::from_fn(|_| {
                    vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice()
                }),
                index_buf: vec![0u8; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            })
        };
        Ok(Self {
            blocks,
            scratch: [fresh(), fresh()],
            scan_contexts: core::array::from_fn(|_| context()),
        })
    }

    /// The budget the contexts and scratch draw, for memory-plan estimates.
    pub(crate) fn budget_bytes() -> usize {
        2 * (3 * crate::store::MAX_PAYLOAD + crate::store::MAX_ASSEMBLED)
            + SCAN_CONTEXTS * (MAX_SPILL_SSTS + 1) * crate::store::MAX_PAYLOAD
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
            if self.views[slot].live {
                let serialized = self.view_dependencies[slot];
                self.view_dependencies[slot] =
                    self.rebind_stored_query_dependencies(serialized, 0)?;
            }
        }
        for slot in 0..self.matviews.len() {
            if self.matviews[slot].live {
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
            if self.views[view_slot].live || self.views[view_slot].pending.is_some() {
                self.view_dependencies[view_slot].rename(class, slot, schema, name);
            }
        }
        for matview_slot in 0..self.matviews.len() {
            if self.matviews[matview_slot].live || self.matviews[matview_slot].pending.is_some() {
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
            if self.views[view_slot].live || self.views[view_slot].pending.is_some() {
                self.view_dependencies[view_slot]
                    .replace_slot(class, old_slot, new_slot, schema, name);
            }
        }
        for matview_slot in 0..self.matviews.len() {
            if self.matviews[matview_slot].live || self.matviews[matview_slot].pending.is_some() {
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
                + size_of::<StoredQueryDependencies>()
                + size_of::<MatviewDef>()
                + size_of::<StoredQueryDependencies>()
                + size_of::<IndexDef>())
            + config.max_tables * MAX_PENDING_TABLE_DEFS * size_of::<PendingTableDefSlot>()
            + MAX_SCHEMAS * size_of::<SchemaDef>()
            + MAX_SEQUENCES * size_of::<SequenceDef>()
            + MAX_DOMAINS * size_of::<DomainDef>()
            + MAX_ENUMS * size_of::<EnumDef>()
            + MAX_COMMENTS * size_of::<CommentEntry>()
            + config.max_connections as usize * size_of::<(u32, u64)>()
            + config.max_connections as usize * config.max_tables * size_of::<(u32, u32)>()
            + ValueIndexPool::budget_bytes(
                config.max_value_indexes,
                config.value_index_rows + config.table_rows,
            )
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let heap = RowHeap::new(budget, config.memtable_bytes)?;
        let mut tables = FixedVec::new(budget, "tables", config.max_tables)?;
        let pending_table_defs = FixedVec::new(
            budget,
            "pending_table_defs",
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
                            default_value: None,
                            default_expr: None,
                            is_generated: false,
                            is_identity: false,
                            identity_always: false,
                            auto_increment_step: 1,
                            domain: None,
                            user_type_schema: None,
                        }; MAX_COLUMNS],
                        n_columns: 0,
                        ..TableDef::empty()
                    },
                    pending_def_slots: [u32::MAX; MAX_PENDING_TABLE_DEFS],
                    n_pending_defs: 0,
                    pending_def_txid: None,
                    rows: FixedMap::new(budget, "table_rows", config.table_rows)?,
                    created_at: 0,
                    live: false,
                    pending_ddl: None,
                    dirty: false,
                    generation: 1,
                    serial_last: [0; MAX_COLUMNS],
                    serial_dirty: false,
                    spill_ssts: [None; MAX_SPILL_SSTS],
                    n_spill_ssts: 0,
                    tombstones: [0; MAX_TOMBSTONES],
                    n_tombstones: 0,
                    tombstones_overflow: false,
                    enforcers: [None; MAX_UNIQUE_ENFORCERS],
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
                    live: false,
                    pending: None,
                })
                .expect("sized to max_tables");
        }
        let view_dependencies =
            stored_query_dependency_slots(budget, "view_dependencies", config.max_tables)?;
        let mut matviews = FixedVec::new(budget, "matviews", config.max_tables)?;
        for _ in 0..config.max_tables {
            matviews
                .push(MatviewDef {
                    created_at: 0,
                    schema: SqlName::parse("").expect("empty name fits"),
                    name: SqlName::parse("").expect("empty name fits"),
                    sql: StackStr::new(),
                    creation_path: StackStr::new(),
                    populated: false,
                    live: false,
                    pending: None,
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
                    live: false,
                    pending: None,
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
                    live: i == 0,
                    pending: None,
                })
                .expect("sized to MAX_SCHEMAS");
        }
        let mut comments = FixedVec::new(budget, "comments", MAX_COMMENTS)?;
        for _ in 0..MAX_COMMENTS {
            comments
                .push(CommentEntry::empty())
                .expect("sized to MAX_COMMENTS");
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
        // A transaction's committed batch is bounded by the overlay, so the
        // physical index carries the cap plus one overlay-worth of headroom: the
        // per-row commit maintenance never overflows, while the cap itself is
        // enforced gracefully at insert time.
        let value_indexes = ValueIndexPool::new(
            budget,
            config.max_value_indexes,
            config.value_index_rows + config.table_rows,
        )?;
        let active_snapshots =
            FixedVec::new(budget, "active_snapshots", config.max_connections as usize)?;
        let table_locks = FixedVec::new(
            budget,
            "table_locks",
            config.max_connections as usize * config.max_tables,
        )?;
        Ok(Self {
            heap,
            tables,
            pending_table_defs,
            views,
            view_dependencies,
            matviews,
            matview_dependencies,
            sequences,
            domains,
            enums,
            indexes,
            schemas,
            comments,
            path: PathContext::public_only(),
            catalog_seq: 0,
            read_snapshot: SNAPSHOT_ALL,
            commit_snapshot: u64::MAX,
            active_snapshots,
            table_locks,
            next_rowid: 1,
            lsn: 0,
            spill: None,
            value_indexes: Some(value_indexes),
            value_index_cap: config.value_index_rows,
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
        self.schemas.iter().enumerate().filter(|(_, n)| n.live)
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
        self.alloc_schema(
            name,
            Some(PendingDdl {
                txid,
                creating: true,
            }),
        )
    }

    fn alloc_schema(
        &mut self,
        name: SqlName,
        pending: Option<PendingDdl>,
    ) -> Result<usize, SqlError> {
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
        self.schemas[slot] = SchemaDef {
            name,
            live: pending.is_none(),
            pending,
        };
        Ok(slot)
    }

    /// Committed drop (journal replay).
    pub fn drop_schema(&mut self, slot: usize) {
        let name = self.schemas[slot].name;
        self.drop_object_comments(CommentClass::Schema, "", name.as_str());
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
            n.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
    }

    /// Promotes an uncommitted CREATE SCHEMA into the committed catalog.
    pub fn commit_schema_create(&mut self, slot: usize) {
        self.schemas[slot].live = true;
        self.schemas[slot].pending = None;
    }

    /// Applies a committed DROP SCHEMA.
    pub fn commit_schema_drop(&mut self, slot: usize) {
        let name = self.schemas[slot].name;
        self.drop_object_comments(CommentClass::Schema, "", name.as_str());
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
        let Some(mut context) = spill
            .scan_contexts
            .iter()
            .find_map(|c| c.try_borrow_mut().ok())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row scans nested deeper than {} concurrent walks",
                SCAN_CONTEXTS
            ));
        };
        let context = &mut *context;
        let mut cursors = [MemberCursor {
            ordinal: 0,
            offset: 0,
            loaded: None,
            loaded_len: 0,
            head: None,
            done: false,
        }; MAX_SPILL_SSTS];
        for (member, cursor) in cursors[..n].iter_mut().enumerate() {
            Self::cursor_advance(spill, table, member, cursor, context)?;
        }
        loop {
            let mut min: Option<u64> = None;
            for cursor in cursors[..n].iter() {
                if let Some((key, ..)) = cursor.head {
                    min = Some(min.map_or(key.rowid, |rowid: u64| rowid.min(key.rowid)));
                }
            }
            let Some(rowid) = min else { return Ok(()) };
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
                    Self::cursor_advance(spill, table, member, cursor, context)?;
                }
            }
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
            if cursor.loaded != Some(cursor.ordinal) {
                let mut blocks = spill.blocks.borrow_mut();
                // Both index shapes resolve through one helper; the index
                // buffer is scratch for the descent and the decompression
                // bounce alike.
                let Some(id) = crate::store::locate_data_block(
                    &mut *blocks,
                    &handle,
                    &mut context.index_buf,
                    cursor.ordinal,
                )
                .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?
                else {
                    cursor.done = true;
                    return Ok(());
                };
                cursor.loaded_len = crate::store::read_data_block(
                    &mut *blocks,
                    &id,
                    &mut context.member_blocks[member],
                    &mut context.index_buf,
                )
                .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?;
                cursor.loaded = Some(cursor.ordinal);
                cursor.offset = 0;
            }
            match crate::store::block_keys_at(
                &context.member_blocks[member][..cursor.loaded_len],
                cursor.offset,
                handle.versioned,
            ) {
                Some((key, tombstone, len, next)) => {
                    cursor.offset = next;
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
        let mut reader = crate::store::SstReader::over(
            &mut scratch.index_buf,
            &mut scratch.data_buf,
            &mut scratch.assembly_buf,
        );
        let mut best: Option<SpillVersion> = None;
        for member in 0..table.n_spill_ssts {
            let handle = table.spill_ssts[member].expect("counted");
            let verdict = reader
                .probe_at(&mut *spill.blocks.borrow_mut(), &handle, rowid, snapshot)
                .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?;
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
                    assembly_buf,
                    ..
                } = &mut *scratch;
                let mut reader = crate::store::SstReader::over(index_buf, data_buf, assembly_buf);
                let got = reader
                    .get_at(&mut *blocks, &handle, rowid, commit_lsn, out)
                    .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?;
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
                    assembly_buf,
                    bounce_buf,
                } = &mut *scratch;
                // The assembly buffer doubles as the row destination: `get`
                // assembles a chained row into the caller buffer directly, so
                // the two uses never overlap. The reader's own staging slot is
                // the bounce buffer (a compressed data block decompresses
                // through it).
                let row_buf = &mut assembly_buf[..len as usize];
                let got = {
                    let mut blocks = spill.blocks.borrow_mut();
                    let mut reader = crate::store::SstReader::over(index_buf, data_buf, bounce_buf);
                    reader
                        .get_at(&mut *blocks, &handle, rowid, commit_lsn, row_buf)
                        .map_err(|e| sql_err!(sqlstate::IO_ERROR, "spill read: {:?}", e))?
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
        scratch: &mut FixedVec<(u32, u64, u8, RowLoc)>,
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
    /// for commit/rollback). Fails fast when another transaction holds an
    /// uncommitted change (SQLSTATE 40001).
    pub fn write_pending(
        &mut self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<Option<Option<RowLoc>>, SqlError> {
        if self.tables[table_index]
            .pending_def_txid
            .is_some_and(|owner| owner != txid)
        {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent table definition change"
            ));
        }
        let oldest_snapshot = self.oldest_snapshot();
        let table = &mut self.tables[table_index];
        if let Some(state) = table.rows.get_mut(&rowid) {
            if let Some(other) = state.locked_by_other(txid) {
                let _ = other;
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent update"
                ));
            }
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
        out: &mut [(usize, u64); MAX_UNIQUE_ENFORCERS],
    ) -> Result<usize, SqlError> {
        let table = &self.tables[table_index];
        let n_enf = table.n_enforcers;
        if n_enf == 0 {
            return Ok(0);
        }
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = table.def.schema(&mut schema);
        let mut cols = [([0u16; MAX_INDEX_COLS], 0usize); MAX_UNIQUE_ENFORCERS];
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
        let mut inserts = [(0usize, 0u64); MAX_UNIQUE_ENFORCERS];
        let n_inserts = match new_loc {
            Some(loc) => self
                .row_enforcer_hashes(table_index, rowid, RowHome::Heap(loc), &mut inserts)
                .expect("new row decodes"),
            None => 0,
        };
        let mut slots = [u32::MAX; MAX_UNIQUE_ENFORCERS];
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
            pool.get_mut(slots[ei])
                .insert(hash, rowid)
                .expect("value index headroom absorbs a commit batch");
        }
    }

    /// Probes the value index for the enforcer covering exactly `columns`,
    /// visiting every candidate rowid whose key hashes to `hash`. Returns true
    /// if an index served the probe; false if no enforcer covers these columns,
    /// so the caller must fall back to a full scan.
    pub fn probe_unique(
        &self,
        table_index: usize,
        columns: &[u16],
        hash: u64,
        mut visit: impl FnMut(u64),
    ) -> bool {
        let table = &self.tables[table_index];
        for i in 0..table.n_enforcers {
            let e = table.enforcers[i].expect("enforcer present");
            if e.columns() == columns {
                self.value_indexes
                    .as_ref()
                    .expect("value index pool present")
                    .get(e.slot)
                    .probe(hash, &mut visit);
                return true;
            }
        }
        false
    }

    /// Whether the enforcer covering `columns` already holds its logical cap of
    /// committed rows, so a further new key would exceed `value_index_rows`.
    pub fn enforcer_at_capacity(&self, table_index: usize, columns: &[u16]) -> bool {
        let table = &self.tables[table_index];
        for i in 0..table.n_enforcers {
            let e = table.enforcers[i].expect("enforcer present");
            if e.columns() == columns {
                return self
                    .value_indexes
                    .as_ref()
                    .expect("value index pool present")
                    .get(e.slot)
                    .len()
                    >= self.value_index_cap;
            }
        }
        false
    }

    /// The configured committed-row cap for a constrained table.
    pub fn value_index_cap(&self) -> usize {
        self.value_index_cap
    }

    /// Releases a table's enforcer index slots back to the pool and clears its
    /// enforcer list. Called before a slot is reused and when a table is
    /// dropped.
    fn release_enforcers(&mut self, table_index: usize) {
        let n = self.tables[table_index].n_enforcers;
        if n == 0 {
            return;
        }
        let mut slots = [u32::MAX; MAX_UNIQUE_ENFORCERS];
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
        self.tables[table_index].enforcers = [None; MAX_UNIQUE_ENFORCERS];
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

    /// Rebuilds a table's value-index enforcers from its current definition and
    /// unique indexes, then repopulates them from the committed rows. Idempotent
    /// — call it whenever the definition, the index set, or the committed rows
    /// change outside the per-row [`Self::commit_row`] maintenance (ALTER, a
    /// committed CREATE, CREATE/DROP INDEX, cold-start replay).
    pub fn refresh_enforcers(&mut self, table_index: usize) -> Result<(), SqlError> {
        self.release_enforcers(table_index);
        let mut want = [([0u16; MAX_INDEX_COLS], 0usize); MAX_UNIQUE_ENFORCERS];
        let mut n_want = 0usize;
        let too_many = || {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a table can have at most {} uniqueness constraints with value indexes",
                MAX_UNIQUE_ENFORCERS
            )
        };
        {
            let def = &self.tables[table_index].def;
            for (i, col) in def.columns().iter().enumerate() {
                if col.unique {
                    if n_want == MAX_UNIQUE_ENFORCERS {
                        return Err(too_many());
                    }
                    want[n_want].0[0] = i as u16;
                    want[n_want].1 = 1;
                    n_want += 1;
                }
            }
            for uk in def.uniques() {
                if n_want == MAX_UNIQUE_ENFORCERS {
                    return Err(too_many());
                }
                let cols = uk.columns();
                want[n_want].0[..cols.len()].copy_from_slice(cols);
                want[n_want].1 = cols.len();
                n_want += 1;
            }
        }
        // A CREATE UNIQUE INDEX keeps its existing full-scan enforcement (a
        // separate feature from the PRIMARY KEY / UNIQUE constraints)
        // — it is not given a value index here, so the pending/live index
        // lifecycle stays out of this path.
        #[allow(clippy::needless_range_loop)]
        for w in 0..n_want {
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
                columns: want[w].0,
                n_cols: want[w].1,
            });
        }
        self.tables[table_index].n_enforcers = n_want;
        self.populate_enforcers(table_index)
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
        let mut slots = [u32::MAX; MAX_UNIQUE_ENFORCERS];
        for (i, s) in slots.iter_mut().enumerate().take(n_enf) {
            *s = self.tables[table_index].enforcers[i]
                .expect("enforcer")
                .slot;
        }
        let mut error: Result<(), SqlError> = Ok(());
        let mut buf = [(0usize, 0u64); MAX_UNIQUE_ENFORCERS];
        self.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(home) = state.committed else {
                return Ok(ControlFlow::Continue(()));
            };
            let n = match self.row_enforcer_hashes(table_index, rowid, home, &mut buf) {
                Ok(n) => n,
                Err(e) => {
                    error = Err(e);
                    return Ok(ControlFlow::Break(()));
                }
            };
            for &(ei, hash) in &buf[..n] {
                if pool.get_mut(slots[ei]).insert(hash, rowid).is_err() {
                    error = Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table \"{}\" exceeds its value-index capacity ({} rows); raise value_index_rows",
                        self.tables[table_index].def.name.as_str(),
                        self.value_index_cap
                    ));
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
        error
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
            let _ = other;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent DDL on \"{}\"",
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
        self.catalog_seq += 1;
        let stamp = self.catalog_seq;
        let table = &mut self.tables[slot];
        table.def = def;
        table.created_at = stamp;
        table.rows.clear();
        table.live = pending.is_none();
        table.pending_ddl = pending;
        table.mark_dirty();
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
                    let name = col.domain.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "reloaded enum column has no type name"
                        )
                    })?;
                    let slot = match col.user_type_schema {
                        Some(schema) => self
                            .enum_slot(schema.as_str(), name.as_str(), 0)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "enum type \"{}.{}\" for a reloaded column does not exist",
                                    schema.as_str(),
                                    name.as_str()
                                )
                            })?,
                        None => self.unique_enum_slot_by_name(name.as_str(), 0)?,
                    };
                    def.columns[i].ctype = if matches!(col.ctype, ColType::Array(_)) {
                        ColType::Array(ArrElem::Enum(slot as u16))
                    } else {
                        ColType::Enum(slot as u16)
                    };
                }
                ColType::Array(ArrElem::Domain { .. }) => {
                    let name = col.domain.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "reloaded domain-array column has no type name"
                        )
                    })?;
                    let slot = match col.user_type_schema {
                        Some(schema) => self
                            .domain_slot(schema.as_str(), name.as_str(), 0)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "domain type \"{}.{}\" for a reloaded column does not exist",
                                    schema.as_str(),
                                    name.as_str()
                                )
                            })?,
                        None => self.unique_domain_slot_by_name(name.as_str(), 0)?,
                    };
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

    /// Transactional create: the table exists only for `txid` until commit.
    /// A name already visible to `txid` is a duplicate (42P07); a name held by
    /// another transaction's uncommitted DDL is a conflict (40001).
    pub fn create_table_in(&mut self, def: TableDef, txid: u32) -> Result<usize, SqlError> {
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
            let _ = other;
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent DDL on \"{}\"",
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
        self.tables[index].live = false;
        self.tables[index].pending_ddl = None;
        self.tables[index].rows.clear();
    }

    /// Rolls back an uncommitted CREATE, freeing the slot.
    pub fn rollback_create(&mut self, index: usize) {
        self.release_enforcers(index);
        self.clear_pending_table_defs(index);
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

    /// Committed views with their slot indices, for OID assignment.
    pub fn views_with_slots(&self) -> impl Iterator<Item = (usize, &ViewDef)> {
        self.views.iter().enumerate().filter(|(_, v)| v.live)
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
        self.matviews.iter().filter(|m| m.live)
    }

    pub fn matviews_with_slots(&self) -> impl Iterator<Item = (usize, &MatviewDef)> {
        self.matviews
            .iter()
            .enumerate()
            .filter(|(_, matview)| matview.live)
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
        if self.matviews.iter().any(|m| {
            m.schema.as_str() == schema.as_str()
                && m.name.as_str() == name.as_str()
                && matches!(m.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
        }
        let Some(new) = self
            .matviews
            .iter()
            .position(|m| !m.live && m.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many materialized views (limit {})",
                self.matviews.len()
            ));
        };
        self.catalog_seq += 1;
        self.matviews[new] = MatviewDef {
            created_at: self.catalog_seq,
            schema,
            name,
            sql: query.sql,
            creation_path: query.creation_path,
            populated,
            live: false,
            pending: Some(PendingDdl {
                txid,
                creating: true,
            }),
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
        if self.matviews.iter().any(|m| {
            m.schema.as_str() == schema
                && m.name.as_str() == name
                && matches!(m.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
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
        if matches!(m.pending, Some(p) if p.txid == txid && p.creating) {
            m.live = false;
            m.pending = None;
        } else {
            m.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
    }

    pub fn commit_matview_create(&mut self, slot: usize) {
        self.matviews[slot].live = true;
        self.matviews[slot].pending = None;
    }

    pub fn commit_matview_drop(&mut self, slot: usize) {
        let (schema, name) = (self.matviews[slot].schema, self.matviews[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.matviews[slot].live = false;
        self.matviews[slot].pending = None;
    }

    pub fn rollback_matview_create(&mut self, slot: usize) {
        self.matviews[slot].live = false;
        self.matviews[slot].pending = None;
    }

    pub fn rollback_matview_drop(&mut self, slot: usize, txid: u32) {
        let m = &mut self.matviews[slot];
        if m.live {
            m.pending = None;
        } else if matches!(m.pending, Some(p) if p.txid == txid) {
            m.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
        }
    }

    // --- Sequences -------------------------------------------------------

    pub fn live_sequences(&self) -> impl Iterator<Item = &SequenceDef> {
        self.sequences.iter().filter(|s| s.live)
    }

    pub fn sequences_with_slots(&self) -> impl Iterator<Item = (usize, &SequenceDef)> {
        self.sequences.iter().enumerate().filter(|(_, s)| s.live)
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
        if self.sequences.iter().any(|s| {
            s.schema.as_str() == schema.as_str()
                && s.name.as_str() == name.as_str()
                && matches!(s.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
        }
        let Some(new) = self
            .sequences
            .iter()
            .position(|s| !s.live && s.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many sequences (limit {})",
                self.sequences.len()
            ));
        };
        self.catalog_seq += 1;
        self.sequences[new] = SequenceDef {
            created_at: self.catalog_seq,
            schema,
            name,
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
            live: false,
            pending: Some(PendingDdl {
                txid,
                creating: true,
            }),
        };
        Ok(new)
    }

    /// Replaces a live sequence's parameters in place (ALTER SEQUENCE). Value
    /// state (last_value/is_called) is untouched unless `restart` is given.
    pub fn alter_sequence(
        &mut self,
        slot: usize,
        spec: SeqSpec,
        restart: Option<i64>,
        owner: Option<SequenceOwner>,
        generator_for: Option<SequenceOwner>,
    ) {
        let s = &mut self.sequences[slot];
        s.data_type = spec.data_type;
        s.increment = spec.increment;
        s.min_value = spec.min_value;
        s.max_value = spec.max_value;
        s.start_value = spec.start_value;
        s.cache = spec.cache;
        s.cycle = spec.cycle;
        s.owner = owner;
        s.generator_for = generator_for;
        if let Some(r) = restart {
            s.last_value.set(r);
            s.is_called.set(false);
            s.dirty.set(true);
        }
    }

    pub fn drop_sequence(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if self.sequences.iter().any(|s| {
            s.schema.as_str() == schema
                && s.name.as_str() == name
                && matches!(s.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self.sequences.iter().position(|s| {
            s.visible_to(txid) && s.schema.as_str() == schema && s.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let s = &mut self.sequences[i];
        if matches!(s.pending, Some(p) if p.txid == txid && p.creating) {
            s.live = false;
            s.pending = None;
        } else {
            s.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
        Ok(Some(i))
    }

    pub fn commit_sequence_create(&mut self, slot: usize) {
        self.sequences[slot].live = true;
        self.sequences[slot].pending = None;
    }

    pub fn commit_sequence_drop(&mut self, slot: usize) {
        let (schema, name) = (self.sequences[slot].schema, self.sequences[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
        self.sequences[slot].live = false;
        self.sequences[slot].pending = None;
    }

    pub fn rollback_sequence_create(&mut self, slot: usize) {
        self.sequences[slot].live = false;
        self.sequences[slot].pending = None;
    }

    pub fn rollback_sequence_drop(&mut self, slot: usize, txid: u32) {
        let s = &mut self.sequences[slot];
        if s.live {
            s.pending = None;
        } else if matches!(s.pending, Some(p) if p.txid == txid) {
            s.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
        }
    }

    /// Applies a replayed/absolute `SequenceAdvance`: set value state directly,
    /// without marking dirty (replay must not re-journal).
    pub fn apply_sequence_advance(&mut self, schema: &str, name: &str, last: i64, is_called: bool) {
        if let Some(i) = self.sequences.iter().position(|s| {
            (s.live || s.pending.is_some())
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
    pub fn find_domain(&self, type_name: &str, txid: u32) -> Option<&DomainDef> {
        let (qualifier, name) = match type_name.split_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, type_name),
        };
        self.find_domain_slot(qualifier, name, txid)
            .map(|slot| &self.domains[slot])
    }

    fn find_domain_slot(&self, qualifier: Option<&str>, name: &str, txid: u32) -> Option<usize> {
        if let Some(schema) = qualifier {
            return self.domains.iter().position(|d| {
                d.visible_to(txid) && d.schema.as_str() == schema && d.name.as_str() == name
            });
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema = self.schemas[*slot as usize].name;
                if let Some(i) = self.domains.iter().position(|d| {
                    d.visible_to(txid)
                        && d.schema.as_str() == schema.as_str()
                        && d.name.as_str() == name
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
    pub fn domain_by_name(&self, name: &str, txid: u32) -> Option<&DomainDef> {
        self.domains
            .iter()
            .find(|d| d.visible_to(txid) && d.name.as_str() == name)
    }

    fn unique_domain_slot_by_name(&self, name: &str, txid: u32) -> Result<usize, SqlError> {
        let mut matches = self
            .domains
            .iter()
            .enumerate()
            .filter(|(_, domain)| domain.visible_to(txid) && domain.name.as_str() == name);
        let Some((slot, _)) = matches.next() else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "domain type \"{}\" for a reloaded column does not exist",
                name
            ));
        };
        if matches.next().is_some() {
            return Err(sql_err!(
                sqlstate::AMBIGUOUS_COLUMN,
                "domain type \"{}\" for a reloaded column is ambiguous",
                name
            ));
        }
        Ok(slot)
    }

    /// The domain named `(schema, name)` visible to `txid`, by slot.
    pub fn domain_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.domains.iter().position(|d| {
            d.visible_to(txid) && d.schema.as_str() == schema && d.name.as_str() == name
        })
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
        self.domains.iter().enumerate().filter(|(_, d)| d.live)
    }

    /// Whether any table column (in any table) is declared with this domain —
    /// the dependency that makes `DROP DOMAIN ... RESTRICT` fail.
    pub fn domain_in_use(&self, schema: &str, name: &str) -> Option<(SqlName, SqlName)> {
        for table in self.tables.iter().filter(|t| t.live) {
            for col in table.def.columns() {
                if col.domain.is_some_and(|domain| domain.as_str() == name)
                    && col
                        .user_type_schema
                        .is_some_and(|domain_schema| domain_schema.as_str() == schema)
                {
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
        if self.domains.iter().any(|d| {
            d.schema.as_str() == schema.as_str()
                && d.name.as_str() == name.as_str()
                && matches!(d.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
        }
        let Some(new) = self
            .domains
            .iter()
            .position(|d| !d.live && d.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many domains (limit {})",
                self.domains.len()
            ));
        };
        self.catalog_seq += 1;
        let pending = (txid != 0).then_some(PendingDdl {
            txid,
            creating: true,
        });
        self.domains[new] = DomainDef {
            created_at: self.catalog_seq,
            schema,
            name,
            base_domain: spec.base_domain,
            base_domain_schema: spec.base_domain_schema,
            base: spec.base,
            base_type_mod: spec.base_type_mod,
            not_null: spec.not_null,
            default_expr: spec.default_expr,
            checks: spec.checks,
            n_checks: spec.n_checks,
            live: txid == 0,
            pending,
        };
        Ok(new)
    }

    /// Replaces a live domain's spec in place (ALTER DOMAIN).
    pub fn alter_domain(&mut self, slot: usize, spec: DomainSpec) {
        let d = &mut self.domains[slot];
        d.base_domain = spec.base_domain;
        d.base_domain_schema = spec.base_domain_schema;
        d.base = spec.base;
        d.base_type_mod = spec.base_type_mod;
        d.not_null = spec.not_null;
        d.default_expr = spec.default_expr;
        d.checks = spec.checks;
        d.n_checks = spec.n_checks;
    }

    pub fn restore_domain(&mut self, slot: usize, prior: DomainDef) {
        self.domains[slot] = prior;
    }

    pub fn restore_domain_nullability(&mut self, slot: usize, prior: bool) {
        self.domains[slot].not_null = prior;
    }

    pub fn restore_domain_default(
        &mut self,
        slot: usize,
        prior: Option<StackStr<DEFAULT_EXPR_MAX>>,
    ) {
        self.domains[slot].default_expr = prior;
    }

    pub fn undo_domain_check_add(&mut self, slot: usize, prior_count: usize) {
        let domain = &mut self.domains[slot];
        for check in domain.checks[prior_count..domain.n_checks].iter_mut() {
            *check = CheckConstraint::EMPTY;
        }
        domain.n_checks = prior_count;
    }

    pub fn restore_domain_check(&mut self, slot: usize, index: usize, prior: CheckConstraint) {
        let domain = &mut self.domains[slot];
        for position in (index..domain.n_checks).rev() {
            domain.checks[position + 1] = domain.checks[position];
        }
        domain.checks[index] = prior;
        domain.n_checks += 1;
    }

    pub fn drop_domain(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if self.domains.iter().any(|d| {
            d.schema.as_str() == schema
                && d.name.as_str() == name
                && matches!(d.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self.domains.iter().position(|d| {
            d.visible_to(txid) && d.schema.as_str() == schema && d.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let d = &mut self.domains[i];
        if matches!(d.pending, Some(p) if p.txid == txid && p.creating) {
            d.live = false;
            d.pending = None;
        } else {
            d.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
        Ok(Some(i))
    }

    pub fn commit_domain_create(&mut self, slot: usize) {
        self.domains[slot].live = true;
        self.domains[slot].pending = None;
    }

    pub fn commit_domain_drop(&mut self, slot: usize) {
        let (schema, name) = (self.domains[slot].schema, self.domains[slot].name);
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.domains[slot].live = false;
        self.domains[slot].pending = None;
    }

    pub fn rollback_domain_create(&mut self, slot: usize) {
        self.domains[slot].live = false;
        self.domains[slot].pending = None;
    }

    pub fn rollback_domain_drop(&mut self, slot: usize, txid: u32) {
        let d = &mut self.domains[slot];
        if d.live {
            d.pending = None;
        } else if matches!(d.pending, Some(p) if p.txid == txid) {
            d.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
        }
    }

    // --- Enum types (CREATE TYPE ... AS ENUM) ---

    /// The slot of a (possibly schema-qualified) enum type name, visible to
    /// `txid`, searching the current path when unqualified.
    fn find_enum_slot(&self, qualifier: Option<&str>, name: &str, txid: u32) -> Option<usize> {
        if let Some(schema) = qualifier {
            return self.enums.iter().position(|e| {
                e.visible_to(txid) && e.schema.as_str() == schema && e.name.as_str() == name
            });
        }
        for entry in self.path.entries() {
            if let PathEntry::Schema(slot) = entry {
                let schema = self.schemas[*slot as usize].name;
                if let Some(i) = self.enums.iter().position(|e| {
                    e.visible_to(txid)
                        && e.schema.as_str() == schema.as_str()
                        && e.name.as_str() == name
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
            .find(|e| e.visible_to(txid) && e.name.as_str() == name)
    }

    /// The slot of an enum named `name` (any schema) visible to `txid`.
    pub fn enum_slot_by_name(&self, name: &str, txid: u32) -> Option<usize> {
        self.enums
            .iter()
            .position(|e| e.visible_to(txid) && e.name.as_str() == name)
    }

    fn unique_enum_slot_by_name(&self, name: &str, txid: u32) -> Result<usize, SqlError> {
        let mut matches = self.enums.iter().enumerate().filter(|(_, enumeration)| {
            enumeration.visible_to(txid) && enumeration.name.as_str() == name
        });
        let Some((slot, _)) = matches.next() else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "enum type \"{}\" for a reloaded column does not exist",
                name
            ));
        };
        if matches.next().is_some() {
            return Err(sql_err!(
                sqlstate::AMBIGUOUS_COLUMN,
                "enum type \"{}\" for a reloaded column is ambiguous",
                name
            ));
        }
        Ok(slot)
    }

    /// The enum named `(schema, name)` visible to `txid`, by slot.
    pub fn enum_slot(&self, schema: &str, name: &str, txid: u32) -> Option<usize> {
        self.enums.iter().position(|e| {
            e.visible_to(txid) && e.schema.as_str() == schema && e.name.as_str() == name
        })
    }

    pub fn enum_def(&self, slot: usize) -> &EnumDef {
        &self.enums[slot]
    }

    pub(crate) fn enum_count(&self) -> usize {
        self.enums.len()
    }

    /// Committed enums carrying their slot indices, for the checkpoint,
    /// `pg_type` and `pg_enum`.
    pub fn live_enums(&self) -> impl Iterator<Item = (usize, &EnumDef)> {
        self.enums.iter().enumerate().filter(|(_, e)| e.live)
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
        if self.enums.iter().any(|e| {
            e.schema.as_str() == schema.as_str()
                && e.name.as_str() == name.as_str()
                && matches!(e.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name.as_str()
            ));
        }
        let Some(new) = self
            .enums
            .iter()
            .position(|e| !e.live && e.pending.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many enum types (limit {})",
                self.enums.len()
            ));
        };
        self.catalog_seq += 1;
        let pending = (txid != 0).then_some(PendingDdl {
            txid,
            creating: true,
        });
        self.enums[new] = EnumDef {
            created_at: self.catalog_seq,
            schema,
            name,
            members: spec.members,
            n_members: spec.n_members,
            live: txid == 0,
            pending,
        };
        Ok(new)
    }

    /// Replaces a live enum's members in place (ALTER TYPE ... ADD VALUE).
    pub fn alter_enum(&mut self, slot: usize, spec: EnumSpec) {
        let e = &mut self.enums[slot];
        e.members = spec.members;
        e.n_members = spec.n_members;
    }

    /// Renames an enum and every persisted reference to its type name. Runtime
    /// slots and value sort keys stay stable; comments are name-keyed and move
    /// with the type just as PostgreSQL keeps the same `pg_type` OID.
    pub fn rename_enum(&mut self, slot: usize, new_name: SqlName) {
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
                    column.domain = Some(new_name);
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

    pub fn restore_enum(&mut self, slot: usize, prior: EnumDef) {
        if self.enums[slot].name != prior.name {
            self.rename_enum(slot, prior.name);
        }
        self.enums[slot] = prior;
    }

    pub fn undo_enum_value_add(&mut self, slot: usize, prior_count: usize) {
        let definition = &mut self.enums[slot];
        for member in definition.members[prior_count..definition.n_members].iter_mut() {
            *member = EnumMember::EMPTY;
        }
        definition.n_members = prior_count;
    }

    pub fn restore_enum_value_name(&mut self, slot: usize, index: usize, prior: SqlName) {
        self.enums[slot].members[index].label = prior;
    }

    pub fn drop_enum(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
        if self.enums.iter().any(|e| {
            e.schema.as_str() == schema
                && e.name.as_str() == name
                && matches!(e.pending, Some(p) if p.txid != txid)
        }) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "could not serialize access: uncommitted DDL on \"{}\" by another transaction",
                name
            ));
        }
        let Some(i) = self.enums.iter().position(|e| {
            e.visible_to(txid) && e.schema.as_str() == schema && e.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let e = &mut self.enums[i];
        if matches!(e.pending, Some(p) if p.txid == txid && p.creating) {
            e.live = false;
            e.pending = None;
        } else {
            e.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
        Ok(Some(i))
    }

    pub fn commit_enum_create(&mut self, slot: usize) {
        self.enums[slot].live = true;
        self.enums[slot].pending = None;
    }

    pub fn commit_enum_drop(&mut self, slot: usize) {
        let (schema, name) = (self.enums[slot].schema, self.enums[slot].name);
        self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        self.enums[slot].live = false;
        self.enums[slot].pending = None;
    }

    pub fn rollback_enum_create(&mut self, slot: usize) {
        self.enums[slot].live = false;
        self.enums[slot].pending = None;
    }

    pub fn rollback_enum_drop(&mut self, slot: usize, txid: u32) {
        let e = &mut self.enums[slot];
        if e.live {
            e.pending = None;
        } else if matches!(e.pending, Some(p) if p.txid == txid) {
            e.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
        }
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
            sql: query.sql,
            creation_path: query.creation_path,
            live: false,
            pending: Some(PendingDdl {
                txid,
                creating: true,
            }),
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
            v.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
    }

    /// Promotes an uncommitted CREATE VIEW into the committed catalog.
    pub fn commit_view_create(&mut self, slot: usize) {
        let schema = self.views[slot].schema;
        let name = self.views[slot].name;
        if let Some(old_slot) = self.views.iter().enumerate().find_map(|(old_slot, view)| {
            (old_slot != slot && view.live && view.schema == schema && view.name == name)
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
        self.views[slot].live = true;
        self.views[slot].pending = None;
    }

    /// Promotes an uncommitted DROP VIEW into the committed catalog.
    pub fn commit_view_drop(&mut self, slot: usize) {
        let (schema, name) = (self.views[slot].schema, self.views[slot].name);
        // CREATE OR REPLACE installs the replacement before retiring this
        // slot. Comments belong to the logical same-named object and survive;
        // an ordinary DROP has no replacement and removes them.
        let replaced = self.views.iter().enumerate().any(|(other, view)| {
            other != slot && view.live && view.schema == schema && view.name == name
        });
        if !replaced {
            self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
            self.drop_object_comments(CommentClass::Type, schema.as_str(), name.as_str());
        }
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
            v.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
        }
    }

    pub fn index_exists(&self, schema: &str, name: &str, txid: u32) -> bool {
        self.indexes
            .iter()
            .any(|x| x.visible_to(txid) && x.schema.as_str() == schema && x.name.as_str() == name)
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
            pending: Some(PendingDdl {
                txid,
                creating: true,
            }),
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
    pub fn drop_index(
        &mut self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<usize>, SqlError> {
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
            x.pending = Some(PendingDdl {
                txid,
                creating: false,
            });
        }
    }

    /// Promotes an uncommitted CREATE INDEX into the committed catalog.
    pub fn commit_index_create(&mut self, slot: usize) {
        self.indexes[slot].live = true;
        self.indexes[slot].pending = None;
    }

    /// Promotes an uncommitted DROP INDEX into the committed catalog.
    pub fn commit_index_drop(&mut self, slot: usize) {
        let (schema, name) = (self.indexes[slot].schema, self.indexes[slot].name);
        self.drop_object_comments(CommentClass::Relation, schema.as_str(), name.as_str());
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
            x.pending = Some(PendingDdl {
                txid,
                creating: true,
            });
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
        self.indexes.iter().filter(|x| x.live)
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
            if x.live
                && x.schema.as_str() == old_schema.as_str()
                && x.table.as_str() == name.as_str()
            {
                x.schema = new_schema;
            }
        }
        for sequence in self.sequences.iter_mut() {
            if !sequence.live {
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
        if let Some(index) = self
            .active_snapshots
            .iter()
            .position(|(owner, _)| *owner == txid)
        {
            self.active_snapshots.swap_remove(index);
        }
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

    pub fn lock_table_access_share(&mut self, txid: u32, table: usize) -> Result<(), SqlError> {
        if self
            .table_locks
            .iter()
            .any(|(owner, slot)| *owner == txid && *slot == table as u32)
        {
            return Ok(());
        }
        self.table_locks.push((txid, table as u32)).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table-lock registry is full ({} locks)",
                self.table_locks.capacity()
            )
        })
    }

    pub fn release_table_locks(&mut self, txid: u32) {
        let mut index = 0usize;
        while index < self.table_locks.len() {
            if self.table_locks[index].0 == txid {
                self.table_locks.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    pub fn has_access_share_locks(&self) -> bool {
        !self.table_locks.is_empty()
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
        c.max_tables = 4;
        c.table_rows = 128;
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
                default_value: None,
                default_expr: None,
                is_generated: false,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                domain: None,
                user_type_schema: None,
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
                default_expr: None,
                is_generated: false,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                domain: None,
                user_type_schema: None,
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
