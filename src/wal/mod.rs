//! Preallocated, checksummed write-ahead log and recovery codec.

pub(crate) mod crc32c;

use crate::sql::eval::sqlstate;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql::eval::SqlError;
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    CheckConstraint, ColumnDefault, ColumnMeta, ColumnStatistics, DependencyClass, FkAction,
    ForeignKey, MAX_COLUMNS, MAX_INDEX_COLS, MAX_MULTICOLUMN_STATISTICS, MultiColumnStatistics,
    OwnedDatum, RoleAttributes, SqlName, StoredQueryDependencies, TableDef, TableStatistics,
    UniqueKey,
};

use crc32c::crc32c;

/// On-disk record header length shared by recovery and logical decoding.
pub(crate) const HEADER_LEN: usize = 24;

/// One contiguous, committed journal range ready for immutable publication.
/// Construction is private to the journal, so callers cannot pair an LSN with
/// offsets from another batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedBatch {
    first_lsn: u64,
    start: u64,
    end: u64,
}

impl CommittedBatch {
    pub const fn first_lsn(self) -> u64 {
        self.first_lsn
    }

    pub const fn byte_len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub const fn start(self) -> u64 {
        self.start
    }
}
const TABLE_STATISTICS_V2: u8 = u8::MAX;

const KIND_CREATE: u8 = 1;
const KIND_DROP: u8 = 2;
const KIND_UPSERT: u8 = 3;
const KIND_DELETE: u8 = 4;
const KIND_CREATE_VIEW: u8 = 5;
const KIND_DROP_VIEW: u8 = 6;
const KIND_CREATE_INDEX: u8 = 7;
const KIND_DROP_INDEX: u8 = 8;
const KIND_SEQUENCE_SET: u8 = 9;
const KIND_CREATE_SCHEMA: u8 = 10;
const KIND_DROP_SCHEMA: u8 = 11;
const KIND_SET_TABLE_SCHEMA: u8 = 12;
const KIND_DROP_FK: u8 = 13;
const KIND_CREATE_MATVIEW: u8 = 14;
const KIND_DROP_MATVIEW: u8 = 15;
const KIND_SET_MATVIEW_POPULATED: u8 = 16;
const KIND_CREATE_SEQUENCE: u8 = 17;
const KIND_DROP_SEQUENCE: u8 = 18;
const KIND_SEQUENCE_ADVANCE: u8 = 19;
const KIND_COMMENT: u8 = 20;
const KIND_CREATE_DOMAIN: u8 = 21;
const KIND_DROP_DOMAIN: u8 = 22;
const KIND_CREATE_ENUM: u8 = 23;
const KIND_DROP_ENUM: u8 = 24;
const KIND_RENAME_ENUM: u8 = 25;
const KIND_ANALYZE: u8 = 26;
const KIND_UPSERT_ROLE: u8 = 27;
const KIND_DROP_ROLE: u8 = 28;
const KIND_UPSERT_ROLE_MEMBERSHIP: u8 = 29;
const KIND_DROP_ROLE_MEMBERSHIP: u8 = 30;
const KIND_SET_OBJECT_OWNER: u8 = 31;
const KIND_SET_OBJECT_ACL: u8 = 32;
const KIND_REWRITE_TABLE: u8 = 33;
const KIND_SET_DEFAULT_ACL: u8 = 34;
const KIND_CREATE_PUBLICATION: u8 = 35;
const KIND_DROP_PUBLICATION: u8 = 36;
const KIND_ALTER_PUBLICATION: u8 = 42;
const KIND_SET_PUBLICATION_OWNER: u8 = 43;
const KIND_RENAME_PUBLICATION: u8 = 44;
const KIND_CREATE_ROUTINE: u8 = 45;
const KIND_DROP_ROUTINE: u8 = 46;
const KIND_ALTER_ROUTINE_IDENTITY: u8 = 47;
const KIND_ALTER_DOMAIN_IDENTITY: u8 = 48;
const KIND_RENAME_INDEX: u8 = 49;
/// A durable transaction boundary. Logical replication may expose only the
/// records preceding one of these markers.
const KIND_COMMIT: u8 = 37;
const KIND_CREATE_REPLICATION_SLOT: u8 = 38;
const KIND_DROP_REPLICATION_SLOT: u8 = 39;
const KIND_ADVANCE_REPLICATION_SLOT: u8 = 40;
const KIND_TRUNCATE: u8 = 41;
const LAST_KIND: u8 = KIND_RENAME_INDEX;
const DOMAIN_PAYLOAD_WITH_PARENT: u8 = u8::MAX;

/// SQLSTATE 53100 disk_full.
const JOURNAL_FULL: &str = "53100";

fn append_stored_dependency_name(buffer: &mut FixedBuf, name: &str) -> bool {
    name.len() <= u8::MAX as usize
        && buffer.append(&[name.len() as u8])
        && buffer.append(name.as_bytes())
}

/// Stored-query dependencies cross the WAL boundary in one of two compact
/// forms: a reference to the creation-time set while appending, or a slice of
/// the encoded record while replaying. Keeping the fixed dependency array out
/// of [`WalOp`] prevents every WAL operation from inheriting the largest
/// variant's stack footprint.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WalStoredQueryDependencies<'a> {
    Captured(&'a StoredQueryDependencies),
    Encoded(&'a [u8]),
    LegacyEmpty,
}

impl WalStoredQueryDependencies<'_> {
    fn encoded_len(self) -> usize {
        match self {
            Self::Captured(dependencies) => {
                2 + dependencies
                    .entries()
                    .iter()
                    .map(|dependency| {
                        1 + 8
                            + 1
                            + dependency.schema.as_str().len()
                            + 1
                            + dependency.name.as_str().len()
                            + 1
                            + dependency.referenced_schema.as_str().len()
                            + 1
                            + dependency.referenced_name.as_str().len()
                    })
                    .sum::<usize>()
            }
            Self::Encoded(bytes) => bytes.len(),
            Self::LegacyEmpty => 0,
        }
    }

    fn append(self, buffer: &mut FixedBuf) -> bool {
        match self {
            Self::Captured(dependencies) => {
                let mut ok = buffer.append(&[0xff, dependencies.entries().len() as u8]);
                for dependency in dependencies.entries() {
                    ok &= buffer.append(&[dependency.class as u8])
                        && buffer.append(&dependency.referenced_columns.to_le_bytes())
                        && append_stored_dependency_name(buffer, dependency.schema.as_str())
                        && append_stored_dependency_name(buffer, dependency.name.as_str())
                        && append_stored_dependency_name(
                            buffer,
                            dependency.referenced_schema.as_str(),
                        )
                        && append_stored_dependency_name(
                            buffer,
                            dependency.referenced_name.as_str(),
                        );
                }
                ok
            }
            Self::Encoded(bytes) => buffer.append(bytes),
            Self::LegacyEmpty => true,
        }
    }

    #[inline(never)]
    pub(crate) fn materialize(self) -> Result<StoredQueryDependencies, SqlError> {
        match self {
            Self::Captured(dependencies) => Ok(*dependencies),
            Self::Encoded(bytes) => decode_stored_query_dependencies(bytes).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt stored-query dependencies in journal"
                )
            }),
            Self::LegacyEmpty => Ok(StoredQueryDependencies::EMPTY),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WalTableStatistics<'a> {
    Captured(&'a TableStatistics),
    Encoded(&'a [u8]),
}

impl WalTableStatistics<'_> {
    fn encoded_len(self) -> usize {
        match self {
            Self::Captured(statistics) => {
                1 + 8
                    + 4
                    + 8
                    + 1
                    + 1
                    + statistics
                        .columns
                        .iter()
                        .filter(|column| column.valid)
                        .count()
                        * (1 + 4 + 8 + 4 + 4)
                    + statistics
                        .multi_columns
                        .iter()
                        .filter(|statistics| statistics.valid)
                        .map(|statistics| 1 + statistics.n_columns as usize * 2 + 8 + 8)
                        .sum::<usize>()
            }
            Self::Encoded(bytes) => bytes.len(),
        }
    }

    fn append(self, buffer: &mut FixedBuf) -> bool {
        match self {
            Self::Captured(statistics) => {
                let valid_columns = statistics
                    .columns
                    .iter()
                    .filter(|column| column.valid)
                    .count();
                let valid_multi_columns = statistics
                    .multi_columns
                    .iter()
                    .filter(|statistics| statistics.valid)
                    .count();
                let mut ok = buffer.append(&[TABLE_STATISTICS_V2])
                    && buffer.append(&statistics.rows.to_le_bytes())
                    && buffer.append(&statistics.average_row_width.to_le_bytes())
                    && buffer.append(&statistics.analyzed_generation.to_le_bytes())
                    && buffer.append(&[valid_columns as u8])
                    && buffer.append(&[valid_multi_columns as u8]);
                for (index, column) in statistics.columns.iter().enumerate() {
                    if !column.valid {
                        continue;
                    }
                    ok &= buffer.append(&[index as u8])
                        && buffer.append(&column.null_fraction_ppm.to_le_bytes())
                        && buffer.append(&column.distinct_values.to_le_bytes())
                        && buffer.append(&column.distinct_fraction_ppm.to_le_bytes())
                        && buffer.append(&column.average_width.to_le_bytes());
                }
                for multi in statistics
                    .multi_columns
                    .iter()
                    .filter(|statistics| statistics.valid)
                {
                    ok &= buffer.append(&[multi.n_columns]);
                    for column in &multi.columns[..multi.n_columns as usize] {
                        ok &= buffer.append(&column.to_le_bytes());
                    }
                    ok &= buffer.append(&multi.non_null_rows.to_le_bytes())
                        && buffer.append(&multi.distinct_values.to_le_bytes());
                }
                ok
            }
            Self::Encoded(bytes) => buffer.append(bytes),
        }
    }

    pub(crate) fn materialize(self) -> Result<TableStatistics, SqlError> {
        match self {
            Self::Captured(statistics) => Ok(*statistics),
            Self::Encoded(bytes) => decode_table_statistics(bytes).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt table statistics in journal"
                )
            }),
        }
    }
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "TableDef is a fixed inline array by design (no heap); WalOp lives briefly on the stack"
)]
pub(crate) enum WalOp<'a> {
    CreateTable(TableDef),
    /// Begins ALTER TABLE's in-place definition/row rewrite. The immediately
    /// following CreateTable record supplies the final definition; this marker
    /// carries the old identity and composed column ordinals without inflating
    /// every WAL operation by another inline TableDef.
    BeginTableRewrite {
        previous_schema: &'a str,
        previous_name: &'a str,
        column_mapping: [u16; MAX_COLUMNS],
    },
    DropTable {
        schema: &'a str,
        name: &'a str,
    },
    Upsert {
        schema: &'a str,
        table: &'a str,
        rowid: u64,
        row: &'a [u8],
        /// True when this replaces a committed row rather than inserting one.
        is_update: bool,
        /// The previous committed image, retained for pgoutput's replica
        /// identity tuple. Inserts carry no old image.
        old_row: Option<&'a [u8]>,
        command_id: u32,
    },
    Delete {
        schema: &'a str,
        table: &'a str,
        rowid: u64,
        /// The removed committed image, retained for pgoutput's replica
        /// identity tuple.
        old_row: Option<&'a [u8]>,
        command_id: u32,
    },
    /// Statement-level logical change. Heap rows remain separate DELETE WAL
    /// records for recovery; the command lets pgoutput publish the protocol's
    /// compact truncate message without guessing from those records.
    Truncate {
        /// Length-prefixed schema/table pairs, excluding the count byte.
        tables: &'a [u8],
        table_count: usize,
        cascade: bool,
        restart_identity: bool,
        command_id: u32,
    },
    CreateView {
        schema: &'a str,
        name: &'a str,
        sql: &'a str,
        /// The creator's search_path, under which the body re-resolves.
        path: &'a str,
        dependencies: WalStoredQueryDependencies<'a>,
    },
    DropView {
        schema: &'a str,
        name: &'a str,
    },
    CreatePublication {
        name: &'a str,
        owner: u16,
        all_tables: bool,
        tables: [u16; crate::storage::MAX_PUBLICATION_TABLES],
        table_count: usize,
        schemas: [u8; crate::storage::MAX_SCHEMAS],
        schema_count: usize,
        publish_insert: bool,
        publish_update: bool,
        publish_delete: bool,
        publish_truncate: bool,
    },
    DropPublication {
        name: &'a str,
    },
    /// The complete post-ALTER definition.  Replay never derives a new
    /// publication from a possibly different predecessor.
    AlterPublication {
        name: &'a str,
        all_tables: bool,
        tables: [u16; crate::storage::MAX_PUBLICATION_TABLES],
        table_count: usize,
        schemas: [u8; crate::storage::MAX_SCHEMAS],
        schema_count: usize,
        publish_insert: bool,
        publish_update: bool,
        publish_delete: bool,
        publish_truncate: bool,
    },
    SetPublicationOwner {
        name: &'a str,
        owner: u16,
    },
    RenamePublication {
        name: &'a str,
        new_name: &'a str,
    },
    /// Marks every preceding record in the committed batch as one atomic
    /// transaction. It has no storage replay effect of its own.
    Commit {
        transaction_id: u32,
    },
    CreateReplicationSlot {
        name: &'a str,
        restart_lsn: u64,
    },
    DropReplicationSlot {
        name: &'a str,
    },
    AdvanceReplicationSlot {
        name: &'a str,
        confirmed_flush_lsn: u64,
    },
    CreateIndex {
        schema: &'a str,
        name: &'a str,
        table: &'a str,
        columns: [u16; MAX_INDEX_COLS],
        /// Canonical source for expression keys; `None` denotes the matching
        /// physical table column in `columns`.
        expressions: [Option<&'a str>; MAX_INDEX_COLS],
        include_columns: [u16; MAX_INDEX_COLS],
        descending: [bool; MAX_INDEX_COLS],
        nulls_first: [bool; MAX_INDEX_COLS],
        n_cols: usize,
        n_include_cols: usize,
        nulls_not_distinct: bool,
        /// Absent for a full-table index; otherwise the canonical predicate
        /// source persisted alongside the physical key columns.
        predicate: Option<&'a str>,
        unique: bool,
    },
    DropIndex {
        schema: &'a str,
        name: &'a str,
    },
    RenameIndex {
        schema: &'a str,
        name: &'a str,
        new_name: &'a str,
    },
    /// A serial/identity column's sequence position: the last value handed
    /// out. Absolute, so replay is idempotent and order-tolerant within a
    /// table's records.
    SequenceSet {
        schema: &'a str,
        table: &'a str,
        column: u16,
        last: i64,
    },
    CreateSchema(&'a str),
    DropSchema(&'a str),
    /// ALTER TABLE ... SET SCHEMA: a definition-only move. Replay moves the
    /// table and its indexes and repoints every inbound foreign key, all
    /// deterministically, so no row images are journaled.
    SetTableSchema {
        schema: &'a str,
        name: &'a str,
        new_schema: &'a str,
    },
    /// DROP SCHEMA CASCADE severing an inbound foreign key on a table that
    /// survives: a definition-only removal, replayed by constraint name.
    DropTableFk {
        schema: &'a str,
        table: &'a str,
        fk_name: &'a str,
    },
    /// CREATE MATERIALIZED VIEW: its rows replay as the backing table's own
    /// Upsert records; this records only the defining query and populated state.
    CreateMatview {
        schema: &'a str,
        name: &'a str,
        sql: &'a str,
        path: &'a str,
        dependencies: WalStoredQueryDependencies<'a>,
        populated: bool,
    },
    DropMatview {
        schema: &'a str,
        name: &'a str,
    },
    /// REFRESH / WITH [NO] DATA changing whether the matview is populated.
    SetMatviewPopulated {
        schema: &'a str,
        name: &'a str,
        populated: bool,
    },
    /// CREATE SEQUENCE (or ALTER SEQUENCE — the full parameter set is journaled
    /// absolutely, so an ALTER replays as a redefinition). `data_type` is the
    /// `SeqType` discriminant.
    CreateSequence {
        schema: &'a str,
        name: &'a str,
        data_type: u8,
        increment: i64,
        min_value: i64,
        max_value: i64,
        start_value: i64,
        cache: i64,
        cycle: bool,
        owner: Option<crate::storage::SequenceOwner>,
        generator_for: Option<crate::storage::SequenceOwner>,
    },
    DropSequence {
        schema: &'a str,
        name: &'a str,
    },
    /// CREATE DOMAIN (or ALTER DOMAIN — journaled absolutely, so an ALTER
    /// replays as a redefinition). Carries the whole definition inline, like
    /// [`WalOp::CreateTable`]; the value's `live`/`pending`/`created_at` are
    /// not journaled (replay sets them).
    CreateDomain(crate::storage::DomainDef),
    DropDomain {
        schema: &'a str,
        name: &'a str,
    },
    /// CREATE TYPE ... AS ENUM (or ALTER TYPE ... ADD VALUE — journaled
    /// absolutely, so an ALTER replays as a redefinition). Carries the whole
    /// definition inline, like [`WalOp::CreateDomain`].
    CreateEnum(crate::storage::EnumDef),
    DropEnum {
        schema: &'a str,
        name: &'a str,
    },
    RenameEnum {
        schema: &'a str,
        old_name: &'a str,
        new_name: &'a str,
    },
    CreateRoutine(crate::storage::RoutineDef),
    DropRoutine {
        schema: &'a str,
        name: &'a str,
        argument_type_codes: &'a [u8],
    },
    AlterRoutineIdentity {
        schema: &'a str,
        name: &'a str,
        argument_type_codes: &'a [u8],
        new_schema: &'a str,
        new_name: &'a str,
    },
    AlterDomainIdentity {
        schema: &'a str,
        name: &'a str,
        new_schema: &'a str,
        new_name: &'a str,
    },
    /// A `nextval`/`setval`/`RESTART` advance: the absolute value state, so
    /// replay is idempotent. Advances are non-transactional (they survive
    /// ROLLBACK), matching PostgreSQL's sequence gaps.
    SequenceAdvance {
        schema: &'a str,
        name: &'a str,
        last: i64,
        is_called: bool,
    },
    /// A `COMMENT ON` set or removal. `class` is the [`CommentClass`]
    /// discriminant; `subid` is 0 for a relation/schema or the column number;
    /// `text == None` is a removal. Absolute, so replay is idempotent.
    ///
    /// [`CommentClass`]: crate::storage::CommentClass
    Comment {
        class: u8,
        schema: &'a str,
        name: &'a str,
        subid: u32,
        text: Option<&'a str>,
    },
    Analyze {
        schema: &'a str,
        table: &'a str,
        statistics: WalTableStatistics<'a>,
    },
    /// Absolute role definition, shared by CREATE and ALTER so replay is
    /// idempotent and a manifest/WAL handoff cannot expose a partial option set.
    UpsertRole {
        name: &'a str,
        attributes: RoleAttributes,
    },
    DropRole {
        name: &'a str,
    },
    UpsertRoleMembership {
        role: &'a str,
        member: &'a str,
        grantor: &'a str,
        options: crate::storage::RoleMembershipOptions,
    },
    DropRoleMembership {
        role: &'a str,
        member: &'a str,
    },
    SetObjectOwner {
        class: u8,
        /// Stable identity for overloaded routines; zero for name-unique classes.
        object_oid: i32,
        schema: &'a str,
        name: &'a str,
        owner: &'a str,
    },
    SetObjectAcl {
        class: u8,
        /// Stable identity for overloaded routines; zero for name-unique classes.
        object_oid: i32,
        schema: &'a str,
        name: &'a str,
        grantee: &'a str,
        grantor: &'a str,
        privileges: crate::storage::PrivilegeSet,
        grant_options: crate::storage::PrivilegeSet,
    },
    SetDefaultAcl {
        owner: &'a str,
        /// Empty denotes the global default; otherwise a schema name.
        schema: &'a str,
        class: u8,
        grantee: &'a str,
        defined: bool,
        privileges: crate::storage::PrivilegeSet,
        grant_options: crate::storage::PrivilegeSet,
    },
}

pub struct Wal {
    file: File,
    /// Bytes ready to become durable. Transactional work reaches this buffer
    /// only through [`Self::commit_stage`], so one connection can never flush
    /// another connection's uncommitted records.
    buffer: FixedBuf,
    /// One fixed staging buffer per possible active connection. A stage is
    /// claimed lazily by transaction id and released at commit or rollback.
    stages: Vec<TransactionStage>,
    /// File offset where the next buffered byte lands.
    write_offset: u64,
    capacity: u64,
    last_lsn: u64,
    dirty: bool,
    /// First LSN of the batch currently buffered (for segment upload).
    batch_first_lsn: u64,
    /// Bytes appended since the last upload capture.
    batch_start_offset: u64,
}

struct TransactionStage {
    transaction_id: u32,
    buffer: FixedBuf,
}

#[derive(Debug)]
pub enum WalSetupError {
    Budget(BudgetError),
    Io(&'static str, std::io::Error),
    /// The journal on disk is larger than `wal_bytes` — refusing to
    /// truncate someone's log because a config shrank.
    ShrinkRefused {
        file: u64,
        config: u64,
    },
    Replay(SqlError),
}

impl std::fmt::Display for WalSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "wal: {e}"),
            Self::Io(what, e) => write!(f, "wal: {what}: {e}"),
            Self::ShrinkRefused { file, config } => write!(
                f,
                "wal: journal is {file} bytes but wal_bytes is {config}; refusing to truncate"
            ),
            Self::Replay(e) => write!(f, "wal: replay failed: {}", e.message.as_str()),
        }
    }
}

impl std::error::Error for WalSetupError {}

impl From<BudgetError> for WalSetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

fn append_record(buffer: &mut FixedBuf, lsn: u64, operation: &WalOp) -> Result<(), SqlError> {
    let payload_len = encoded_payload_len(operation);
    let total = HEADER_LEN + payload_len;
    if buffer.capacity() - buffer.len() < total {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "transaction exceeds wal_buffer_bytes ({}); raise it or commit in smaller batches",
            buffer.capacity()
        ));
    }
    let mark = buffer.mark();
    let mut appended = buffer.append(&[0u8; 4]);
    appended &= buffer.append(&(payload_len as u32).to_le_bytes());
    appended &= buffer.append(&lsn.to_le_bytes());
    appended &= buffer.append(&[op_kind(operation), 0, 0, 0, 0, 0, 0, 0]);
    appended &= append_payload(buffer, operation);
    assert!(appended, "record size was checked against buffer capacity");
    assert_eq!(
        buffer.len(),
        mark + total,
        "WAL payload length must match its typed encoding: {operation:?}"
    );

    let filled = buffer.filled_mut();
    let crc = crc32c(&filled[mark + 4..mark + total]);
    filled[mark..mark + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

impl Wal {
    /// Opens (creating and preallocating if needed) `<data_dir>/journal.wal`.
    pub fn open(config: &Config, budget: &mut Budget) -> Result<Self, WalSetupError> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| WalSetupError::Io("create data_dir", e))?;
        let path = format!("{}/journal.wal", config.data_dir);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalSetupError::Io("open journal", e))?;
        let len = file
            .metadata()
            .map_err(|e| WalSetupError::Io("stat journal", e))?
            .len();
        let capacity = config.wal_bytes as u64;
        if len > capacity {
            return Err(WalSetupError::ShrinkRefused {
                file: len,
                config: capacity,
            });
        }
        if len < capacity {
            file.set_len(capacity)
                .map_err(|e| WalSetupError::Io("preallocate journal", e))?;
        }
        let mut stages = Vec::with_capacity(config.max_connections as usize);
        for _ in 0..config.max_connections {
            stages.push(TransactionStage {
                transaction_id: 0,
                buffer: FixedBuf::new(budget, "transaction_wal_stage", config.wal_buffer_bytes)?,
            });
        }
        Ok(Self {
            file,
            buffer: FixedBuf::new(budget, "wal_buffer", config.wal_buffer_bytes)?,
            stages,
            write_offset: 0,
            capacity,
            last_lsn: 0,
            dirty: false,
            batch_first_lsn: 0,
            batch_start_offset: 0,
        })
    }

    pub fn last_lsn(&self) -> u64 {
        self.last_lsn
    }

    pub fn used_bytes(&self) -> u64 {
        self.write_offset + self.buffer.len() as u64
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    /// Replays every valid record from the start of the journal, stopping
    /// at the first invalid or non-monotonic one (the tail). Positions the
    /// write cursor there. Records with `lsn <= floor` are scanned but not
    /// yielded — they are already covered by the checkpoint the caller
    /// loaded (a crash between manifest publication and journal reset
    /// leaves such records behind). Each yielded record is the raw bytes
    /// from the kind byte onward, as [`decode_record`] accepts; the caller
    /// merges them with uploaded-segment records by LSN before applying, so
    /// a journal that restarts mid-history (disk wipe) or ends early (torn
    /// write) cannot reorder or lose committed records. Startup only.
    pub(crate) fn replay(
        &mut self,
        floor: u64,
        mut apply: impl for<'a> FnMut(u64, &'a [u8]) -> Result<(), SqlError>,
    ) -> Result<(), WalSetupError> {
        self.buffer.clear();
        let mut file_offset = 0u64; // next byte to read from the file
        'outer: loop {
            let space = self.buffer.writable();
            if space.is_empty() {
                // A record larger than the buffer can never be written by
                // append(), so this is corruption; stop here.
                break;
            }
            let want = space.len().min((self.capacity - file_offset) as usize);
            if want == 0 {
                break;
            }
            let n = self
                .file
                .read_at(&mut space[..want], file_offset)
                .map_err(|e| WalSetupError::Io("read journal", e))?;
            if n == 0 {
                break;
            }
            self.buffer.advance(n);
            file_offset += n as u64;

            loop {
                let data = self.buffer.readable();
                if data.len() < HEADER_LEN {
                    continue 'outer;
                }
                let stored_crc = u32::from_le_bytes(data[0..4].try_into().unwrap());
                let payload_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
                let lsn = u64::from_le_bytes(data[8..16].try_into().unwrap());
                let kind = data[16];
                if !(KIND_CREATE..=LAST_KIND).contains(&kind)
                    || payload_len > self.buffer.capacity() - HEADER_LEN
                    || lsn <= self.last_lsn
                {
                    break 'outer;
                }
                let total = HEADER_LEN + payload_len;
                if data.len() < total {
                    continue 'outer;
                }
                if crc32c(&data[4..total]) != stored_crc {
                    break 'outer;
                }
                // Validate the framing while the record is contiguous in the
                // buffer, then hand the caller the raw bytes from the kind
                // byte onward (as `decode_record` accepts).
                if decode_op(kind, &data[HEADER_LEN..total]).is_none() {
                    break 'outer;
                }
                if lsn > floor {
                    apply(lsn, &data[16..total]).map_err(WalSetupError::Replay)?;
                }
                self.last_lsn = lsn;
                self.write_offset += total as u64;
                self.buffer.consume(total);
            }
        }
        self.buffer.clear();
        Ok(())
    }

    fn stage_index(&self, transaction_id: u32) -> Option<usize> {
        assert_ne!(
            transaction_id, 0,
            "zero is reserved for an unclaimed WAL stage"
        );
        self.stages
            .iter()
            .position(|stage| stage.transaction_id == transaction_id)
    }

    fn stage_index_or_claim(&mut self, transaction_id: u32) -> Result<usize, SqlError> {
        if let Some(index) = self.stage_index(transaction_id) {
            return Ok(index);
        }
        let Some(index) = self
            .stages
            .iter()
            .position(|stage| stage.transaction_id == 0)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "concurrent WAL staging exceeds max_connections ({})",
                self.stages.len()
            ));
        };
        self.stages[index].transaction_id = transaction_id;
        self.stages[index].buffer.clear();
        Ok(index)
    }

    /// Byte position inside one transaction's private stage. Savepoints use
    /// this to discard only their own tail.
    pub fn stage_mark(&self, transaction_id: u32) -> usize {
        self.stage_index(transaction_id)
            .map_or(0, |index| self.stages[index].buffer.mark())
    }

    /// Current record/byte totals in one transaction's private stage.
    /// EXPLAIN WAL snapshots this before and after execution; publication
    /// still owns the same bytes and does not depend on the telemetry.
    pub(crate) fn stage_stats(&self, transaction_id: u32) -> (u64, u64) {
        let Some(index) = self.stage_index(transaction_id) else {
            return (0, 0);
        };
        let staged = self.stages[index].buffer.readable();
        let mut offset = 0usize;
        let mut records = 0u64;
        while offset < staged.len() {
            debug_assert!(staged.len() - offset >= HEADER_LEN);
            let payload_len =
                u32::from_le_bytes(staged[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let total = HEADER_LEN + payload_len;
            debug_assert!(offset + total <= staged.len());
            offset += total;
            records = records.saturating_add(1);
        }
        (records, staged.len() as u64)
    }

    pub fn truncate_stage(&mut self, transaction_id: u32, mark: usize) {
        let Some(index) = self.stage_index(transaction_id) else {
            debug_assert_eq!(mark, 0);
            return;
        };
        self.stages[index].buffer.truncate_to(mark);
        if self.stages[index].buffer.is_empty() {
            self.stages[index].transaction_id = 0;
        }
    }

    pub fn discard_stage(&mut self, transaction_id: u32) {
        if let Some(index) = self.stage_index(transaction_id) {
            self.stages[index].buffer.clear();
            self.stages[index].transaction_id = 0;
        }
    }

    /// Encodes one record into a transaction-private buffer. The LSN is
    /// provisional: commit rewrites staged records into commit order before
    /// their CRCs are finalized in the durable batch.
    pub(crate) fn stage(
        &mut self,
        transaction_id: u32,
        provisional_lsn: u64,
        operation: &WalOp,
    ) -> Result<(), SqlError> {
        let index = self.stage_index_or_claim(transaction_id)?;
        append_record(&mut self.stages[index].buffer, provisional_lsn, operation)
    }

    /// Publishes exactly one transaction's staged records into the durable
    /// batch, assigning monotonically increasing commit-order LSNs. Returns
    /// the last assigned LSN, or `lsn_floor` for a transaction with no WAL.
    pub fn commit_stage(&mut self, transaction_id: u32, lsn_floor: u64) -> Result<u64, SqlError> {
        let Some(index) = self.stage_index(transaction_id) else {
            return Ok(lsn_floor);
        };
        let staged_len = self.stages[index].buffer.len();
        if staged_len == 0 {
            self.stages[index].buffer.clear();
            self.stages[index].transaction_id = 0;
            return Ok(lsn_floor);
        }
        let staged = self.stages[index].buffer.readable();
        let mut record_count = 0_u64;
        let mut staged_offset = 0;
        while staged_offset < staged_len {
            assert!(
                staged_len - staged_offset >= HEADER_LEN,
                "staged WAL contains a complete record header"
            );
            let payload_len = u32::from_le_bytes(
                staged[staged_offset + 4..staged_offset + 8]
                    .try_into()
                    .expect("record header is complete"),
            ) as usize;
            let total = HEADER_LEN + payload_len;
            assert!(
                staged_offset + total <= staged_len,
                "staged WAL contains a complete encoded record"
            );
            staged_offset += total;
            record_count += 1;
        }
        let final_lsn = lsn_floor
            .checked_add(record_count)
            .and_then(|lsn| lsn.checked_add(1))
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted"))?;
        let commit_bytes = HEADER_LEN;
        if self.buffer.capacity() - self.buffer.len() < staged_len + commit_bytes {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction exceeds wal_buffer_bytes ({}); raise it or commit in smaller batches",
                self.buffer.capacity()
            ));
        }
        if self
            .write_offset
            .checked_add(self.buffer.len() as u64)
            .and_then(|used| used.checked_add((staged_len + commit_bytes) as u64))
            .is_none_or(|used| used > self.capacity)
        {
            return Err(sql_err!(
                JOURNAL_FULL,
                "WAL journal is full; run CHECKPOINT (or raise wal_bytes)"
            ));
        }
        assert!(
            self.buffer.is_empty(),
            "a prior committed WAL batch must be flushed before another commit"
        );
        assert!(
            lsn_floor >= self.last_lsn,
            "commit LSN floor must not regress"
        );

        let durable_mark = self.buffer.mark();
        let appended = self.buffer.append(self.stages[index].buffer.readable());
        assert!(
            appended,
            "stage size was checked against durable buffer capacity"
        );

        let mut next_lsn = lsn_floor;
        let mut offset = durable_mark;
        while offset < durable_mark + staged_len {
            let payload_len = u32::from_le_bytes(
                self.buffer.filled_mut()[offset + 4..offset + 8]
                    .try_into()
                    .expect("record header is complete"),
            ) as usize;
            let total = HEADER_LEN + payload_len;
            assert!(
                offset + total <= durable_mark + staged_len,
                "staged WAL contains a complete encoded record"
            );
            next_lsn += 1;
            let filled = self.buffer.filled_mut();
            filled[offset + 8..offset + 16].copy_from_slice(&next_lsn.to_le_bytes());
            let crc = crc32c(&filled[offset + 4..offset + total]);
            filled[offset..offset + 4].copy_from_slice(&crc.to_le_bytes());
            offset += total;
        }
        assert_eq!(next_lsn + 1, final_lsn);
        // The marker is appended only after every operation has been assigned
        // its final LSN and checksum, so a torn tail can never look committed.
        append_record(
            &mut self.buffer,
            final_lsn,
            &WalOp::Commit { transaction_id },
        )?;
        if staged_len > 0 && self.batch_first_lsn == 0 {
            self.batch_first_lsn = lsn_floor + 1;
            self.batch_start_offset = self.write_offset + durable_mark as u64;
        }
        self.last_lsn = final_lsn;
        self.dirty = true;
        self.stages[index].buffer.clear();
        self.stages[index].transaction_id = 0;
        Ok(final_lsn)
    }

    /// Appends one already-committed record. This exists for recovery-format
    /// tests; transactional SQL must use [`Self::stage`] and
    /// [`Self::commit_stage`].
    #[cfg(test)]
    pub(crate) fn append_committed(&mut self, lsn: u64, operation: &WalOp) -> Result<(), SqlError> {
        if self.write_offset
            + self.buffer.len() as u64
            + (HEADER_LEN + encoded_payload_len(operation)) as u64
            > self.capacity
        {
            return Err(sql_err!(
                JOURNAL_FULL,
                "WAL journal is full; run CHECKPOINT (or raise wal_bytes)"
            ));
        }
        assert!(lsn > self.last_lsn, "LSNs must be strictly increasing");
        if self.batch_first_lsn == 0 {
            self.batch_first_lsn = lsn;
            self.batch_start_offset = self.write_offset + self.buffer.len() as u64;
        }
        append_record(&mut self.buffer, lsn, operation)?;
        self.last_lsn = lsn;
        self.dirty = true;
        Ok(())
    }

    /// The committed range awaiting immutable publication.
    pub fn last_committed_batch(&self) -> Option<CommittedBatch> {
        if self.batch_first_lsn == 0 {
            return None;
        }
        Some(CommittedBatch {
            first_lsn: self.batch_first_lsn,
            start: self.batch_start_offset,
            end: self.write_offset,
        })
    }

    /// Bytes of committed-but-not-yet-uploaded WAL accumulated in the current
    /// upload batch (the marker is cleared once its bytes are uploaded). Zero
    /// when nothing awaits upload.
    pub fn pending_batch_bytes(&self) -> u64 {
        if self.batch_first_lsn == 0 {
            return 0;
        }
        self.write_offset.saturating_sub(self.batch_start_offset)
    }

    /// Reads `len` bytes at file `offset` into `out` (for segment upload).
    pub fn read_range(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(out, offset)
    }

    /// Reads one complete transaction strictly newer than `floor` without
    /// changing recovery state. `scratch` is caller-owned fixed storage: the
    /// callback receives the complete framed transaction only after its Commit
    /// record has passed CRC and framing validation.
    pub fn next_committed_after(
        &self,
        floor: u64,
        scratch: &mut FixedBuf,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<Option<u64>, SqlError> {
        scratch.clear();
        let mut offset = 0u64;
        let mut previous_lsn = 0u64;
        while offset < self.write_offset {
            let mut header = [0u8; HEADER_LEN];
            self.file
                .read_exact_at(&mut header, offset)
                .map_err(|_| sql_err!(sqlstate::IO_ERROR, "cannot read durable WAL record"))?;
            let payload_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let lsn = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let total = HEADER_LEN.checked_add(payload_len).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "corrupt durable WAL record length"
                )
            })?;
            if total > self.buffer.capacity()
                || offset
                    .checked_add(total as u64)
                    .is_none_or(|end| end > self.write_offset)
                || lsn <= previous_lsn
            {
                return Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "corrupt durable WAL record"
                ));
            }
            previous_lsn = lsn;
            if lsn <= floor {
                offset += total as u64;
                continue;
            }
            if scratch.capacity() - scratch.len() < total {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "one committed WAL transaction exceeds replication buffer"
                ));
            }
            let mark = scratch.mark();
            assert!(scratch.append(&header));
            let payload = &mut scratch.writable()[..payload_len];
            self.file
                .read_exact_at(payload, offset + HEADER_LEN as u64)
                .map_err(|_| sql_err!(sqlstate::IO_ERROR, "cannot read durable WAL payload"))?;
            scratch.advance(payload_len);
            if crc32c(&scratch.filled_mut()[mark + 4..mark + total])
                != u32::from_le_bytes(header[..4].try_into().unwrap())
                || decode_op(header[16], &scratch.readable()[mark + 24..mark + total]).is_none()
            {
                return Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "corrupt durable WAL record"
                ));
            }
            offset += total as u64;
            if header[16] != KIND_COMMIT {
                continue;
            }
            apply(lsn, scratch.readable())?;
            scratch.clear();
            return Ok(Some(lsn));
        }
        scratch.clear();
        Ok(None)
    }

    /// Makes everything appended so far durable. Aborts the process on I/O
    /// failure: the in-memory state is already ahead of the journal, and
    /// restart-with-replay is the only consistent way forward.
    pub fn commit(&mut self) {
        if !self.dirty {
            return;
        }
        self.flush_buffer();
        if !fsync_durable(self.file.as_raw_fd()) {
            die("pos3ql: WAL fsync failed; aborting for consistency\n");
        }
        self.dirty = false;
    }

    /// After a checkpoint made everything up to the current LSN durable in
    /// object storage, the journal restarts from the beginning. Stale bytes
    /// beyond the new tail are defused by the monotonic-LSN replay rule.
    pub fn reset_after_checkpoint(&mut self) {
        self.buffer.clear();
        self.write_offset = 0;
        self.dirty = false;
        self.batch_first_lsn = 0;
        self.batch_start_offset = 0;
    }

    /// Clears the current batch marker after its bytes were captured for
    /// upload, so the next transaction starts a fresh segment.
    pub fn clear_batch_marker(&mut self) {
        self.batch_first_lsn = 0;
    }

    fn flush_buffer(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let data = self.buffer.readable();
        if self.file.write_all_at(data, self.write_offset).is_err() {
            die("pos3ql: WAL write failed; aborting for consistency\n");
        }
        self.write_offset += data.len() as u64;
        let n = data.len();
        self.buffer.consume(n);
    }
}

/// Durable sync: F_FULLFSYNC on macOS (plain fsync does not reach the
/// platter there), fdatasync on Linux, fsync elsewhere.
fn fsync_durable(fd: std::os::fd::RawFd) -> bool {
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC, 0) };
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::fdatasync(fd) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let rc = unsafe { libc::fsync(fd) };
    rc == 0
}

/// Post-freeze fatal path: raw write + abort (no allocating panic).
fn die(msg: &str) -> ! {
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
    }
    std::process::abort();
}

fn op_kind(operation: &WalOp) -> u8 {
    match operation {
        WalOp::CreateTable(_) => KIND_CREATE,
        WalOp::DropTable { .. } => KIND_DROP,
        WalOp::Upsert { .. } => KIND_UPSERT,
        WalOp::Delete { .. } => KIND_DELETE,
        WalOp::Truncate { .. } => KIND_TRUNCATE,
        WalOp::CreateView { .. } => KIND_CREATE_VIEW,
        WalOp::DropView { .. } => KIND_DROP_VIEW,
        WalOp::CreatePublication { .. } => KIND_CREATE_PUBLICATION,
        WalOp::DropPublication { .. } => KIND_DROP_PUBLICATION,
        WalOp::AlterPublication { .. } => KIND_ALTER_PUBLICATION,
        WalOp::SetPublicationOwner { .. } => KIND_SET_PUBLICATION_OWNER,
        WalOp::RenamePublication { .. } => KIND_RENAME_PUBLICATION,
        WalOp::Commit { .. } => KIND_COMMIT,
        WalOp::CreateReplicationSlot { .. } => KIND_CREATE_REPLICATION_SLOT,
        WalOp::DropReplicationSlot { .. } => KIND_DROP_REPLICATION_SLOT,
        WalOp::AdvanceReplicationSlot { .. } => KIND_ADVANCE_REPLICATION_SLOT,
        WalOp::CreateIndex { .. } => KIND_CREATE_INDEX,
        WalOp::DropIndex { .. } => KIND_DROP_INDEX,
        WalOp::RenameIndex { .. } => KIND_RENAME_INDEX,
        WalOp::SequenceSet { .. } => KIND_SEQUENCE_SET,
        WalOp::CreateSchema(_) => KIND_CREATE_SCHEMA,
        WalOp::DropSchema(_) => KIND_DROP_SCHEMA,
        WalOp::SetTableSchema { .. } => KIND_SET_TABLE_SCHEMA,
        WalOp::DropTableFk { .. } => KIND_DROP_FK,
        WalOp::CreateMatview { .. } => KIND_CREATE_MATVIEW,
        WalOp::DropMatview { .. } => KIND_DROP_MATVIEW,
        WalOp::SetMatviewPopulated { .. } => KIND_SET_MATVIEW_POPULATED,
        WalOp::CreateSequence { .. } => KIND_CREATE_SEQUENCE,
        WalOp::DropSequence { .. } => KIND_DROP_SEQUENCE,
        WalOp::SequenceAdvance { .. } => KIND_SEQUENCE_ADVANCE,
        WalOp::Comment { .. } => KIND_COMMENT,
        WalOp::CreateDomain(_) => KIND_CREATE_DOMAIN,
        WalOp::DropDomain { .. } => KIND_DROP_DOMAIN,
        WalOp::CreateEnum(_) => KIND_CREATE_ENUM,
        WalOp::DropEnum { .. } => KIND_DROP_ENUM,
        WalOp::RenameEnum { .. } => KIND_RENAME_ENUM,
        WalOp::CreateRoutine(_) => KIND_CREATE_ROUTINE,
        WalOp::DropRoutine { .. } => KIND_DROP_ROUTINE,
        WalOp::AlterRoutineIdentity { .. } => KIND_ALTER_ROUTINE_IDENTITY,
        WalOp::AlterDomainIdentity { .. } => KIND_ALTER_DOMAIN_IDENTITY,
        WalOp::Analyze { .. } => KIND_ANALYZE,
        WalOp::UpsertRole { .. } => KIND_UPSERT_ROLE,
        WalOp::DropRole { .. } => KIND_DROP_ROLE,
        WalOp::UpsertRoleMembership { .. } => KIND_UPSERT_ROLE_MEMBERSHIP,
        WalOp::DropRoleMembership { .. } => KIND_DROP_ROLE_MEMBERSHIP,
        WalOp::SetObjectOwner { .. } => KIND_SET_OBJECT_OWNER,
        WalOp::SetObjectAcl { .. } => KIND_SET_OBJECT_ACL,
        WalOp::BeginTableRewrite { .. } => KIND_REWRITE_TABLE,
        WalOp::SetDefaultAcl { .. } => KIND_SET_DEFAULT_ACL,
    }
}

fn encoded_payload_len(operation: &WalOp) -> usize {
    match operation {
        WalOp::CreateTable(def) => {
            let mut n = 1 + def.name.as_str().len() + 2;
            for c in def.columns() {
                let default_value = c.default.constant().copied();
                n += 1 + c.name.as_str().len() + 2 + 4 + encoded_default_len(&default_value);
                // Non-constant DEFAULT text: 2-byte length prefix + bytes.
                n += 2 + c
                    .default
                    .expression()
                    .map(|e| e.as_str().len())
                    .unwrap_or(0);
                // auto_increment_step (i64).
                n += 8;
                // User-defined column: name, then a format marker and schema.
                if let Some(identity) = c.user_type {
                    n += 1 + identity.name.as_str().len();
                    n += 2 + identity.schema.as_str().len();
                }
            }
            // uniques
            n += 1;
            for uk in def.uniques() {
                n += 1 + uk.name.as_str().len() + 2 + uk.n_cols * 2;
            }
            // checks
            n += 1;
            for check in def.checks() {
                n += 1 + check.name.as_str().len() + 2 + check.expression.as_str().len();
            }
            // foreign keys
            n += 1;
            for fk in def.fkeys() {
                n += 1
                    + fk.name.as_str().len()
                    + 1
                    + fk.n_cols * 2
                    + 1
                    + fk.parent.as_str().len()
                    + 1
                    + fk.n_parent_cols * 2
                    + 2;
            }
            // Trailing schema block (absent in journals from before schemas
            // existed; replay defaults those to public).
            n += 1 + def.schema.as_str().len();
            for fk in def.fkeys() {
                n += 1 + fk.parent_schema.as_str().len();
            }
            n
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            column_mapping,
        } => 1 + previous_schema.len() + 1 + previous_name.len() + column_mapping.len() * 2,
        WalOp::DropTable { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::Upsert {
            schema,
            table,
            row,
            old_row,
            ..
        } => {
            1 + table.len()
                + 8
                + 4
                + row.len()
                + 1
                + schema.len()
                + 2
                + old_row.map_or(0, |old| 4 + old.len())
                + 4
        }
        WalOp::Delete {
            schema,
            table,
            old_row,
            ..
        } => {
            1 + table.len() + 8 + 1 + schema.len() + 1 + old_row.map_or(0, |old| 4 + old.len()) + 4
        }
        WalOp::Truncate { tables, .. } => 1 + tables.len() + 1 + 4,
        WalOp::CreateView {
            schema,
            name,
            sql,
            path,
            dependencies,
        } => {
            1 + name.len()
                + 2
                + sql.len()
                + 1
                + schema.len()
                + 2
                + path.len()
                + dependencies.encoded_len()
        }
        WalOp::DropView { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::CreatePublication {
            name,
            table_count,
            schema_count,
            ..
        } => 1 + name.len() + 2 + 1 + 1 + 1 + table_count * 2 + schema_count,
        WalOp::DropPublication { name } => 1 + name.len(),
        WalOp::AlterPublication {
            name,
            table_count,
            schema_count,
            ..
        } => 1 + name.len() + 1 + 1 + 1 + table_count * 2 + schema_count,
        WalOp::SetPublicationOwner { name, .. } => 1 + name.len() + 2,
        WalOp::RenamePublication { name, new_name } => 1 + name.len() + 1 + new_name.len(),
        WalOp::Commit { .. } => 4,
        WalOp::CreateReplicationSlot { name, .. } => 1 + name.len() + 8,
        WalOp::DropReplicationSlot { name } => 1 + name.len(),
        WalOp::AdvanceReplicationSlot { name, .. } => 1 + name.len() + 8,
        WalOp::CreateIndex {
            schema,
            name,
            table,
            n_cols,
            predicate,
            n_include_cols,
            expressions,
            ..
        } => {
            1 + name.len()
                + 1
                + table.len()
                + 1
                + 1
                + n_cols * 2
                + 1
                + schema.len()
                + 1
                + n_cols
                + 2
                + predicate.map_or(0, |text| 2 + text.len())
                + 2
                + n_include_cols * 2
                + 2
                + 2
                + expressions
                    .iter()
                    .take(*n_cols)
                    .enumerate()
                    .filter(|(_, value)| value.is_some())
                    .map(|(_, value)| 2 + value.unwrap().len())
                    .sum::<usize>()
        }
        WalOp::DropIndex { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::RenameIndex {
            schema,
            name,
            new_name,
        } => 1 + schema.len() + 1 + name.len() + 1 + new_name.len(),
        WalOp::SequenceSet { schema, table, .. } => 1 + table.len() + 2 + 8 + 1 + schema.len(),
        WalOp::CreateSchema(name) | WalOp::DropSchema(name) => 1 + name.len(),
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        } => 1 + schema.len() + 1 + name.len() + 1 + new_schema.len(),
        WalOp::DropTableFk {
            schema,
            table,
            fk_name,
        } => 1 + schema.len() + 1 + table.len() + 1 + fk_name.len(),
        WalOp::CreateMatview {
            schema,
            name,
            sql,
            path,
            dependencies,
            ..
        } => {
            1 + name.len()
                + 2
                + sql.len()
                + 1
                + schema.len()
                + 2
                + path.len()
                + 1
                + dependencies.encoded_len()
        }
        WalOp::DropMatview { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::SetMatviewPopulated { schema, name, .. } => 1 + name.len() + 1 + schema.len() + 1,
        WalOp::CreateSequence {
            schema,
            name,
            owner,
            generator_for,
            ..
        } => {
            // name, schema, then 1 (data_type) + 5×8 (increment/min/max/start/
            // cache) + 1 (cycle).
            1 + name.len()
                + 1
                + schema.len()
                + 1
                + 5 * 8
                + 1
                + 1
                + owner.map_or(0, |owner| {
                    1 + owner.table_schema.as_str().len()
                        + 1
                        + owner.table.as_str().len()
                        + 1
                        + owner.column.as_str().len()
                })
                + 1
                + generator_for.map_or(0, |generator| {
                    1 + generator.table_schema.as_str().len()
                        + 1
                        + generator.table.as_str().len()
                        + 1
                        + generator.column.as_str().len()
                })
        }
        WalOp::DropSequence { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::CreateDomain(def) => {
            let de = def.default_expr.map(|e| e.as_str().len()).unwrap_or(0);
            let parent = def.base_domain.map(|d| d.name.as_str().len()).unwrap_or(0);
            let parent_schema = def
                .base_domain
                .map(|d| d.schema.as_str().len())
                .unwrap_or(0);
            let mut n = 1
                + def.name.as_str().len()
                + 1
                + def.schema.as_str().len()
                + 1
                + 1
                + parent
                + 1
                + parent_schema
                + 1
                + 4
                + 1
                + 2
                + de
                + 1;
            for c in def.checks() {
                n += 1 + c.name.as_str().len() + 2 + c.expression.as_str().len();
            }
            n
        }
        WalOp::DropDomain { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::CreateEnum(def) => {
            let mut n = 1 + def.name.as_str().len() + 1 + def.schema.as_str().len() + 1;
            for m in def.members() {
                n += 1 + m.label.as_str().len() + 8;
            }
            n
        }
        WalOp::DropEnum { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::RenameEnum {
            schema,
            old_name,
            new_name,
        } => 1 + old_name.len() + 1 + schema.len() + 1 + new_name.len(),
        WalOp::CreateRoutine(def) => {
            8 + 2
                + 1
                + def.name.as_str().len()
                + 1
                + def.schema.as_str().len()
                + 1
                + def
                    .arguments()
                    .iter()
                    .map(|argument| 1 + argument.name.as_str().len() + 1)
                    .sum::<usize>()
                + 1
                + 2
                + def.body.as_str().len()
                + usize::from(!matches!(
                    def.kind,
                    crate::storage::RoutineKind::Function { .. }
                ))
                + match def.kind {
                    crate::storage::RoutineKind::TableFunction => {
                        1 + def.result_columns[..def.result_column_count]
                            .iter()
                            .map(|column| 1 + column.name.as_str().len() + 1)
                            .sum::<usize>()
                    }
                    crate::storage::RoutineKind::Function { .. }
                    | crate::storage::RoutineKind::SetFunction { .. }
                    | crate::storage::RoutineKind::Procedure => 0,
                }
        }
        WalOp::DropRoutine {
            schema,
            name,
            argument_type_codes,
        } => 1 + name.len() + 1 + schema.len() + 1 + argument_type_codes.len(),
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_type_codes,
            new_schema,
            new_name,
        } => {
            1 + name.len()
                + 1
                + schema.len()
                + 1
                + argument_type_codes.len()
                + 1
                + new_schema.len()
                + 1
                + new_name.len()
        }
        WalOp::AlterDomainIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => 1 + name.len() + 1 + schema.len() + 1 + new_schema.len() + 1 + new_name.len(),
        WalOp::SequenceAdvance { schema, name, .. } => 1 + name.len() + 1 + schema.len() + 8 + 1,
        WalOp::Comment {
            schema, name, text, ..
        } => 1 + name.len() + 1 + schema.len() + 1 + 4 + 1 + text.map_or(0, |t| 2 + t.len()),
        WalOp::Analyze {
            schema,
            table,
            statistics,
        } => 1 + table.len() + 1 + schema.len() + statistics.encoded_len(),
        WalOp::UpsertRole { name, attributes } => {
            1 + name.len() + 2 + 4 + 16 + 32 + 32 + 4 + 1 + attributes.valid_until.as_str().len()
        }
        WalOp::DropRole { name } => 1 + name.len(),
        WalOp::UpsertRoleMembership {
            role,
            member,
            grantor,
            ..
        } => 1 + role.len() + 1 + member.len() + 1 + grantor.len() + 1,
        WalOp::DropRoleMembership { role, member } => 1 + role.len() + 1 + member.len(),
        WalOp::SetObjectOwner {
            class,
            schema,
            name,
            owner,
            ..
        } => {
            1 + usize::from(*class == crate::storage::AccessClass::Routine as u8) * 4
                + 1
                + schema.len()
                + 1
                + name.len()
                + 1
                + owner.len()
        }
        WalOp::SetObjectAcl {
            class,
            schema,
            name,
            grantee,
            grantor,
            ..
        } => {
            1 + usize::from(*class == crate::storage::AccessClass::Routine as u8) * 4
                + 1
                + schema.len()
                + 1
                + name.len()
                + 1
                + grantee.len()
                + 1
                + grantor.len()
                + 4
        }
        WalOp::SetDefaultAcl {
            owner,
            schema,
            grantee,
            ..
        } => 1 + owner.len() + 1 + schema.len() + 1 + 1 + grantee.len() + 1 + 4,
    }
}

/// Bytes this operation occupies in the journal, including its fixed record
/// header. EXPLAIN uses the production codec's sizing rule so WAL telemetry
/// cannot drift from the bytes commit will write.
pub(crate) fn encoded_record_len(operation: &WalOp) -> usize {
    HEADER_LEN + encoded_payload_len(operation)
}

fn append_payload(buffer: &mut FixedBuf, operation: &WalOp) -> bool {
    let name_bytes = |buffer: &mut FixedBuf, s: &str| -> bool {
        buffer.append(&[s.len() as u8]) && buffer.append(s.as_bytes())
    };
    match operation {
        WalOp::CreateTable(def) => {
            let mut ok = name_bytes(buffer, def.name.as_str());
            ok &= buffer.append(&(def.n_columns as u16).to_le_bytes());
            for c in def.columns() {
                ok &= name_bytes(buffer, c.name.as_str());
                // Bit 7 (the last free per-column flag bit) marks a domain-typed
                // column, whose domain name is appended after the fixed fields.
                let flags = u8::from(c.not_null)
                    | (u8::from(c.unique) << 1)
                    | (u8::from(c.primary) << 2)
                    | (u8::from(c.auto_increment) << 3)
                    | (u8::from(c.default.is_generated()) << 4)
                    | (u8::from(c.is_identity) << 5)
                    | (u8::from(c.identity_always) << 6)
                    | (u8::from(c.user_type.is_some()) << 7);
                ok &= buffer.append(&[c.ctype.code(), flags]);
                ok &= buffer.append(&c.type_mod.to_le_bytes());
                let default_value = c.default.constant().copied();
                ok &= append_default(buffer, &default_value);
                let de = c.default.expression().map_or("", |e| e.as_str());
                ok &= buffer.append(&(de.len() as u16).to_le_bytes());
                ok &= buffer.append(de.as_bytes());
                ok &= buffer.append(&c.auto_increment_step.to_le_bytes());
                if let Some(identity) = c.user_type {
                    ok &= name_bytes(buffer, identity.name.as_str());
                    ok &= buffer.append(&[u8::MAX]);
                    ok &= name_bytes(buffer, identity.schema.as_str());
                }
            }
            // Multi-column UNIQUE/PRIMARY KEY constraints.
            ok &= buffer.append(&[def.n_uniques as u8]);
            for uk in def.uniques() {
                ok &= name_bytes(buffer, uk.name.as_str());
                ok &= buffer.append(&[u8::from(uk.is_primary), uk.n_cols as u8]);
                for &c in uk.columns() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
            }
            // CHECK constraints.
            ok &= buffer.append(&[def.n_checks as u8]);
            for check in def.checks() {
                ok &= name_bytes(buffer, check.name.as_str());
                let e = check.expression.as_str();
                ok &= buffer.append(&(e.len() as u16).to_le_bytes());
                ok &= buffer.append(e.as_bytes());
            }
            // FOREIGN KEY constraints.
            ok &= buffer.append(&[def.n_fkeys as u8]);
            for fk in def.fkeys() {
                ok &= name_bytes(buffer, fk.name.as_str());
                ok &= buffer.append(&[fk.n_cols as u8]);
                for &c in fk.columns() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
                ok &= name_bytes(buffer, fk.parent.as_str());
                ok &= buffer.append(&[fk.n_parent_cols as u8]);
                for &c in fk.parent_cols() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
                ok &= buffer.append(&[fk.on_delete.code(), fk.on_update.code()]);
            }
            ok &= name_bytes(buffer, def.schema.as_str());
            for fk in def.fkeys() {
                ok &= name_bytes(buffer, fk.parent_schema.as_str());
            }
            ok
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            column_mapping,
        } => {
            let mut ok = name_bytes(buffer, previous_schema) && name_bytes(buffer, previous_name);
            for column in column_mapping {
                ok &= buffer.append(&column.to_le_bytes());
            }
            ok
        }
        WalOp::DropTable { schema, name } => name_bytes(buffer, name) && name_bytes(buffer, schema),
        WalOp::Upsert {
            schema,
            table,
            rowid,
            row,
            is_update,
            old_row,
            command_id,
        } => {
            name_bytes(buffer, table)
                && buffer.append(&rowid.to_le_bytes())
                && buffer.append(&(row.len() as u32).to_le_bytes())
                && buffer.append(row)
                && name_bytes(buffer, schema)
                && buffer.append(&[u8::from(*is_update)])
                && buffer.append(&[u8::from(old_row.is_some())])
                && old_row.is_none_or(|old| {
                    buffer.append(&(old.len() as u32).to_le_bytes()) && buffer.append(old)
                })
                && buffer.append(&command_id.to_le_bytes())
        }
        WalOp::Delete {
            schema,
            table,
            rowid,
            old_row,
            command_id,
        } => {
            name_bytes(buffer, table)
                && buffer.append(&rowid.to_le_bytes())
                && name_bytes(buffer, schema)
                && buffer.append(&[u8::from(old_row.is_some())])
                && old_row.is_none_or(|old| {
                    buffer.append(&(old.len() as u32).to_le_bytes()) && buffer.append(old)
                })
                && buffer.append(&command_id.to_le_bytes())
        }
        WalOp::Truncate {
            tables,
            table_count,
            cascade,
            restart_identity,
            command_id,
        } => {
            *table_count <= u8::MAX as usize
                && buffer.append(&[*table_count as u8])
                && buffer.append(tables)
                && buffer.append(&[u8::from(*cascade) | (u8::from(*restart_identity) << 1)])
                && buffer.append(&command_id.to_le_bytes())
        }
        WalOp::CreateView {
            schema,
            name,
            sql,
            path,
            dependencies,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&(sql.len() as u16).to_le_bytes())
                && buffer.append(sql.as_bytes())
                && name_bytes(buffer, schema)
                && buffer.append(&(path.len() as u16).to_le_bytes())
                && buffer.append(path.as_bytes())
                && dependencies.append(buffer)
        }
        WalOp::DropView { schema, name } => name_bytes(buffer, name) && name_bytes(buffer, schema),
        WalOp::CreatePublication {
            name,
            owner,
            all_tables,
            tables,
            table_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            schemas,
            schema_count,
        } => {
            let flags = u8::from(*all_tables)
                | (u8::from(*publish_insert) << 1)
                | (u8::from(*publish_update) << 2)
                | (u8::from(*publish_delete) << 3)
                | (u8::from(*publish_truncate) << 4);
            let mut ok = name_bytes(buffer, name)
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&[flags, *table_count as u8, *schema_count as u8]);
            for table in &tables[..*table_count] {
                ok = ok && buffer.append(&table.to_le_bytes());
            }
            ok = ok && buffer.append(&schemas[..*schema_count]);
            ok
        }
        WalOp::DropPublication { name } => name_bytes(buffer, name),
        WalOp::AlterPublication {
            name,
            all_tables,
            tables,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
        } => {
            let flags = u8::from(*all_tables)
                | (u8::from(*publish_insert) << 1)
                | (u8::from(*publish_update) << 2)
                | (u8::from(*publish_delete) << 3)
                | (u8::from(*publish_truncate) << 4);
            let mut ok = name_bytes(buffer, name)
                && buffer.append(&[flags, *table_count as u8, *schema_count as u8]);
            for table in &tables[..*table_count] {
                ok = ok && buffer.append(&table.to_le_bytes());
            }
            ok = ok && buffer.append(&schemas[..*schema_count]);
            ok
        }
        WalOp::SetPublicationOwner { name, owner } => {
            name_bytes(buffer, name) && buffer.append(&owner.to_le_bytes())
        }
        WalOp::RenamePublication { name, new_name } => {
            name_bytes(buffer, name) && name_bytes(buffer, new_name)
        }
        WalOp::Commit { transaction_id } => buffer.append(&transaction_id.to_le_bytes()),
        WalOp::CreateReplicationSlot { name, restart_lsn } => {
            name_bytes(buffer, name) && buffer.append(&restart_lsn.to_le_bytes())
        }
        WalOp::DropReplicationSlot { name } => name_bytes(buffer, name),
        WalOp::AdvanceReplicationSlot {
            name,
            confirmed_flush_lsn,
        } => name_bytes(buffer, name) && buffer.append(&confirmed_flush_lsn.to_le_bytes()),
        WalOp::CreateIndex {
            schema,
            name,
            table,
            columns,
            expressions,
            include_columns,
            descending,
            nulls_first,
            n_cols,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            unique,
        } => {
            let mut ok = name_bytes(buffer, name)
                && name_bytes(buffer, table)
                && buffer.append(&[u8::from(*unique), *n_cols as u8]);
            for c in &columns[..*n_cols] {
                ok &= buffer.append(&c.to_le_bytes());
            }
            ok &= name_bytes(buffer, schema);
            ok &= buffer.append(&[0xa1]);
            for i in 0..*n_cols {
                ok &= buffer.append(&[u8::from(descending[i]) | (u8::from(nulls_first[i]) << 1)]);
            }
            ok &= buffer.append(&[0xa2]);
            ok &= match predicate {
                Some(text) => {
                    text.len() <= u16::MAX as usize
                        && buffer.append(&[1])
                        && buffer.append(&(text.len() as u16).to_le_bytes())
                        && buffer.append(text.as_bytes())
                }
                None => buffer.append(&[0]),
            };
            ok &= buffer.append(&[0xa3, *n_include_cols as u8]);
            for column in &include_columns[..*n_include_cols] {
                ok &= buffer.append(&column.to_le_bytes());
            }
            ok &= buffer.append(&[0xa4, u8::from(*nulls_not_distinct)]);
            let expression_mask = expressions[..*n_cols]
                .iter()
                .enumerate()
                .fold(0u8, |mask, (index, expression)| {
                    mask | (u8::from(expression.is_some()) << index)
                });
            ok &= buffer.append(&[0xa5, expression_mask]);
            for expression in expressions[..*n_cols].iter().flatten() {
                ok &= expression.len() <= u16::MAX as usize
                    && buffer.append(&(expression.len() as u16).to_le_bytes())
                    && buffer.append(expression.as_bytes());
            }
            ok
        }
        WalOp::DropIndex { schema, name } => name_bytes(buffer, name) && name_bytes(buffer, schema),
        WalOp::RenameIndex {
            schema,
            name,
            new_name,
        } => name_bytes(buffer, schema) && name_bytes(buffer, name) && name_bytes(buffer, new_name),
        WalOp::SequenceSet {
            schema,
            table,
            column,
            last,
        } => {
            let mut ok = name_bytes(buffer, table);
            ok &= buffer.append(&column.to_le_bytes());
            ok &= buffer.append(&last.to_le_bytes());
            ok &= name_bytes(buffer, schema);
            ok
        }
        WalOp::CreateSchema(name) | WalOp::DropSchema(name) => name_bytes(buffer, name),
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        } => {
            name_bytes(buffer, schema) && name_bytes(buffer, name) && name_bytes(buffer, new_schema)
        }
        WalOp::DropTableFk {
            schema,
            table,
            fk_name,
        } => name_bytes(buffer, schema) && name_bytes(buffer, table) && name_bytes(buffer, fk_name),
        WalOp::CreateMatview {
            schema,
            name,
            sql,
            path,
            dependencies,
            populated,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&(sql.len() as u16).to_le_bytes())
                && buffer.append(sql.as_bytes())
                && name_bytes(buffer, schema)
                && buffer.append(&(path.len() as u16).to_le_bytes())
                && buffer.append(path.as_bytes())
                && buffer.append(&[u8::from(*populated)])
                && dependencies.append(buffer)
        }
        WalOp::DropMatview { schema, name } => {
            name_bytes(buffer, name) && name_bytes(buffer, schema)
        }
        WalOp::SetMatviewPopulated {
            schema,
            name,
            populated,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(&[u8::from(*populated)])
        }
        WalOp::CreateSequence {
            schema,
            name,
            data_type,
            increment,
            min_value,
            max_value,
            start_value,
            cache,
            cycle,
            owner,
            generator_for,
        } => {
            let mut ok = name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(&[*data_type])
                && buffer.append(&increment.to_le_bytes())
                && buffer.append(&min_value.to_le_bytes())
                && buffer.append(&max_value.to_le_bytes())
                && buffer.append(&start_value.to_le_bytes())
                && buffer.append(&cache.to_le_bytes())
                && buffer.append(&[u8::from(*cycle)])
                && buffer.append(&[u8::from(owner.is_some())]);
            if let Some(owner) = owner {
                ok &= name_bytes(buffer, owner.table_schema.as_str())
                    && name_bytes(buffer, owner.table.as_str())
                    && name_bytes(buffer, owner.column.as_str());
            }
            ok &= buffer.append(&[u8::from(generator_for.is_some())]);
            if let Some(generator) = generator_for {
                ok &= name_bytes(buffer, generator.table_schema.as_str())
                    && name_bytes(buffer, generator.table.as_str())
                    && name_bytes(buffer, generator.column.as_str());
            }
            ok
        }
        WalOp::DropSequence { schema, name } => {
            name_bytes(buffer, name) && name_bytes(buffer, schema)
        }
        WalOp::CreateDomain(def) => {
            let de = def.default_expr.as_ref().map(|e| e.as_str()).unwrap_or("");
            let mut ok = name_bytes(buffer, def.name.as_str())
                && name_bytes(buffer, def.schema.as_str())
                && buffer.append(&[DOMAIN_PAYLOAD_WITH_PARENT])
                && name_bytes(
                    buffer,
                    def.base_domain
                        .as_ref()
                        .map(|d| d.name.as_str())
                        .unwrap_or(""),
                )
                && name_bytes(
                    buffer,
                    def.base_domain
                        .as_ref()
                        .map(|identity| identity.schema.as_str())
                        .unwrap_or(""),
                )
                && buffer.append(&[def.base.code()])
                && buffer.append(&def.base_type_mod.to_le_bytes())
                && buffer.append(&[u8::from(def.not_null)])
                && buffer.append(&(de.len() as u16).to_le_bytes())
                && buffer.append(de.as_bytes())
                && buffer.append(&[def.n_checks as u8]);
            for c in def.checks() {
                ok &= name_bytes(buffer, c.name.as_str())
                    && buffer.append(&(c.expression.as_str().len() as u16).to_le_bytes())
                    && buffer.append(c.expression.as_str().as_bytes());
            }
            ok
        }
        WalOp::DropDomain { schema, name } => {
            name_bytes(buffer, name) && name_bytes(buffer, schema)
        }
        WalOp::CreateEnum(def) => {
            let mut ok = name_bytes(buffer, def.name.as_str())
                && name_bytes(buffer, def.schema.as_str())
                && buffer.append(&[def.n_members as u8]);
            for m in def.members() {
                ok &= name_bytes(buffer, m.label.as_str()) && buffer.append(&m.sort.to_le_bytes());
            }
            ok
        }
        WalOp::DropEnum { schema, name } => name_bytes(buffer, name) && name_bytes(buffer, schema),
        WalOp::RenameEnum {
            schema,
            old_name,
            new_name,
        } => {
            name_bytes(buffer, old_name)
                && name_bytes(buffer, schema)
                && name_bytes(buffer, new_name)
        }
        WalOp::CreateRoutine(def) => {
            let mut ok = buffer.append(&def.created_at.to_le_bytes())
                && buffer.append(&def.ownership.owner.to_le_bytes())
                && name_bytes(buffer, def.name.as_str())
                && name_bytes(buffer, def.schema.as_str())
                && buffer.append(&[def.argument_count as u8]);
            for argument in def.arguments() {
                ok &= name_bytes(buffer, argument.name.as_str())
                    && buffer.append(&[argument.ctype.code()]);
            }
            ok &= buffer.append(&[def.kind.function_result().unwrap_or(ColType::Text).code()])
                && buffer.append(&(def.body.as_str().len() as u16).to_le_bytes())
                && buffer.append(def.body.as_str().as_bytes());
            if !matches!(def.kind, crate::storage::RoutineKind::Function { .. }) {
                ok &= buffer.append(&[def.kind.wire_code()]);
            }
            if matches!(def.kind, crate::storage::RoutineKind::TableFunction) {
                ok &= def.result_column_count <= u8::MAX as usize
                    && buffer.append(&[def.result_column_count as u8]);
                for column in &def.result_columns[..def.result_column_count] {
                    ok &= name_bytes(buffer, column.name.as_str())
                        && buffer.append(&[column.ctype.code()]);
                }
            }
            ok
        }
        WalOp::DropRoutine {
            schema,
            name,
            argument_type_codes,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && argument_type_codes.len() <= u8::MAX as usize
                && buffer.append(&[argument_type_codes.len() as u8])
                && buffer.append(argument_type_codes)
        }
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_type_codes,
            new_schema,
            new_name,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && argument_type_codes.len() <= u8::MAX as usize
                && buffer.append(&[argument_type_codes.len() as u8])
                && buffer.append(argument_type_codes)
                && name_bytes(buffer, new_schema)
                && name_bytes(buffer, new_name)
        }
        WalOp::AlterDomainIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && name_bytes(buffer, new_schema)
                && name_bytes(buffer, new_name)
        }
        WalOp::SequenceAdvance {
            schema,
            name,
            last,
            is_called,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(&last.to_le_bytes())
                && buffer.append(&[u8::from(*is_called)])
        }
        WalOp::Comment {
            class,
            schema,
            name,
            subid,
            text,
        } => {
            let mut ok = name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(&[*class])
                && buffer.append(&subid.to_le_bytes());
            match text {
                Some(t) => {
                    ok &= buffer.append(&[1u8])
                        && buffer.append(&(t.len() as u16).to_le_bytes())
                        && buffer.append(t.as_bytes());
                }
                None => ok &= buffer.append(&[0u8]),
            }
            ok
        }
        WalOp::UpsertRole { name, attributes } => {
            let flags = u16::from(attributes.superuser)
                | (u16::from(attributes.inherit) << 1)
                | (u16::from(attributes.create_role) << 2)
                | (u16::from(attributes.create_database) << 3)
                | (u16::from(attributes.can_login) << 4)
                | (u16::from(attributes.replication) << 5)
                | (u16::from(attributes.bypass_row_level_security) << 6)
                | (u16::from(attributes.has_password) << 7)
                | (u16::from(attributes.has_valid_until) << 8);
            name_bytes(buffer, name)
                && buffer.append(&flags.to_le_bytes())
                && buffer.append(&attributes.connection_limit.to_le_bytes())
                && buffer.append(&attributes.password.salt)
                && buffer.append(&attributes.password.stored_key)
                && buffer.append(&attributes.password.server_key)
                && buffer.append(&attributes.password.iterations.to_le_bytes())
                && name_bytes(buffer, attributes.valid_until.as_str())
        }
        WalOp::DropRole { name } => name_bytes(buffer, name),
        WalOp::UpsertRoleMembership {
            role,
            member,
            grantor,
            options,
        } => {
            let flags = u8::from(options.admin)
                | (u8::from(options.inherit) << 1)
                | (u8::from(options.set) << 2);
            name_bytes(buffer, role)
                && name_bytes(buffer, member)
                && name_bytes(buffer, grantor)
                && buffer.append(&[flags])
        }
        WalOp::DropRoleMembership { role, member } => {
            name_bytes(buffer, role) && name_bytes(buffer, member)
        }
        WalOp::SetObjectOwner {
            class,
            object_oid,
            schema,
            name,
            owner,
        } => {
            buffer.append(&[*class])
                && (*class != crate::storage::AccessClass::Routine as u8
                    || buffer.append(&object_oid.to_le_bytes()))
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && name_bytes(buffer, owner)
        }
        WalOp::SetObjectAcl {
            class,
            object_oid,
            schema,
            name,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            buffer.append(&[*class])
                && (*class != crate::storage::AccessClass::Routine as u8
                    || buffer.append(&object_oid.to_le_bytes()))
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && name_bytes(buffer, grantee)
                && name_bytes(buffer, grantor)
                && buffer.append(&privileges.0.to_le_bytes())
                && buffer.append(&grant_options.0.to_le_bytes())
        }
        WalOp::SetDefaultAcl {
            owner,
            schema,
            class,
            grantee,
            defined,
            privileges,
            grant_options,
        } => {
            name_bytes(buffer, owner)
                && name_bytes(buffer, schema)
                && buffer.append(&[*class])
                && name_bytes(buffer, grantee)
                && buffer.append(&[u8::from(*defined)])
                && buffer.append(&privileges.0.to_le_bytes())
                && buffer.append(&grant_options.0.to_le_bytes())
        }
        WalOp::Analyze {
            schema,
            table,
            statistics,
        } => name_bytes(buffer, table) && name_bytes(buffer, schema) && statistics.append(buffer),
    }
}

/// Decodes an uploaded-segment record starting at the kind byte. The
/// on-disk record header is `crc(4) len(4) lsn(8) kind(1) pad(7)`; callers
/// pass the slice from the kind byte onward, so the payload begins 8 bytes
/// in (kind + 7 pad), matching the local journal layout.
pub(crate) fn decode_record(record: &[u8]) -> Option<WalOp<'_>> {
    if record.len() < 8 {
        return None;
    }
    decode_op(record[0], &record[8..])
}

fn stored_dependency_name<'a>(payload: &'a [u8], at: &mut usize) -> Option<&'a str> {
    let length = *payload.get(*at)? as usize;
    *at += 1;
    let bytes = payload.get(*at..*at + length)?;
    *at += length;
    core::str::from_utf8(bytes).ok()
}

fn validate_stored_query_dependencies(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    let (count, mut at, has_columns) = if first == 0xff {
        let Some(&count) = payload.get(1) else {
            return false;
        };
        (count, 2, true)
    } else {
        (first, 1, false)
    };
    if count as usize > crate::storage::MAX_STORED_QUERY_DEPENDENCIES {
        return false;
    }
    for _ in 0..count {
        let Some(&class) = payload.get(at) else {
            return false;
        };
        if DependencyClass::from_code(class).is_none() {
            return false;
        }
        at += 1;
        if has_columns {
            if payload.get(at..at + 8).is_none() {
                return false;
            }
            at += 8;
        }
        for _ in 0..4 {
            if stored_dependency_name(payload, &mut at).is_none() {
                return false;
            }
        }
    }
    at == payload.len()
}

#[inline(never)]
fn decode_stored_query_dependencies(payload: &[u8]) -> Option<StoredQueryDependencies> {
    if !validate_stored_query_dependencies(payload) {
        return None;
    }
    let has_columns = payload[0] == 0xff;
    let count = payload[has_columns as usize] as usize;
    let mut at = if has_columns { 2 } else { 1 };
    let mut dependencies = StoredQueryDependencies::EMPTY;
    for _ in 0..count {
        let class = DependencyClass::from_code(payload[at])?;
        at += 1;
        let referenced_columns = if has_columns {
            let columns = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            columns
        } else {
            0
        };
        let schema = stored_dependency_name(payload, &mut at)?;
        let name = stored_dependency_name(payload, &mut at)?;
        let referenced_schema = stored_dependency_name(payload, &mut at)?;
        let referenced_name = stored_dependency_name(payload, &mut at)?;
        dependencies
            .serialized_push_with_columns(
                class,
                SqlName::parse(schema).ok()?,
                SqlName::parse(name).ok()?,
                SqlName::parse(referenced_schema).ok()?,
                SqlName::parse(referenced_name).ok()?,
                referenced_columns,
            )
            .ok()?;
    }
    Some(dependencies)
}

fn decode_table_statistics(payload: &[u8]) -> Option<TableStatistics> {
    let mut at = 0usize;
    let version_two = payload.first().copied() == Some(TABLE_STATISTICS_V2);
    if version_two {
        at += 1;
    }
    let rows = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
    at += 8;
    let average_row_width = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
    at += 4;
    let analyzed_generation = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
    at += 8;
    let count = *payload.get(at)? as usize;
    at += 1;
    if count > MAX_COLUMNS {
        return None;
    }
    let multi_count = if version_two {
        let count = *payload.get(at)? as usize;
        at += 1;
        if count > MAX_MULTICOLUMN_STATISTICS {
            return None;
        }
        count
    } else {
        0
    };
    let mut statistics = TableStatistics {
        valid: true,
        rows,
        average_row_width,
        analyzed_generation,
        columns: [ColumnStatistics::EMPTY; MAX_COLUMNS],
        multi_columns: [MultiColumnStatistics::EMPTY; MAX_MULTICOLUMN_STATISTICS],
    };
    for _ in 0..count {
        let column = *payload.get(at)? as usize;
        at += 1;
        if column >= MAX_COLUMNS || statistics.columns[column].valid {
            return None;
        }
        let null_fraction_ppm = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        if null_fraction_ppm > 1_000_000 {
            return None;
        }
        let distinct_values = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        let distinct_fraction_ppm = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        if distinct_fraction_ppm > 1_000_000 {
            return None;
        }
        let average_width = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        statistics.columns[column] = ColumnStatistics {
            valid: true,
            null_fraction_ppm,
            distinct_values,
            distinct_fraction_ppm,
            average_width,
        };
    }
    for multi_index in 0..multi_count {
        let n_columns = *payload.get(at)? as usize;
        at += 1;
        if !(2..=MAX_INDEX_COLS).contains(&n_columns) {
            return None;
        }
        let mut columns = [0u16; MAX_INDEX_COLS];
        for column in &mut columns[..n_columns] {
            *column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            if usize::from(*column) >= MAX_COLUMNS {
                return None;
            }
        }
        let non_null_rows = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        let distinct_values = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        if statistics.multi_columns[..multi_index]
            .iter()
            .any(|existing| {
                existing.n_columns as usize == n_columns
                    && existing.columns[..n_columns] == columns[..n_columns]
            })
        {
            return None;
        }
        statistics.multi_columns[multi_index] = MultiColumnStatistics {
            valid: true,
            columns,
            n_columns: n_columns as u8,
            non_null_rows,
            distinct_values,
        };
    }
    (at == payload.len()).then_some(statistics)
}

fn decode_op(kind: u8, payload: &[u8]) -> Option<WalOp<'_>> {
    let mut at = 0usize;
    let take_name = |at: &mut usize| -> Option<&str> {
        let len = *payload.get(*at)? as usize;
        *at += 1;
        let raw = payload.get(*at..*at + len)?;
        *at += len;
        core::str::from_utf8(raw).ok()
    };
    match kind {
        KIND_CREATE => {
            let name = take_name(&mut at)?;
            let n_cols = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            if n_cols > MAX_COLUMNS {
                return None;
            }
            let mut def = TableDef {
                name: SqlName::parse(name).ok()?,
                columns: [ColumnMeta {
                    name: SqlName::parse("").ok()?,
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
                n_columns: n_cols,
                ..TableDef::empty()
            };
            for i in 0..n_cols {
                let col_name = take_name(&mut at)?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                let type_mod = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap());
                at += 4;
                let default_value = decode_default(payload, &mut at)?;
                let de_len =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let default_expr = if de_len > 0 {
                    let s = core::str::from_utf8(payload.get(at..at + de_len)?).ok()?;
                    at += de_len;
                    Some(crate::util::StackStr::from_str(s))
                } else {
                    None
                };
                let auto_increment_step =
                    i64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
                at += 8;
                // Bit 7 set means a durable user-type identity follows.
                let user_type = if meta[1] & 128 != 0 {
                    let name = SqlName::parse(take_name(&mut at)?).ok()?;
                    let schema = if payload.get(at).copied() == Some(u8::MAX) {
                        at += 1;
                        SqlName::parse(take_name(&mut at)?).ok()?
                    } else {
                        return None;
                    };
                    Some(crate::storage::UserTypeName { schema, name })
                } else {
                    None
                };
                let default =
                    ColumnDefault::from_parts(default_value, default_expr, meta[1] & 16 != 0)?;
                def.columns[i] = ColumnMeta {
                    name: SqlName::parse(col_name).ok()?,
                    ctype: ColType::from_code(meta[0])?,
                    type_mod,
                    not_null: meta[1] & 1 != 0,
                    unique: meta[1] & 2 != 0,
                    primary: meta[1] & 4 != 0,
                    auto_increment: meta[1] & 8 != 0,
                    default,
                    is_identity: meta[1] & 32 != 0,
                    identity_always: meta[1] & 64 != 0,
                    auto_increment_step,
                    user_type,
                };
            }
            // Multi-column UNIQUE/PRIMARY KEY constraints.
            let n_uniques = *payload.get(at)? as usize;
            at += 1;
            if n_uniques > crate::storage::MAX_UNIQUES {
                return None;
            }
            def.n_uniques = n_uniques;
            for u in 0..n_uniques {
                let uname = take_name(&mut at)?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                let n = meta[1] as usize;
                if n > MAX_INDEX_COLS {
                    return None;
                }
                let mut uk = UniqueKey::EMPTY;
                uk.name = SqlName::parse(uname).ok()?;
                uk.is_primary = meta[0] != 0;
                uk.n_cols = n;
                for c in uk.columns.iter_mut().take(n) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                def.uniques[u] = uk;
            }
            // CHECK constraints.
            let n_checks = *payload.get(at)? as usize;
            at += 1;
            if n_checks > crate::storage::MAX_CHECKS {
                return None;
            }
            def.n_checks = n_checks;
            for k in 0..n_checks {
                let constraint_name = take_name(&mut at)?;
                let elen =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let raw = payload.get(at..at + elen)?;
                at += elen;
                let text = core::str::from_utf8(raw).ok()?;
                let mut check = CheckConstraint::EMPTY;
                check.name = SqlName::parse(constraint_name).ok()?;
                core::fmt::Write::write_str(&mut check.expression, text).ok()?;
                if check.expression.is_truncated() {
                    return None;
                }
                def.checks[k] = check;
            }
            // FOREIGN KEY constraints.
            let n_fkeys = *payload.get(at)? as usize;
            at += 1;
            if n_fkeys > crate::storage::MAX_FKEYS {
                return None;
            }
            def.n_fkeys = n_fkeys;
            for f in 0..n_fkeys {
                let fname = take_name(&mut at)?;
                let nc = *payload.get(at)? as usize;
                at += 1;
                if nc > MAX_INDEX_COLS {
                    return None;
                }
                let mut fk = ForeignKey::EMPTY;
                fk.name = SqlName::parse(fname).ok()?;
                fk.n_cols = nc;
                for c in fk.columns.iter_mut().take(nc) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                let parent_name = take_name(&mut at)?;
                fk.parent = SqlName::parse(parent_name).ok()?;
                let np = *payload.get(at)? as usize;
                at += 1;
                if np > MAX_INDEX_COLS {
                    return None;
                }
                fk.n_parent_cols = np;
                for c in fk.parent_cols.iter_mut().take(np) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                let acts = payload.get(at..at + 2)?;
                at += 2;
                fk.on_delete = FkAction::from_code(acts[0])?;
                fk.on_update = FkAction::from_code(acts[1])?;
                def.fkeys[f] = fk;
            }
            // Trailing schema block; a journal from before schemas existed
            // ends here, and everything defaults to public.
            if at < payload.len() {
                def.schema = SqlName::parse(take_name(&mut at)?).ok()?;
                for f in 0..def.n_fkeys {
                    def.fkeys[f].parent_schema = SqlName::parse(take_name(&mut at)?).ok()?;
                }
            } else {
                def.schema = SqlName::parse("public").ok()?;
                for f in 0..def.n_fkeys {
                    def.fkeys[f].parent_schema = SqlName::parse("public").ok()?;
                }
            }
            (at == payload.len()).then_some(WalOp::CreateTable(def))
        }
        KIND_REWRITE_TABLE => {
            let previous_schema = take_name(&mut at)?;
            let previous_name = take_name(&mut at)?;
            let mut column_mapping = [u16::MAX; MAX_COLUMNS];
            for target in &mut column_mapping {
                *target = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
            }
            (at == payload.len()).then_some(WalOp::BeginTableRewrite {
                previous_schema,
                previous_name,
                column_mapping,
            })
        }
        KIND_DROP => {
            let name = take_name(&mut at)?;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            (at == payload.len()).then_some(WalOp::DropTable { schema, name })
        }
        KIND_UPSERT => {
            let table = take_name(&mut at)?;
            let rowid = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            let row_len = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap()) as usize;
            at += 4;
            let row = payload.get(at..at + row_len)?;
            at += row_len;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            let (is_update, old_row) = if at == payload.len() {
                (false, None)
            } else {
                let is_update = *payload.get(at)? != 0;
                at += 1;
                let has_old = *payload.get(at)? != 0;
                at += 1;
                let old_row = if has_old {
                    let length =
                        u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                    at += 4;
                    let old = payload.get(at..at + length)?;
                    at += length;
                    Some(old)
                } else {
                    None
                };
                (is_update, old_row)
            };
            let command_id = if at == payload.len() {
                0
            } else {
                let command_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                command_id
            };
            (at == payload.len()).then_some(WalOp::Upsert {
                schema,
                table,
                rowid,
                row,
                is_update,
                old_row,
                command_id,
            })
        }
        KIND_DELETE => {
            let table = take_name(&mut at)?;
            let rowid = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            let old_row = if at == payload.len() {
                None
            } else {
                let has_old = *payload.get(at)? != 0;
                at += 1;
                if has_old {
                    let length =
                        u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                    at += 4;
                    let old = payload.get(at..at + length)?;
                    at += length;
                    Some(old)
                } else {
                    None
                }
            };
            let command_id = if at == payload.len() {
                0
            } else {
                let command_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                command_id
            };
            (at == payload.len()).then_some(WalOp::Delete {
                schema,
                table,
                rowid,
                old_row,
                command_id,
            })
        }
        KIND_TRUNCATE => {
            let count = *payload.get(at)? as usize;
            at += 1;
            if count > crate::sql::txn::MAX_TRUNCATE_TABLES {
                return None;
            }
            let tables_start = at;
            for _ in 0..count {
                let _ = take_name(&mut at)?;
                let _ = take_name(&mut at)?;
            }
            let tables_end = at;
            let flags = *payload.get(at)?;
            at += 1;
            if flags & !3 != 0 {
                return None;
            }
            let command_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            (at == payload.len()).then_some(WalOp::Truncate {
                tables: &payload[tables_start..tables_end],
                table_count: count,
                cascade: flags & 1 != 0,
                restart_identity: flags & 2 != 0,
                command_id,
            })
        }
        KIND_CREATE_VIEW => {
            let name = take_name(&mut at)?;
            let sql_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            let raw = payload.get(at..at + sql_len)?;
            at += sql_len;
            let sql = core::str::from_utf8(raw).ok()?;
            let (schema, path) = if at < payload.len() {
                let schema = take_name(&mut at)?;
                let path_len =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let raw = payload.get(at..at + path_len)?;
                at += path_len;
                (schema, core::str::from_utf8(raw).ok()?)
            } else {
                ("public", "\"$user\", public")
            };
            let dependencies = if at < payload.len() {
                let encoded = payload.get(at..)?;
                if !validate_stored_query_dependencies(encoded) {
                    return None;
                }
                at = payload.len();
                WalStoredQueryDependencies::Encoded(encoded)
            } else {
                WalStoredQueryDependencies::LegacyEmpty
            };
            (at == payload.len()).then_some(WalOp::CreateView {
                schema,
                name,
                sql,
                path,
                dependencies,
            })
        }
        KIND_DROP_VIEW => {
            let name = take_name(&mut at)?;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            (at == payload.len()).then_some(WalOp::DropView { schema, name })
        }
        KIND_CREATE_PUBLICATION => {
            let name = take_name(&mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let flags = *payload.get(at)?;
            at += 1;
            let count = *payload.get(at)? as usize;
            at += 1;
            let schema_count = *payload.get(at)? as usize;
            at += 1;
            if count > crate::storage::MAX_PUBLICATION_TABLES {
                return None;
            }
            if schema_count > crate::storage::MAX_SCHEMAS {
                return None;
            }
            let mut tables = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
            for table in &mut tables[..count] {
                *table = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
            }
            let mut schemas = [u8::MAX; crate::storage::MAX_SCHEMAS];
            schemas[..schema_count].copy_from_slice(payload.get(at..at + schema_count)?);
            at += schema_count;
            (at == payload.len()).then_some(WalOp::CreatePublication {
                name,
                owner,
                all_tables: flags & 1 != 0,
                tables,
                table_count: count,
                schemas,
                schema_count,
                publish_insert: flags & 2 != 0,
                publish_update: flags & 4 != 0,
                publish_delete: flags & 8 != 0,
                publish_truncate: flags & 16 != 0,
            })
        }
        KIND_DROP_PUBLICATION => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropPublication { name })
        }
        KIND_ALTER_PUBLICATION => {
            let name = take_name(&mut at)?;
            let flags = *payload.get(at)?;
            at += 1;
            let count = *payload.get(at)? as usize;
            at += 1;
            let schema_count = *payload.get(at)? as usize;
            at += 1;
            if count > crate::storage::MAX_PUBLICATION_TABLES {
                return None;
            }
            if schema_count > crate::storage::MAX_SCHEMAS {
                return None;
            }
            let mut tables = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
            for table in &mut tables[..count] {
                *table = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
            }
            let mut schemas = [u8::MAX; crate::storage::MAX_SCHEMAS];
            schemas[..schema_count].copy_from_slice(payload.get(at..at + schema_count)?);
            at += schema_count;
            (at == payload.len()).then_some(WalOp::AlterPublication {
                name,
                all_tables: flags & 1 != 0,
                tables,
                table_count: count,
                schemas,
                schema_count,
                publish_insert: flags & 2 != 0,
                publish_update: flags & 4 != 0,
                publish_delete: flags & 8 != 0,
                publish_truncate: flags & 16 != 0,
            })
        }
        KIND_SET_PUBLICATION_OWNER => {
            let name = take_name(&mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::SetPublicationOwner { name, owner })
        }
        KIND_RENAME_PUBLICATION => {
            let name = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::RenamePublication { name, new_name })
        }
        KIND_COMMIT if payload.is_empty() => Some(WalOp::Commit { transaction_id: 0 }),
        KIND_COMMIT => {
            let transaction_id = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            (payload.len() == 4).then_some(WalOp::Commit { transaction_id })
        }
        KIND_CREATE_REPLICATION_SLOT => {
            let name = take_name(&mut at)?;
            let restart_lsn = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            (at == payload.len()).then_some(WalOp::CreateReplicationSlot { name, restart_lsn })
        }
        KIND_DROP_REPLICATION_SLOT => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropReplicationSlot { name })
        }
        KIND_ADVANCE_REPLICATION_SLOT => {
            let name = take_name(&mut at)?;
            let confirmed_flush_lsn = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            (at == payload.len()).then_some(WalOp::AdvanceReplicationSlot {
                name,
                confirmed_flush_lsn,
            })
        }
        KIND_CREATE_MATVIEW => {
            let name = take_name(&mut at)?;
            let sql_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            let sql = core::str::from_utf8(payload.get(at..at + sql_len)?).ok()?;
            at += sql_len;
            let schema = take_name(&mut at)?;
            let path_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            let path = core::str::from_utf8(payload.get(at..at + path_len)?).ok()?;
            at += path_len;
            let populated = *payload.get(at)? != 0;
            at += 1;
            let dependencies = if at < payload.len() {
                let encoded = payload.get(at..)?;
                if !validate_stored_query_dependencies(encoded) {
                    return None;
                }
                at = payload.len();
                WalStoredQueryDependencies::Encoded(encoded)
            } else {
                WalStoredQueryDependencies::LegacyEmpty
            };
            (at == payload.len()).then_some(WalOp::CreateMatview {
                schema,
                name,
                sql,
                path,
                dependencies,
                populated,
            })
        }
        KIND_DROP_MATVIEW => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropMatview { schema, name })
        }
        KIND_SET_MATVIEW_POPULATED => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let populated = *payload.get(at)? != 0;
            at += 1;
            (at == payload.len()).then_some(WalOp::SetMatviewPopulated {
                schema,
                name,
                populated,
            })
        }
        KIND_CREATE_INDEX => {
            let name = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let unique = *payload.get(at)? != 0;
            at += 1;
            let n_cols = *payload.get(at)? as usize;
            at += 1;
            if n_cols > MAX_INDEX_COLS {
                return None;
            }
            let mut columns = [0u16; MAX_INDEX_COLS];
            let mut expressions = [None; MAX_INDEX_COLS];
            for c in columns.iter_mut().take(n_cols) {
                *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                at += 2;
            }
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            let mut descending = [false; MAX_INDEX_COLS];
            let mut nulls_first = [false; MAX_INDEX_COLS];
            let mut predicate = None;
            let mut include_columns = [0u16; MAX_INDEX_COLS];
            let mut n_include_cols = 0usize;
            let mut nulls_not_distinct = false;
            if at < payload.len() {
                if *payload.get(at)? != 0xa1 {
                    return None;
                }
                at += 1;
                for i in 0..n_cols {
                    let flags = *payload.get(at)?;
                    at += 1;
                    if flags & !0b11 != 0 {
                        return None;
                    }
                    descending[i] = flags & 1 != 0;
                    nulls_first[i] = flags & 2 != 0;
                }
            }
            if at < payload.len() {
                if *payload.get(at)? != 0xa2 {
                    return None;
                }
                at += 1;
                match *payload.get(at)? {
                    0 => at += 1,
                    1 => {
                        at += 1;
                        let len =
                            u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                        at += 2;
                        let raw = payload.get(at..at + len)?;
                        at += len;
                        predicate = Some(core::str::from_utf8(raw).ok()?);
                    }
                    _ => return None,
                }
            }
            if at < payload.len() {
                if *payload.get(at)? != 0xa3 {
                    return None;
                }
                at += 1;
                n_include_cols = *payload.get(at)? as usize;
                at += 1;
                if n_include_cols > MAX_INDEX_COLS {
                    return None;
                }
                for column in include_columns.iter_mut().take(n_include_cols) {
                    *column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                }
            }
            if at < payload.len() {
                if *payload.get(at)? != 0xa4 {
                    return None;
                }
                at += 1;
                nulls_not_distinct = match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                at += 1;
            }
            if at < payload.len() {
                if *payload.get(at)? != 0xa5 {
                    return None;
                }
                at += 1;
                let mask = *payload.get(at)?;
                at += 1;
                if mask >> n_cols != 0 {
                    return None;
                }
                for (index, expression) in expressions.iter_mut().enumerate().take(n_cols) {
                    if mask & (1 << index) == 0 {
                        continue;
                    }
                    let len =
                        u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                    at += 2;
                    let raw = payload.get(at..at + len)?;
                    at += len;
                    *expression = Some(core::str::from_utf8(raw).ok()?);
                }
            }
            (at == payload.len()).then_some(WalOp::CreateIndex {
                schema,
                name,
                table,
                columns,
                expressions,
                include_columns,
                descending,
                nulls_first,
                n_cols,
                n_include_cols,
                nulls_not_distinct,
                predicate,
                unique,
            })
        }
        KIND_DROP_INDEX => {
            let name = take_name(&mut at)?;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            (at == payload.len()).then_some(WalOp::DropIndex { schema, name })
        }
        KIND_RENAME_INDEX => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::RenameIndex {
                schema,
                name,
                new_name,
            })
        }
        KIND_SEQUENCE_SET => {
            let table = take_name(&mut at)?;
            let column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
            at += 2;
            let last = i64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            let schema = if at < payload.len() {
                take_name(&mut at)?
            } else {
                "public"
            };
            (at == payload.len()).then_some(WalOp::SequenceSet {
                schema,
                table,
                column,
                last,
            })
        }
        KIND_CREATE_SEQUENCE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let data_type = *payload.get(at)?;
            at += 1;
            let take_i64 = |at: &mut usize| -> Option<i64> {
                let v = i64::from_le_bytes(payload.get(*at..*at + 8)?.try_into().unwrap());
                *at += 8;
                Some(v)
            };
            let increment = take_i64(&mut at)?;
            let min_value = take_i64(&mut at)?;
            let max_value = take_i64(&mut at)?;
            let start_value = take_i64(&mut at)?;
            let cache = take_i64(&mut at)?;
            let cycle = *payload.get(at)? != 0;
            at += 1;
            // Ownership was added as an optional suffix; old journals end
            // after `cycle` and replay as unowned sequences.
            let owner = if at == payload.len() {
                None
            } else {
                let has_owner = *payload.get(at)? != 0;
                at += 1;
                if has_owner {
                    Some(crate::storage::SequenceOwner {
                        table_schema: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                        table: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                        column: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                    })
                } else {
                    None
                }
            };
            let generator_for = if at == payload.len() {
                None
            } else {
                let has_generator = *payload.get(at)? != 0;
                at += 1;
                if has_generator {
                    Some(crate::storage::SequenceOwner {
                        table_schema: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                        table: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                        column: crate::storage::SqlName::parse(take_name(&mut at)?).ok()?,
                    })
                } else {
                    None
                }
            };
            (at == payload.len()).then_some(WalOp::CreateSequence {
                schema,
                name,
                data_type,
                increment,
                min_value,
                max_value,
                start_value,
                cache,
                cycle,
                owner,
                generator_for,
            })
        }
        KIND_DROP_SEQUENCE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropSequence { schema, name })
        }
        KIND_SEQUENCE_ADVANCE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let last = i64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            let is_called = *payload.get(at)? != 0;
            at += 1;
            (at == payload.len()).then_some(WalOp::SequenceAdvance {
                schema,
                name,
                last,
                is_called,
            })
        }
        KIND_COMMENT => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let class = *payload.get(at)?;
            at += 1;
            let subid = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap());
            at += 4;
            let present = *payload.get(at)?;
            at += 1;
            let text = if present != 0 {
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let raw = payload.get(at..at + len)?;
                at += len;
                Some(core::str::from_utf8(raw).ok()?)
            } else {
                None
            };
            (at == payload.len()).then_some(WalOp::Comment {
                class,
                schema,
                name,
                subid,
                text,
            })
        }
        KIND_CREATE_DOMAIN => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let base_domain = if *payload.get(at)? == DOMAIN_PAYLOAD_WITH_PARENT {
                at += 1;
                let base_domain_name = take_name(&mut at)?;
                let base_domain = if base_domain_name.is_empty() {
                    None
                } else {
                    Some(SqlName::parse(base_domain_name).ok()?)
                };
                let base_domain_schema_name = take_name(&mut at)?;
                let base_domain_schema = if base_domain_schema_name.is_empty() {
                    None
                } else {
                    Some(SqlName::parse(base_domain_schema_name).ok()?)
                };
                match (base_domain, base_domain_schema) {
                    (None, None) => None,
                    (Some(name), Some(schema)) => {
                        Some(crate::storage::UserTypeName { schema, name })
                    }
                    _ => return None,
                }
            } else {
                None
            };
            let base = crate::sql::types::ColType::from_code(*payload.get(at)?)?;
            at += 1;
            let base_type_mod = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap());
            at += 4;
            let not_null = *payload.get(at)? != 0;
            at += 1;
            let de_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            let de = core::str::from_utf8(payload.get(at..at + de_len)?).ok()?;
            at += de_len;
            let default_expr = (de_len > 0).then(|| crate::util::StackStr::from_str(de));
            let n_checks = *payload.get(at)? as usize;
            at += 1;
            if n_checks > crate::storage::MAX_DOMAIN_CHECKS {
                return None;
            }
            let mut checks =
                [crate::storage::CheckConstraint::EMPTY; crate::storage::MAX_DOMAIN_CHECKS];
            for check in checks.iter_mut().take(n_checks) {
                let cname = take_name(&mut at)?;
                let elen =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let expr = core::str::from_utf8(payload.get(at..at + elen)?).ok()?;
                at += elen;
                *check = crate::storage::CheckConstraint {
                    name: SqlName::parse(cname).ok()?,
                    expression: crate::util::StackStr::from_str(expr),
                };
            }
            (at == payload.len()).then_some(WalOp::CreateDomain(crate::storage::DomainDef {
                created_at: 0,
                schema: SqlName::parse(schema).ok()?,
                name: SqlName::parse(name).ok()?,
                ownership: crate::storage::Ownership::BOOTSTRAP,
                base_domain,
                base,
                base_type_mod,
                not_null,
                default_expr,
                checks,
                n_checks,
                pending_definition: None,
                ddl_state: crate::storage::CatalogDdlState::Absent,
            }))
        }
        KIND_DROP_DOMAIN => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropDomain { schema, name })
        }
        KIND_CREATE_ENUM => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let n_members = *payload.get(at)? as usize;
            at += 1;
            if n_members > crate::storage::MAX_ENUM_LABELS {
                return None;
            }
            let mut members = [crate::storage::EnumMember::EMPTY; crate::storage::MAX_ENUM_LABELS];
            for member in members.iter_mut().take(n_members) {
                let label = take_name(&mut at)?;
                let sort = f64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
                at += 8;
                *member = crate::storage::EnumMember {
                    label: SqlName::parse(label).ok()?,
                    sort,
                };
            }
            (at == payload.len()).then_some(WalOp::CreateEnum(crate::storage::EnumDef {
                created_at: 0,
                schema: SqlName::parse(schema).ok()?,
                name: SqlName::parse(name).ok()?,
                ownership: crate::storage::Ownership::BOOTSTRAP,
                members,
                n_members,
                pending_definition: None,
                ddl_state: crate::storage::CatalogDdlState::Absent,
            }))
        }
        KIND_DROP_ENUM => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropEnum { schema, name })
        }
        KIND_RENAME_ENUM => {
            let old_name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::RenameEnum {
                schema,
                old_name,
                new_name,
            })
        }
        KIND_CREATE_ROUTINE => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let argument_count = *payload.get(at)? as usize;
            at += 1;
            if argument_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                return None;
            }
            let mut arguments =
                [crate::storage::RoutineArgumentDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
            for argument in arguments.iter_mut().take(argument_count) {
                let argument_name = take_name(&mut at)?;
                let ctype = ColType::from_code(*payload.get(at)?)?;
                at += 1;
                *argument = crate::storage::RoutineArgumentDef {
                    name: SqlName::parse(argument_name).ok()?,
                    ctype,
                };
            }
            let result_code = *payload.get(at)?;
            at += 1;
            let body_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            if body_len > crate::storage::ROUTINE_SQL_MAX {
                return None;
            }
            let body = core::str::from_utf8(payload.get(at..at + body_len)?).ok()?;
            at += body_len;
            let mut result_columns =
                [crate::storage::RoutineArgumentDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut result_column_count = 0;
            let kind = if at == payload.len() {
                crate::storage::RoutineKind::Function {
                    result: ColType::from_code(result_code)?,
                }
            } else {
                let code = *payload.get(at)?;
                at += 1;
                if code == 3 {
                    result_column_count = *payload.get(at)? as usize;
                    at += 1;
                    if result_column_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                        return None;
                    }
                    for column in result_columns.iter_mut().take(result_column_count) {
                        let name = take_name(&mut at)?;
                        let ctype = ColType::from_code(*payload.get(at)?)?;
                        at += 1;
                        *column = crate::storage::RoutineArgumentDef {
                            name: SqlName::parse(name).ok()?,
                            ctype,
                        };
                    }
                    crate::storage::RoutineKind::TableFunction
                } else {
                    crate::storage::RoutineKind::from_wire_code(
                        code,
                        ColType::from_code(result_code)?,
                    )?
                }
            };
            (at == payload.len()).then_some(WalOp::CreateRoutine(crate::storage::RoutineDef {
                created_at,
                schema: SqlName::parse(schema).ok()?,
                name: SqlName::parse(name).ok()?,
                pending_identity: None,
                arguments,
                argument_count,
                kind,
                result_columns,
                result_column_count,
                body: crate::util::StackStr::from_str(body),
                ownership: crate::storage::Ownership {
                    owner,
                    pending: None,
                },
                ddl_state: crate::storage::CatalogDdlState::Absent,
            }))
        }
        KIND_DROP_ROUTINE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let count = *payload.get(at)? as usize;
            at += 1;
            let argument_type_codes = payload.get(at..at + count)?;
            at += count;
            if at != payload.len() {
                return None;
            }
            Some(WalOp::DropRoutine {
                schema,
                name,
                argument_type_codes,
            })
        }
        KIND_ALTER_ROUTINE_IDENTITY => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let count = *payload.get(at)? as usize;
            at += 1;
            let argument_type_codes = payload.get(at..at + count)?;
            at += count;
            let new_schema = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::AlterRoutineIdentity {
                schema,
                name,
                argument_type_codes,
                new_schema,
                new_name,
            })
        }
        KIND_ALTER_DOMAIN_IDENTITY => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let new_schema = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::AlterDomainIdentity {
                schema,
                name,
                new_schema,
                new_name,
            })
        }
        KIND_CREATE_SCHEMA => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::CreateSchema(name))
        }
        KIND_DROP_SCHEMA => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropSchema(name))
        }
        KIND_SET_TABLE_SCHEMA => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let new_schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::SetTableSchema {
                schema,
                name,
                new_schema,
            })
        }
        KIND_DROP_FK => {
            let schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let fk_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropTableFk {
                schema,
                table,
                fk_name,
            })
        }
        KIND_ANALYZE => {
            let table = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let encoded = payload.get(at..)?;
            decode_table_statistics(encoded)?;
            Some(WalOp::Analyze {
                schema,
                table,
                statistics: WalTableStatistics::Encoded(encoded),
            })
        }
        KIND_UPSERT_ROLE => {
            let name = take_name(&mut at)?;
            let flags = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            if flags & !0x01ff != 0 {
                return None;
            }
            let connection_limit = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let salt = payload.get(at..at + 16)?.try_into().ok()?;
            at += 16;
            let stored_key = payload.get(at..at + 32)?.try_into().ok()?;
            at += 32;
            let server_key = payload.get(at..at + 32)?.try_into().ok()?;
            at += 32;
            let iterations = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let valid_until = take_name(&mut at)?;
            if at != payload.len()
                || valid_until.len() > crate::storage::ROLE_VALID_UNTIL_MAX
                || (flags & (1 << 7) != 0 && iterations == 0)
            {
                return None;
            }
            Some(WalOp::UpsertRole {
                name,
                attributes: RoleAttributes {
                    superuser: flags & 1 != 0,
                    inherit: flags & (1 << 1) != 0,
                    create_role: flags & (1 << 2) != 0,
                    create_database: flags & (1 << 3) != 0,
                    can_login: flags & (1 << 4) != 0,
                    replication: flags & (1 << 5) != 0,
                    bypass_row_level_security: flags & (1 << 6) != 0,
                    connection_limit,
                    password: crate::storage::RolePassword {
                        salt,
                        stored_key,
                        server_key,
                        iterations,
                    },
                    has_password: flags & (1 << 7) != 0,
                    valid_until: crate::util::StackStr::from_str(valid_until),
                    has_valid_until: flags & (1 << 8) != 0,
                },
            })
        }
        KIND_DROP_ROLE => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropRole { name })
        }
        KIND_UPSERT_ROLE_MEMBERSHIP => {
            let role = take_name(&mut at)?;
            let member = take_name(&mut at)?;
            let grantor = take_name(&mut at)?;
            let flags = *payload.get(at)?;
            at += 1;
            if at != payload.len() || flags & !0x07 != 0 {
                return None;
            }
            Some(WalOp::UpsertRoleMembership {
                role,
                member,
                grantor,
                options: crate::storage::RoleMembershipOptions {
                    admin: flags & 1 != 0,
                    inherit: flags & 2 != 0,
                    set: flags & 4 != 0,
                },
            })
        }
        KIND_DROP_ROLE_MEMBERSHIP => {
            let role = take_name(&mut at)?;
            let member = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropRoleMembership { role, member })
        }
        KIND_SET_OBJECT_OWNER => {
            let class = *payload.get(at)?;
            at += 1;
            crate::storage::AccessClass::from_u8(class)?;
            let object_oid = if class == crate::storage::AccessClass::Routine as u8 {
                let object_oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                object_oid
            } else {
                0
            };
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let owner = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::SetObjectOwner {
                class,
                object_oid,
                schema,
                name,
                owner,
            })
        }
        KIND_SET_OBJECT_ACL => {
            let class = *payload.get(at)?;
            at += 1;
            crate::storage::AccessClass::from_u8(class)?;
            let object_oid = if class == crate::storage::AccessClass::Routine as u8 {
                let object_oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                object_oid
            } else {
                0
            };
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let grantee = take_name(&mut at)?;
            let grantor = take_name(&mut at)?;
            let privileges = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let grant_options = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::SetObjectAcl {
                class,
                object_oid,
                schema,
                name,
                grantee,
                grantor,
                privileges: crate::storage::PrivilegeSet(privileges),
                grant_options: crate::storage::PrivilegeSet(grant_options),
            })
        }
        KIND_SET_DEFAULT_ACL => {
            let owner = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let class = *payload.get(at)?;
            at += 1;
            crate::storage::DefaultPrivilegeClass::from_u8(class)?;
            let grantee = take_name(&mut at)?;
            let defined = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let privileges = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let grant_options = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::SetDefaultAcl {
                owner,
                schema,
                class,
                grantee,
                defined,
                privileges: crate::storage::PrivilegeSet(privileges),
                grant_options: crate::storage::PrivilegeSet(grant_options),
            })
        }
        _ => None,
    }
}

pub(crate) fn encoded_default_len(d: &Option<OwnedDatum>) -> usize {
    1 + match d {
        None | Some(OwnedDatum::Null) => 0,
        Some(OwnedDatum::Bool(_)) => 1,
        Some(OwnedDatum::Int4(_)) => 4,
        Some(OwnedDatum::Int8(_)) | Some(OwnedDatum::Float8(_)) => 8,
        Some(OwnedDatum::Date(_)) => 4,
        Some(OwnedDatum::Timestamp(_))
        | Some(OwnedDatum::Timestamptz(_))
        | Some(OwnedDatum::Time(_)) => 8,
        Some(OwnedDatum::Timetz(..)) => 12,
        Some(OwnedDatum::Interval(_)) | Some(OwnedDatum::Uuid(_)) => 16,
        Some(OwnedDatum::Text { len, .. }) => 1 + *len as usize,
        Some(OwnedDatum::Numeric { nbytes, .. }) => 6 + *nbytes as usize,
        Some(OwnedDatum::Inet(_)) | Some(OwnedDatum::Cidr(_)) => 18,
        Some(OwnedDatum::Macaddr(_)) => 6,
        Some(OwnedDatum::Macaddr8(_)) => 8,
        // slot(2) + sort(8) + len(1) + label bytes.
        Some(OwnedDatum::Enum { len, .. }) => 11 + *len as usize,
        Some(OwnedDatum::Json { len, .. }) | Some(OwnedDatum::Bit { len, .. }) => 2 + *len as usize,
        Some(OwnedDatum::Bytea { len, .. }) => 1 + *len as usize,
        Some(OwnedDatum::Array { len, .. }) => 5 + *len as usize,
        Some(OwnedDatum::Range { len, .. }) => 3 + *len as usize,
    }
}

pub(crate) fn append_default(buffer: &mut FixedBuf, d: &Option<OwnedDatum>) -> bool {
    let mut scratch = [0u8; MAX_DEFAULT_ENCODED];
    let n = encode_default_bytes(d, &mut scratch);
    buffer.append(&scratch[..n])
}

/// Largest encoded default: an enum's tag, identity, ordering key, length and
/// bounded label. Array payloads carry less metadata.
pub(crate) const MAX_DEFAULT_ENCODED: usize = 12 + crate::storage::MAX_DEFAULT_TEXT;

/// Stack encoding of a column default; returns the byte count.
pub(crate) fn encode_default_bytes(d: &Option<OwnedDatum>, out: &mut [u8]) -> usize {
    match d {
        None => {
            out[0] = 0;
            1
        }
        Some(OwnedDatum::Null) => {
            out[0] = 1;
            1
        }
        Some(OwnedDatum::Bool(b)) => {
            out[0] = 2;
            out[1] = u8::from(*b);
            2
        }
        Some(OwnedDatum::Int4(v)) => {
            out[0] = 3;
            out[1..5].copy_from_slice(&v.to_le_bytes());
            5
        }
        Some(OwnedDatum::Int8(v)) => {
            out[0] = 4;
            out[1..9].copy_from_slice(&v.to_le_bytes());
            9
        }
        Some(OwnedDatum::Float8(v)) => {
            out[0] = 5;
            out[1..9].copy_from_slice(&v.to_le_bytes());
            9
        }
        Some(OwnedDatum::Date(value)) => {
            out[0] = 13;
            out[1..5].copy_from_slice(&value.to_le_bytes());
            5
        }
        Some(OwnedDatum::Timestamp(value)) => {
            out[0] = 14;
            out[1..9].copy_from_slice(&value.to_le_bytes());
            9
        }
        Some(OwnedDatum::Timestamptz(value)) => {
            out[0] = 15;
            out[1..9].copy_from_slice(&value.to_le_bytes());
            9
        }
        Some(OwnedDatum::Time(value)) => {
            out[0] = 16;
            out[1..9].copy_from_slice(&value.to_le_bytes());
            9
        }
        Some(OwnedDatum::Timetz(time, zone)) => {
            out[0] = 17;
            out[1..9].copy_from_slice(&time.to_le_bytes());
            out[9..13].copy_from_slice(&zone.to_le_bytes());
            13
        }
        Some(OwnedDatum::Interval(value)) => {
            out[0] = 18;
            out[1..5].copy_from_slice(&value.months.to_le_bytes());
            out[5..9].copy_from_slice(&value.days.to_le_bytes());
            out[9..17].copy_from_slice(&value.micros.to_le_bytes());
            17
        }
        Some(OwnedDatum::Uuid(value)) => {
            out[0] = 19;
            out[1..17].copy_from_slice(value);
            17
        }
        Some(OwnedDatum::Text { len, bytes }) => {
            out[0] = 6;
            out[1] = *len;
            out[2..2 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            2 + *len as usize
        }
        Some(OwnedDatum::Numeric {
            sign,
            weight,
            dscale,
            nbytes,
            digits,
        }) => {
            out[0] = 7;
            out[1] = *sign;
            out[2..4].copy_from_slice(&weight.to_le_bytes());
            out[4..6].copy_from_slice(&dscale.to_le_bytes());
            out[6] = *nbytes;
            out[7..7 + *nbytes as usize].copy_from_slice(&digits[..*nbytes as usize]);
            7 + *nbytes as usize
        }
        Some(OwnedDatum::Inet(n)) | Some(OwnedDatum::Cidr(n)) => {
            out[0] = if matches!(d, Some(OwnedDatum::Cidr(_))) {
                9
            } else {
                8
            };
            out[1] = n.family();
            out[2] = n.bits();
            out[3..19].copy_from_slice(n.addr());
            19
        }
        Some(OwnedDatum::Macaddr(b)) => {
            out[0] = 10;
            out[1..7].copy_from_slice(b);
            7
        }
        Some(OwnedDatum::Macaddr8(b)) => {
            out[0] = 11;
            out[1..9].copy_from_slice(b);
            9
        }
        Some(OwnedDatum::Enum {
            slot,
            sort,
            len,
            bytes,
        }) => {
            out[0] = 12;
            out[1..3].copy_from_slice(&slot.to_le_bytes());
            out[3..11].copy_from_slice(&sort.to_le_bytes());
            out[11] = *len;
            out[12..12 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            12 + *len as usize
        }
        Some(OwnedDatum::Json { jsonb, len, bytes }) => {
            out[0] = 20;
            out[1] = u8::from(*jsonb);
            out[2] = *len;
            out[3..3 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            3 + *len as usize
        }
        Some(OwnedDatum::Array {
            element,
            len,
            bytes,
        }) => {
            out[0] = 21;
            out[1] = element.code();
            let (base_code, enum_slot) = match element {
                crate::sql::types::ArrElem::Domain {
                    base_code,
                    enum_slot,
                    ..
                } => (*base_code, *enum_slot),
                _ => (0, crate::sql::types::ColType::ENUM_SLOT_UNRESOLVED),
            };
            out[2] = base_code;
            out[3..5].copy_from_slice(&enum_slot.to_le_bytes());
            out[5] = *len;
            out[6..6 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            6 + *len as usize
        }
        Some(OwnedDatum::Range {
            kind,
            multirange,
            len,
            bytes,
        }) => {
            out[0] = 22;
            out[1] = kind.code();
            out[2] = u8::from(*multirange);
            out[3] = *len;
            out[4..4 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            4 + *len as usize
        }
        Some(OwnedDatum::Bit {
            varying,
            len,
            bytes,
        }) => {
            out[0] = 23;
            out[1] = u8::from(*varying);
            out[2] = *len;
            out[3..3 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            3 + *len as usize
        }
        Some(OwnedDatum::Bytea { len, bytes }) => {
            out[0] = 24;
            out[1] = *len;
            out[2..2 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            2 + *len as usize
        }
    }
}

/// Also used by the manifest codec.
pub(crate) fn decode_default(payload: &[u8], at: &mut usize) -> Option<Option<OwnedDatum>> {
    let tag = *payload.get(*at)?;
    *at += 1;
    Some(match tag {
        0 => None,
        1 => Some(OwnedDatum::Null),
        2 => {
            let b = *payload.get(*at)?;
            *at += 1;
            Some(OwnedDatum::Bool(b != 0))
        }
        3 => {
            let b = payload.get(*at..*at + 4)?;
            *at += 4;
            Some(OwnedDatum::Int4(i32::from_le_bytes(b.try_into().unwrap())))
        }
        4 => {
            let b = payload.get(*at..*at + 8)?;
            *at += 8;
            Some(OwnedDatum::Int8(i64::from_le_bytes(b.try_into().unwrap())))
        }
        5 => {
            let b = payload.get(*at..*at + 8)?;
            *at += 8;
            Some(OwnedDatum::Float8(f64::from_le_bytes(
                b.try_into().unwrap(),
            )))
        }
        6 => {
            let len = *payload.get(*at)? as usize;
            *at += 1;
            if len > crate::storage::MAX_DEFAULT_TEXT {
                return None;
            }
            let raw = payload.get(*at..*at + len)?;
            *at += len;
            core::str::from_utf8(raw).ok()?;
            let mut bytes = [0u8; crate::storage::MAX_DEFAULT_TEXT];
            bytes[..len].copy_from_slice(raw);
            Some(OwnedDatum::Text {
                len: len as u8,
                bytes,
            })
        }
        7 => {
            let sign = *payload.get(*at)?;
            let weight = i16::from_le_bytes(payload.get(*at + 1..*at + 3)?.try_into().unwrap());
            let dscale = u16::from_le_bytes(payload.get(*at + 3..*at + 5)?.try_into().unwrap());
            let nbytes = *payload.get(*at + 5)? as usize;
            *at += 6;
            if nbytes > crate::storage::MAX_DEFAULT_TEXT {
                return None;
            }
            let raw = payload.get(*at..*at + nbytes)?;
            *at += nbytes;
            let mut digits = [0u8; crate::storage::MAX_DEFAULT_TEXT];
            digits[..nbytes].copy_from_slice(raw);
            Some(OwnedDatum::Numeric {
                sign,
                weight,
                dscale,
                nbytes: nbytes as u8,
                digits,
            })
        }
        8 | 9 => {
            let b = payload.get(*at..*at + 18)?;
            *at += 18;
            Some(if tag == 9 {
                OwnedDatum::Cidr(crate::sql::net::NetAddr::new_cidr(
                    b[0],
                    b[1],
                    b[2..18].try_into().unwrap(),
                )?)
            } else {
                OwnedDatum::Inet(crate::sql::net::NetAddr::new(
                    b[0],
                    b[1],
                    b[2..18].try_into().unwrap(),
                )?)
            })
        }
        10 => {
            let b = payload.get(*at..*at + 6)?;
            *at += 6;
            Some(OwnedDatum::Macaddr(b.try_into().unwrap()))
        }
        11 => {
            let b = payload.get(*at..*at + 8)?;
            *at += 8;
            Some(OwnedDatum::Macaddr8(b.try_into().unwrap()))
        }
        12 => {
            let slot = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().unwrap());
            let sort = f64::from_le_bytes(payload.get(*at + 2..*at + 10)?.try_into().unwrap());
            let len = *payload.get(*at + 10)? as usize;
            *at += 11;
            if len > crate::storage::MAX_DEFAULT_TEXT {
                return None;
            }
            let raw = payload.get(*at..*at + len)?;
            *at += len;
            core::str::from_utf8(raw).ok()?;
            let mut bytes = [0u8; crate::storage::MAX_DEFAULT_TEXT];
            bytes[..len].copy_from_slice(raw);
            Some(OwnedDatum::Enum {
                slot,
                sort,
                len: len as u8,
                bytes,
            })
        }
        13 => {
            let bytes = payload.get(*at..*at + 4)?;
            *at += 4;
            Some(OwnedDatum::Date(i32::from_le_bytes(
                bytes.try_into().unwrap(),
            )))
        }
        14..=16 => {
            let bytes = payload.get(*at..*at + 8)?;
            *at += 8;
            let value = i64::from_le_bytes(bytes.try_into().unwrap());
            Some(match tag {
                14 => OwnedDatum::Timestamp(value),
                15 => OwnedDatum::Timestamptz(value),
                _ => OwnedDatum::Time(value),
            })
        }
        17 => {
            let bytes = payload.get(*at..*at + 12)?;
            *at += 12;
            Some(OwnedDatum::Timetz(
                i64::from_le_bytes(bytes[..8].try_into().unwrap()),
                i32::from_le_bytes(bytes[8..].try_into().unwrap()),
            ))
        }
        18 => {
            let bytes = payload.get(*at..*at + 16)?;
            *at += 16;
            Some(OwnedDatum::Interval(crate::sql::types::Interval {
                months: i32::from_le_bytes(bytes[..4].try_into().unwrap()),
                days: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                micros: i64::from_le_bytes(bytes[8..].try_into().unwrap()),
            }))
        }
        19 => {
            let bytes = payload.get(*at..*at + 16)?;
            *at += 16;
            Some(OwnedDatum::Uuid(bytes.try_into().unwrap()))
        }
        20 => {
            let jsonb = *payload.get(*at)? != 0;
            let len = *payload.get(*at + 1)? as usize;
            *at += 2;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::Json {
                jsonb,
                len: len as u8,
                bytes,
            })
        }
        21 => {
            let code = *payload.get(*at)?;
            let base_code = *payload.get(*at + 1)?;
            let enum_slot = u16::from_le_bytes(payload.get(*at + 2..*at + 4)?.try_into().unwrap());
            let len = *payload.get(*at + 4)? as usize;
            *at += 5;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            let mut element = crate::sql::types::ArrElem::from_code(code)?;
            if let crate::sql::types::ArrElem::Domain { slot, .. } = element {
                crate::sql::types::ColType::from_code(base_code)?;
                element = crate::sql::types::ArrElem::Domain {
                    slot,
                    base_code,
                    enum_slot,
                };
            }
            Some(OwnedDatum::Array {
                element,
                len: len as u8,
                bytes,
            })
        }
        22 => {
            let kind = crate::sql::types::RangeKind::from_code(*payload.get(*at)?)?;
            let multirange = *payload.get(*at + 1)? != 0;
            let len = *payload.get(*at + 2)? as usize;
            *at += 3;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::Range {
                kind,
                multirange,
                len: len as u8,
                bytes,
            })
        }
        23 => {
            let varying = *payload.get(*at)? != 0;
            let len = *payload.get(*at + 1)? as usize;
            *at += 2;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::Bit {
                varying,
                len: len as u8,
                bytes,
            })
        }
        24 => {
            let len = *payload.get(*at)? as usize;
            *at += 1;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            Some(OwnedDatum::Bytea {
                len: len as u8,
                bytes,
            })
        }
        _ => return None,
    })
}

fn decode_bounded_default_bytes(
    payload: &[u8],
    at: &mut usize,
    len: usize,
) -> Option<[u8; crate::storage::MAX_DEFAULT_TEXT]> {
    if len > crate::storage::MAX_DEFAULT_TEXT {
        return None;
    }
    let raw = payload.get(*at..*at + len)?;
    *at += len;
    let mut bytes = [0; crate::storage::MAX_DEFAULT_TEXT];
    bytes[..len].copy_from_slice(raw);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_default_codec_size_and_round_trip_agree() {
        let mut text = [0u8; crate::storage::MAX_DEFAULT_TEXT];
        text[..3].copy_from_slice(b"101");
        let defaults = [
            None,
            Some(OwnedDatum::Date(8_767)),
            Some(OwnedDatum::Interval(crate::sql::types::Interval {
                months: 1,
                days: 2,
                micros: 3,
            })),
            Some(OwnedDatum::Uuid([7; 16])),
            Some(OwnedDatum::Json {
                jsonb: true,
                len: 3,
                bytes: text,
            }),
            Some(OwnedDatum::Array {
                element: crate::sql::types::ArrElem::Int4,
                len: 3,
                bytes: text,
            }),
            Some(OwnedDatum::Range {
                kind: crate::sql::types::RangeKind::Int4,
                multirange: false,
                len: 3,
                bytes: text,
            }),
            Some(OwnedDatum::Bit {
                varying: false,
                len: 3,
                bytes: text,
            }),
            Some(OwnedDatum::Bytea {
                len: 3,
                bytes: text,
            }),
        ];
        for default in defaults {
            let mut encoded = [0u8; MAX_DEFAULT_ENCODED];
            let len = encode_default_bytes(&default, &mut encoded);
            assert_eq!(len, encoded_default_len(&default));
            let mut at = 0;
            assert_eq!(decode_default(&encoded[..len], &mut at), Some(default));
            assert_eq!(at, len);
        }
    }

    #[test]
    fn typed_default_decoder_rejects_invalid_tags_and_lengths() {
        let mut oversized_bytea = [0u8; MAX_DEFAULT_ENCODED];
        oversized_bytea[..2].copy_from_slice(&[24, (crate::storage::MAX_DEFAULT_TEXT + 1) as u8]);
        let mut at = 0;
        assert_eq!(decode_default(&oversized_bytea, &mut at), None);

        let mut invalid_range = [0u8; MAX_DEFAULT_ENCODED];
        invalid_range[..4].copy_from_slice(&[22, 42, 0, 0]);
        let mut at = 0;
        assert_eq!(decode_default(&invalid_range[..4], &mut at), None);
    }

    #[test]
    fn stored_query_dependency_columns_round_trip_in_wal() {
        let mut dependencies = StoredQueryDependencies::EMPTY;
        dependencies
            .serialized_push_with_columns(
                DependencyClass::Table,
                SqlName::parse("public").unwrap(),
                SqlName::parse("items").unwrap(),
                SqlName::parse("").unwrap(),
                SqlName::parse("items").unwrap(),
                0b101,
            )
            .unwrap();
        let mut budget = crate::mem::budget::Budget::new(1024);
        let mut encoded = FixedBuf::new(&mut budget, "wal dependency test", 256).unwrap();
        assert!(WalStoredQueryDependencies::Captured(&dependencies).append(&mut encoded));
        assert_eq!(
            WalStoredQueryDependencies::Encoded(encoded.readable())
                .materialize()
                .unwrap(),
            dependencies
        );
    }

    #[test]
    fn operation_size_is_bounded_by_the_table_definition_variant() {
        assert!(
            core::mem::size_of::<WalOp<'static>>() <= core::mem::size_of::<TableDef>() + 64,
            "WalOp grew to {} bytes",
            core::mem::size_of::<WalOp<'static>>()
        );
    }

    #[test]
    fn object_acl_codec_keeps_routine_identity_and_decodes_legacy_records() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "routine acl wal", 1024).unwrap();
        append_record(
            &mut buffer,
            9,
            &WalOp::SetObjectAcl {
                class: crate::storage::AccessClass::Routine as u8,
                object_oid: 16_401,
                schema: "public",
                name: "answer",
                grantee: "PUBLIC",
                grantor: "postgres",
                privileges: crate::storage::PrivilegeSet::EXECUTE,
                grant_options: crate::storage::PrivilegeSet::NONE,
            },
        )
        .unwrap();
        let WalOp::SetObjectAcl { object_oid, .. } =
            decode_record(&buffer.readable()[16..]).unwrap()
        else {
            panic!("expected object ACL WAL operation");
        };
        assert_eq!(object_oid, 16_401);

        let legacy_acl = [
            crate::storage::AccessClass::Table as u8,
            6,
            b'p',
            b'u',
            b'b',
            b'l',
            b'i',
            b'c',
            1,
            b't',
            6,
            b'P',
            b'U',
            b'B',
            b'L',
            b'I',
            b'C',
            8,
            b'p',
            b'o',
            b's',
            b't',
            b'g',
            b'r',
            b'e',
            b's',
            1,
            0,
            0,
            0,
        ];
        let WalOp::SetObjectAcl {
            object_oid,
            schema,
            name,
            ..
        } = decode_op(KIND_SET_OBJECT_ACL, &legacy_acl).unwrap()
        else {
            panic!("expected legacy object ACL WAL operation");
        };
        assert_eq!((object_oid, schema, name), (0, "public", "t"));
    }

    fn test_config(dir: &str) -> Config {
        let mut c = Config::default_dev();
        c.data_dir = dir.to_string();
        c.wal_bytes = 1 << 16;
        c.wal_buffer_bytes = 1 << 12;
        c
    }

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("pos3ql-wal-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir.to_str().unwrap().to_string()
    }

    fn sample_def() -> TableDef {
        let mut def = TableDef {
            name: SqlName::parse("t").unwrap(),
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
            n_columns: 2,
            ..TableDef::empty()
        };
        def.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            type_mod: -1,
            not_null: true,
            unique: true,
            primary: true,
            auto_increment: false,
            default: ColumnDefault::NONE,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type: None,
        };
        def.columns[1] = ColumnMeta {
            name: SqlName::parse("v").unwrap(),
            ctype: ColType::Text,
            type_mod: -1,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default: ColumnDefault::Constant {
                value: OwnedDatum::Int4(7),
                expression: crate::util::StackStr::from_str("7"),
            },
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type: None,
        };
        // A multi-column UNIQUE, a CHECK, and a FOREIGN KEY, so the WAL
        // round-trip covers every constraint kind.
        let mut uk = UniqueKey::EMPTY;
        uk.name = SqlName::parse("t_id_v_key").unwrap();
        uk.columns[0] = 0;
        uk.columns[1] = 1;
        uk.n_cols = 2;
        def.uniques[0] = uk;
        def.n_uniques = 1;
        let mut check = CheckConstraint::EMPTY;
        check.name = SqlName::parse("t_check").unwrap();
        core::fmt::Write::write_str(&mut check.expression, "id > 0").unwrap();
        def.checks[0] = check;
        def.n_checks = 1;
        let mut fk = ForeignKey::EMPTY;
        fk.name = SqlName::parse("t_id_fkey").unwrap();
        fk.columns[0] = 0;
        fk.n_cols = 1;
        fk.parent = SqlName::parse("parent").unwrap();
        fk.parent_cols[0] = 3;
        fk.n_parent_cols = 1;
        fk.on_delete = FkAction::Restrict;
        def.fkeys[0] = fk;
        def.n_fkeys = 1;
        def
    }

    fn collect_replay(wal: &mut Wal) -> Vec<String> {
        collect_replay_from(wal, 0)
    }

    fn collect_replay_from(wal: &mut Wal, floor: u64) -> Vec<String> {
        let mut seen = Vec::new();
        wal.replay(floor, |lsn, record| {
            seen.push(format!("{lsn}:{:?}", decode_record(record).unwrap()));
            Ok(())
        })
        .unwrap();
        seen
    }

    #[test]
    fn roundtrip_all_ops() {
        let dir = temp_dir("roundtrip");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        let sequence_link = crate::storage::SequenceOwner {
            table_schema: crate::storage::SqlName::parse("public").unwrap(),
            table: crate::storage::SqlName::parse("t").unwrap(),
            column: crate::storage::SqlName::parse("id").unwrap(),
        };
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.append_committed(1, &WalOp::CreateTable(sample_def()))
                .unwrap();
            wal.append_committed(
                2,
                &WalOp::Upsert {
                    schema: "public",
                    table: "t",
                    rowid: 1,
                    row: b"ROWBYTES",
                    is_update: false,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.append_committed(
                3,
                &WalOp::Delete {
                    schema: "public",
                    table: "t",
                    rowid: 1,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.append_committed(
                4,
                &WalOp::DropTable {
                    schema: "public",
                    name: "t",
                },
            )
            .unwrap();
            wal.append_committed(
                5,
                &WalOp::CreateSequence {
                    schema: "public",
                    name: "s",
                    data_type: 1,
                    increment: 2,
                    min_value: 1,
                    max_value: 100,
                    start_value: 5,
                    cache: 1,
                    cycle: true,
                    owner: Some(sequence_link),
                    generator_for: Some(sequence_link),
                },
            )
            .unwrap();
            wal.append_committed(
                6,
                &WalOp::SequenceAdvance {
                    schema: "public",
                    name: "s",
                    last: 42,
                    is_called: true,
                },
            )
            .unwrap();
            wal.append_committed(
                7,
                &WalOp::DropSequence {
                    schema: "public",
                    name: "s",
                },
            )
            .unwrap();
            wal.append_committed(
                8,
                &WalOp::Comment {
                    class: 0,
                    schema: "public",
                    name: "t",
                    subid: 2,
                    text: Some("a column comment"),
                },
            )
            .unwrap();
            wal.append_committed(
                9,
                &WalOp::Comment {
                    class: 1,
                    schema: "",
                    name: "s",
                    subid: 0,
                    text: None,
                },
            )
            .unwrap();
            wal.append_committed(
                10,
                &WalOp::CreateReplicationSlot {
                    name: "changes",
                    restart_lsn: 10,
                },
            )
            .unwrap();
            wal.append_committed(
                11,
                &WalOp::AdvanceReplicationSlot {
                    name: "changes",
                    confirmed_flush_lsn: 11,
                },
            )
            .unwrap();
            wal.append_committed(12, &WalOp::DropReplicationSlot { name: "changes" })
                .unwrap();
            let mut publication_tables = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
            publication_tables[0] = 3;
            publication_tables[1] = 7;
            wal.append_committed(
                13,
                &WalOp::CreatePublication {
                    name: "changes",
                    owner: 7,
                    all_tables: false,
                    tables: publication_tables,
                    table_count: 2,
                    schemas: [u8::MAX; crate::storage::MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: true,
                    publish_delete: true,
                    publish_truncate: true,
                },
            )
            .unwrap();
            wal.append_committed(
                14,
                &WalOp::AlterPublication {
                    name: "changes",
                    all_tables: false,
                    tables: publication_tables,
                    table_count: 2,
                    schemas: [u8::MAX; crate::storage::MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: false,
                    publish_delete: true,
                    publish_truncate: false,
                },
            )
            .unwrap();
            wal.append_committed(
                15,
                &WalOp::SetPublicationOwner {
                    name: "changes",
                    owner: 9,
                },
            )
            .unwrap();
            wal.append_committed(
                16,
                &WalOp::RenamePublication {
                    name: "changes",
                    new_name: "renamed_changes",
                },
            )
            .unwrap();
            wal.append_committed(
                17,
                &WalOp::AlterRoutineIdentity {
                    schema: "public",
                    name: "routine",
                    argument_type_codes: &[23],
                    new_schema: "other",
                    new_name: "renamed_routine",
                },
            )
            .unwrap();
            wal.append_committed(
                18,
                &WalOp::AlterDomainIdentity {
                    schema: "public",
                    name: "domain",
                    new_schema: "other",
                    new_name: "renamed_domain",
                },
            )
            .unwrap();
            wal.append_committed(
                19,
                &WalOp::RenameIndex {
                    schema: "public",
                    name: "old_index",
                    new_name: "new_index",
                },
            )
            .unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(seen.len(), 19);
        assert!(seen[0].starts_with("1:CreateTable"));
        // Constraints survive the encode/replay round-trip.
        assert!(seen[0].contains("t_id_v_key"), "unique key: {}", seen[0]);
        assert!(seen[0].contains("t_check"), "check: {}", seen[0]);
        assert!(
            seen[0].contains("t_id_fkey") && seen[0].contains("parent"),
            "fkey: {}",
            seen[0]
        );
        assert!(seen[1].contains("rowid: 1"));
        assert!(seen[3].starts_with("4:DropTable"));
        // Sequence ops survive the encode/replay round-trip.
        assert!(
            seen[4].contains("CreateSequence"),
            "seq create: {}",
            seen[4]
        );
        assert!(
            seen[4].contains("cycle: true") && seen[4].contains("increment: 2"),
            "seq params: {}",
            seen[4]
        );
        assert!(
            seen[5].contains("SequenceAdvance") && seen[5].contains("last: 42"),
            "seq advance: {}",
            seen[5]
        );
        assert!(seen[6].contains("DropSequence"), "seq drop: {}", seen[6]);
        // Comment ops survive the encode/replay round-trip (set and removal).
        assert!(
            seen[7].contains("Comment")
                && seen[7].contains("subid: 2")
                && seen[7].contains("a column comment"),
            "comment set: {}",
            seen[7]
        );
        assert!(
            seen[8].contains("Comment") && seen[8].contains("text: None"),
            "comment removal: {}",
            seen[8]
        );
        assert!(
            seen[9].contains("CreateReplicationSlot") && seen[9].contains("changes"),
            "replication slot: {}",
            seen[9]
        );
        assert!(seen[10].contains("AdvanceReplicationSlot"), "{}", seen[10]);
        assert!(seen[11].contains("DropReplicationSlot"), "{}", seen[11]);
        assert!(
            seen[12].contains("CreatePublication") && seen[12].contains("owner: 7"),
            "publication creation: {}",
            seen[12]
        );
        assert!(
            seen[13].contains("AlterPublication")
                && seen[14].contains("SetPublicationOwner")
                && seen[15].contains("RenamePublication")
                && seen[13].contains("table_count: 2")
                && seen[13].contains("publish_update: false"),
            "publication alter: {}",
            seen[13]
        );
        assert!(
            seen[16].contains("AlterRoutineIdentity") && seen[16].contains("renamed_routine"),
            "routine identity: {}",
            seen[16]
        );
        assert!(
            seen[17].contains("AlterDomainIdentity") && seen[17].contains("renamed_domain"),
            "domain identity: {}",
            seen[17]
        );
        assert!(
            seen[18].contains("RenameIndex") && seen[18].contains("new_index"),
            "index identity: {}",
            seen[18]
        );
        assert_eq!(wal.last_lsn(), 19);
        // Appending continues after the replayed tail.
        wal.append_committed(
            20,
            &WalOp::DropTable {
                schema: "public",
                name: "u",
            },
        )
        .unwrap();
        wal.commit();
    }

    #[test]
    fn table_rewrite_marker_roundtrip_preserves_column_mapping() {
        let dir = temp_dir("table-rewrite-roundtrip");
        let config = test_config(&dir);
        let mut column_mapping = [u16::MAX; MAX_COLUMNS];
        column_mapping[0] = 0;
        column_mapping[1] = 1;
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.append_committed(
                1,
                &WalOp::BeginTableRewrite {
                    previous_schema: "public",
                    previous_name: "t",
                    column_mapping,
                },
            )
            .unwrap();
            wal.commit();
        }
        let mut replay_budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut replay_budget).unwrap();
        let mut seen = false;
        wal.replay(0, |lsn, record| {
            let WalOp::BeginTableRewrite {
                previous_schema,
                previous_name,
                column_mapping,
            } = decode_record(record).unwrap()
            else {
                panic!("expected table rewrite");
            };
            assert_eq!(lsn, 1);
            assert_eq!(previous_schema, "public");
            assert_eq!(previous_name, "t");
            assert_eq!(column_mapping[0], 0);
            assert_eq!(column_mapping[1], 1);
            assert!(column_mapping[2..].iter().all(|column| *column == u16::MAX));
            seen = true;
            Ok(())
        })
        .unwrap();
        assert!(seen);
    }

    #[test]
    fn legacy_index_payload_defaults_to_ascending_nulls_last() {
        let payload = [
            1, b'u', 1, b't', 1, 2, 0, 0, 1, 0, 6, b'p', b'u', b'b', b'l', b'i', b'c',
        ];
        let Some(WalOp::CreateIndex {
            schema,
            name,
            table,
            columns,
            expressions,
            descending,
            nulls_first,
            n_cols,
            include_columns,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            unique,
        }) = decode_op(KIND_CREATE_INDEX, &payload)
        else {
            panic!("legacy CREATE INDEX payload must decode");
        };
        assert_eq!(schema, "public");
        assert_eq!(name, "u");
        assert_eq!(table, "t");
        assert_eq!(&columns[..n_cols], [0, 1]);
        assert!(expressions.iter().all(Option::is_none));
        assert!(unique);
        assert!(!descending[..n_cols].iter().any(|direction| *direction));
        assert!(!nulls_first[..n_cols].iter().any(|placement| *placement));
        assert_eq!(predicate, None);
        assert_eq!(n_include_cols, 0);
        assert!(include_columns.iter().all(|column| *column == 0));
        assert!(!nulls_not_distinct);
    }

    #[test]
    fn partial_index_payload_round_trips_without_name_length_limits() {
        let operation = WalOp::CreateIndex {
            schema: "public",
            name: "active_values",
            table: "rows",
            columns: [1; MAX_INDEX_COLS],
            expressions: [
                Some("lower(value)"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ],
            include_columns: [2; MAX_INDEX_COLS],
            descending: [false; MAX_INDEX_COLS],
            nulls_first: [false; MAX_INDEX_COLS],
            n_cols: 1,
            n_include_cols: 1,
            nulls_not_distinct: true,
            predicate: Some("active AND value IS NOT NULL"),
            unique: true,
        };
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(
            &mut budget,
            "partial index WAL payload",
            encoded_payload_len(&operation),
        )
        .unwrap();
        assert!(append_payload(&mut payload, &operation));
        assert_eq!(payload.len(), encoded_payload_len(&operation));
        let Some(WalOp::CreateIndex {
            predicate,
            include_columns,
            n_include_cols,
            nulls_not_distinct,
            ..
        }) = decode_op(KIND_CREATE_INDEX, payload.readable())
        else {
            panic!("partial index WAL payload must decode");
        };
        assert_eq!(n_include_cols, 1);
        assert_eq!(include_columns[0], 2);
        assert!(nulls_not_distinct);
        assert_eq!(predicate, Some("active AND value IS NOT NULL"));
    }

    #[test]
    fn legacy_domain_payload_without_parent_fields_still_decodes() {
        fn push_name(payload: &mut Vec<u8>, value: &str) {
            payload.push(value.len() as u8);
            payload.extend_from_slice(value.as_bytes());
        }

        let mut payload = Vec::new();
        push_name(&mut payload, "positive");
        push_name(&mut payload, "public");
        payload.push(ColType::Int4.code());
        payload.extend_from_slice(&(-1_i32).to_le_bytes());
        payload.push(1);
        payload.extend_from_slice(&(1_u16).to_le_bytes());
        payload.extend_from_slice(b"7");
        payload.push(1);
        push_name(&mut payload, "positive_check");
        payload.extend_from_slice(&(9_u16).to_le_bytes());
        payload.extend_from_slice(b"VALUE > 0");

        let WalOp::CreateDomain(domain) =
            decode_op(KIND_CREATE_DOMAIN, &payload).expect("legacy domain payload")
        else {
            panic!("decoded the wrong WAL operation");
        };
        assert_eq!(domain.schema.as_str(), "public");
        assert_eq!(domain.name.as_str(), "positive");
        assert_eq!(domain.base, ColType::Int4);
        assert_eq!(domain.base_domain, None);
        assert_eq!(domain.default_expr.expect("domain default").as_str(), "7");
        assert_eq!(domain.checks()[0].expression.as_str(), "VALUE > 0");
    }

    #[test]
    fn domain_payload_rejects_partial_parent_identity() {
        fn push_name(payload: &mut Vec<u8>, value: &str) {
            payload.push(value.len() as u8);
            payload.extend_from_slice(value.as_bytes());
        }

        let mut payload = Vec::new();
        push_name(&mut payload, "child");
        push_name(&mut payload, "public");
        payload.push(DOMAIN_PAYLOAD_WITH_PARENT);
        push_name(&mut payload, "parent");
        push_name(&mut payload, "");
        payload.push(ColType::Int4.code());
        payload.extend_from_slice(&(-1_i32).to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&(0_u16).to_le_bytes());
        payload.push(0);

        assert!(decode_op(KIND_CREATE_DOMAIN, &payload).is_none());
    }

    #[test]
    fn transaction_stages_publish_independently_in_commit_order() {
        let dir = temp_dir("transaction-stages");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.stage(
                11,
                1,
                &WalOp::Delete {
                    schema: "public",
                    table: "late",
                    rowid: 1,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            let savepoint = wal.stage_mark(11);
            wal.stage(
                11,
                2,
                &WalOp::Delete {
                    schema: "public",
                    table: "savepoint_discarded",
                    rowid: 2,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.truncate_stage(11, savepoint);
            wal.stage(
                22,
                3,
                &WalOp::Delete {
                    schema: "public",
                    table: "middle",
                    rowid: 20,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.stage(
                33,
                4,
                &WalOp::Delete {
                    schema: "public",
                    table: "rolled_back",
                    rowid: 30,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();

            assert_eq!(wal.commit_stage(22, 50).unwrap(), 52);
            wal.commit();
            wal.discard_stage(33);
            assert_eq!(wal.commit_stage(11, 52).unwrap(), 54);
            wal.commit();
        }

        let mut replay_budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut replay_budget).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(seen.len(), 4);
        assert!(seen[0].starts_with("51:") && seen[0].contains("middle"));
        assert!(seen[1].starts_with("52:Commit"));
        assert!(seen[2].starts_with("53:") && seen[2].contains("late"));
        assert!(seen[3].starts_with("54:Commit"));
        assert!(!seen.iter().any(|record| {
            record.contains("savepoint_discarded") || record.contains("rolled_back")
        }));
    }

    #[test]
    fn transaction_stage_pool_is_bounded_and_reusable() {
        let dir = temp_dir("transaction-stage-pool");
        let mut config = test_config(&dir);
        config.max_connections = 2;
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        let operation = WalOp::Delete {
            schema: "public",
            table: "t",
            rowid: 1,
            old_row: None,
            command_id: 0,
        };
        wal.stage(1, 1, &operation).unwrap();
        wal.stage(2, 2, &operation).unwrap();
        let error = wal.stage(3, 3, &operation).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        wal.discard_stage(1);
        wal.stage(3, 3, &operation).unwrap();
    }

    #[test]
    fn truncate_round_trips_as_a_statement_event() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "truncate wal", 1024).unwrap();
        let expected_tables = [
            6, b'p', b'u', b'b', b'l', b'i', b'c', 5, b'f', b'i', b'r', b's', b't', 5, b'a', b'u',
            b'd', b'i', b't', 6, b's', b'e', b'c', b'o', b'n', b'd',
        ];
        append_record(
            &mut buffer,
            9,
            &WalOp::Truncate {
                tables: &expected_tables,
                table_count: 2,
                cascade: true,
                restart_identity: true,
                command_id: 17,
            },
        )
        .unwrap();
        let WalOp::Truncate {
            tables,
            table_count,
            cascade,
            restart_identity,
            command_id,
        } = decode_record(&buffer.readable()[16..]).unwrap()
        else {
            panic!("expected truncate WAL operation")
        };
        assert_eq!(table_count, 2);
        assert_eq!(tables, expected_tables);
        assert!(cascade);
        assert!(restart_identity);
        assert_eq!(command_id, 17);
    }

    #[test]
    fn corrupt_record_truncates_tail() {
        let dir = temp_dir("corrupt");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=3 {
                wal.append_committed(
                    lsn,
                    &WalOp::Delete {
                        schema: "public",
                        table: "t",
                        rowid: lsn,
                        old_row: None,
                        command_id: 0,
                    },
                )
                .unwrap();
            }
            wal.commit();
        }
        // Flip one byte in the second record's payload.
        let path = format!("{dir}/journal.wal");
        let mut bytes = std::fs::read(&path).unwrap();
        let record_len = HEADER_LEN
            + encoded_payload_len(&WalOp::Delete {
                schema: "public",
                table: "t",
                rowid: 1,
                old_row: None,
                command_id: 0,
            });
        bytes[record_len + HEADER_LEN] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(
            seen.len(),
            1,
            "only the record before the corruption survives"
        );
    }

    #[test]
    fn replay_floor_skips_checkpointed_records() {
        let dir = temp_dir("floor");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=5 {
                wal.append_committed(
                    lsn,
                    &WalOp::Delete {
                        schema: "public",
                        table: "t",
                        rowid: lsn,
                        old_row: None,
                        command_id: 0,
                    },
                )
                .unwrap();
            }
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay_from(&mut wal, 3);
        assert_eq!(seen.len(), 2, "only records above the floor apply");
        assert!(seen[0].starts_with("4:"));
        assert_eq!(wal.last_lsn(), 5, "scan still tracks the true tail");
    }

    #[test]
    fn reset_after_checkpoint_defuses_stale_tail() {
        let dir = temp_dir("reset");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=10 {
                wal.append_committed(
                    lsn,
                    &WalOp::Delete {
                        schema: "public",
                        table: "t",
                        rowid: lsn,
                        old_row: None,
                        command_id: 0,
                    },
                )
                .unwrap();
            }
            wal.commit();
            // Checkpoint at lsn 10; journal restarts with two tail records.
            wal.reset_after_checkpoint();
            wal.append_committed(
                11,
                &WalOp::Delete {
                    schema: "public",
                    table: "t",
                    rowid: 11,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.append_committed(
                12,
                &WalOp::Delete {
                    schema: "public",
                    table: "t",
                    rowid: 12,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        // The checkpoint says floor = 10; stale records 3..10 still sit in
        // the file beyond the new tail but must not replay.
        let seen = collect_replay_from(&mut wal, 10);
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("11:"));
        assert!(seen[1].starts_with("12:"));
    }

    #[test]
    fn journal_full_is_a_clean_error() {
        let dir = temp_dir("full");
        let mut config = test_config(&dir);
        config.wal_bytes = 256;
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        let mut lsn = 0;
        let err = loop {
            lsn += 1;
            match wal.append_committed(
                lsn,
                &WalOp::Upsert {
                    schema: "public",
                    table: "t",
                    rowid: lsn,
                    row: &[0u8; 32],
                    is_update: false,
                    old_row: None,
                    command_id: 0,
                },
            ) {
                Ok(()) => {}
                Err(e) => break e,
            }
        };
        assert_eq!(err.sqlstate, "53100");
        wal.commit();
    }

    #[test]
    fn oversized_record_is_rejected() {
        let dir = temp_dir("oversized");
        let mut config = test_config(&dir);
        config.wal_buffer_bytes = 128;
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        let big = [0u8; 256];
        let err = wal
            .append_committed(
                1,
                &WalOp::Upsert {
                    schema: "public",
                    table: "t",
                    rowid: 1,
                    row: &big,
                    is_update: false,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap_err();
        assert_eq!(err.sqlstate, "54000");
    }

    #[test]
    fn transaction_staging_does_not_allocate() {
        let dir = temp_dir("noalloc");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for lsn in 1..=16 {
                wal.stage(
                    1,
                    lsn,
                    &WalOp::Delete {
                        schema: "public",
                        table: "t",
                        rowid: lsn,
                        old_row: None,
                        command_id: 0,
                    },
                )
                .unwrap();
            }
            assert_eq!(wal.commit_stage(1, 16).unwrap(), 33);
        });
        wal.commit();
    }

    #[test]
    fn committed_cursor_never_exposes_a_partial_transaction() {
        let dir = temp_dir("committed-cursor");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        wal.stage(
            1,
            1,
            &WalOp::Delete {
                schema: "public",
                table: "t",
                rowid: 1,
                old_row: None,
                command_id: 0,
            },
        )
        .unwrap();
        wal.commit();
        let mut scratch = FixedBuf::new(&mut budget, "cursor scratch", 4096).unwrap();
        assert!(
            wal.next_committed_after(0, &mut scratch, |_, _| Ok(()))
                .unwrap()
                .is_none()
        );

        let commit_lsn = wal.commit_stage(1, 0).unwrap();
        wal.commit();
        let mut seen = [(0u64, 0u8); 2];
        let mut count = 0;
        assert_eq!(
            wal.next_committed_after(0, &mut scratch, |lsn, record| {
                let mut at = 0;
                while at < record.len() {
                    let length =
                        u32::from_le_bytes(record[at + 4..at + 8].try_into().unwrap()) as usize;
                    let total = HEADER_LEN + length;
                    seen[count] = (
                        u64::from_le_bytes(record[at + 8..at + 16].try_into().unwrap()),
                        record[at + 16],
                    );
                    count += 1;
                    at += total;
                }
                assert_eq!(lsn, commit_lsn);
                Ok(())
            })
            .unwrap(),
            Some(commit_lsn)
        );
        assert_eq!(count, 2);
        assert_eq!(seen[0].1, KIND_DELETE);
        assert_eq!(seen[1].1, KIND_COMMIT);
        let mut transaction_id = 0;
        wal.next_committed_after(0, &mut scratch, |_, transaction| {
            let commit = &transaction[transaction.len() - (HEADER_LEN + 4)..];
            if let WalOp::Commit {
                transaction_id: value,
            } = decode_record(&commit[16..]).unwrap()
            {
                transaction_id = value;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(transaction_id, 1);
        assert!(
            wal.next_committed_after(commit_lsn, &mut scratch, |_, _| Ok(()))
                .unwrap()
                .is_none()
        );
    }
}
