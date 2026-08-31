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
use crate::sql::types::{BtreeOperatorClass, ColType};
use crate::sql_err;
use crate::storage::{
    CheckConstraint, ColumnDefault, ColumnMeta, ColumnStatistics, DependencyClass,
    ExtendedStatisticsData, ExtendedStatisticsMcv, FkAction, ForeignKey, MAX_COLUMNS,
    MAX_INDEX_COLS, OwnedDatum, PartitionBound, PartitionBoundValue, PartitionDef,
    PartitionStrategy, RoleAttributes, SerializedStoredQueryDependency, SqlName,
    StoredDependencyIdentity, StoredQueryDependencies, TableDef, TableStatistics, UniqueKey,
};
use crate::util::StackStr;

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
const TABLE_STATISTICS_V3: u8 = u8::MAX - 1;

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
const KIND_CREATE_SUBSCRIPTION: u8 = 50;
const KIND_DROP_SUBSCRIPTION: u8 = 51;
const KIND_ADVANCE_SUBSCRIPTION: u8 = 52;
const KIND_SET_SUBSCRIPTION_ENABLED: u8 = 53;
const KIND_ALTER_SUBSCRIPTION: u8 = 54;
const KIND_CREATE_TRIGGER: u8 = 55;
const KIND_DROP_TRIGGER: u8 = 56;
const KIND_ALTER_TRIGGER: u8 = 57;
const KIND_CREATE_COMPOSITE: u8 = 58;
const KIND_DROP_COMPOSITE: u8 = 59;
const KIND_ALTER_ENUM_IDENTITY: u8 = 60;
const KIND_SET_SUBSCRIPTION_BOOTSTRAP: u8 = 61;
const KIND_RESET_SUBSCRIPTION_RELATIONS: u8 = 62;
const KIND_ADD_SUBSCRIPTION_RELATION: u8 = 63;
const KIND_COMPLETE_SUBSCRIPTION_CLEANUP: u8 = 64;
const KIND_FAIL_SUBSCRIPTION: u8 = 65;
const KIND_SET_SUBSCRIPTION_OWNER: u8 = 66;
const KIND_RENAME_SUBSCRIPTION: u8 = 67;
const KIND_ALTER_REPLICATION_SLOT: u8 = 68;
const KIND_SET_POLICY: u8 = 69;
const KIND_DROP_POLICY: u8 = 70;
const KIND_ALTER_INDEX_DEFINITION: u8 = 71;
const KIND_CREATE_TABLESPACE: u8 = 72;
const KIND_ALTER_TABLESPACE: u8 = 73;
const KIND_DROP_TABLESPACE: u8 = 74;
const KIND_SET_EXTENDED_STATISTICS: u8 = 75;
const KIND_DROP_EXTENDED_STATISTICS: u8 = 76;
const KIND_ANALYZE_EXTENDED_STATISTICS: u8 = 77;
const KIND_UPSERT_EXTENSION: u8 = 78;
const KIND_DROP_EXTENSION: u8 = 79;
const KIND_SET_EXTENSION_DEPENDENCY: u8 = 80;
const KIND_SET_SEQUENCE_SCHEMA: u8 = 81;
const KIND_SET_VIEW_SCHEMA: u8 = 82;
const KIND_SET_EXTENSION_CONFIG: u8 = 83;
const KIND_SET_ROLE_SETTING: u8 = 84;
const KIND_SET_COLUMN_ACL: u8 = 85;
const KIND_SET_CAST: u8 = 86;
const KIND_DROP_CAST: u8 = 87;
const KIND_SET_OPERATOR: u8 = 88;
const KIND_DROP_OPERATOR: u8 = 89;
const KIND_SET_OPERATOR_FAMILY: u8 = 90;
const KIND_DROP_OPERATOR_FAMILY: u8 = 91;
const KIND_SET_OPERATOR_CLASS: u8 = 92;
const KIND_DROP_OPERATOR_CLASS: u8 = 93;
const KIND_CREATE_DATABASE: u8 = 94;
const KIND_ALTER_DATABASE: u8 = 95;
const KIND_DROP_DATABASE: u8 = 96;
const KIND_SET_SYSTEM_SETTING: u8 = 97;
const KIND_DATABASE_SCOPE: u8 = 98;
const KIND_SET_COLLATION: u8 = 99;
const KIND_DROP_COLLATION: u8 = 100;
const KIND_SET_CONVERSION: u8 = 101;
const KIND_DROP_CONVERSION: u8 = 102;
const KIND_SET_EVENT_TRIGGER: u8 = 103;
const KIND_DROP_EVENT_TRIGGER: u8 = 104;
const KIND_SET_RULE: u8 = 105;
const KIND_DROP_RULE: u8 = 106;
const KIND_PREPARE_TRANSACTION: u8 = 107;
const KIND_COMMIT_PREPARED: u8 = 108;
const KIND_ROLLBACK_PREPARED: u8 = 109;
const KIND_PREPARED_LOCKS: u8 = 110;
const KIND_SET_TEXT_SEARCH: u8 = 111;
const KIND_DROP_TEXT_SEARCH: u8 = 112;
const KIND_CREATE_LARGE_OBJECT: u8 = 113;
const KIND_DROP_LARGE_OBJECT: u8 = 114;
const KIND_SET_FOREIGN_DATA_WRAPPER: u8 = 115;
const KIND_SET_FOREIGN_SERVER: u8 = 116;
const KIND_SET_USER_MAPPING: u8 = 117;
const KIND_SET_FOREIGN_TABLE: u8 = 118;
/// A durable transaction boundary. Logical replication may expose only the
/// records preceding one of these markers.
const KIND_COMMIT: u8 = 37;
const KIND_CREATE_REPLICATION_SLOT: u8 = 38;
const KIND_DROP_REPLICATION_SLOT: u8 = 39;
const KIND_ADVANCE_REPLICATION_SLOT: u8 = 40;
const KIND_TRUNCATE: u8 = 41;
const LAST_KIND: u8 = KIND_SET_FOREIGN_TABLE;
const DOMAIN_PAYLOAD_WITH_BASE_SLOT: u8 = u8::MAX;
const NO_DOMAIN_BASE_SLOT: u16 = u16::MAX;

fn access_class_has_oid(class: u8) -> bool {
    class == crate::storage::AccessClass::Routine as u8
        || class == crate::storage::AccessClass::LargeObject as u8
}

/// A domain's direct enum/composite base uses its durable catalog slot during
/// replay. Names are catalog projections and can change after an older domain
/// image was written.
fn domain_base_slot(def: &crate::storage::DomainDef) -> u16 {
    match (def.base_user_type, def.base) {
        (Some(_), crate::sql::types::ColType::Enum(slot))
        | (Some(_), crate::sql::types::ColType::Composite(slot)) => slot,
        (None, _) => NO_DOMAIN_BASE_SLOT,
        (Some(_), _) => NO_DOMAIN_BASE_SLOT,
    }
}

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
}

impl WalStoredQueryDependencies<'_> {
    fn encoded_len(self) -> usize {
        match self {
            Self::Captured(dependencies) => {
                2 + dependencies
                    .entries()
                    .iter()
                    .map(|dependency| {
                        1 + 4
                            + 8
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
        }
    }

    fn append(self, buffer: &mut FixedBuf) -> bool {
        match self {
            Self::Captured(dependencies) => {
                let mut ok = buffer.append(&[0xff, dependencies.entries().len() as u8]);
                for dependency in dependencies.entries() {
                    ok &= buffer.append(&[dependency.class as u8])
                        && buffer.append(&dependency.identity.encoded().to_le_bytes())
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
                    + statistics
                        .columns
                        .iter()
                        .filter(|column| column.valid)
                        .count()
                        * (1 + 4 + 8 + 4 + 4)
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
                let mut ok = buffer.append(&[TABLE_STATISTICS_V3])
                    && buffer.append(&statistics.rows.to_le_bytes())
                    && buffer.append(&statistics.average_row_width.to_le_bytes())
                    && buffer.append(&statistics.analyzed_generation.to_le_bytes())
                    && buffer.append(&[valid_columns as u8]);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalExtendedStatisticsKey<'a> {
    Column(&'a str),
    Expression(&'a str),
}

impl WalExtendedStatisticsKey<'_> {
    pub(crate) const EMPTY: Self = Self::Column("");
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WalExtendedStatisticsData<'a> {
    Captured(&'a ExtendedStatisticsData),
    Encoded(&'a [u8]),
}

impl WalExtendedStatisticsData<'_> {
    fn encoded_len(self) -> usize {
        match self {
            Self::Captured(data) => {
                2 + 8 * 4
                    + data.dependencies_ppm.len() * 4
                    + data.expression_statistics.len() * (1 + 4 + 8 + 4 + 4)
                    + 2
                    + data.mcv[..usize::from(data.n_mcv)]
                        .iter()
                        .map(|entry| 8 + 8 + 2 + entry.values.as_str().len())
                        .sum::<usize>()
            }
            Self::Encoded(bytes) => bytes.len(),
        }
    }

    fn append(self, buffer: &mut FixedBuf) -> bool {
        match self {
            Self::Captured(data) => {
                let mut ok = buffer.append(&[u8::from(data.valid), u8::from(data.inherited)])
                    && buffer.append(&data.analyzed_generation.to_le_bytes())
                    && buffer.append(&data.rows.to_le_bytes())
                    && buffer.append(&data.non_null_rows.to_le_bytes())
                    && buffer.append(&data.distinct_values.to_le_bytes());
                for strength in data.dependencies_ppm {
                    ok &= buffer.append(&strength.to_le_bytes());
                }
                for column in data.expression_statistics {
                    ok &= buffer.append(&[u8::from(column.valid)])
                        && buffer.append(&column.null_fraction_ppm.to_le_bytes())
                        && buffer.append(&column.distinct_values.to_le_bytes())
                        && buffer.append(&column.distinct_fraction_ppm.to_le_bytes())
                        && buffer.append(&column.average_width.to_le_bytes());
                }
                ok &= buffer.append(&data.n_mcv.to_le_bytes());
                for entry in &data.mcv[..usize::from(data.n_mcv)] {
                    ok &= buffer.append(&entry.hash.to_le_bytes())
                        && buffer.append(&entry.count.to_le_bytes())
                        && buffer.append(&(entry.values.as_str().len() as u16).to_le_bytes())
                        && buffer.append(entry.values.as_str().as_bytes());
                }
                ok
            }
            Self::Encoded(bytes) => buffer.append(bytes),
        }
    }

    pub(crate) fn materialize(self) -> Result<ExtendedStatisticsData, SqlError> {
        match self {
            Self::Captured(data) => Ok(*data),
            Self::Encoded(bytes) => decode_extended_statistics_data(bytes).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt extended statistics in journal"
                )
            }),
        }
    }
}

/// A trigger's durable relation class. Recovery must not reinterpret a view
/// trigger as a table trigger based only on a matching schema/name pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerTargetKind {
    Table,
    View,
}

impl TriggerTargetKind {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Table => 0,
            Self::View => 1,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Table),
            1 => Some(Self::View),
            _ => None,
        }
    }
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "TableDef is a fixed inline array by design (no heap); WalOp lives briefly on the stack"
)]
pub(crate) enum WalOp<'a> {
    /// Database identity for following records in a staged transaction.
    DatabaseScope {
        oid: i32,
    },
    CreateLargeObject {
        oid: u32,
        created_at: u64,
        allocated: bool,
    },
    DropLargeObject {
        oid: u32,
    },
    SetForeignDataWrapper {
        slot: u16,
        created_at: u64,
        owner: u16,
        definition: Option<crate::storage::foreign::ForeignDataWrapperDefinition>,
    },
    SetForeignServer {
        slot: u16,
        created_at: u64,
        owner: u16,
        definition: Option<crate::storage::foreign::ForeignServerDefinition>,
    },
    SetUserMapping {
        slot: u16,
        created_at: u64,
        definition: Option<crate::storage::foreign::UserMappingDefinition>,
    },
    SetForeignTable {
        slot: u16,
        created_at: u64,
        definition: Option<crate::storage::foreign::ForeignTableDefinition>,
    },
    CreateTable(TableDef),
    /// Begins ALTER TABLE's in-place definition/row rewrite. The immediately
    /// following CreateTable record supplies the final definition; this marker
    /// carries the old identity and composed column ordinals without inflating
    /// every WAL operation by another inline TableDef.
    BeginTableRewrite {
        previous_schema: &'a str,
        previous_name: &'a str,
        preserve_rows: bool,
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
        security_invoker: bool,
        dependencies: WalStoredQueryDependencies<'a>,
    },
    DropView {
        schema: &'a str,
        name: &'a str,
    },
    SetRule {
        slot: u16,
        created_at: u64,
        target: TriggerTargetKind,
        table_schema: &'a str,
        table: &'a str,
        name: &'a str,
        event: crate::storage::RewriteEvent,
        mode: crate::storage::RewriteMode,
        source: &'a str,
        condition: Option<crate::storage::RuleTextSpan>,
        actions: [crate::storage::RuleTextSpan; crate::storage::MAX_RULE_ACTIONS],
        action_count: u8,
        returning_action: Option<u8>,
        path: &'a str,
        dependencies: WalStoredQueryDependencies<'a>,
    },
    DropRule {
        target: TriggerTargetKind,
        table_schema: &'a str,
        table: &'a str,
        name: &'a str,
    },
    CreatePublication {
        name: &'a str,
        owner: u16,
        all_tables: bool,
        tables: [u16; crate::storage::MAX_PUBLICATION_TABLES],
        table_column_masks: [u64; crate::storage::MAX_PUBLICATION_TABLES],
        table_filter_sql: [StackStr<{ crate::storage::PUBLICATION_FILTER_SQL_MAX }>;
            crate::storage::MAX_PUBLICATION_TABLES],
        table_count: usize,
        schemas: [u8; crate::storage::MAX_SCHEMAS],
        schema_count: usize,
        publish_insert: bool,
        publish_update: bool,
        publish_delete: bool,
        publish_truncate: bool,
        publish_via_partition_root: bool,
        publish_generated_columns: crate::storage::PublishGeneratedColumns,
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
        table_column_masks: [u64; crate::storage::MAX_PUBLICATION_TABLES],
        table_filter_sql: [StackStr<{ crate::storage::PUBLICATION_FILTER_SQL_MAX }>;
            crate::storage::MAX_PUBLICATION_TABLES],
        table_count: usize,
        schemas: [u8; crate::storage::MAX_SCHEMAS],
        schema_count: usize,
        publish_insert: bool,
        publish_update: bool,
        publish_delete: bool,
        publish_truncate: bool,
        publish_via_partition_root: bool,
        publish_generated_columns: crate::storage::PublishGeneratedColumns,
    },
    SetPublicationOwner {
        name: &'a str,
        owner: u16,
    },
    RenamePublication {
        name: &'a str,
        new_name: &'a str,
    },
    CreateSubscription {
        name: &'a str,
        owner: u16,
        connection: &'a str,
        publications: [crate::storage::SqlName; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS],
        publication_count: usize,
        enabled: bool,
        slot: crate::storage::SubscriptionSlot,
        behavior: crate::storage::SubscriptionBehavior,
        bootstrap: crate::storage::SubscriptionBootstrap,
    },
    DropSubscription {
        name: &'a str,
    },
    /// The confirmed publisher position paired with the local transaction
    /// that applied it.  Recovery must replay it after the transaction's row
    /// images, never from a transport-side acknowledgement alone.
    AdvanceSubscription {
        name: &'a str,
        created_at: u64,
        definition_generation: u64,
        confirmed_lsn: u64,
    },
    SetSubscriptionEnabled {
        name: &'a str,
        enabled: bool,
    },
    SetSubscriptionBootstrap {
        name: &'a str,
        bootstrap: crate::storage::SubscriptionBootstrap,
    },
    ResetSubscriptionRelations {
        name: &'a str,
        created_at: u64,
        definition_generation: u64,
    },
    AddSubscriptionRelation {
        name: &'a str,
        created_at: u64,
        definition_generation: u64,
        schema: &'a str,
        table: &'a str,
    },
    CompleteSubscriptionCleanup {
        name: &'a str,
        created_at: u64,
    },
    FailSubscription {
        name: &'a str,
        created_at: u64,
        definition_generation: u64,
        sqlstate: &'a str,
        message: &'a str,
    },
    AlterSubscription {
        name: &'a str,
        connection: &'a str,
        publications: [crate::storage::SqlName; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS],
        publication_count: usize,
        slot: crate::storage::SubscriptionSlot,
        behavior: crate::storage::SubscriptionBehavior,
    },
    SetSubscriptionOwner {
        name: &'a str,
        owner: u16,
    },
    RenameSubscription {
        name: &'a str,
        new_name: &'a str,
    },
    CreateTrigger {
        name: &'a str,
        target: TriggerTargetKind,
        table_schema: &'a str,
        table: &'a str,
        function_schema: &'a str,
        function: &'a str,
        or_replace: bool,
        constraint: bool,
        constraint_timing: u8,
        referenced_schema: Option<&'a str>,
        referenced_table: Option<&'a str>,
        timing: u8,
        level: crate::sql::ast::TriggerLevel,
        events: crate::sql::ast::TriggerEvents,
        update_columns: u64,
        old_table: Option<&'a str>,
        new_table: Option<&'a str>,
        when: Option<&'a str>,
        arguments: [&'a str; crate::storage::MAX_TRIGGER_ARGUMENTS],
        argument_count: usize,
    },
    DropTrigger {
        name: &'a str,
        target: TriggerTargetKind,
        table_schema: &'a str,
        table: &'a str,
    },
    AlterTrigger {
        name: &'a str,
        target: TriggerTargetKind,
        table_schema: &'a str,
        table: &'a str,
        new_name: &'a str,
        enabled: u8,
    },
    SetPolicy {
        schema: &'a str,
        table: &'a str,
        name: &'a str,
        command: u8,
        permissive: bool,
        roles: [SqlName; crate::storage::MAX_POLICY_ROLES],
        role_count: usize,
        using: Option<&'a str>,
        with_check: Option<&'a str>,
        dependencies: WalStoredQueryDependencies<'a>,
    },
    DropPolicy {
        schema: &'a str,
        table: &'a str,
        name: &'a str,
    },
    /// Marks every preceding record in the committed batch as one atomic
    /// transaction. It has no storage replay effect of its own.
    Commit {
        transaction_id: u32,
    },
    /// Terminates a durable batch without making its preceding operations
    /// visible. The batch remains addressable by `gid` until a later typed
    /// resolution record commits or rolls it back.
    PrepareTransaction {
        transaction_id: u32,
        owner: u16,
        database: i32,
        prepared_at: i64,
        gid: &'a str,
    },
    PreparedLocks {
        transaction_id: u32,
        encoded: &'a [u8],
    },
    CommitPrepared {
        gid: &'a str,
    },
    RollbackPrepared {
        gid: &'a str,
    },
    CreateReplicationSlot {
        name: &'a str,
        restart_lsn: u64,
        behavior: crate::storage::ReplicationSlotBehavior,
    },
    AlterReplicationSlot {
        name: &'a str,
        behavior: crate::storage::ReplicationSlotBehavior,
    },
    DropReplicationSlot {
        name: &'a str,
    },
    AdvanceReplicationSlot {
        name: &'a str,
        confirmed_flush_lsn: u64,
    },
    CreateIndex {
        created_at: u64,
        schema: &'a str,
        name: &'a str,
        table: &'a str,
        columns: [u16; MAX_INDEX_COLS],
        /// Canonical source for expression keys; `None` denotes the matching
        /// physical table column in `columns`.
        expressions: [Option<&'a str>; MAX_INDEX_COLS],
        include_columns: [u16; MAX_INDEX_COLS],
        collations: [crate::sql::ast::Collation; MAX_INDEX_COLS],
        explicit_collations: [bool; MAX_INDEX_COLS],
        operator_classes: [Option<crate::storage::IndexOperatorClass>; MAX_INDEX_COLS],
        resolved_operator_classes: [Option<crate::storage::IndexOperatorClass>; MAX_INDEX_COLS],
        descending: [bool; MAX_INDEX_COLS],
        nulls_first: [bool; MAX_INDEX_COLS],
        n_cols: usize,
        n_include_cols: usize,
        nulls_not_distinct: bool,
        /// Absent for a full-table index; otherwise the canonical predicate
        /// source persisted alongside the physical key columns.
        predicate: Option<&'a str>,
        unique: bool,
        definition: crate::storage::IndexMutableDefinition,
    },
    AlterIndexDefinition {
        schema: &'a str,
        name: &'a str,
        definition: crate::storage::IndexMutableDefinition,
    },
    CreateTablespace {
        created_at: u64,
        name: &'a str,
        location: &'a str,
        options: crate::storage::TablespaceOptions,
        owner: u16,
    },
    AlterTablespace {
        name: &'a str,
        new_name: &'a str,
        options: crate::storage::TablespaceOptions,
        owner: u16,
    },
    DropTablespace {
        name: &'a str,
    },
    CreateDatabase {
        oid: i32,
        template_oid: i32,
        definition: crate::storage::DatabaseDefinition,
        owner: u16,
    },
    AlterDatabase {
        oid: i32,
        definition: crate::storage::DatabaseDefinition,
        owner: u16,
    },
    DropDatabase {
        oid: i32,
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
    UpsertExtension {
        name: &'a str,
        schema: &'a str,
        version: &'a str,
        relocatable: bool,
        owner: &'a str,
        created_at: u64,
    },
    DropExtension {
        name: &'a str,
    },
    SetExtensionDependency {
        extension: &'a str,
        class: crate::storage::AccessClass,
        object_oid: i32,
        schema: &'a str,
        name: &'a str,
        kind: crate::storage::ExtensionDependencyKind,
        exists: bool,
    },
    SetExtensionConfig {
        extension: &'a str,
        ordinal: u16,
        relation_kind: crate::storage::ExtensionConfigRelationKind,
        schema: &'a str,
        name: &'a str,
        condition: &'a str,
        exists: bool,
    },
    /// ALTER TABLE ... SET SCHEMA: a definition-only move. Replay moves the
    /// table and its indexes and repoints every inbound foreign key, all
    /// deterministically, so no row images are journaled.
    SetTableSchema {
        schema: &'a str,
        name: &'a str,
        new_schema: &'a str,
    },
    SetSequenceSchema {
        schema: &'a str,
        name: &'a str,
        new_schema: &'a str,
    },
    SetViewSchema {
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
    AlterEnumIdentity {
        schema: &'a str,
        name: &'a str,
        new_schema: &'a str,
        new_name: &'a str,
    },
    /// CREATE TYPE ... AS (...). Each field carries its durable user-type
    /// identity beside the representation code, so recovery never trusts a
    /// catalog slot from a prior process.
    /// A composite definition at its durable catalog identity. The slot is
    /// part of the record: fields and rows refer to it directly.
    CreateComposite {
        slot: u16,
        definition: crate::storage::CompositeDef,
    },
    DropComposite {
        schema: &'a str,
        name: &'a str,
    },
    CreateRoutine {
        definition: crate::storage::RoutineDef,
        dependencies: WalStoredQueryDependencies<'a>,
    },
    SetCast(crate::storage::CastDef),
    DropCast {
        source: crate::storage::RoutineResult,
        target: crate::storage::RoutineResult,
    },
    SetOperator {
        created_at: u64,
        definition: crate::storage::OperatorDefinition,
    },
    DropOperator {
        schema: &'a str,
        name: &'a str,
        signature: crate::storage::OperatorSignature,
    },
    SetOperatorFamily {
        created_at: u64,
        definition: crate::storage::OperatorFamilyDefinition,
    },
    DropOperatorFamily {
        schema: &'a str,
        name: &'a str,
    },
    SetOperatorClass {
        created_at: u64,
        definition: crate::storage::OperatorClassDefinition,
    },
    DropOperatorClass {
        schema: &'a str,
        name: &'a str,
    },
    SetCollation {
        slot: u8,
        created_at: u64,
        definition: crate::storage::CollationDefinition,
    },
    DropCollation {
        schema: &'a str,
        name: &'a str,
    },
    SetConversion {
        slot: u8,
        created_at: u64,
        definition: crate::storage::ConversionDefinition,
    },
    DropConversion {
        schema: &'a str,
        name: &'a str,
    },
    SetTextSearch {
        slot: u8,
        created_at: u64,
        definition: crate::storage::TextSearchDefinition,
    },
    DropTextSearch {
        kind: crate::sql::ast::TextSearchObjectKind,
        schema: &'a str,
        name: &'a str,
    },
    SetEventTrigger {
        slot: u8,
        created_at: u64,
        definition: crate::storage::EventTriggerDefinition,
    },
    DropEventTrigger {
        name: &'a str,
    },
    DropRoutine {
        schema: &'a str,
        name: &'a str,
        argument_signature: &'a [u8],
    },
    AlterRoutineIdentity {
        schema: &'a str,
        name: &'a str,
        argument_signature: &'a [u8],
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
    /// Complete catalog image for CREATE and every ALTER. Names identify the
    /// table during cold replay; keys are already parsed into durable forms.
    SetExtendedStatistics {
        created_at: u64,
        schema: &'a str,
        name: &'a str,
        table_schema: &'a str,
        table: &'a str,
        target: Option<u16>,
        kinds: u8,
        expression_only: bool,
        keys: [WalExtendedStatisticsKey<'a>; crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
        key_count: usize,
    },
    DropExtendedStatistics {
        schema: &'a str,
        name: &'a str,
    },
    AnalyzeExtendedStatistics {
        schema: &'a str,
        name: &'a str,
        statistics: WalExtendedStatisticsData<'a>,
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
    SetRoleSetting {
        role: Option<&'a str>,
        database: Option<i32>,
        name: &'a str,
        value: Option<&'a str>,
    },
    SetSystemSetting {
        name: &'a str,
        value: Option<&'a str>,
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
    SetColumnAcl {
        class: u8,
        schema: &'a str,
        name: &'a str,
        column: u16,
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
    current_database: crate::storage::DatabaseOid,
}

struct TransactionStage {
    transaction_id: u32,
    database: crate::storage::DatabaseOid,
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
                database: crate::storage::DatabaseOid::POSTGRES,
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
            current_database: crate::storage::DatabaseOid::POSTGRES,
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

    /// Replays complete committed or prepared batches from the journal.
    /// Valid records preceding a torn terminal marker are discarded together;
    /// no transaction prefix can become visible after a crash. Positions the
    /// write cursor at the last complete boundary. Startup only.
    pub(crate) fn replay(
        &mut self,
        floor: u64,
        mut apply: impl for<'a> FnMut(u64, &'a [u8]) -> Result<(), SqlError>,
    ) -> Result<(), WalSetupError> {
        self.buffer.clear();
        let mut file_offset = 0u64; // next byte to read from the file
        let mut parsed_offset = 0u64;
        let mut last_seen_lsn = 0u64;
        let mut pending: Vec<(u64, Vec<u8>)> = Vec::new();
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
                    || lsn <= last_seen_lsn
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
                if decode_op(kind, &data[HEADER_LEN..total]).is_none() {
                    break 'outer;
                }
                last_seen_lsn = lsn;
                pending.push((lsn, data[16..total].to_vec()));
                parsed_offset += total as u64;
                self.buffer.consume(total);
                if !matches!(kind, KIND_COMMIT | KIND_PREPARE_TRANSACTION) {
                    continue;
                }

                for (record_lsn, record) in &pending {
                    if *record_lsn > floor {
                        apply(*record_lsn, record).map_err(WalSetupError::Replay)?;
                    }
                }
                self.last_lsn = lsn;
                self.write_offset = parsed_offset;
                pending.clear();
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
            if self.stages[index].database != self.current_database {
                return Err(sql_err!(
                    sqlstate::INVALID_TRANSACTION_TERMINATION,
                    "transaction WAL cannot cross database catalogs"
                ));
            }
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
        self.stages[index].database = self.current_database;
        self.stages[index].buffer.clear();
        Ok(index)
    }

    pub(crate) fn select_database(&mut self, database: crate::storage::DatabaseOid) {
        self.current_database = database;
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

    pub(crate) fn commit_boundary_bytes() -> usize {
        HEADER_LEN + encoded_payload_len(&WalOp::Commit { transaction_id: 0 })
    }

    pub(crate) fn prepare_boundary_bytes(
        metadata: crate::sql::two_phase::PreparedTransactionMetadata,
    ) -> usize {
        HEADER_LEN
            + encoded_payload_len(&WalOp::PrepareTransaction {
                transaction_id: metadata.transaction_id,
                owner: metadata.owner,
                database: metadata.database.get(),
                prepared_at: metadata.prepared_at,
                gid: metadata.gid.as_str(),
            })
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
        let fresh = self.stage_index(transaction_id).is_none();
        let index = self.stage_index_or_claim(transaction_id)?;
        if fresh {
            append_record(
                &mut self.stages[index].buffer,
                provisional_lsn,
                &WalOp::DatabaseScope {
                    oid: self.current_database.get(),
                },
            )?;
        }
        append_record(&mut self.stages[index].buffer, provisional_lsn, operation)
    }

    /// Publishes exactly one transaction's staged records into the durable
    /// batch, assigning monotonically increasing commit-order LSNs. Returns
    /// the last assigned LSN, or `lsn_floor` for a transaction with no WAL.
    pub fn commit_stage(&mut self, transaction_id: u32, lsn_floor: u64) -> Result<u64, SqlError> {
        self.finish_stage(
            transaction_id,
            lsn_floor,
            &WalOp::Commit { transaction_id },
            None,
        )
    }

    pub(crate) fn prepare_stage(
        &mut self,
        metadata: crate::sql::two_phase::PreparedTransactionMetadata,
        lsn_floor: u64,
        records: &mut FixedBuf,
    ) -> Result<u64, SqlError> {
        if self.stage_index(metadata.transaction_id).is_none() {
            let index = self.stage_index_or_claim(metadata.transaction_id)?;
            append_record(
                &mut self.stages[index].buffer,
                lsn_floor,
                &WalOp::DatabaseScope {
                    oid: metadata.database.get(),
                },
            )?;
        }
        self.finish_stage(
            metadata.transaction_id,
            lsn_floor,
            &WalOp::PrepareTransaction {
                transaction_id: metadata.transaction_id,
                owner: metadata.owner,
                database: metadata.database.get(),
                prepared_at: metadata.prepared_at,
                gid: metadata.gid.as_str(),
            },
            Some(records),
        )
    }

    fn finish_stage(
        &mut self,
        transaction_id: u32,
        lsn_floor: u64,
        boundary: &WalOp<'_>,
        mut captured_records: Option<&mut FixedBuf>,
    ) -> Result<u64, SqlError> {
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
        let boundary_bytes = HEADER_LEN + encoded_payload_len(boundary);
        if self.buffer.capacity() - self.buffer.len() < staged_len + boundary_bytes {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction exceeds wal_buffer_bytes ({}); raise it or commit in smaller batches",
                self.buffer.capacity()
            ));
        }
        if self
            .write_offset
            .checked_add(self.buffer.len() as u64)
            .and_then(|used| used.checked_add((staged_len + boundary_bytes) as u64))
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
        if let Some(records) = captured_records.as_mut() {
            records.clear();
            let durable = self.buffer.readable();
            let mut at = durable_mark;
            while at < durable_mark + staged_len {
                let payload_len =
                    u32::from_le_bytes(durable[at + 4..at + 8].try_into().unwrap()) as usize;
                let total = HEADER_LEN + payload_len;
                let raw = &durable[at + 16..at + total];
                let lsn = &durable[at + 8..at + 16];
                if !records.append(lsn)
                    || !records.append(&(raw.len() as u32).to_le_bytes())
                    || !records.append(raw)
                {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "prepared transaction exceeds wal_buffer_bytes ({})",
                        records.capacity()
                    ));
                }
                at += total;
            }
        }
        // A transaction becomes replayable only when its typed terminal marker
        // is present. Recovery retains prepare batches and applies commit
        // batches; a torn prefix is neither.
        append_record(&mut self.buffer, final_lsn, boundary)?;
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
        WalOp::DatabaseScope { .. } => KIND_DATABASE_SCOPE,
        WalOp::CreateLargeObject { .. } => KIND_CREATE_LARGE_OBJECT,
        WalOp::DropLargeObject { .. } => KIND_DROP_LARGE_OBJECT,
        WalOp::SetForeignDataWrapper { .. } => KIND_SET_FOREIGN_DATA_WRAPPER,
        WalOp::SetForeignServer { .. } => KIND_SET_FOREIGN_SERVER,
        WalOp::SetUserMapping { .. } => KIND_SET_USER_MAPPING,
        WalOp::SetForeignTable { .. } => KIND_SET_FOREIGN_TABLE,
        WalOp::CreateTable(_) => KIND_CREATE,
        WalOp::DropTable { .. } => KIND_DROP,
        WalOp::Upsert { .. } => KIND_UPSERT,
        WalOp::Delete { .. } => KIND_DELETE,
        WalOp::Truncate { .. } => KIND_TRUNCATE,
        WalOp::CreateView { .. } => KIND_CREATE_VIEW,
        WalOp::DropView { .. } => KIND_DROP_VIEW,
        WalOp::SetRule { .. } => KIND_SET_RULE,
        WalOp::DropRule { .. } => KIND_DROP_RULE,
        WalOp::CreatePublication { .. } => KIND_CREATE_PUBLICATION,
        WalOp::DropPublication { .. } => KIND_DROP_PUBLICATION,
        WalOp::AlterPublication { .. } => KIND_ALTER_PUBLICATION,
        WalOp::SetPublicationOwner { .. } => KIND_SET_PUBLICATION_OWNER,
        WalOp::RenamePublication { .. } => KIND_RENAME_PUBLICATION,
        WalOp::CreateSubscription { .. } => KIND_CREATE_SUBSCRIPTION,
        WalOp::DropSubscription { .. } => KIND_DROP_SUBSCRIPTION,
        WalOp::AdvanceSubscription { .. } => KIND_ADVANCE_SUBSCRIPTION,
        WalOp::SetSubscriptionEnabled { .. } => KIND_SET_SUBSCRIPTION_ENABLED,
        WalOp::SetSubscriptionBootstrap { .. } => KIND_SET_SUBSCRIPTION_BOOTSTRAP,
        WalOp::ResetSubscriptionRelations { .. } => KIND_RESET_SUBSCRIPTION_RELATIONS,
        WalOp::AddSubscriptionRelation { .. } => KIND_ADD_SUBSCRIPTION_RELATION,
        WalOp::CompleteSubscriptionCleanup { .. } => KIND_COMPLETE_SUBSCRIPTION_CLEANUP,
        WalOp::FailSubscription { .. } => KIND_FAIL_SUBSCRIPTION,
        WalOp::AlterSubscription { .. } => KIND_ALTER_SUBSCRIPTION,
        WalOp::SetSubscriptionOwner { .. } => KIND_SET_SUBSCRIPTION_OWNER,
        WalOp::RenameSubscription { .. } => KIND_RENAME_SUBSCRIPTION,
        WalOp::AlterReplicationSlot { .. } => KIND_ALTER_REPLICATION_SLOT,
        WalOp::CreateTrigger { .. } => KIND_CREATE_TRIGGER,
        WalOp::DropTrigger { .. } => KIND_DROP_TRIGGER,
        WalOp::AlterTrigger { .. } => KIND_ALTER_TRIGGER,
        WalOp::SetPolicy { .. } => KIND_SET_POLICY,
        WalOp::DropPolicy { .. } => KIND_DROP_POLICY,
        WalOp::Commit { .. } => KIND_COMMIT,
        WalOp::PrepareTransaction { .. } => KIND_PREPARE_TRANSACTION,
        WalOp::PreparedLocks { .. } => KIND_PREPARED_LOCKS,
        WalOp::CommitPrepared { .. } => KIND_COMMIT_PREPARED,
        WalOp::RollbackPrepared { .. } => KIND_ROLLBACK_PREPARED,
        WalOp::CreateReplicationSlot { .. } => KIND_CREATE_REPLICATION_SLOT,
        WalOp::DropReplicationSlot { .. } => KIND_DROP_REPLICATION_SLOT,
        WalOp::AdvanceReplicationSlot { .. } => KIND_ADVANCE_REPLICATION_SLOT,
        WalOp::CreateIndex { .. } => KIND_CREATE_INDEX,
        WalOp::AlterIndexDefinition { .. } => KIND_ALTER_INDEX_DEFINITION,
        WalOp::CreateTablespace { .. } => KIND_CREATE_TABLESPACE,
        WalOp::AlterTablespace { .. } => KIND_ALTER_TABLESPACE,
        WalOp::DropTablespace { .. } => KIND_DROP_TABLESPACE,
        WalOp::CreateDatabase { .. } => KIND_CREATE_DATABASE,
        WalOp::AlterDatabase { .. } => KIND_ALTER_DATABASE,
        WalOp::DropDatabase { .. } => KIND_DROP_DATABASE,
        WalOp::DropIndex { .. } => KIND_DROP_INDEX,
        WalOp::RenameIndex { .. } => KIND_RENAME_INDEX,
        WalOp::SequenceSet { .. } => KIND_SEQUENCE_SET,
        WalOp::CreateSchema(_) => KIND_CREATE_SCHEMA,
        WalOp::DropSchema(_) => KIND_DROP_SCHEMA,
        WalOp::UpsertExtension { .. } => KIND_UPSERT_EXTENSION,
        WalOp::DropExtension { .. } => KIND_DROP_EXTENSION,
        WalOp::SetExtensionDependency { .. } => KIND_SET_EXTENSION_DEPENDENCY,
        WalOp::SetTableSchema { .. } => KIND_SET_TABLE_SCHEMA,
        WalOp::SetSequenceSchema { .. } => KIND_SET_SEQUENCE_SCHEMA,
        WalOp::SetViewSchema { .. } => KIND_SET_VIEW_SCHEMA,
        WalOp::SetExtensionConfig { .. } => KIND_SET_EXTENSION_CONFIG,
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
        WalOp::AlterEnumIdentity { .. } => KIND_ALTER_ENUM_IDENTITY,
        WalOp::CreateComposite { .. } => KIND_CREATE_COMPOSITE,
        WalOp::DropComposite { .. } => KIND_DROP_COMPOSITE,
        WalOp::CreateRoutine { .. } => KIND_CREATE_ROUTINE,
        WalOp::SetCast(_) => KIND_SET_CAST,
        WalOp::DropCast { .. } => KIND_DROP_CAST,
        WalOp::SetOperator { .. } => KIND_SET_OPERATOR,
        WalOp::DropOperator { .. } => KIND_DROP_OPERATOR,
        WalOp::SetOperatorFamily { .. } => KIND_SET_OPERATOR_FAMILY,
        WalOp::DropOperatorFamily { .. } => KIND_DROP_OPERATOR_FAMILY,
        WalOp::SetOperatorClass { .. } => KIND_SET_OPERATOR_CLASS,
        WalOp::DropOperatorClass { .. } => KIND_DROP_OPERATOR_CLASS,
        WalOp::SetCollation { .. } => KIND_SET_COLLATION,
        WalOp::DropCollation { .. } => KIND_DROP_COLLATION,
        WalOp::SetConversion { .. } => KIND_SET_CONVERSION,
        WalOp::DropConversion { .. } => KIND_DROP_CONVERSION,
        WalOp::SetTextSearch { .. } => KIND_SET_TEXT_SEARCH,
        WalOp::DropTextSearch { .. } => KIND_DROP_TEXT_SEARCH,
        WalOp::SetEventTrigger { .. } => KIND_SET_EVENT_TRIGGER,
        WalOp::DropEventTrigger { .. } => KIND_DROP_EVENT_TRIGGER,
        WalOp::DropRoutine { .. } => KIND_DROP_ROUTINE,
        WalOp::AlterRoutineIdentity { .. } => KIND_ALTER_ROUTINE_IDENTITY,
        WalOp::AlterDomainIdentity { .. } => KIND_ALTER_DOMAIN_IDENTITY,
        WalOp::Analyze { .. } => KIND_ANALYZE,
        WalOp::SetExtendedStatistics { .. } => KIND_SET_EXTENDED_STATISTICS,
        WalOp::DropExtendedStatistics { .. } => KIND_DROP_EXTENDED_STATISTICS,
        WalOp::AnalyzeExtendedStatistics { .. } => KIND_ANALYZE_EXTENDED_STATISTICS,
        WalOp::UpsertRole { .. } => KIND_UPSERT_ROLE,
        WalOp::DropRole { .. } => KIND_DROP_ROLE,
        WalOp::UpsertRoleMembership { .. } => KIND_UPSERT_ROLE_MEMBERSHIP,
        WalOp::DropRoleMembership { .. } => KIND_DROP_ROLE_MEMBERSHIP,
        WalOp::SetRoleSetting { .. } => KIND_SET_ROLE_SETTING,
        WalOp::SetSystemSetting { .. } => KIND_SET_SYSTEM_SETTING,
        WalOp::SetObjectOwner { .. } => KIND_SET_OBJECT_OWNER,
        WalOp::SetObjectAcl { .. } => KIND_SET_OBJECT_ACL,
        WalOp::SetColumnAcl { .. } => KIND_SET_COLUMN_ACL,
        WalOp::BeginTableRewrite { .. } => KIND_REWRITE_TABLE,
        WalOp::SetDefaultAcl { .. } => KIND_SET_DEFAULT_ACL,
    }
}

fn encoded_payload_len(operation: &WalOp) -> usize {
    fn foreign_options_len(options: crate::storage::foreign::ForeignOptions) -> usize {
        1 + options
            .entries()
            .iter()
            .map(|option| 1 + option.name.as_str().len() + 2 + option.value.as_str().len())
            .sum::<usize>()
    }
    fn optional_foreign_value_len(
        value: Option<crate::util::StackStr<{ crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX }>>,
    ) -> usize {
        1 + value.map_or(0, |value| 2 + value.as_str().len())
    }
    fn routine_result_len(result: crate::storage::RoutineResult) -> usize {
        2 + result.user_type.map_or(0, |identity| {
            1 + identity.schema.as_str().len() + 1 + identity.name.as_str().len()
        })
    }
    fn operator_signature_len(signature: crate::storage::OperatorSignature) -> usize {
        1 + signature.left.map_or(0, routine_result_len)
            + signature.right.map_or(0, routine_result_len)
    }
    fn text_search_definition_len(definition: crate::storage::TextSearchDefinition) -> usize {
        let common =
            1 + definition.schema().as_str().len() + 1 + definition.name().as_str().len() + 4;
        common
            + match definition {
                crate::storage::TextSearchDefinition::Parser { .. } => 20,
                crate::storage::TextSearchDefinition::Template { .. } => 10,
                crate::storage::TextSearchDefinition::Dictionary { options, .. } => {
                    8 + 1 + options.as_str().len() + 2
                }
                crate::storage::TextSearchDefinition::Configuration { mappings, .. } => {
                    8 + mappings
                        .counts
                        .iter()
                        .map(|count| 1 + usize::from(*count) * 4)
                        .sum::<usize>()
                }
            }
    }
    match operation {
        WalOp::DatabaseScope { .. } => 4,
        WalOp::CreateLargeObject { .. } => 13,
        WalOp::DropLargeObject { .. } => 4,
        WalOp::SetForeignDataWrapper { definition, .. } => {
            13 + definition.map_or(0, |definition| {
                1 + definition.name.as_str().len() + 2 + foreign_options_len(definition.options)
            })
        }
        WalOp::SetForeignServer { definition, .. } => {
            13 + definition.map_or(0, |definition| {
                1 + definition.name.as_str().len()
                    + 2
                    + optional_foreign_value_len(definition.server_type)
                    + optional_foreign_value_len(definition.version)
                    + foreign_options_len(definition.options)
            })
        }
        WalOp::SetUserMapping { definition, .. } => {
            11 + definition.map_or(0, |definition| {
                2 + 3 + foreign_options_len(definition.options)
            })
        }
        WalOp::SetForeignTable { definition, .. } => {
            11 + definition.map_or(0, |definition| {
                4 + foreign_options_len(definition.options)
                    + 1
                    + definition.column_options.entries().len() * 3
                    + definition
                        .column_options
                        .entries()
                        .iter()
                        .map(|column| {
                            1 + column.option.name.as_str().len()
                                + 2
                                + column.option.value.as_str().len()
                        })
                        .sum::<usize>()
            })
        }
        WalOp::CreateTable(def) => {
            let mut n = 1 + def.name.as_str().len() + 2 + 2 + 3;
            for c in def.columns() {
                let default_value = c.default.constant().copied();
                n += 1 + c.name.as_str().len() + 3 + 4 + encoded_default_len(&default_value);
                // Non-constant DEFAULT text: 2-byte length prefix + bytes.
                n += 2 + c
                    .default
                    .expression()
                    .map(|e| e.as_str().len())
                    .unwrap_or(0);
                // auto_increment_step (i64).
                n += 8;
                n += 2; // attstattarget
                // User-defined column: name, then a format marker and schema.
                if let Some(identity) = c.user_type {
                    n += 1 + identity.name.as_str().len();
                    n += 2 + identity.schema.as_str().len();
                }
            }
            // uniques
            n += 1;
            for uk in def.uniques() {
                n += 1 + uk.name.as_str().len() + 3 + uk.n_cols * 2;
            }
            // checks
            n += 1;
            for check in def.checks() {
                n += 1 + check.name.as_str().len() + 3 + check.expression.as_str().len();
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
                    + 4;
            }
            // exclusion constraints
            n += 1;
            for exclusion in def.exclusions() {
                n += 1
                    + exclusion.name.as_str().len()
                    + 2
                    + exclusion.n_cols * 3
                    + 2
                    + exclusion
                        .predicate
                        .as_ref()
                        .map_or(0, |predicate| predicate.as_str().len());
            }
            n += 1 + def.schema.as_str().len();
            for fk in def.fkeys() {
                n += 1 + fk.parent_schema.as_str().len();
            }
            n += def.n_columns;
            n += encoded_partition_len(def.partition);
            n += 3;
            n
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            ..
        } => 1 + previous_schema.len() + 1 + previous_name.len() + 1 + MAX_COLUMNS * 2,
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
            security_invoker: _,
            dependencies,
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
        WalOp::DropView { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::SetRule {
            table_schema,
            table,
            name,
            source,
            action_count,
            path,
            dependencies,
            ..
        } => {
            2 + 8
                + 1
                + 1
                + table_schema.len()
                + 1
                + table.len()
                + 1
                + name.len()
                + 2
                + 2
                + source.len()
                + 4
                + 1
                + usize::from(*action_count) * 4
                + 1
                + 2
                + path.len()
                + dependencies.encoded_len()
        }
        WalOp::DropRule {
            table_schema,
            table,
            name,
            ..
        } => 1 + 1 + table_schema.len() + 1 + table.len() + 1 + name.len(),
        WalOp::CreatePublication {
            name,
            table_count,
            schema_count,
            table_filter_sql,
            ..
        } => {
            1 + name.len()
                + 2
                + 1
                + 1
                + 1
                + table_count * 10
                + schema_count
                + table_filter_sql[..*table_count]
                    .iter()
                    .map(|filter| 2 + filter.as_str().len())
                    .sum::<usize>()
        }
        WalOp::DropPublication { name } => 1 + name.len(),
        WalOp::AlterPublication {
            name,
            table_count,
            schema_count,
            table_filter_sql,
            ..
        } => {
            1 + name.len()
                + 1
                + 1
                + 1
                + table_count * 10
                + schema_count
                + table_filter_sql[..*table_count]
                    .iter()
                    .map(|filter| 2 + filter.as_str().len())
                    .sum::<usize>()
        }
        WalOp::SetPublicationOwner { name, .. } => 1 + name.len() + 2,
        WalOp::RenamePublication { name, new_name } => 1 + name.len() + 1 + new_name.len(),
        WalOp::CreateSubscription {
            name,
            connection,
            publications,
            publication_count,
            slot,
            ..
        } => {
            1 + name.len()
                + 2
                + 2
                + connection.len()
                + 1
                + publications[..*publication_count]
                    .iter()
                    .map(|name| 1 + name.as_str().len())
                    .sum::<usize>()
                + 1
                + 1
                + 18
                + match slot {
                    crate::storage::SubscriptionSlot::Absent => 1,
                    crate::storage::SubscriptionSlot::External(name)
                    | crate::storage::SubscriptionSlot::Managed(name) => 2 + name.as_str().len(),
                }
        }
        WalOp::DropSubscription { name } => 1 + name.len(),
        WalOp::AdvanceSubscription { name, .. } => 1 + name.len() + 8 + 8 + 8,
        WalOp::SetSubscriptionEnabled { name, .. } => 1 + name.len() + 1,
        WalOp::SetSubscriptionBootstrap { name, .. } => 1 + name.len() + 1,
        WalOp::ResetSubscriptionRelations { name, .. } => 1 + name.len() + 8 + 8,
        WalOp::AddSubscriptionRelation {
            name,
            schema,
            table,
            ..
        } => 1 + name.len() + 8 + 8 + 1 + schema.len() + 1 + table.len(),
        WalOp::CompleteSubscriptionCleanup { name, .. } => 1 + name.len() + 8,
        WalOp::FailSubscription { name, message, .. } => {
            1 + name.len() + 8 + 8 + 5 + 1 + message.len()
        }
        WalOp::AlterSubscription {
            name,
            connection,
            publications,
            publication_count,
            slot,
            ..
        } => {
            1 + name.len()
                + 2
                + connection.len()
                + 1
                + publications[..*publication_count]
                    .iter()
                    .map(|publication| 1 + publication.as_str().len())
                    .sum::<usize>()
                + 18
                + match slot {
                    crate::storage::SubscriptionSlot::Absent => 1,
                    crate::storage::SubscriptionSlot::External(name)
                    | crate::storage::SubscriptionSlot::Managed(name) => 2 + name.as_str().len(),
                }
        }
        WalOp::SetSubscriptionOwner { name, .. } => 1 + name.len() + 2,
        WalOp::RenameSubscription { name, new_name } => 1 + name.len() + 1 + new_name.len(),
        WalOp::CreateTrigger {
            name,
            target: _,
            table_schema,
            table,
            function_schema,
            function,
            referenced_schema,
            referenced_table,
            old_table,
            new_table,
            when,
            arguments,
            argument_count,
            ..
        } => {
            1 + name.len()
                + 1
                + 1
                + table_schema.len()
                + 1
                + table.len()
                + 1
                + function_schema.len()
                + 1
                + function.len()
                + 4
                + referenced_schema.map_or(0, str::len)
                + referenced_table.map_or(0, str::len)
                + 16
                + old_table.map_or(0, str::len)
                + new_table.map_or(0, str::len)
                + when.map_or(0, str::len)
                + 1
                + arguments[..*argument_count]
                    .iter()
                    .map(|argument| 1 + argument.len())
                    .sum::<usize>()
        }
        WalOp::DropTrigger {
            name,
            target: _,
            table_schema,
            table,
        } => 1 + name.len() + 1 + 1 + table_schema.len() + 1 + table.len(),
        WalOp::AlterTrigger {
            name,
            target: _,
            table_schema,
            table,
            new_name,
            ..
        } => 1 + name.len() + 1 + 1 + table_schema.len() + 1 + table.len() + 1 + new_name.len() + 1,
        WalOp::SetPolicy {
            schema,
            table,
            name,
            roles,
            role_count,
            using,
            with_check,
            dependencies,
            ..
        } => {
            1 + schema.len()
                + 1
                + table.len()
                + 1
                + name.len()
                + 3
                + roles[..*role_count]
                    .iter()
                    .map(|role| 1 + role.as_str().len())
                    .sum::<usize>()
                + 2
                + using.map_or(0, str::len)
                + 2
                + with_check.map_or(0, str::len)
                + dependencies.encoded_len()
        }
        WalOp::DropPolicy {
            schema,
            table,
            name,
        } => 1 + schema.len() + 1 + table.len() + 1 + name.len(),
        WalOp::Commit { .. } => 4,
        WalOp::PrepareTransaction { gid, .. } => 4 + 2 + 4 + 8 + 1 + gid.len(),
        WalOp::PreparedLocks { encoded, .. } => 4 + 4 + encoded.len(),
        WalOp::CommitPrepared { gid } | WalOp::RollbackPrepared { gid } => 1 + gid.len(),
        WalOp::CreateReplicationSlot { name, .. } => 1 + name.len() + 8 + 1,
        WalOp::AlterReplicationSlot { name, .. } => 1 + name.len() + 1,
        WalOp::DropReplicationSlot { name } => 1 + name.len(),
        WalOp::AdvanceReplicationSlot { name, .. } => 1 + name.len() + 8,
        WalOp::CreateIndex {
            created_at: _,
            schema,
            name,
            table,
            n_cols,
            predicate,
            n_include_cols,
            expressions,
            ..
        } => {
            8 + 1
                + name.len()
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
                + n_cols * 2
                + n_cols * 5
                + 1
                + n_cols * 5
                + 3
                + 2
                + 1
                + 1
                + MAX_INDEX_COLS * 2
                + 2
                + 1
                + 1
                + 1
        }
        WalOp::AlterIndexDefinition { schema, name, .. } => {
            1 + schema.len() + 1 + name.len() + 2 + 1 + 1 + MAX_INDEX_COLS * 2 + 2 + 3
        }
        WalOp::CreateTablespace { name, location, .. } => {
            8 + 1 + name.len() + 2 + location.len() + 24 + 2
        }
        WalOp::AlterTablespace { name, new_name, .. } => {
            1 + name.len() + 1 + new_name.len() + 24 + 2
        }
        WalOp::DropTablespace { name } => 1 + name.len(),
        WalOp::CreateDatabase { definition, .. } => 10 + database_definition_len(*definition),
        WalOp::AlterDatabase { definition, .. } => 6 + database_definition_len(*definition),
        WalOp::DropDatabase { .. } => 4,
        WalOp::DropIndex { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::RenameIndex {
            schema,
            name,
            new_name,
        } => 1 + schema.len() + 1 + name.len() + 1 + new_name.len(),
        WalOp::SequenceSet { schema, table, .. } => 1 + table.len() + 2 + 8 + 1 + schema.len(),
        WalOp::CreateSchema(name) | WalOp::DropSchema(name) => 1 + name.len(),
        WalOp::UpsertExtension {
            name,
            schema,
            version,
            owner,
            ..
        } => 8 + 1 + name.len() + 1 + schema.len() + 1 + version.len() + 1 + owner.len() + 1,
        WalOp::DropExtension { name } => 1 + name.len(),
        WalOp::SetExtensionDependency {
            extension,
            class,
            schema,
            name,
            ..
        } => {
            1 + extension.len()
                + 1
                + usize::from(*class == crate::storage::AccessClass::Routine) * 4
                + 1
                + schema.len()
                + 1
                + name.len()
                + 2
        }
        WalOp::SetExtensionConfig {
            extension,
            schema,
            name,
            condition,
            ..
        } => {
            1 + extension.len()
                + 2
                + 1
                + 1
                + schema.len()
                + 1
                + name.len()
                + 2
                + condition.len()
                + 1
        }
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        }
        | WalOp::SetSequenceSchema {
            schema,
            name,
            new_schema,
        }
        | WalOp::SetViewSchema {
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
            let base_name = def
                .base_user_type
                .map(|d| d.name.as_str().len())
                .unwrap_or(0);
            let base_schema = def
                .base_user_type
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
                + base_name
                + 1
                + base_schema
                + 1
                + 2
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
        WalOp::AlterEnumIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => 1 + schema.len() + 1 + name.len() + 1 + new_schema.len() + 1 + new_name.len(),
        WalOp::CreateComposite {
            definition: def, ..
        } => {
            let mut n = 2 + 1 + def.name.as_str().len() + 1 + def.schema.as_str().len() + 1;
            for field in def.fields() {
                n += 1 + field.name.as_str().len() + 2 + 1 + 1 + 1 + 4 + 1 + 1;
                if let Some(identity) = field.user_type {
                    n += 1 + identity.schema.as_str().len() + 1 + identity.name.as_str().len();
                }
            }
            n
        }
        WalOp::DropComposite { schema, name } => 1 + name.len() + 1 + schema.len(),
        WalOp::CreateRoutine {
            definition: def,
            dependencies,
        } => {
            8 + 2
                + 3
                + 4
                + def.attributes.cost_bits.map_or(1, |_| 9)
                + def.attributes.rows_bits.map_or(1, |_| 9)
                + 1
                + def
                    .configs()
                    .iter()
                    .map(|config| 1 + config.name.as_str().len() + 2 + config.value.as_str().len())
                    .sum::<usize>()
                + 1
                + def.name.as_str().len()
                + 1
                + def.schema.as_str().len()
                + 1
                + def
                    .arguments()
                    .iter()
                    .map(|argument| {
                        1 + argument.name.as_str().len()
                            + 1
                            + 1
                            + argument.user_type.map_or(0, |identity| {
                                1 + identity.schema.as_str().len()
                                    + 1
                                    + identity.name.as_str().len()
                            })
                    })
                    .sum::<usize>()
                + 1
                + def
                    .parameters()
                    .iter()
                    .map(|parameter| {
                        1 + parameter.name.as_str().len()
                            + 1
                            + 1
                            + parameter.user_type.map_or(0, |identity| {
                                1 + identity.schema.as_str().len()
                                    + 1
                                    + identity.name.as_str().len()
                            })
                            + 1
                            + 2
                            + parameter
                                .mode
                                .default()
                                .map_or(0, |default| default.as_str().len())
                    })
                    .sum::<usize>()
                + 1
                + 1
                + match def.kind {
                    crate::storage::RoutineKind::Function { result }
                    | crate::storage::RoutineKind::SetFunction { result } => {
                        result.user_type.map_or(0, |identity| {
                            1 + identity.schema.as_str().len() + 1 + identity.name.as_str().len()
                        })
                    }
                    crate::storage::RoutineKind::Aggregate(aggregate) => {
                        aggregate.result_type.user_type.map_or(0, |identity| {
                            1 + identity.schema.as_str().len() + 1 + identity.name.as_str().len()
                        })
                    }
                    _ => 0,
                }
                + 2
                + match def.kind {
                    crate::storage::RoutineKind::Aggregate(aggregate) => {
                        aggregate.encode_wire().as_str().len()
                    }
                    _ => def.body.as_str().len(),
                }
                + 1
                + match def.kind {
                    crate::storage::RoutineKind::TableFunction
                    | crate::storage::RoutineKind::RecordFunction { .. } => {
                        1 + def.result_columns[..def.result_column_count]
                            .iter()
                            .map(|column| {
                                1 + column.name.as_str().len()
                                    + 1
                                    + 1
                                    + column.user_type.map_or(0, |identity| {
                                        1 + identity.schema.as_str().len()
                                            + 1
                                            + identity.name.as_str().len()
                                    })
                            })
                            .sum::<usize>()
                    }
                    crate::storage::RoutineKind::Function { .. }
                    | crate::storage::RoutineKind::SetFunction { .. }
                    | crate::storage::RoutineKind::Trigger
                    | crate::storage::RoutineKind::EventTrigger
                    | crate::storage::RoutineKind::Procedure
                    | crate::storage::RoutineKind::Aggregate(_) => 0,
                }
                + 1
                + def.creation_path.as_str().len()
                + dependencies.encoded_len()
        }
        WalOp::SetCast(definition) => {
            8 + routine_result_len(definition.source)
                + routine_result_len(definition.target)
                + 1
                + usize::from(matches!(
                    definition.method,
                    crate::storage::CastMethod::Function(_)
                )) * 4
                + 1
        }
        WalOp::DropCast { source, target } => {
            routine_result_len(*source) + routine_result_len(*target)
        }
        WalOp::SetOperator { definition, .. } => {
            let result = definition
                .implementation
                .result()
                .unwrap_or(crate::storage::RoutineResult::TEXT);
            8 + 1
                + definition.schema.as_str().len()
                + 1
                + definition.name.as_str().len()
                + operator_signature_len(definition.signature)
                + routine_result_len(result)
                + 4
                + 4
                + 4
                + 1
                + 4
        }
        WalOp::DropOperator {
            schema,
            name,
            signature,
        } => 1 + schema.len() + 1 + name.len() + operator_signature_len(*signature),
        WalOp::SetOperatorFamily { definition, .. } => {
            8 + 1
                + definition.schema.as_str().len()
                + 1
                + definition.name.as_str().len()
                + 4
                + 1
                + definition
                    .operators
                    .iter()
                    .filter(|member| member.used)
                    .map(|member| {
                        1 + routine_result_len(member.left) + routine_result_len(member.right) + 4
                    })
                    .sum::<usize>()
                + 1
                + definition
                    .functions
                    .iter()
                    .filter(|member| member.used)
                    .map(|member| {
                        routine_result_len(member.left) + routine_result_len(member.right) + 4
                    })
                    .sum::<usize>()
        }
        WalOp::DropOperatorFamily { schema, name } | WalOp::DropOperatorClass { schema, name } => {
            1 + schema.len() + 1 + name.len()
        }
        WalOp::SetCollation { definition, .. } => {
            1 + 8
                + 1
                + definition.schema.as_str().len()
                + 1
                + definition.name.as_str().len()
                + 4
                + 3
                + 1
                + definition.collate.as_str().len()
                + 1
                + definition.ctype.as_str().len()
                + 1
                + definition.locale.as_str().len()
                + 1
                + definition.rules.as_str().len()
                + 1
                + definition.version.as_str().len()
                + 1
        }
        WalOp::DropCollation { schema, name } | WalOp::DropConversion { schema, name } => {
            1 + schema.len() + 1 + name.len()
        }
        WalOp::SetConversion { definition, .. } => {
            1 + 8 + 1 + definition.schema.as_str().len() + 1 + definition.name.as_str().len() + 11
        }
        WalOp::SetTextSearch { definition, .. } => {
            1 + 8 + 1 + text_search_definition_len(*definition)
        }
        WalOp::DropTextSearch { schema, name, .. } => 1 + 1 + schema.len() + 1 + name.len(),
        WalOp::SetEventTrigger { definition, .. } => {
            1 + 8
                + 1
                + definition.name.as_str().len()
                + 7
                + definition
                    .tags
                    .values()
                    .iter()
                    .map(|tag| 1 + tag.as_str().len())
                    .sum::<usize>()
        }
        WalOp::DropEventTrigger { name } => 1 + name.len(),
        WalOp::SetOperatorClass { definition, .. } => {
            8 + 1
                + definition.schema.as_str().len()
                + 1
                + definition.name.as_str().len()
                + 4
                + 4
                + routine_result_len(definition.input)
                + routine_result_len(definition.storage)
                + 1
                + 1
                + definition
                    .operators
                    .iter()
                    .filter(|member| member.used)
                    .map(|member| {
                        1 + routine_result_len(member.left) + routine_result_len(member.right) + 4
                    })
                    .sum::<usize>()
                + 1
                + definition
                    .functions
                    .iter()
                    .filter(|member| member.used)
                    .map(|member| {
                        routine_result_len(member.left) + routine_result_len(member.right) + 4
                    })
                    .sum::<usize>()
        }
        WalOp::DropRoutine {
            schema,
            name,
            argument_signature,
        } => 1 + name.len() + 1 + schema.len() + argument_signature.len(),
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_signature,
            new_schema,
            new_name,
        } => {
            1 + name.len()
                + 1
                + schema.len()
                + argument_signature.len()
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
        WalOp::SetExtendedStatistics {
            schema,
            name,
            table_schema,
            table,
            keys,
            key_count,
            ..
        } => {
            8 + 1
                + schema.len()
                + 1
                + name.len()
                + 1
                + table_schema.len()
                + 1
                + table.len()
                + 2
                + 1
                + 1
                + 1
                + keys[..*key_count]
                    .iter()
                    .map(|key| match key {
                        WalExtendedStatisticsKey::Column(column) => 1 + 1 + column.len(),
                        WalExtendedStatisticsKey::Expression(expression) => {
                            1 + 2 + expression.len()
                        }
                    })
                    .sum::<usize>()
        }
        WalOp::DropExtendedStatistics { schema, name } => 1 + schema.len() + 1 + name.len(),
        WalOp::AnalyzeExtendedStatistics {
            schema,
            name,
            statistics,
        } => 1 + schema.len() + 1 + name.len() + statistics.encoded_len(),
        WalOp::UpsertRole { name, attributes } => {
            1 + name.len()
                + 2
                + 4
                + 16
                + 32
                + 32
                + 4
                + 1
                + attributes
                    .valid_until
                    .as_ref()
                    .map_or(0, |value| value.as_str().len())
        }
        WalOp::DropRole { name } => 1 + name.len(),
        WalOp::UpsertRoleMembership {
            role,
            member,
            grantor,
            ..
        } => 1 + role.len() + 1 + member.len() + 1 + grantor.len() + 1,
        WalOp::DropRoleMembership { role, member } => 1 + role.len() + 1 + member.len(),
        WalOp::SetRoleSetting {
            role,
            database,
            name,
            value,
        } => {
            1 + role.map_or(0, |role| 1 + role.len())
                + usize::from(database.is_some()) * 4
                + 1
                + name.len()
                + value.map_or(0, |value| 2 + value.len())
        }
        WalOp::SetSystemSetting { name, value } => {
            1 + name.len() + 1 + value.map_or(0, |value| 2 + value.len())
        }
        WalOp::SetObjectOwner {
            class,
            schema,
            name,
            owner,
            ..
        } => {
            1 + usize::from(access_class_has_oid(*class)) * 4
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
            1 + usize::from(access_class_has_oid(*class)) * 4
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
        WalOp::SetColumnAcl {
            schema,
            name,
            grantee,
            grantor,
            ..
        } => 1 + 1 + schema.len() + 1 + name.len() + 2 + 1 + grantee.len() + 1 + grantor.len() + 4,
        WalOp::SetDefaultAcl {
            owner,
            schema,
            grantee,
            ..
        } => 1 + owner.len() + 1 + schema.len() + 1 + 1 + grantee.len() + 1 + 4,
    }
}

fn database_definition_len(definition: crate::storage::DatabaseDefinition) -> usize {
    1 + definition.name.as_str().len()
        + 2
        + 2
        + definition.collate.as_str().len()
        + 2
        + definition.ctype.as_str().len()
        + 2
        + definition.locale.as_str().len()
        + 2
        + definition.collation_version.as_str().len()
        + 1
        + 4
        + 2
}

fn encoded_partition_len(partition: PartitionDef) -> usize {
    let scheme = partition
        .scheme
        .map_or(0, |scheme| 2 + usize::from(scheme.n_keys) * 2);
    let attachment = partition.attachment.map_or(0, |attachment| {
        2 + match attachment.bound {
            PartitionBound::Default => 1,
            PartitionBound::Hash { .. } => 1 + 8,
            PartitionBound::List { n_values, values } => {
                2 + values[..usize::from(n_values)]
                    .iter()
                    .map(|value| encoded_default_len(&Some(*value)))
                    .sum::<usize>()
            }
            PartitionBound::Range {
                n_keys,
                lower,
                upper,
            } => {
                2 + (0..usize::from(n_keys))
                    .map(|i| encoded_bound_value_len(lower[i]) + encoded_bound_value_len(upper[i]))
                    .sum::<usize>()
            }
        }
    });
    1 + scheme + attachment
}

fn encoded_bound_value_len(value: PartitionBoundValue) -> usize {
    match value {
        PartitionBoundValue::MinValue | PartitionBoundValue::MaxValue => 1,
        PartitionBoundValue::Value(value) => 1 + encoded_default_len(&Some(value)),
    }
}

fn append_partition(buffer: &mut FixedBuf, partition: PartitionDef) -> bool {
    let flags =
        u8::from(partition.scheme.is_some()) | (u8::from(partition.attachment.is_some()) << 1);
    let mut ok = buffer.append(&[flags]);
    if let Some(scheme) = partition.scheme {
        let strategy = match scheme.strategy {
            PartitionStrategy::Range => 0,
            PartitionStrategy::List => 1,
            PartitionStrategy::Hash => 2,
        };
        ok &= buffer.append(&[strategy, scheme.n_keys]);
        for key in &scheme.keys[..usize::from(scheme.n_keys)] {
            ok &= buffer.append(&key.to_le_bytes());
        }
    }
    if let Some(attachment) = partition.attachment {
        ok &= buffer.append(&attachment.parent.to_le_bytes());
        match attachment.bound {
            PartitionBound::Default => ok &= buffer.append(&[0]),
            PartitionBound::Range {
                lower,
                upper,
                n_keys,
            } => {
                ok &= buffer.append(&[1, n_keys]);
                for i in 0..usize::from(n_keys) {
                    ok &= append_bound_value(buffer, lower[i])
                        && append_bound_value(buffer, upper[i]);
                }
            }
            PartitionBound::List { values, n_values } => {
                ok &= buffer.append(&[2, n_values]);
                for value in &values[..usize::from(n_values)] {
                    ok &= append_default(buffer, &Some(*value));
                }
            }
            PartitionBound::Hash { modulus, remainder } => {
                ok &= buffer.append(&[3])
                    && buffer.append(&modulus.to_le_bytes())
                    && buffer.append(&remainder.to_le_bytes())
            }
        }
    }
    ok
}

fn append_bound_value(buffer: &mut FixedBuf, value: PartitionBoundValue) -> bool {
    match value {
        PartitionBoundValue::MinValue => buffer.append(&[0]),
        PartitionBoundValue::Value(value) => {
            buffer.append(&[1]) && append_default(buffer, &Some(value))
        }
        PartitionBoundValue::MaxValue => buffer.append(&[2]),
    }
}

/// Bytes this operation occupies in the journal, including its fixed record
/// header. EXPLAIN uses the production codec's sizing rule so WAL telemetry
/// cannot drift from the bytes commit will write.
pub(crate) fn encoded_record_len(operation: &WalOp) -> usize {
    HEADER_LEN + encoded_payload_len(operation)
}

fn append_subscription_behavior(
    buffer: &mut FixedBuf,
    behavior: crate::storage::SubscriptionBehavior,
) -> bool {
    buffer.append(&[
        u8::from(behavior.binary),
        behavior.streaming.code(),
        behavior.synchronous_commit.code(),
        u8::from(behavior.two_phase),
        u8::from(behavior.disable_on_error),
        u8::from(behavior.password_required),
        u8::from(behavior.run_as_owner),
        behavior.origin.code(),
        u8::from(behavior.failover),
        u8::from(behavior.skip_lsn.is_some()),
    ]) && buffer.append(&behavior.skip_lsn.unwrap_or(0).to_le_bytes())
}

fn append_index_definition(
    buffer: &mut FixedBuf,
    definition: crate::storage::IndexMutableDefinition,
) -> bool {
    let fillfactor = definition.options.fillfactor.unwrap_or(0);
    let deduplicate = match definition.options.deduplicate_items {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let kind = match definition.kind {
        crate::storage::IndexKind::Ordinary => 0,
        crate::storage::IndexKind::Partitioned { valid: false } => 1,
        crate::storage::IndexKind::Partitioned { valid: true } => 2,
    };
    let mut ok = buffer.append(&definition.tablespace.to_le_bytes())
        && buffer.append(&[fillfactor, deduplicate]);
    for statistic in definition.statistics {
        ok &= buffer.append(&statistic.to_le_bytes());
    }
    ok &= buffer.append(&definition.parent.unwrap_or(u16::MAX).to_le_bytes());
    ok && buffer.append(&[
        kind,
        u8::from(definition.clustered),
        u8::from(definition.replica_identity),
    ])
}

fn append_tablespace_options(
    buffer: &mut FixedBuf,
    options: crate::storage::TablespaceOptions,
) -> bool {
    buffer.append(
        &options
            .random_page_cost
            .map_or(u64::MAX, crate::sql::ast::TablespaceCost::bits)
            .to_le_bytes(),
    ) && buffer.append(
        &options
            .seq_page_cost
            .map_or(u64::MAX, crate::sql::ast::TablespaceCost::bits)
            .to_le_bytes(),
    ) && buffer.append(
        &options
            .effective_io_concurrency
            .unwrap_or(i32::MIN)
            .to_le_bytes(),
    ) && buffer.append(
        &options
            .maintenance_io_concurrency
            .unwrap_or(i32::MIN)
            .to_le_bytes(),
    )
}

fn append_database_definition(
    buffer: &mut FixedBuf,
    definition: crate::storage::DatabaseDefinition,
) -> bool {
    let short = |buffer: &mut FixedBuf, value: &str| {
        value.len() <= u16::MAX as usize
            && buffer.append(&(value.len() as u16).to_le_bytes())
            && buffer.append(value.as_bytes())
    };
    definition.name.as_str().len() <= u8::MAX as usize
        && buffer.append(&[definition.name.as_str().len() as u8])
        && buffer.append(definition.name.as_str().as_bytes())
        && buffer.append(&[
            definition.encoding.code() as u8,
            definition.locale_provider.code(),
        ])
        && short(buffer, definition.collate.as_str())
        && short(buffer, definition.ctype.as_str())
        && short(buffer, definition.locale.as_str())
        && short(buffer, definition.collation_version.as_str())
        && buffer.append(&[
            u8::from(definition.allow_connections) | (u8::from(definition.is_template) << 1)
        ])
        && buffer.append(&definition.connection_limit.to_le_bytes())
        && buffer.append(&definition.tablespace.to_le_bytes())
}

fn append_payload(buffer: &mut FixedBuf, operation: &WalOp) -> bool {
    let name_bytes = |buffer: &mut FixedBuf, s: &str| -> bool {
        buffer.append(&[s.len() as u8]) && buffer.append(s.as_bytes())
    };
    fn append_routine_result(buffer: &mut FixedBuf, result: crate::storage::RoutineResult) -> bool {
        let mut ok = buffer.append(&[result.ctype.code()]);
        match result.user_type {
            Some(identity) => {
                ok &= buffer.append(&[1]);
                for name in [identity.schema.as_str(), identity.name.as_str()] {
                    ok &= buffer.append(&[name.len() as u8]) && buffer.append(name.as_bytes());
                }
            }
            None => ok &= buffer.append(&[0]),
        }
        ok
    }
    fn append_operator_signature(
        buffer: &mut FixedBuf,
        signature: crate::storage::OperatorSignature,
    ) -> bool {
        let flags = u8::from(signature.left.is_some()) | (u8::from(signature.right.is_some()) << 1);
        let mut ok = buffer.append(&[flags]);
        if let Some(left) = signature.left {
            ok &= append_routine_result(buffer, left);
        }
        if let Some(right) = signature.right {
            ok &= append_routine_result(buffer, right);
        }
        ok
    }
    fn append_text_search_definition(
        buffer: &mut FixedBuf,
        definition: crate::storage::TextSearchDefinition,
    ) -> bool {
        let name = |buffer: &mut FixedBuf, value: &str| {
            value.len() <= u8::MAX as usize
                && buffer.append(&[value.len() as u8])
                && buffer.append(value.as_bytes())
        };
        let behavior = |value| match value {
            crate::storage::TextSearchDictionaryBehavior::Simple { accept } => {
                [0, u8::from(accept)]
            }
            crate::storage::TextSearchDictionaryBehavior::EnglishStem => [1, 0],
        };
        let mut ok = name(buffer, definition.schema().as_str())
            && name(buffer, definition.name().as_str())
            && buffer.append(&definition.oid().to_le_bytes());
        ok &= match definition {
            crate::storage::TextSearchDefinition::Parser {
                start,
                gettoken,
                end,
                headline,
                lextypes,
                ..
            } => {
                buffer.append(&start.to_le_bytes())
                    && buffer.append(&gettoken.to_le_bytes())
                    && buffer.append(&end.to_le_bytes())
                    && buffer.append(&headline.to_le_bytes())
                    && buffer.append(&lextypes.to_le_bytes())
            }
            crate::storage::TextSearchDefinition::Template {
                init,
                lexize,
                behavior: executable,
                ..
            } => {
                buffer.append(&init.to_le_bytes())
                    && buffer.append(&lexize.to_le_bytes())
                    && buffer.append(&behavior(executable))
            }
            crate::storage::TextSearchDefinition::Dictionary {
                owner,
                template,
                options,
                behavior: executable,
                ..
            } => {
                buffer.append(&owner.to_le_bytes())
                    && buffer.append(&template.to_le_bytes())
                    && name(buffer, options.as_str())
                    && buffer.append(&behavior(executable))
            }
            crate::storage::TextSearchDefinition::Configuration {
                owner,
                parser,
                mappings,
                ..
            } => {
                let mut encoded =
                    buffer.append(&owner.to_le_bytes()) && buffer.append(&parser.to_le_bytes());
                for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
                    let count = mappings.counts[token];
                    encoded &= count as usize <= crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN
                        && buffer.append(&[count]);
                    for dictionary in mappings.dictionaries[token].iter().take(count as usize) {
                        encoded &= buffer.append(&dictionary.to_le_bytes());
                    }
                }
                encoded
            }
        };
        ok
    }
    fn append_foreign_options(
        buffer: &mut FixedBuf,
        options: crate::storage::foreign::ForeignOptions,
    ) -> bool {
        let mut ok = buffer.append(&[options.entries().len() as u8]);
        for option in options.entries() {
            let name = option.name.as_str();
            let value = option.value.as_str();
            ok &= name.len() <= u8::MAX as usize
                && value.len() <= u16::MAX as usize
                && buffer.append(&[name.len() as u8])
                && buffer.append(name.as_bytes())
                && buffer.append(&(value.len() as u16).to_le_bytes())
                && buffer.append(value.as_bytes());
        }
        ok
    }
    fn append_optional_foreign_value<const N: usize>(
        buffer: &mut FixedBuf,
        value: Option<crate::util::StackStr<N>>,
    ) -> bool {
        match value {
            None => buffer.append(&[0]),
            Some(value) => {
                let value = value.as_str();
                buffer.append(&[1])
                    && value.len() <= u16::MAX as usize
                    && buffer.append(&(value.len() as u16).to_le_bytes())
                    && buffer.append(value.as_bytes())
            }
        }
    }
    match operation {
        WalOp::DatabaseScope { oid } => buffer.append(&oid.to_le_bytes()),
        WalOp::CreateLargeObject {
            oid,
            created_at,
            allocated,
        } => {
            buffer.append(&oid.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&[u8::from(*allocated)])
        }
        WalOp::DropLargeObject { oid } => buffer.append(&oid.to_le_bytes()),
        WalOp::SetForeignDataWrapper {
            slot,
            created_at,
            owner,
            definition,
        } => {
            let mut ok = buffer.append(&slot.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&[u8::from(definition.is_some())]);
            if let Some(definition) = definition {
                ok &= name_bytes(buffer, definition.name.as_str())
                    && buffer.append(&[
                        match definition.handler {
                            crate::storage::foreign::ForeignDataHandler::None => 0,
                            crate::storage::foreign::ForeignDataHandler::Postgres => 1,
                        },
                        match definition.validator {
                            crate::storage::foreign::ForeignDataValidator::None => 0,
                            crate::storage::foreign::ForeignDataValidator::Postgres => 1,
                        },
                    ])
                    && append_foreign_options(buffer, definition.options);
            }
            ok
        }
        WalOp::SetForeignServer {
            slot,
            created_at,
            owner,
            definition,
        } => {
            let mut ok = buffer.append(&slot.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&[u8::from(definition.is_some())]);
            if let Some(definition) = definition {
                ok &= name_bytes(buffer, definition.name.as_str())
                    && buffer.append(&definition.wrapper.to_le_bytes())
                    && append_optional_foreign_value(buffer, definition.server_type)
                    && append_optional_foreign_value(buffer, definition.version)
                    && append_foreign_options(buffer, definition.options);
            }
            ok
        }
        WalOp::SetUserMapping {
            slot,
            created_at,
            definition,
        } => {
            let mut ok = buffer.append(&slot.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&[u8::from(definition.is_some())]);
            if let Some(definition) = definition {
                let (kind, role) = match definition.user {
                    crate::storage::foreign::ForeignMappingUser::Public => (0, u16::MAX),
                    crate::storage::foreign::ForeignMappingUser::Role(role) => (1, role),
                };
                ok &= buffer.append(&definition.server.to_le_bytes())
                    && buffer.append(&[kind])
                    && buffer.append(&role.to_le_bytes())
                    && append_foreign_options(buffer, definition.options);
            }
            ok
        }
        WalOp::SetForeignTable {
            slot,
            created_at,
            definition,
        } => {
            let mut ok = buffer.append(&slot.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&[u8::from(definition.is_some())]);
            if let Some(definition) = definition {
                ok &= buffer.append(&definition.table.to_le_bytes())
                    && buffer.append(&definition.server.to_le_bytes())
                    && append_foreign_options(buffer, definition.options)
                    && buffer.append(&[definition.column_options.entries().len() as u8]);
                for column in definition.column_options.entries() {
                    ok &= buffer.append(&column.column.to_le_bytes());
                    let mut one = crate::storage::foreign::ForeignOptions::EMPTY;
                    ok &= one
                        .restore_option(column.option.name.as_str(), column.option.value.as_str())
                        .is_ok()
                        && append_foreign_options(buffer, one);
                }
            }
            ok
        }
        WalOp::CreateTable(def) => {
            let mut ok = name_bytes(buffer, def.name.as_str());
            ok &= buffer.append(&(def.n_columns as u16).to_le_bytes());
            ok &= buffer.append(&[
                u8::from(def.has_toast),
                match def.kind {
                    crate::storage::TableKind::Local => 0,
                    crate::storage::TableKind::Foreign => 1,
                },
            ]);
            ok &= buffer.append(&def.tablespace.to_le_bytes());
            ok &= buffer.append(&[def.access_method.code()]);
            for c in def.columns() {
                ok &= name_bytes(buffer, c.name.as_str());
                // Bit 7 (the last free per-column flag bit) marks a domain-typed
                // column, whose domain name is appended after the fixed fields.
                let flags = u8::from(c.not_null.is_required())
                    | (u8::from(c.unique) << 1)
                    | (u8::from(c.primary) << 2)
                    | (u8::from(c.auto_increment) << 3)
                    | (u8::from(c.default.is_generated()) << 4)
                    | (u8::from(c.is_identity) << 5)
                    | (u8::from(c.identity_always) << 6)
                    | (u8::from(c.user_type.is_some()) << 7);
                ok &= buffer.append(&[c.ctype.code(), flags]);
                ok &= buffer.append(&[c.not_null.code()]);
                ok &= buffer.append(&c.type_mod.to_le_bytes());
                let default_value = c.default.constant().copied();
                ok &= append_default(buffer, &default_value);
                let de = c.default.expression().map_or("", |e| e.as_str());
                ok &= buffer.append(&(de.len() as u16).to_le_bytes());
                ok &= buffer.append(de.as_bytes());
                ok &= buffer.append(&c.auto_increment_step.to_le_bytes());
                ok &= buffer.append(&c.statistics_target.to_le_bytes());
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
                ok &= buffer.append(&[u8::from(uk.is_primary), uk.n_cols as u8, uk.timing.code()]);
                for &c in uk.columns() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
            }
            // CHECK constraints.
            ok &= buffer.append(&[def.n_checks as u8]);
            for check in def.checks() {
                ok &= name_bytes(buffer, check.name.as_str());
                let e = check.expression.as_str();
                ok &= buffer.append(&[check.validation.code()]);
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
                ok &= buffer.append(&[
                    fk.on_delete.code(),
                    fk.on_update.code(),
                    fk.timing.code(),
                    fk.validation.code(),
                ]);
            }
            ok &= buffer.append(&[def.n_exclusions as u8]);
            for exclusion in def.exclusions() {
                ok &= name_bytes(buffer, exclusion.name.as_str());
                ok &= buffer.append(&[exclusion.n_cols as u8, exclusion.timing.code()]);
                for position in 0..exclusion.n_cols {
                    ok &= buffer.append(&exclusion.columns[position].to_le_bytes());
                    ok &= buffer.append(&[exclusion.operators[position].code()]);
                }
                let predicate = exclusion.predicate.as_ref().map(|value| value.as_str());
                ok &= buffer.append(
                    &(predicate.map_or(u16::MAX, |value| value.len() as u16)).to_le_bytes(),
                );
                if let Some(predicate) = predicate {
                    ok &= buffer.append(predicate.as_bytes());
                }
            }
            ok &= name_bytes(buffer, def.schema.as_str());
            for fk in def.fkeys() {
                ok &= name_bytes(buffer, fk.parent_schema.as_str());
            }
            for column in def.columns() {
                ok &= buffer.append(&[column.collation.code()]);
            }
            ok &= append_partition(buffer, def.partition);
            ok &= buffer.append(&[
                u8::from(def.row_level_security.enabled),
                u8::from(def.row_level_security.forced),
                def.replica_identity.code(),
            ]);
            ok
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            preserve_rows,
            column_mapping,
        } => {
            let mut ok = name_bytes(buffer, previous_schema)
                && name_bytes(buffer, previous_name)
                && buffer.append(&[u8::from(*preserve_rows)]);
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
            security_invoker,
            dependencies,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&(sql.len() as u16).to_le_bytes())
                && buffer.append(sql.as_bytes())
                && name_bytes(buffer, schema)
                && buffer.append(&(path.len() as u16).to_le_bytes())
                && buffer.append(path.as_bytes())
                && buffer.append(&[u8::from(*security_invoker)])
                && dependencies.append(buffer)
        }
        WalOp::DropView { schema, name } => name_bytes(buffer, name) && name_bytes(buffer, schema),
        WalOp::SetRule {
            slot,
            created_at,
            target,
            table_schema,
            table,
            name,
            event,
            mode,
            source,
            condition,
            actions,
            action_count,
            returning_action,
            path,
            dependencies,
        } => {
            let condition = condition.unwrap_or(crate::storage::RuleTextSpan {
                start: u16::MAX,
                len: 0,
            });
            let mut ok = buffer.append(&slot.to_le_bytes())
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&[target.code()])
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
                && name_bytes(buffer, name)
                && buffer.append(&[*event as u8, *mode as u8])
                && source.len() <= u16::MAX as usize
                && buffer.append(&(source.len() as u16).to_le_bytes())
                && buffer.append(source.as_bytes())
                && buffer.append(&condition.start.to_le_bytes())
                && buffer.append(&condition.len.to_le_bytes())
                && buffer.append(&[*action_count]);
            for action in &actions[..usize::from(*action_count)] {
                ok &= buffer.append(&action.start.to_le_bytes())
                    && buffer.append(&action.len.to_le_bytes());
            }
            ok && buffer.append(&[returning_action.unwrap_or(u8::MAX)])
                && path.len() <= u16::MAX as usize
                && buffer.append(&(path.len() as u16).to_le_bytes())
                && buffer.append(path.as_bytes())
                && dependencies.append(buffer)
        }
        WalOp::DropRule {
            target,
            table_schema,
            table,
            name,
        } => {
            buffer.append(&[target.code()])
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
                && name_bytes(buffer, name)
        }
        WalOp::CreatePublication {
            name,
            owner,
            all_tables,
            tables,
            table_column_masks,
            table_filter_sql,
            table_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
            schemas,
            schema_count,
        } => {
            let flags = u8::from(*all_tables)
                | (u8::from(*publish_insert) << 1)
                | (u8::from(*publish_update) << 2)
                | (u8::from(*publish_delete) << 3)
                | (u8::from(*publish_truncate) << 4)
                | (u8::from(*publish_via_partition_root) << 5);
            let flags = flags
                | (u8::from(matches!(
                    publish_generated_columns,
                    crate::storage::PublishGeneratedColumns::Stored
                )) << 6);
            let mut ok = name_bytes(buffer, name)
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&[flags, *table_count as u8, *schema_count as u8]);
            for table in &tables[..*table_count] {
                ok = ok && buffer.append(&table.to_le_bytes());
            }
            for mask in &table_column_masks[..*table_count] {
                ok = ok && buffer.append(&mask.to_le_bytes());
            }
            for filter in &table_filter_sql[..*table_count] {
                ok = ok
                    && buffer.append(&(filter.as_str().len() as u16).to_le_bytes())
                    && buffer.append(filter.as_str().as_bytes());
            }
            ok = ok && buffer.append(&schemas[..*schema_count]);
            ok
        }
        WalOp::DropPublication { name } => name_bytes(buffer, name),
        WalOp::AlterPublication {
            name,
            all_tables,
            tables,
            table_column_masks,
            table_filter_sql,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
        } => {
            let flags = u8::from(*all_tables)
                | (u8::from(*publish_insert) << 1)
                | (u8::from(*publish_update) << 2)
                | (u8::from(*publish_delete) << 3)
                | (u8::from(*publish_truncate) << 4)
                | (u8::from(*publish_via_partition_root) << 5);
            let flags = flags
                | (u8::from(matches!(
                    publish_generated_columns,
                    crate::storage::PublishGeneratedColumns::Stored
                )) << 6);
            let mut ok = name_bytes(buffer, name)
                && buffer.append(&[flags, *table_count as u8, *schema_count as u8]);
            for table in &tables[..*table_count] {
                ok = ok && buffer.append(&table.to_le_bytes());
            }
            for mask in &table_column_masks[..*table_count] {
                ok = ok && buffer.append(&mask.to_le_bytes());
            }
            for filter in &table_filter_sql[..*table_count] {
                ok = ok
                    && buffer.append(&(filter.as_str().len() as u16).to_le_bytes())
                    && buffer.append(filter.as_str().as_bytes());
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
        WalOp::CreateSubscription {
            name,
            owner,
            connection,
            publications,
            publication_count,
            enabled,
            slot,
            behavior,
            bootstrap,
        } => {
            connection.len() <= u16::MAX as usize
                && *publication_count <= crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS
                && name_bytes(buffer, name)
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&(connection.len() as u16).to_le_bytes())
                && buffer.append(connection.as_bytes())
                && buffer.append(&[*publication_count as u8])
                && publications[..*publication_count]
                    .iter()
                    .all(|publication| name_bytes(buffer, publication.as_str()))
                && buffer.append(&[u8::from(*enabled)])
                && buffer.append(&[bootstrap.code()])
                && append_subscription_behavior(buffer, *behavior)
                && match slot {
                    crate::storage::SubscriptionSlot::Absent => buffer.append(&[0]),
                    crate::storage::SubscriptionSlot::External(name) => {
                        buffer.append(&[1]) && name_bytes(buffer, name.as_str())
                    }
                    crate::storage::SubscriptionSlot::Managed(name) => {
                        buffer.append(&[2]) && name_bytes(buffer, name.as_str())
                    }
                }
        }
        WalOp::DropSubscription { name } => name_bytes(buffer, name),
        WalOp::AdvanceSubscription {
            name,
            created_at,
            definition_generation,
            confirmed_lsn,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&definition_generation.to_le_bytes())
                && buffer.append(&confirmed_lsn.to_le_bytes())
        }
        WalOp::SetSubscriptionEnabled { name, enabled } => {
            name_bytes(buffer, name) && buffer.append(&[u8::from(*enabled)])
        }
        WalOp::SetSubscriptionBootstrap { name, bootstrap } => {
            name_bytes(buffer, name) && buffer.append(&[bootstrap.code()])
        }
        WalOp::ResetSubscriptionRelations {
            name,
            created_at,
            definition_generation,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&definition_generation.to_le_bytes())
        }
        WalOp::AddSubscriptionRelation {
            name,
            created_at,
            definition_generation,
            schema,
            table,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&definition_generation.to_le_bytes())
                && name_bytes(buffer, schema)
                && name_bytes(buffer, table)
        }
        WalOp::CompleteSubscriptionCleanup { name, created_at } => {
            name_bytes(buffer, name) && buffer.append(&created_at.to_le_bytes())
        }
        WalOp::FailSubscription {
            name,
            created_at,
            definition_generation,
            sqlstate,
            message,
        } => {
            sqlstate.len() == 5
                && message.len() <= u8::MAX as usize
                && name_bytes(buffer, name)
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&definition_generation.to_le_bytes())
                && buffer.append(sqlstate.as_bytes())
                && name_bytes(buffer, message)
        }
        WalOp::AlterSubscription {
            name,
            connection,
            publications,
            publication_count,
            slot,
            behavior,
        } => {
            connection.len() <= u16::MAX as usize
                && *publication_count <= crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS
                && name_bytes(buffer, name)
                && buffer.append(&(connection.len() as u16).to_le_bytes())
                && buffer.append(connection.as_bytes())
                && buffer.append(&[*publication_count as u8])
                && publications[..*publication_count]
                    .iter()
                    .all(|publication| name_bytes(buffer, publication.as_str()))
                && append_subscription_behavior(buffer, *behavior)
                && match slot {
                    crate::storage::SubscriptionSlot::Absent => buffer.append(&[0]),
                    crate::storage::SubscriptionSlot::External(name) => {
                        buffer.append(&[1]) && name_bytes(buffer, name.as_str())
                    }
                    crate::storage::SubscriptionSlot::Managed(name) => {
                        buffer.append(&[2]) && name_bytes(buffer, name.as_str())
                    }
                }
        }
        WalOp::SetSubscriptionOwner { name, owner } => {
            name_bytes(buffer, name) && buffer.append(&owner.to_le_bytes())
        }
        WalOp::RenameSubscription { name, new_name } => {
            name_bytes(buffer, name) && name_bytes(buffer, new_name)
        }
        WalOp::Commit { transaction_id } => buffer.append(&transaction_id.to_le_bytes()),
        WalOp::PrepareTransaction {
            transaction_id,
            owner,
            database,
            prepared_at,
            gid,
        } => {
            buffer.append(&transaction_id.to_le_bytes())
                && buffer.append(&owner.to_le_bytes())
                && buffer.append(&database.to_le_bytes())
                && buffer.append(&prepared_at.to_le_bytes())
                && name_bytes(buffer, gid)
        }
        WalOp::PreparedLocks {
            transaction_id,
            encoded,
        } => {
            encoded.len() <= u32::MAX as usize
                && buffer.append(&transaction_id.to_le_bytes())
                && buffer.append(&(encoded.len() as u32).to_le_bytes())
                && buffer.append(encoded)
        }
        WalOp::CommitPrepared { gid } | WalOp::RollbackPrepared { gid } => name_bytes(buffer, gid),
        WalOp::CreateReplicationSlot {
            name,
            restart_lsn,
            behavior,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&restart_lsn.to_le_bytes())
                && buffer.append(&[behavior.code()])
        }
        WalOp::AlterReplicationSlot { name, behavior } => {
            name_bytes(buffer, name) && buffer.append(&[behavior.code()])
        }
        WalOp::DropReplicationSlot { name } => name_bytes(buffer, name),
        WalOp::AdvanceReplicationSlot {
            name,
            confirmed_flush_lsn,
        } => name_bytes(buffer, name) && buffer.append(&confirmed_flush_lsn.to_le_bytes()),
        WalOp::CreateIndex {
            created_at,
            schema,
            name,
            table,
            columns,
            expressions,
            include_columns,
            collations,
            explicit_collations,
            operator_classes,
            resolved_operator_classes,
            descending,
            nulls_first,
            n_cols,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            unique,
            definition,
        } => {
            let mut ok = buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, name)
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
            ok &= buffer.append(&[0xa6]);
            for position in 0..*n_cols {
                ok &= buffer.append(&[
                    collations[position].code(),
                    u8::from(explicit_collations[position]),
                ]);
            }
            ok &= buffer.append(&[0xa7]);
            for operator_class in &operator_classes[..*n_cols] {
                ok &= match operator_class {
                    None => buffer.append(&[0, 0, 0, 0, 0]),
                    Some(crate::storage::IndexOperatorClass::Builtin(class)) => {
                        buffer.append(&[1, class.code(), 0, 0, 0])
                    }
                    Some(crate::storage::IndexOperatorClass::Catalog(oid)) => {
                        buffer.append(&[2]) && buffer.append(&oid.get().to_le_bytes())
                    }
                };
            }
            ok &= buffer.append(&[0xa9]);
            for operator_class in &resolved_operator_classes[..*n_cols] {
                ok &= match operator_class {
                    Some(crate::storage::IndexOperatorClass::Builtin(class)) => {
                        buffer.append(&[1, class.code(), 0, 0, 0])
                    }
                    Some(crate::storage::IndexOperatorClass::Catalog(oid)) => {
                        buffer.append(&[2]) && buffer.append(&oid.get().to_le_bytes())
                    }
                    None => false,
                };
            }
            ok &= buffer.append(&[0xa8]);
            ok && append_index_definition(buffer, *definition)
        }
        WalOp::AlterIndexDefinition {
            schema,
            name,
            definition,
        } => {
            name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && append_index_definition(buffer, *definition)
        }
        WalOp::CreateTablespace {
            created_at,
            name,
            location,
            options,
            owner,
        } => {
            buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, name)
                && location.len() <= u16::MAX as usize
                && buffer.append(&(location.len() as u16).to_le_bytes())
                && buffer.append(location.as_bytes())
                && append_tablespace_options(buffer, *options)
                && buffer.append(&owner.to_le_bytes())
        }
        WalOp::AlterTablespace {
            name,
            new_name,
            options,
            owner,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, new_name)
                && append_tablespace_options(buffer, *options)
                && buffer.append(&owner.to_le_bytes())
        }
        WalOp::DropTablespace { name } => name_bytes(buffer, name),
        WalOp::CreateDatabase {
            oid,
            template_oid,
            definition,
            owner,
        } => {
            buffer.append(&oid.to_le_bytes())
                && buffer.append(&template_oid.to_le_bytes())
                && append_database_definition(buffer, *definition)
                && buffer.append(&owner.to_le_bytes())
        }
        WalOp::AlterDatabase {
            oid,
            definition,
            owner,
        } => {
            buffer.append(&oid.to_le_bytes())
                && append_database_definition(buffer, *definition)
                && buffer.append(&owner.to_le_bytes())
        }
        WalOp::DropDatabase { oid } => buffer.append(&oid.to_le_bytes()),
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
        WalOp::UpsertExtension {
            name,
            schema,
            version,
            relocatable,
            owner,
            created_at,
        } => {
            buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && name_bytes(buffer, version)
                && buffer.append(&[u8::from(*relocatable)])
                && name_bytes(buffer, owner)
        }
        WalOp::DropExtension { name } => name_bytes(buffer, name),
        WalOp::SetExtensionDependency {
            extension,
            class,
            object_oid,
            schema,
            name,
            kind,
            exists,
        } => {
            name_bytes(buffer, extension)
                && buffer.append(&[*class as u8])
                && (*class != crate::storage::AccessClass::Routine
                    || buffer.append(&object_oid.to_le_bytes()))
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && buffer.append(&[kind.to_u8(), u8::from(*exists)])
        }
        WalOp::SetExtensionConfig {
            extension,
            ordinal,
            relation_kind,
            schema,
            name,
            condition,
            exists,
        } => {
            name_bytes(buffer, extension)
                && buffer.append(&ordinal.to_le_bytes())
                && buffer.append(&[relation_kind.to_u8()])
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && u16::try_from(condition.len()).ok().is_some_and(|length| {
                    buffer.append(&length.to_le_bytes()) && buffer.append(condition.as_bytes())
                })
                && buffer.append(&[u8::from(*exists)])
        }
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        }
        | WalOp::SetSequenceSchema {
            schema,
            name,
            new_schema,
        }
        | WalOp::SetViewSchema {
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
                && buffer.append(&[DOMAIN_PAYLOAD_WITH_BASE_SLOT])
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
                && name_bytes(
                    buffer,
                    def.base_user_type
                        .as_ref()
                        .map(|identity| identity.name.as_str())
                        .unwrap_or(""),
                )
                && name_bytes(
                    buffer,
                    def.base_user_type
                        .as_ref()
                        .map(|identity| identity.schema.as_str())
                        .unwrap_or(""),
                )
                && buffer.append(&[def.base.code()])
                && buffer.append(&domain_base_slot(def).to_le_bytes())
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
        WalOp::AlterEnumIdentity {
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
        WalOp::CreateComposite {
            slot,
            definition: def,
        } => {
            let mut ok = buffer.append(&slot.to_le_bytes())
                && name_bytes(buffer, def.name.as_str())
                && name_bytes(buffer, def.schema.as_str())
                && buffer.append(&[def.n_fields as u8]);
            for field in def.fields() {
                ok &= name_bytes(buffer, field.name.as_str())
                    && buffer.append(&field.attribute_number.to_le_bytes())
                    && buffer.append(&[u8::from(field.dropped)])
                    && buffer.append(&[u8::from(field.not_null)])
                    && buffer.append(&[field.ctype.code()])
                    && buffer.append(&field.type_mod.to_le_bytes())
                    && buffer.append(&[field.collation.code()]);
                match field.user_type {
                    Some(identity) => {
                        ok &= buffer.append(&[1])
                            && name_bytes(buffer, identity.schema.as_str())
                            && name_bytes(buffer, identity.name.as_str())
                    }
                    None => ok &= buffer.append(&[0]),
                }
            }
            ok
        }
        WalOp::DropComposite { schema, name } => {
            name_bytes(buffer, name) && name_bytes(buffer, schema)
        }
        WalOp::CreateRoutine {
            definition: def,
            dependencies,
        } => {
            let mut ok = buffer.append(&def.created_at.to_le_bytes())
                && buffer.append(&def.ownership.owner.to_le_bytes())
                && name_bytes(buffer, def.name.as_str())
                && name_bytes(buffer, def.schema.as_str())
                && buffer.append(&[def.argument_count as u8]);
            for argument in def.arguments() {
                ok &= name_bytes(buffer, argument.name.as_str())
                    && buffer.append(&[argument.ctype.code()]);
                match argument.user_type {
                    Some(identity) => {
                        ok &= buffer.append(&[1])
                            && name_bytes(buffer, identity.schema.as_str())
                            && name_bytes(buffer, identity.name.as_str());
                    }
                    None => ok &= buffer.append(&[0]),
                }
            }
            ok &= buffer.append(&[def.parameter_count as u8]);
            for parameter in def.parameters() {
                ok &= name_bytes(buffer, parameter.name.as_str())
                    && buffer.append(&[parameter.ctype.code()]);
                match parameter.user_type {
                    Some(identity) => {
                        ok &= buffer.append(&[1])
                            && name_bytes(buffer, identity.schema.as_str())
                            && name_bytes(buffer, identity.name.as_str());
                    }
                    None => ok &= buffer.append(&[0]),
                }
                let default = parameter.mode.default();
                ok &= buffer.append(&[parameter.mode.code()]);
                let default = default.as_ref().map_or("", crate::util::StackStr::as_str);
                ok &= buffer.append(&(default.len() as u16).to_le_bytes())
                    && buffer.append(default.as_bytes());
            }
            let result = match def.kind {
                crate::storage::RoutineKind::Function { result }
                | crate::storage::RoutineKind::SetFunction { result } => result,
                crate::storage::RoutineKind::Aggregate(aggregate) => aggregate.result_type,
                _ => crate::storage::RoutineResult::TEXT,
            };
            ok &= buffer.append(&[result.ctype.code()]);
            match result.user_type {
                Some(identity) => {
                    ok &= buffer.append(&[1])
                        && name_bytes(buffer, identity.schema.as_str())
                        && name_bytes(buffer, identity.name.as_str());
                }
                None => ok &= buffer.append(&[0]),
            }
            let aggregate_body = match def.kind {
                crate::storage::RoutineKind::Aggregate(aggregate) => Some(aggregate.encode_wire()),
                _ => None,
            };
            let body = aggregate_body
                .as_ref()
                .map_or(def.body.as_str(), crate::util::StackStr::as_str);
            ok &=
                buffer.append(&(body.len() as u16).to_le_bytes()) && buffer.append(body.as_bytes());
            ok &= buffer.append(&[
                u8::from(def.attributes.strict),
                def.attributes.volatility.code(),
                def.attributes.parallel.code(),
                def.body_kind.code(),
                def.language.code(),
                u8::from(def.attributes.security_definer),
                u8::from(def.attributes.leakproof),
            ]);
            for estimate in [def.attributes.cost_bits, def.attributes.rows_bits] {
                match estimate {
                    Some(bits) => {
                        ok &= buffer.append(&[1]) && buffer.append(&bits.to_le_bytes());
                    }
                    None => ok &= buffer.append(&[0]),
                }
            }
            ok &= buffer.append(&[def.config_count as u8]);
            for config in def.configs() {
                ok &= name_bytes(buffer, config.name.as_str());
                let value = config.value.as_str().as_bytes();
                ok &= buffer.append(&(value.len() as u16).to_le_bytes()) && buffer.append(value);
            }
            ok &= buffer.append(&[def.kind.wire_code()]);
            if matches!(
                def.kind,
                crate::storage::RoutineKind::TableFunction
                    | crate::storage::RoutineKind::RecordFunction { .. }
            ) {
                ok &= def.result_column_count <= u8::MAX as usize
                    && buffer.append(&[def.result_column_count as u8]);
                for column in &def.result_columns[..def.result_column_count] {
                    ok &= name_bytes(buffer, column.name.as_str())
                        && buffer.append(&[column.ctype.code()]);
                    match column.user_type {
                        Some(identity) => {
                            ok &= buffer.append(&[1])
                                && name_bytes(buffer, identity.schema.as_str())
                                && name_bytes(buffer, identity.name.as_str());
                        }
                        None => ok &= buffer.append(&[0]),
                    }
                }
            }
            ok &= name_bytes(buffer, def.creation_path.as_str()) && dependencies.append(buffer);
            ok
        }
        WalOp::SetCast(definition) => {
            let mut ok = buffer.append(&definition.created_at.to_le_bytes())
                && append_routine_result(buffer, definition.source)
                && append_routine_result(buffer, definition.target);
            ok &= match definition.method {
                crate::storage::CastMethod::Function(slot) => {
                    buffer.append(b"f") && buffer.append(&slot.to_le_bytes())
                }
                crate::storage::CastMethod::Binary => buffer.append(b"b"),
                crate::storage::CastMethod::InOut => buffer.append(b"i"),
            };
            ok && buffer.append(&[definition.context.code()])
        }
        WalOp::DropCast { source, target } => {
            append_routine_result(buffer, *source) && append_routine_result(buffer, *target)
        }
        WalOp::SetOperator {
            created_at,
            definition,
        } => {
            let result = definition
                .implementation
                .result()
                .unwrap_or(crate::storage::RoutineResult::TEXT);
            let function = definition.implementation.routine().unwrap_or(0);
            buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.schema.as_str())
                && name_bytes(buffer, definition.name.as_str())
                && append_operator_signature(buffer, definition.signature)
                && append_routine_result(buffer, result)
                && buffer.append(&function.to_le_bytes())
                && buffer.append(&definition.commutator.unwrap_or(0).to_le_bytes())
                && buffer.append(&definition.negator.unwrap_or(0).to_le_bytes())
                && buffer
                    .append(&[u8::from(definition.hashes) | (u8::from(definition.merges) << 1)])
                && buffer.append(&definition.owner.to_le_bytes())
        }
        WalOp::DropOperator {
            schema,
            name,
            signature,
        } => {
            name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && append_operator_signature(buffer, *signature)
        }
        WalOp::SetCollation {
            slot,
            created_at,
            definition,
        } => {
            buffer.append(&[*slot])
                && buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.schema.as_str())
                && name_bytes(buffer, definition.name.as_str())
                && buffer.append(&definition.owner.to_le_bytes())
                && buffer.append(&[
                    definition.provider as u8,
                    u8::from(definition.deterministic),
                    definition
                        .encoding
                        .map_or(u8::MAX, |encoding| encoding.code() as u8),
                ])
                && name_bytes(buffer, definition.collate.as_str())
                && name_bytes(buffer, definition.ctype.as_str())
                && name_bytes(buffer, definition.locale.as_str())
                && name_bytes(buffer, definition.rules.as_str())
                && name_bytes(buffer, definition.version.as_str())
                && buffer.append(&[match definition.behavior {
                    crate::storage::CollationBehavior::Bytewise => 0,
                    crate::storage::CollationBehavior::Database => 1,
                }])
        }
        WalOp::DropCollation { schema, name } | WalOp::DropConversion { schema, name } => {
            name_bytes(buffer, schema) && name_bytes(buffer, name)
        }
        WalOp::SetConversion {
            slot,
            created_at,
            definition,
        } => {
            buffer.append(&[*slot])
                && buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.schema.as_str())
                && name_bytes(buffer, definition.name.as_str())
                && buffer.append(&definition.owner.to_le_bytes())
                && buffer.append(&[
                    definition.source.code() as u8,
                    definition.destination.code() as u8,
                ])
                && buffer.append(&definition.procedure.to_le_bytes())
                && buffer.append(&[u8::from(definition.default)])
        }
        WalOp::SetTextSearch {
            slot,
            created_at,
            definition,
        } => {
            buffer.append(&[*slot])
                && buffer.append(&created_at.to_le_bytes())
                && buffer.append(&[match definition.kind() {
                    crate::sql::ast::TextSearchObjectKind::Parser => 0,
                    crate::sql::ast::TextSearchObjectKind::Template => 1,
                    crate::sql::ast::TextSearchObjectKind::Dictionary => 2,
                    crate::sql::ast::TextSearchObjectKind::Configuration => 3,
                }])
                && append_text_search_definition(buffer, *definition)
        }
        WalOp::DropTextSearch { kind, schema, name } => {
            buffer.append(&[match kind {
                crate::sql::ast::TextSearchObjectKind::Parser => 0,
                crate::sql::ast::TextSearchObjectKind::Template => 1,
                crate::sql::ast::TextSearchObjectKind::Dictionary => 2,
                crate::sql::ast::TextSearchObjectKind::Configuration => 3,
            }]) && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
        }
        WalOp::SetEventTrigger {
            slot,
            created_at,
            definition,
        } => {
            buffer.append(&[*slot])
                && buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.name.as_str())
                && buffer.append(&[definition.event.code()])
                && buffer.append(&definition.function.to_le_bytes())
                && buffer.append(&[definition.enabled.code()])
                && buffer.append(&definition.ownership.committed().owner.to_le_bytes())
                && buffer.append(&[definition.tags.values().len() as u8])
                && definition
                    .tags
                    .values()
                    .iter()
                    .all(|tag| name_bytes(buffer, tag.as_str()))
        }
        WalOp::DropEventTrigger { name } => name_bytes(buffer, name),
        WalOp::SetOperatorFamily {
            created_at,
            definition,
        } => {
            let operator_count = definition
                .operators
                .iter()
                .filter(|member| member.used)
                .count();
            let function_count = definition
                .functions
                .iter()
                .filter(|member| member.used)
                .count();
            let mut ok = buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.schema.as_str())
                && name_bytes(buffer, definition.name.as_str())
                && buffer.append(&definition.owner.to_le_bytes())
                && buffer.append(&[operator_count as u8]);
            for member in definition.operators.iter().filter(|member| member.used) {
                ok &= buffer.append(&[member.strategy.number()])
                    && append_routine_result(buffer, member.left)
                    && append_routine_result(buffer, member.right)
                    && buffer.append(&member.operator.to_le_bytes());
            }
            ok &= buffer.append(&[function_count as u8]);
            for member in definition.functions.iter().filter(|member| member.used) {
                ok &= append_routine_result(buffer, member.left)
                    && append_routine_result(buffer, member.right)
                    && buffer.append(&member.function.to_le_bytes());
            }
            ok
        }
        WalOp::DropOperatorFamily { schema, name } | WalOp::DropOperatorClass { schema, name } => {
            name_bytes(buffer, schema) && name_bytes(buffer, name)
        }
        WalOp::SetOperatorClass {
            created_at,
            definition,
        } => {
            let operator_count = definition
                .operators
                .iter()
                .filter(|member| member.used)
                .count();
            let function_count = definition
                .functions
                .iter()
                .filter(|member| member.used)
                .count();
            let mut ok = buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, definition.schema.as_str())
                && name_bytes(buffer, definition.name.as_str())
                && buffer.append(&definition.owner.to_le_bytes())
                && buffer.append(&definition.family.to_le_bytes())
                && append_routine_result(buffer, definition.input)
                && append_routine_result(buffer, definition.storage)
                && buffer.append(&[u8::from(definition.default)])
                && buffer.append(&[operator_count as u8]);
            for member in definition.operators.iter().filter(|member| member.used) {
                ok &= buffer.append(&[member.strategy.number()])
                    && append_routine_result(buffer, member.left)
                    && append_routine_result(buffer, member.right)
                    && buffer.append(&member.operator.to_le_bytes());
            }
            ok &= buffer.append(&[function_count as u8]);
            for member in definition.functions.iter().filter(|member| member.used) {
                ok &= append_routine_result(buffer, member.left)
                    && append_routine_result(buffer, member.right)
                    && buffer.append(&member.function.to_le_bytes());
            }
            ok
        }
        WalOp::DropRoutine {
            schema,
            name,
            argument_signature,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(argument_signature)
        }
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_signature,
            new_schema,
            new_name,
        } => {
            name_bytes(buffer, name)
                && name_bytes(buffer, schema)
                && buffer.append(argument_signature)
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
            let password = attributes
                .password
                .unwrap_or(crate::storage::RolePassword::EMPTY);
            let flags = u16::from(attributes.superuser)
                | (u16::from(attributes.inherit) << 1)
                | (u16::from(attributes.create_role) << 2)
                | (u16::from(attributes.create_database) << 3)
                | (u16::from(attributes.can_login) << 4)
                | (u16::from(attributes.replication) << 5)
                | (u16::from(attributes.bypass_row_level_security) << 6)
                | (u16::from(attributes.password.is_some()) << 7)
                | (u16::from(attributes.valid_until.is_some()) << 8);
            name_bytes(buffer, name)
                && buffer.append(&flags.to_le_bytes())
                && buffer.append(&attributes.connection_limit.to_le_bytes())
                && buffer.append(&password.salt)
                && buffer.append(&password.stored_key)
                && buffer.append(&password.server_key)
                && buffer.append(&password.iterations.to_le_bytes())
                && name_bytes(
                    buffer,
                    attributes
                        .valid_until
                        .as_ref()
                        .map_or("", |value| value.as_str()),
                )
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
        WalOp::SetRoleSetting {
            role,
            database,
            name,
            value,
        } => {
            let flags = u8::from(database.is_some())
                | (u8::from(role.is_some()) << 1)
                | (u8::from(value.is_some()) << 2);
            buffer.append(&[flags])
                && role.is_none_or(|role| name_bytes(buffer, role))
                && database.is_none_or(|database| buffer.append(&database.to_le_bytes()))
                && name_bytes(buffer, name)
                && value.is_none_or(|value| {
                    buffer.append(&(value.len() as u16).to_le_bytes())
                        && buffer.append(value.as_bytes())
                })
        }
        WalOp::SetSystemSetting { name, value } => {
            name_bytes(buffer, name)
                && buffer.append(&[u8::from(value.is_some())])
                && value.is_none_or(|value| {
                    buffer.append(&(value.len() as u16).to_le_bytes())
                        && buffer.append(value.as_bytes())
                })
        }
        WalOp::SetObjectOwner {
            class,
            object_oid,
            schema,
            name,
            owner,
        } => {
            buffer.append(&[*class])
                && (!access_class_has_oid(*class) || buffer.append(&object_oid.to_le_bytes()))
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
                && (!access_class_has_oid(*class) || buffer.append(&object_oid.to_le_bytes()))
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && name_bytes(buffer, grantee)
                && name_bytes(buffer, grantor)
                && buffer.append(&privileges.0.to_le_bytes())
                && buffer.append(&grant_options.0.to_le_bytes())
        }
        WalOp::SetColumnAcl {
            class,
            schema,
            name,
            column,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            buffer.append(&[*class])
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && buffer.append(&column.to_le_bytes())
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
        WalOp::SetExtendedStatistics {
            created_at,
            schema,
            name,
            table_schema,
            table,
            target,
            kinds,
            expression_only,
            keys,
            key_count,
        } => {
            let mut ok = *key_count <= crate::storage::MAX_EXTENDED_STATISTICS_KEYS
                && buffer.append(&created_at.to_le_bytes())
                && name_bytes(buffer, schema)
                && name_bytes(buffer, name)
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
                && buffer.append(&target.map_or(-1i16, |value| value as i16).to_le_bytes())
                && buffer.append(&[*kinds, u8::from(*expression_only), *key_count as u8]);
            for key in &keys[..*key_count] {
                ok &= match key {
                    WalExtendedStatisticsKey::Column(column) => {
                        buffer.append(&[0]) && name_bytes(buffer, column)
                    }
                    WalExtendedStatisticsKey::Expression(expression) => {
                        buffer.append(&[1])
                            && u16::try_from(expression.len()).ok().is_some_and(|length| {
                                buffer.append(&length.to_le_bytes())
                                    && buffer.append(expression.as_bytes())
                            })
                    }
                };
            }
            ok
        }
        WalOp::DropExtendedStatistics { schema, name } => {
            name_bytes(buffer, schema) && name_bytes(buffer, name)
        }
        WalOp::AnalyzeExtendedStatistics {
            schema,
            name,
            statistics,
        } => name_bytes(buffer, schema) && name_bytes(buffer, name) && statistics.append(buffer),
        WalOp::CreateTrigger {
            name,
            target,
            table_schema,
            table,
            function_schema,
            function,
            or_replace,
            constraint,
            constraint_timing,
            referenced_schema,
            referenced_table,
            timing,
            level,
            events,
            update_columns,
            old_table,
            new_table,
            when,
            arguments,
            argument_count,
        } => {
            let when_len = when.map_or(0usize, |value| value.len() + 1);
            name_bytes(buffer, name)
                && buffer.append(&[target.code()])
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
                && name_bytes(buffer, function_schema)
                && name_bytes(buffer, function)
                && buffer.append(&[
                    u8::from(*or_replace),
                    u8::from(*constraint),
                    *constraint_timing,
                ])
                && name_bytes(buffer, referenced_schema.unwrap_or(""))
                && name_bytes(buffer, referenced_table.unwrap_or(""))
                && buffer.append(&[*timing, level.code(), events.bits()])
                && buffer.append(&update_columns.to_le_bytes())
                && name_bytes(buffer, old_table.unwrap_or(""))
                && name_bytes(buffer, new_table.unwrap_or(""))
                && u16::try_from(when_len)
                    .ok()
                    .is_some_and(|length| buffer.append(&length.to_le_bytes()))
                && when.is_none_or(|value| buffer.append(value.as_bytes()))
                && (*argument_count <= crate::storage::MAX_TRIGGER_ARGUMENTS)
                && buffer.append(&[*argument_count as u8])
                && arguments[..*argument_count]
                    .iter()
                    .all(|argument| name_bytes(buffer, argument))
        }
        WalOp::DropTrigger {
            name,
            target,
            table_schema,
            table,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&[target.code()])
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
        }
        WalOp::AlterTrigger {
            name,
            target,
            table_schema,
            table,
            new_name,
            enabled,
        } => {
            name_bytes(buffer, name)
                && buffer.append(&[target.code()])
                && name_bytes(buffer, table_schema)
                && name_bytes(buffer, table)
                && name_bytes(buffer, new_name)
                && buffer.append(&[*enabled])
        }
        WalOp::SetPolicy {
            schema,
            table,
            name,
            command,
            permissive,
            roles,
            role_count,
            using,
            with_check,
            dependencies,
        } => {
            let append_expression = |buffer: &mut FixedBuf, value: Option<&str>| {
                let length = value.map_or(u16::MAX, |source| source.len() as u16);
                buffer.append(&length.to_le_bytes())
                    && value.is_none_or(|source| buffer.append(source.as_bytes()))
            };
            name_bytes(buffer, schema)
                && name_bytes(buffer, table)
                && name_bytes(buffer, name)
                && buffer.append(&[*command, u8::from(*permissive)])
                && (*role_count <= crate::storage::MAX_POLICY_ROLES)
                && buffer.append(&[*role_count as u8])
                && roles[..*role_count]
                    .iter()
                    .all(|role| name_bytes(buffer, role.as_str()))
                && append_expression(buffer, *using)
                && append_expression(buffer, *with_check)
                && dependencies.append(buffer)
        }
        WalOp::DropPolicy {
            schema,
            table,
            name,
        } => name_bytes(buffer, schema) && name_bytes(buffer, table) && name_bytes(buffer, name),
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
        if payload.get(at..at + 4).is_none() {
            return false;
        }
        let identity = i32::from_le_bytes(payload[at..at + 4].try_into().expect("length checked"));
        if StoredDependencyIdentity::decode(
            DependencyClass::from_code(class).expect("validated class"),
            identity,
        )
        .is_none()
        {
            return false;
        }
        at += 4;
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
        let identity = StoredDependencyIdentity::decode(
            class,
            i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?),
        )?;
        at += 4;
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
            .serialized_push(SerializedStoredQueryDependency {
                class,
                identity,
                schema: SqlName::parse(schema).ok()?,
                name: SqlName::parse(name).ok()?,
                referenced_schema: SqlName::parse(referenced_schema).ok()?,
                referenced_name: SqlName::parse(referenced_name).ok()?,
                referenced_columns,
            })
            .ok()?;
    }
    Some(dependencies)
}

fn decode_table_statistics(payload: &[u8]) -> Option<TableStatistics> {
    if payload.first().copied() != Some(TABLE_STATISTICS_V3) {
        return None;
    }
    let mut at = 1usize;
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
    let mut statistics = TableStatistics {
        valid: true,
        rows,
        average_row_width,
        analyzed_generation,
        columns: [ColumnStatistics::EMPTY; MAX_COLUMNS],
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
    (at == payload.len()).then_some(statistics)
}

fn decode_extended_statistics_data(payload: &[u8]) -> Option<ExtendedStatisticsData> {
    let mut at = 0usize;
    let valid = match *payload.get(at)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    at += 1;
    let inherited = match *payload.get(at)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    at += 1;
    let take_u64 = |at: &mut usize| {
        let value = u64::from_le_bytes(payload.get(*at..*at + 8)?.try_into().ok()?);
        *at += 8;
        Some(value)
    };
    let analyzed_generation = take_u64(&mut at)?;
    let rows = take_u64(&mut at)?;
    let non_null_rows = take_u64(&mut at)?;
    let distinct_values = take_u64(&mut at)?;
    let mut data = ExtendedStatisticsData {
        valid,
        inherited,
        analyzed_generation,
        rows,
        non_null_rows,
        distinct_values,
        dependencies_ppm: [0; crate::storage::MAX_EXTENDED_STATISTICS_KEYS
            * crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
        expression_statistics: [ColumnStatistics::EMPTY;
            crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
        mcv: [ExtendedStatisticsMcv::EMPTY; crate::storage::MAX_EXTENDED_STATISTICS_MCV],
        n_mcv: 0,
    };
    for strength in &mut data.dependencies_ppm {
        *strength = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        if *strength > 1_000_000 {
            return None;
        }
    }
    for column in &mut data.expression_statistics {
        let column_valid = match *payload.get(at)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        at += 1;
        let null_fraction_ppm = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        let column_distinct = take_u64(&mut at)?;
        let distinct_fraction_ppm = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        let average_width = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        if null_fraction_ppm > 1_000_000 || distinct_fraction_ppm > 1_000_000 {
            return None;
        }
        *column = ColumnStatistics {
            valid: column_valid,
            null_fraction_ppm,
            distinct_values: column_distinct,
            distinct_fraction_ppm,
            average_width,
        };
    }
    let n_mcv = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    if n_mcv > crate::storage::MAX_EXTENDED_STATISTICS_MCV {
        return None;
    }
    for entry in &mut data.mcv[..n_mcv] {
        let hash = take_u64(&mut at)?;
        let count = take_u64(&mut at)?;
        let length = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        let value = core::str::from_utf8(payload.get(at..at + length)?).ok()?;
        at += length;
        let values = StackStr::from_str(value);
        if values.is_truncated() {
            return None;
        }
        *entry = ExtendedStatisticsMcv {
            valid: true,
            hash,
            count,
            values,
        };
    }
    data.n_mcv = n_mcv as u16;
    (at == payload.len()).then_some(data)
}

fn decode_subscription_behavior(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::SubscriptionBehavior> {
    let boolean = |value| match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let behavior = crate::storage::SubscriptionBehavior {
        binary: boolean(*payload.get(*at)?)?,
        streaming: crate::storage::SubscriptionStreaming::from_code(*payload.get(*at + 1)?)?,
        synchronous_commit: crate::storage::SubscriptionSynchronousCommit::from_code(
            *payload.get(*at + 2)?,
        )?,
        two_phase: boolean(*payload.get(*at + 3)?)?,
        disable_on_error: boolean(*payload.get(*at + 4)?)?,
        password_required: boolean(*payload.get(*at + 5)?)?,
        run_as_owner: boolean(*payload.get(*at + 6)?)?,
        origin: crate::storage::SubscriptionOrigin::from_code(*payload.get(*at + 7)?)?,
        failover: boolean(*payload.get(*at + 8)?)?,
        skip_lsn: match *payload.get(*at + 9)? {
            0 => None,
            1 => Some(u64::from_le_bytes(
                payload.get(*at + 10..*at + 18)?.try_into().ok()?,
            )),
            _ => return None,
        },
    };
    *at += 18;
    Some(behavior)
}

fn decode_index_definition(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::IndexMutableDefinition> {
    let tablespace = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
    *at += 2;
    let fillfactor = match *payload.get(*at)? {
        0 => None,
        value @ 10..=100 => Some(value),
        _ => return None,
    };
    *at += 1;
    let deduplicate_items = match *payload.get(*at)? {
        0 => None,
        1 => Some(false),
        2 => Some(true),
        _ => return None,
    };
    *at += 1;
    let mut statistics = [-1; MAX_INDEX_COLS];
    for statistic in &mut statistics {
        *statistic = i16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
        *at += 2;
        if !(-1..=10_000).contains(statistic) {
            return None;
        }
    }
    let parent = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
    *at += 2;
    let kind = match *payload.get(*at)? {
        0 => crate::storage::IndexKind::Ordinary,
        1 => crate::storage::IndexKind::Partitioned { valid: false },
        2 => crate::storage::IndexKind::Partitioned { valid: true },
        _ => return None,
    };
    *at += 1;
    let clustered = match *payload.get(*at)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    *at += 1;
    let replica_identity = match *payload.get(*at)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    *at += 1;
    if clustered && !matches!(kind, crate::storage::IndexKind::Ordinary) {
        return None;
    }
    Some(crate::storage::IndexMutableDefinition {
        tablespace,
        options: crate::storage::IndexStorageOptions {
            fillfactor,
            deduplicate_items,
        },
        statistics,
        parent: (parent != u16::MAX).then_some(parent),
        kind,
        clustered,
        replica_identity,
    })
}

fn decode_tablespace_options(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::TablespaceOptions> {
    let cost = |raw| {
        if raw == u64::MAX {
            Some(None)
        } else {
            crate::sql::ast::TablespaceCost::from_bits(raw).map(Some)
        }
    };
    let concurrency = |raw| (raw != i32::MIN).then_some(raw);
    let random_page_cost_bits = u64::from_le_bytes(payload.get(*at..*at + 8)?.try_into().ok()?);
    *at += 8;
    let seq_page_cost_bits = u64::from_le_bytes(payload.get(*at..*at + 8)?.try_into().ok()?);
    *at += 8;
    let effective_io_concurrency = i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
    *at += 4;
    let maintenance_io_concurrency =
        i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
    *at += 4;
    Some(crate::storage::TablespaceOptions {
        random_page_cost: cost(random_page_cost_bits)?,
        seq_page_cost: cost(seq_page_cost_bits)?,
        effective_io_concurrency: concurrency(effective_io_concurrency),
        maintenance_io_concurrency: concurrency(maintenance_io_concurrency),
    })
}

fn decode_database_definition(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::DatabaseDefinition> {
    let name_len = usize::from(*payload.get(*at)?);
    *at += 1;
    let name = core::str::from_utf8(payload.get(*at..*at + name_len)?).ok()?;
    *at += name_len;
    let encoding = crate::storage::DatabaseEncoding::from_code(*payload.get(*at)?)?;
    *at += 1;
    let locale_provider = crate::storage::DatabaseLocaleProvider::from_code(*payload.get(*at)?)?;
    *at += 1;
    let short = |at: &mut usize| -> Option<&str> {
        let len = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?) as usize;
        *at += 2;
        let value = core::str::from_utf8(payload.get(*at..*at + len)?).ok()?;
        *at += len;
        Some(value)
    };
    let collate = short(at)?;
    let ctype = short(at)?;
    let locale = short(at)?;
    let collation_version = short(at)?;
    let flags = *payload.get(*at)?;
    *at += 1;
    if flags & !3 != 0 {
        return None;
    }
    let connection_limit = i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
    *at += 4;
    let tablespace = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
    *at += 2;
    let collate = crate::util::StackStr::from_str(collate);
    let ctype = crate::util::StackStr::from_str(ctype);
    let locale = crate::util::StackStr::from_str(locale);
    let collation_version = crate::util::StackStr::from_str(collation_version);
    if collate.is_truncated()
        || ctype.is_truncated()
        || locale.is_truncated()
        || collation_version.is_truncated()
        || connection_limit < -1
    {
        return None;
    }
    Some(crate::storage::DatabaseDefinition {
        name: crate::storage::SqlName::parse(name).ok()?,
        encoding,
        locale_provider,
        collate,
        ctype,
        locale,
        collation_version,
        allow_connections: flags & 1 != 0,
        connection_limit,
        is_template: flags & 2 != 0,
        tablespace,
    })
}

fn decode_catalog_name<'a>(payload: &'a [u8], at: &mut usize) -> Option<&'a str> {
    let length = usize::from(*payload.get(*at)?);
    *at += 1;
    let value = core::str::from_utf8(payload.get(*at..*at + length)?).ok()?;
    *at += length;
    Some(value)
}

fn decode_routine_result(payload: &[u8], at: &mut usize) -> Option<crate::storage::RoutineResult> {
    let ctype = ColType::from_code(*payload.get(*at)?)?;
    *at += 1;
    let user_type = match *payload.get(*at)? {
        0 => {
            *at += 1;
            None
        }
        1 => {
            *at += 1;
            Some(crate::storage::UserTypeName {
                schema: SqlName::parse(decode_catalog_name(payload, at)?).ok()?,
                name: SqlName::parse(decode_catalog_name(payload, at)?).ok()?,
            })
        }
        _ => return None,
    };
    Some(crate::storage::RoutineResult { ctype, user_type })
}

fn decode_operator_signature(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::OperatorSignature> {
    let flags = *payload.get(*at)?;
    *at += 1;
    if flags & !3 != 0 || flags == 0 {
        return None;
    }
    let left = if flags & 1 != 0 {
        Some(decode_routine_result(payload, at)?)
    } else {
        None
    };
    let right = if flags & 2 != 0 {
        Some(decode_routine_result(payload, at)?)
    } else {
        None
    };
    Some(crate::storage::OperatorSignature { left, right })
}

fn decode_foreign_options(
    payload: &[u8],
    at: &mut usize,
) -> Option<crate::storage::foreign::ForeignOptions> {
    let count = *payload.get(*at)? as usize;
    *at += 1;
    if count > crate::storage::foreign::MAX_FOREIGN_OPTIONS {
        return None;
    }
    let mut options = crate::storage::foreign::ForeignOptions::EMPTY;
    for _ in 0..count {
        let name_len = *payload.get(*at)? as usize;
        *at += 1;
        let name = core::str::from_utf8(payload.get(*at..*at + name_len)?).ok()?;
        *at += name_len;
        let value_len = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?) as usize;
        *at += 2;
        let value = core::str::from_utf8(payload.get(*at..*at + value_len)?).ok()?;
        *at += value_len;
        options.restore_option(name, value).ok()?;
    }
    Some(options)
}

fn decode_optional_foreign_value(
    payload: &[u8],
    at: &mut usize,
) -> Option<Option<StackStr<{ crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX }>>> {
    match *payload.get(*at)? {
        0 => {
            *at += 1;
            Some(None)
        }
        1 => {
            *at += 1;
            let len = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?) as usize;
            *at += 2;
            let value = core::str::from_utf8(payload.get(*at..*at + len)?).ok()?;
            *at += len;
            let value = StackStr::from_str(value);
            (!value.is_truncated()).then_some(Some(value))
        }
        _ => None,
    }
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
        KIND_DATABASE_SCOPE => {
            let oid = i32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            at = 4;
            (at == payload.len()).then_some(WalOp::DatabaseScope { oid })
        }
        KIND_CREATE_LARGE_OBJECT => {
            let oid = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            let created_at = u64::from_le_bytes(payload.get(4..12)?.try_into().ok()?);
            let allocated = match *payload.get(12)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at = 13;
            (oid != 0 && at == payload.len()).then_some(WalOp::CreateLargeObject {
                oid,
                created_at,
                allocated,
            })
        }
        KIND_DROP_LARGE_OBJECT => {
            let oid = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            at = 4;
            (oid != 0 && at == payload.len()).then_some(WalOp::DropLargeObject { oid })
        }
        KIND_SET_FOREIGN_DATA_WRAPPER => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let definition = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let name = SqlName::parse(take_name(&mut at)?).ok()?;
                    let handler = match *payload.get(at)? {
                        0 => crate::storage::foreign::ForeignDataHandler::None,
                        1 => crate::storage::foreign::ForeignDataHandler::Postgres,
                        _ => return None,
                    };
                    at += 1;
                    let validator = match *payload.get(at)? {
                        0 => crate::storage::foreign::ForeignDataValidator::None,
                        1 => crate::storage::foreign::ForeignDataValidator::Postgres,
                        _ => return None,
                    };
                    at += 1;
                    Some(crate::storage::foreign::ForeignDataWrapperDefinition {
                        name,
                        handler,
                        validator,
                        options: decode_foreign_options(payload, &mut at)?,
                    })
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetForeignDataWrapper {
                slot,
                created_at,
                owner,
                definition,
            })
        }
        KIND_SET_FOREIGN_SERVER => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let definition = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let name = SqlName::parse(take_name(&mut at)?).ok()?;
                    let wrapper = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                    Some(crate::storage::foreign::ForeignServerDefinition {
                        name,
                        wrapper,
                        server_type: decode_optional_foreign_value(payload, &mut at)?,
                        version: decode_optional_foreign_value(payload, &mut at)?,
                        options: decode_foreign_options(payload, &mut at)?,
                    })
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetForeignServer {
                slot,
                created_at,
                owner,
                definition,
            })
        }
        KIND_SET_USER_MAPPING => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let server = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                    let user = match *payload.get(at)? {
                        0 => crate::storage::foreign::ForeignMappingUser::Public,
                        1 => {
                            let role =
                                u16::from_le_bytes(payload.get(at + 1..at + 3)?.try_into().ok()?);
                            crate::storage::foreign::ForeignMappingUser::Role(role)
                        }
                        _ => return None,
                    };
                    at += 3;
                    Some(crate::storage::foreign::UserMappingDefinition {
                        server,
                        user,
                        options: decode_foreign_options(payload, &mut at)?,
                    })
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetUserMapping {
                slot,
                created_at,
                definition,
            })
        }
        KIND_SET_FOREIGN_TABLE => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let table = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                    let server = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                    let options = decode_foreign_options(payload, &mut at)?;
                    let count = *payload.get(at)? as usize;
                    at += 1;
                    if count > crate::storage::foreign::MAX_FOREIGN_COLUMN_OPTIONS {
                        return None;
                    }
                    let mut column_options = crate::storage::foreign::ForeignColumnOptions::EMPTY;
                    for _ in 0..count {
                        let column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                        at += 2;
                        let one = decode_foreign_options(payload, &mut at)?;
                        if one.entries().len() != 1 {
                            return None;
                        }
                        column_options.append(column, one).ok()?;
                    }
                    Some(crate::storage::foreign::ForeignTableDefinition {
                        table,
                        server,
                        options,
                        column_options,
                    })
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetForeignTable {
                slot,
                created_at,
                definition,
            })
        }
        KIND_CREATE => {
            let name = take_name(&mut at)?;
            let n_cols = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            if n_cols > MAX_COLUMNS {
                return None;
            }
            let has_toast = *payload.get(at)? != 0;
            at += 1;
            let kind = match *payload.get(at)? {
                0 => crate::storage::TableKind::Local,
                1 => crate::storage::TableKind::Foreign,
                _ => return None,
            };
            at += 1;
            let tablespace = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let access_method = crate::storage::TableAccessMethod::from_code(*payload.get(at)?)?;
            at += 1;
            let mut def = TableDef {
                name: SqlName::parse(name).ok()?,
                columns: [ColumnMeta {
                    name: SqlName::parse("").ok()?,
                    ctype: ColType::Bool,
                    type_mod: -1,
                    collation: crate::sql::ast::Collation::None,
                    not_null: crate::storage::NotNullOrigin::Nullable,
                    unique: false,
                    primary: false,
                    auto_increment: false,
                    default: ColumnDefault::NONE,
                    is_identity: false,
                    identity_always: false,
                    auto_increment_step: 1,
                    user_type: None,
                    statistics_target: -1,
                }; MAX_COLUMNS],
                n_columns: n_cols,
                has_toast,
                kind,
                tablespace,
                access_method,
                ..TableDef::empty()
            };
            for i in 0..n_cols {
                let col_name = take_name(&mut at)?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                let not_null = crate::storage::NotNullOrigin::from_code(*payload.get(at)?)?;
                at += 1;
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
                let statistics_target =
                    i16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
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
                    collation: crate::sql::ast::Collation::None,
                    not_null,
                    unique: meta[1] & 2 != 0,
                    primary: meta[1] & 4 != 0,
                    auto_increment: meta[1] & 8 != 0,
                    default,
                    is_identity: meta[1] & 32 != 0,
                    identity_always: meta[1] & 64 != 0,
                    auto_increment_step,
                    user_type,
                    statistics_target,
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
                let meta = payload.get(at..at + 3)?;
                at += 3;
                let n = meta[1] as usize;
                if n > MAX_INDEX_COLS {
                    return None;
                }
                let mut uk = UniqueKey::EMPTY;
                uk.name = SqlName::parse(uname).ok()?;
                uk.is_primary = meta[0] != 0;
                uk.timing = crate::storage::ConstraintTiming::from_code(meta[2])?;
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
                let validation =
                    crate::storage::ConstraintValidation::from_code(*payload.get(at)?)?;
                at += 1;
                let elen =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let raw = payload.get(at..at + elen)?;
                at += elen;
                let text = core::str::from_utf8(raw).ok()?;
                let mut check = CheckConstraint::EMPTY;
                check.name = SqlName::parse(constraint_name).ok()?;
                check.validation = validation;
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
                let acts = payload.get(at..at + 4)?;
                at += 4;
                fk.on_delete = FkAction::from_code(acts[0])?;
                fk.on_update = FkAction::from_code(acts[1])?;
                fk.timing = crate::storage::ConstraintTiming::from_code(acts[2])?;
                fk.validation = crate::storage::ConstraintValidation::from_code(acts[3])?;
                def.fkeys[f] = fk;
            }
            let n_exclusions = *payload.get(at)? as usize;
            at += 1;
            if n_exclusions > crate::storage::MAX_EXCLUSIONS {
                return None;
            }
            def.n_exclusions = n_exclusions;
            for index in 0..n_exclusions {
                let mut exclusion = crate::storage::ExclusionConstraint::EMPTY;
                exclusion.name = SqlName::parse(take_name(&mut at)?).ok()?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                exclusion.n_cols = meta[0] as usize;
                if exclusion.n_cols == 0 || exclusion.n_cols > MAX_INDEX_COLS {
                    return None;
                }
                exclusion.timing = crate::storage::ConstraintTiming::from_code(meta[1])?;
                for position in 0..exclusion.n_cols {
                    exclusion.columns[position] =
                        u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                    exclusion.operators[position] =
                        crate::storage::ExclusionOperator::from_code(*payload.get(at)?)?;
                    at += 1;
                }
                let predicate_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
                if predicate_len != u16::MAX {
                    let predicate_len = predicate_len as usize;
                    let source = core::str::from_utf8(payload.get(at..at + predicate_len)?).ok()?;
                    at += predicate_len;
                    let predicate = crate::util::StackStr::from_str(source);
                    if predicate.is_truncated() {
                        return None;
                    }
                    exclusion.predicate = Some(predicate);
                }
                def.exclusions[index] = exclusion;
            }
            def.schema = SqlName::parse(take_name(&mut at)?).ok()?;
            for f in 0..def.n_fkeys {
                def.fkeys[f].parent_schema = SqlName::parse(take_name(&mut at)?).ok()?;
            }
            let collations = payload.get(at..at + n_cols)?;
            at += n_cols;
            for (column, code) in def.columns[..n_cols].iter_mut().zip(collations) {
                column.collation = match code {
                    0 => crate::sql::ast::Collation::Default,
                    1 => crate::sql::ast::Collation::C,
                    2 => crate::sql::ast::Collation::Posix,
                    3 => crate::sql::ast::Collation::UcsBasic,
                    4 => crate::sql::ast::Collation::None,
                    _ => return None,
                }
            }
            def.partition = decode_partition(payload, &mut at)?;
            def.row_level_security = crate::storage::RowLevelSecurityState {
                enabled: match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
                forced: match *payload.get(at + 1)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
            };
            at += 2;
            def.replica_identity =
                crate::storage::ReplicaIdentityMode::from_code(*payload.get(at)?)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::CreateTable(def))
        }
        KIND_REWRITE_TABLE => {
            let previous_schema = take_name(&mut at)?;
            let previous_name = take_name(&mut at)?;
            let preserve_rows = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let mut column_mapping = [u16::MAX; MAX_COLUMNS];
            for target in &mut column_mapping {
                *target = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
            }
            (at == payload.len()).then_some(WalOp::BeginTableRewrite {
                previous_schema,
                previous_name,
                preserve_rows,
                column_mapping,
            })
        }
        KIND_DROP => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
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
            let schema = take_name(&mut at)?;
            let is_update = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let old_row = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let length =
                        u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                    at += 4;
                    let old = payload.get(at..at + length)?;
                    at += length;
                    Some(old)
                }
                _ => return None,
            };
            let command_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
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
            let schema = take_name(&mut at)?;
            let old_row = match *payload.get(at)? {
                0 => {
                    at += 1;
                    None
                }
                1 => {
                    at += 1;
                    let length =
                        u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                    at += 4;
                    let old = payload.get(at..at + length)?;
                    at += length;
                    Some(old)
                }
                _ => return None,
            };
            let command_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
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
            let schema = take_name(&mut at)?;
            let path_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let raw = payload.get(at..at + path_len)?;
            at += path_len;
            let path = core::str::from_utf8(raw).ok()?;
            let security_invoker = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let encoded = payload.get(at..)?;
            if !validate_stored_query_dependencies(encoded) {
                return None;
            }
            at = payload.len();
            let dependencies = WalStoredQueryDependencies::Encoded(encoded);
            (at == payload.len()).then_some(WalOp::CreateView {
                schema,
                name,
                sql,
                path,
                security_invoker,
                dependencies,
            })
        }
        KIND_DROP_VIEW => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropView { schema, name })
        }
        KIND_SET_RULE => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let target = TriggerTargetKind::from_code(*payload.get(at)?)?;
            at += 1;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let event = crate::storage::RewriteEvent::from_code(*payload.get(at)?)?;
            at += 1;
            let mode = crate::storage::RewriteMode::from_code(*payload.get(at)?)?;
            at += 1;
            let source_len = usize::from(u16::from_le_bytes(
                payload.get(at..at + 2)?.try_into().ok()?,
            ));
            at += 2;
            let source = core::str::from_utf8(payload.get(at..at + source_len)?).ok()?;
            at += source_len;
            let condition_start = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let condition_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let valid_span = |span: crate::storage::RuleTextSpan| {
                let start = usize::from(span.start);
                let end = start.checked_add(usize::from(span.len));
                end.is_some_and(|end| {
                    end <= source.len()
                        && source.is_char_boundary(start)
                        && source.is_char_boundary(end)
                })
            };
            let condition = if condition_start == u16::MAX {
                if condition_len != 0 {
                    return None;
                }
                None
            } else {
                let span = crate::storage::RuleTextSpan {
                    start: condition_start,
                    len: condition_len,
                };
                valid_span(span).then_some(span)?.into()
            };
            let action_count = *payload.get(at)?;
            at += 1;
            if usize::from(action_count) > crate::storage::MAX_RULE_ACTIONS {
                return None;
            }
            let mut actions = [crate::storage::RuleTextSpan { start: 0, len: 0 };
                crate::storage::MAX_RULE_ACTIONS];
            for action in &mut actions[..usize::from(action_count)] {
                action.start = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
                action.len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
                if !valid_span(*action) {
                    return None;
                }
            }
            let returning_action = match *payload.get(at)? {
                u8::MAX => None,
                index if index < action_count => Some(index),
                _ => return None,
            };
            at += 1;
            let path_len = usize::from(u16::from_le_bytes(
                payload.get(at..at + 2)?.try_into().ok()?,
            ));
            at += 2;
            let path = core::str::from_utf8(payload.get(at..at + path_len)?).ok()?;
            at += path_len;
            let encoded = payload.get(at..)?;
            if !validate_stored_query_dependencies(encoded) {
                return None;
            }
            let dependencies = WalStoredQueryDependencies::Encoded(encoded);
            Some(WalOp::SetRule {
                slot,
                created_at,
                target,
                table_schema,
                table,
                name,
                event,
                mode,
                source,
                condition,
                actions,
                action_count,
                returning_action,
                path,
                dependencies,
            })
        }
        KIND_DROP_RULE => {
            let target = TriggerTargetKind::from_code(*payload.get(at)?)?;
            at += 1;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropRule {
                target,
                table_schema,
                table,
                name,
            })
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
            let mut table_column_masks = [0u64; crate::storage::MAX_PUBLICATION_TABLES];
            for mask in &mut table_column_masks[..count] {
                *mask = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
                at += 8;
            }
            let mut table_filter_sql = [StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES];
            for filter in &mut table_filter_sql[..count] {
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 2;
                core::fmt::Write::write_str(
                    filter,
                    core::str::from_utf8(payload.get(at..at + len)?).ok()?,
                )
                .ok()?;
                if filter.is_truncated() {
                    return None;
                }
                at += len;
            }
            let mut schemas = [u8::MAX; crate::storage::MAX_SCHEMAS];
            schemas[..schema_count].copy_from_slice(payload.get(at..at + schema_count)?);
            at += schema_count;
            (at == payload.len()).then_some(WalOp::CreatePublication {
                name,
                owner,
                all_tables: flags & 1 != 0,
                tables,
                table_column_masks,
                table_filter_sql,
                table_count: count,
                schemas,
                schema_count,
                publish_insert: flags & 2 != 0,
                publish_update: flags & 4 != 0,
                publish_delete: flags & 8 != 0,
                publish_truncate: flags & 16 != 0,
                publish_via_partition_root: flags & 32 != 0,
                publish_generated_columns: if flags & 64 != 0 {
                    crate::storage::PublishGeneratedColumns::Stored
                } else {
                    crate::storage::PublishGeneratedColumns::None
                },
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
            let mut table_column_masks = [0u64; crate::storage::MAX_PUBLICATION_TABLES];
            for mask in &mut table_column_masks[..count] {
                *mask = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
                at += 8;
            }
            let mut table_filter_sql = [StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES];
            for filter in &mut table_filter_sql[..count] {
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 2;
                core::fmt::Write::write_str(
                    filter,
                    core::str::from_utf8(payload.get(at..at + len)?).ok()?,
                )
                .ok()?;
                if filter.is_truncated() {
                    return None;
                }
                at += len;
            }
            let mut schemas = [u8::MAX; crate::storage::MAX_SCHEMAS];
            schemas[..schema_count].copy_from_slice(payload.get(at..at + schema_count)?);
            at += schema_count;
            (at == payload.len()).then_some(WalOp::AlterPublication {
                name,
                all_tables: flags & 1 != 0,
                tables,
                table_column_masks,
                table_filter_sql,
                table_count: count,
                schemas,
                schema_count,
                publish_insert: flags & 2 != 0,
                publish_update: flags & 4 != 0,
                publish_delete: flags & 8 != 0,
                publish_truncate: flags & 16 != 0,
                publish_via_partition_root: flags & 32 != 0,
                publish_generated_columns: if flags & 64 != 0 {
                    crate::storage::PublishGeneratedColumns::Stored
                } else {
                    crate::storage::PublishGeneratedColumns::None
                },
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
        KIND_CREATE_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let connection_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let connection = core::str::from_utf8(payload.get(at..at + connection_len)?).ok()?;
            at += connection_len;
            let count = *payload.get(at)? as usize;
            at += 1;
            if count > crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS {
                return None;
            }
            let mut publications =
                [crate::storage::SqlName::EMPTY; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
            for publication in &mut publications[..count] {
                *publication = crate::storage::SqlName::parse(take_name(&mut at)?).ok()?;
            }
            let enabled = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let bootstrap = crate::storage::SubscriptionBootstrap::from_code(*payload.get(at)?)?;
            at += 1;
            let behavior = decode_subscription_behavior(payload, &mut at)?;
            let slot_kind = *payload.get(at)?;
            at += 1;
            let slot = match slot_kind {
                0 => crate::storage::SubscriptionSlot::Absent,
                1 => crate::storage::SubscriptionSlot::External(
                    crate::storage::ReplicationSlotName::parse(take_name(&mut at)?).ok()?,
                ),
                2 => crate::storage::SubscriptionSlot::Managed(
                    crate::storage::ReplicationSlotName::parse(take_name(&mut at)?).ok()?,
                ),
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::CreateSubscription {
                name,
                owner,
                connection,
                publications,
                publication_count: count,
                enabled,
                slot,
                behavior,
                bootstrap,
            })
        }
        KIND_DROP_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropSubscription { name })
        }
        KIND_ADVANCE_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition_generation =
                u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let confirmed_lsn = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            (at == payload.len()).then_some(WalOp::AdvanceSubscription {
                name,
                created_at,
                definition_generation,
                confirmed_lsn,
            })
        }
        KIND_SET_SUBSCRIPTION_ENABLED => {
            let name = take_name(&mut at)?;
            let enabled = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            (at == payload.len()).then_some(WalOp::SetSubscriptionEnabled { name, enabled })
        }
        KIND_SET_SUBSCRIPTION_BOOTSTRAP => {
            let name = take_name(&mut at)?;
            let bootstrap = crate::storage::SubscriptionBootstrap::from_code(*payload.get(at)?)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::SetSubscriptionBootstrap { name, bootstrap })
        }
        KIND_RESET_SUBSCRIPTION_RELATIONS => {
            let name = take_name(&mut at)?;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition_generation =
                u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            (at == payload.len()).then_some(WalOp::ResetSubscriptionRelations {
                name,
                created_at,
                definition_generation,
            })
        }
        KIND_ADD_SUBSCRIPTION_RELATION => {
            let name = take_name(&mut at)?;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition_generation =
                u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::AddSubscriptionRelation {
                name,
                created_at,
                definition_generation,
                schema,
                table,
            })
        }
        KIND_COMPLETE_SUBSCRIPTION_CLEANUP => {
            let name = take_name(&mut at)?;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            (at == payload.len()).then_some(WalOp::CompleteSubscriptionCleanup { name, created_at })
        }
        KIND_FAIL_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let definition_generation =
                u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let sqlstate = core::str::from_utf8(payload.get(at..at + 5)?).ok()?;
            crate::sql::eval::SqlState::parse(sqlstate)?;
            at += 5;
            let message = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::FailSubscription {
                name,
                created_at,
                definition_generation,
                sqlstate,
                message,
            })
        }
        KIND_ALTER_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            let connection_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let connection = core::str::from_utf8(payload.get(at..at + connection_len)?).ok()?;
            at += connection_len;
            let count = *payload.get(at)? as usize;
            at += 1;
            if count > crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS {
                return None;
            }
            let mut publications =
                [crate::storage::SqlName::EMPTY; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
            for publication in &mut publications[..count] {
                *publication = crate::storage::SqlName::parse(take_name(&mut at)?).ok()?;
            }
            let behavior = decode_subscription_behavior(payload, &mut at)?;
            let slot_kind = *payload.get(at)?;
            at += 1;
            let slot = match slot_kind {
                0 => crate::storage::SubscriptionSlot::Absent,
                1 => crate::storage::SubscriptionSlot::External(
                    crate::storage::ReplicationSlotName::parse(take_name(&mut at)?).ok()?,
                ),
                2 => crate::storage::SubscriptionSlot::Managed(
                    crate::storage::ReplicationSlotName::parse(take_name(&mut at)?).ok()?,
                ),
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::AlterSubscription {
                name,
                connection,
                publications,
                publication_count: count,
                slot,
                behavior,
            })
        }
        KIND_SET_SUBSCRIPTION_OWNER => {
            let name = take_name(&mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::SetSubscriptionOwner { name, owner })
        }
        KIND_RENAME_SUBSCRIPTION => {
            let name = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::RenameSubscription { name, new_name })
        }
        KIND_CREATE_TRIGGER => {
            let name = take_name(&mut at)?;
            let target = TriggerTargetKind::from_code(*payload.get(at)?)?;
            at += 1;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let function_schema = take_name(&mut at)?;
            let function = take_name(&mut at)?;
            let or_replace = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            let constraint = match *payload.get(at + 1)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            let constraint_timing = *payload.get(at + 2)?;
            crate::storage::ConstraintTiming::from_code(constraint_timing)?;
            at += 3;
            let referenced_schema = take_name(&mut at)?;
            let referenced_table = take_name(&mut at)?;
            let referenced_schema = (!referenced_schema.is_empty()).then_some(referenced_schema);
            let referenced_table = (!referenced_table.is_empty()).then_some(referenced_table);
            if (!constraint
                && (constraint_timing != crate::storage::ConstraintTiming::NotDeferrable.code()
                    || referenced_schema.is_some()
                    || referenced_table.is_some()))
                || referenced_schema.is_some() != referenced_table.is_some()
            {
                return None;
            }
            let timing = *payload.get(at)?;
            let typed_timing = crate::sql::ast::TriggerTiming::from_code(timing)?;
            let level = crate::sql::ast::TriggerLevel::from_code(*payload.get(at + 1)?)?;
            let events = crate::sql::ast::TriggerEvents::from_bits(*payload.get(at + 2)?)?;
            at += 3;
            let update_columns = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let old_table = take_name(&mut at)?;
            let new_table = take_name(&mut at)?;
            let old_table = (!old_table.is_empty()).then_some(old_table);
            let new_table = (!new_table.is_empty()).then_some(new_table);
            let when_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let when = if when_len == 0 {
                None
            } else {
                let length = when_len - 1;
                let value = core::str::from_utf8(payload.get(at..at + length)?).ok()?;
                at += length;
                Some(value)
            };
            let argument_count = usize::from(*payload.get(at)?);
            at += 1;
            if argument_count > crate::storage::MAX_TRIGGER_ARGUMENTS {
                return None;
            }
            let mut arguments = [""; crate::storage::MAX_TRIGGER_ARGUMENTS];
            for argument in arguments.iter_mut().take(argument_count) {
                *argument = take_name(&mut at)?;
            }
            let transition_tables =
                crate::storage::TriggerTransitionTables::from_names(old_table, new_table)?;
            (!(or_replace && constraint)
                && crate::storage::trigger_shape_is_valid(
                    target == TriggerTargetKind::View,
                    constraint,
                    typed_timing,
                    level,
                    events,
                    update_columns,
                    transition_tables,
                )
                && at == payload.len())
            .then_some(WalOp::CreateTrigger {
                name,
                target,
                table_schema,
                table,
                function_schema,
                function,
                or_replace,
                constraint,
                constraint_timing,
                referenced_schema,
                referenced_table,
                timing,
                level,
                events,
                update_columns,
                old_table,
                new_table,
                when,
                arguments,
                argument_count,
            })
        }
        KIND_DROP_TRIGGER => {
            let name = take_name(&mut at)?;
            let target = TriggerTargetKind::from_code(*payload.get(at)?)?;
            at += 1;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropTrigger {
                name,
                target,
                table_schema,
                table,
            })
        }
        KIND_ALTER_TRIGGER => {
            let name = take_name(&mut at)?;
            let target = TriggerTargetKind::from_code(*payload.get(at)?)?;
            at += 1;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            let enabled = *payload.get(at)?;
            crate::storage::TriggerEnabled::from_code(enabled)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::AlterTrigger {
                name,
                target,
                table_schema,
                table,
                new_name,
                enabled,
            })
        }
        KIND_SET_POLICY => {
            let schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let command = *payload.get(at)?;
            crate::storage::PolicyCommandKind::from_code(command)?;
            let permissive = match *payload.get(at + 1)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 2;
            let role_count = usize::from(*payload.get(at)?);
            at += 1;
            if role_count == 0 || role_count > crate::storage::MAX_POLICY_ROLES {
                return None;
            }
            let mut roles = [SqlName::EMPTY; crate::storage::MAX_POLICY_ROLES];
            for role in &mut roles[..role_count] {
                *role = SqlName::parse(take_name(&mut at)?).ok()?;
            }
            let mut take_expression = || {
                let length = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
                if length == u16::MAX {
                    return Some(None);
                }
                let length = usize::from(length);
                let source = core::str::from_utf8(payload.get(at..at + length)?).ok()?;
                at += length;
                Some(Some(source))
            };
            let using = take_expression()?;
            let with_check = take_expression()?;
            let encoded = payload.get(at..)?;
            decode_stored_query_dependencies(encoded)?;
            at = payload.len();
            (at == payload.len()).then_some(WalOp::SetPolicy {
                schema,
                table,
                name,
                command,
                permissive,
                roles,
                role_count,
                using,
                with_check,
                dependencies: WalStoredQueryDependencies::Encoded(encoded),
            })
        }
        KIND_DROP_POLICY => {
            let schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropPolicy {
                schema,
                table,
                name,
            })
        }
        KIND_COMMIT if payload.is_empty() => Some(WalOp::Commit { transaction_id: 0 }),
        KIND_COMMIT => {
            let transaction_id = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            (payload.len() == 4).then_some(WalOp::Commit { transaction_id })
        }
        KIND_PREPARE_TRANSACTION => {
            let transaction_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let database = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let prepared_at = i64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let gid = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::PrepareTransaction {
                transaction_id,
                owner,
                database,
                prepared_at,
                gid,
            })
        }
        KIND_PREPARED_LOCKS => {
            let transaction_id = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let length = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
            at += 4;
            let encoded = payload.get(at..at + length)?;
            at += length;
            (at == payload.len()).then_some(WalOp::PreparedLocks {
                transaction_id,
                encoded,
            })
        }
        KIND_COMMIT_PREPARED => {
            let gid = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::CommitPrepared { gid })
        }
        KIND_ROLLBACK_PREPARED => {
            let gid = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::RollbackPrepared { gid })
        }
        KIND_CREATE_REPLICATION_SLOT => {
            let name = take_name(&mut at)?;
            let restart_lsn = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let behavior = crate::storage::ReplicationSlotBehavior::from_code(*payload.get(at)?)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::CreateReplicationSlot {
                name,
                restart_lsn,
                behavior,
            })
        }
        KIND_ALTER_REPLICATION_SLOT => {
            let name = take_name(&mut at)?;
            let behavior = crate::storage::ReplicationSlotBehavior::from_code(*payload.get(at)?)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::AlterReplicationSlot { name, behavior })
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
            let encoded = payload.get(at..)?;
            if !validate_stored_query_dependencies(encoded) {
                return None;
            }
            at = payload.len();
            let dependencies = WalStoredQueryDependencies::Encoded(encoded);
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
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
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
            let schema = take_name(&mut at)?;
            let mut descending = [false; MAX_INDEX_COLS];
            let mut nulls_first = [false; MAX_INDEX_COLS];
            let mut predicate = None;
            let mut include_columns = [0u16; MAX_INDEX_COLS];
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
            if *payload.get(at)? != 0xa3 {
                return None;
            }
            at += 1;
            let n_include_cols = *payload.get(at)? as usize;
            at += 1;
            if n_include_cols > MAX_INDEX_COLS {
                return None;
            }
            for column in include_columns.iter_mut().take(n_include_cols) {
                *column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
            }
            if *payload.get(at)? != 0xa4 {
                return None;
            }
            at += 1;
            let nulls_not_distinct = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
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
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 1;
                at += 1;
                let raw = payload.get(at..at + len)?;
                at += len;
                *expression = Some(core::str::from_utf8(raw).ok()?);
            }
            if *payload.get(at)? != 0xa6 {
                return None;
            }
            at += 1;
            let mut collations = [crate::sql::ast::Collation::Default; MAX_INDEX_COLS];
            let mut explicit_collations = [false; MAX_INDEX_COLS];
            for position in 0..n_cols {
                let code = *payload.get(at)?;
                at += 1;
                explicit_collations[position] = match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                at += 1;
                collations[position] = crate::sql::ast::Collation::from_code(code)?;
            }
            if *payload.get(at)? != 0xa7 {
                return None;
            }
            at += 1;
            let mut operator_classes = [None; MAX_INDEX_COLS];
            for operator_class in operator_classes.iter_mut().take(n_cols) {
                let tag = *payload.get(at)?;
                let value = payload.get(at + 1..at + 5)?;
                at += 5;
                *operator_class = match tag {
                    0 if value == [0, 0, 0, 0] => None,
                    1 if value[1..] == [0, 0, 0] => {
                        Some(crate::storage::IndexOperatorClass::Builtin(
                            BtreeOperatorClass::from_code(value[0])?,
                        ))
                    }
                    2 => Some(crate::storage::IndexOperatorClass::Catalog(
                        crate::storage::OperatorClassOid::parse(i32::from_le_bytes(
                            value.try_into().ok()?,
                        ))?,
                    )),
                    _ => return None,
                };
            }
            if *payload.get(at)? != 0xa9 {
                return None;
            }
            at += 1;
            let mut resolved_operator_classes = [None; MAX_INDEX_COLS];
            for operator_class in resolved_operator_classes.iter_mut().take(n_cols) {
                let tag = *payload.get(at)?;
                let value = payload.get(at + 1..at + 5)?;
                at += 5;
                *operator_class = match tag {
                    1 if value[1..] == [0, 0, 0] => {
                        Some(crate::storage::IndexOperatorClass::Builtin(
                            BtreeOperatorClass::from_code(value[0])?,
                        ))
                    }
                    2 => Some(crate::storage::IndexOperatorClass::Catalog(
                        crate::storage::OperatorClassOid::parse(i32::from_le_bytes(
                            value.try_into().ok()?,
                        ))?,
                    )),
                    _ => return None,
                };
            }
            if *payload.get(at)? != 0xa8 {
                return None;
            }
            at += 1;
            let definition = decode_index_definition(payload, &mut at)?;
            (at == payload.len()).then_some(WalOp::CreateIndex {
                created_at,
                schema,
                name,
                table,
                columns,
                expressions,
                include_columns,
                collations,
                explicit_collations,
                operator_classes,
                resolved_operator_classes,
                descending,
                nulls_first,
                n_cols,
                n_include_cols,
                nulls_not_distinct,
                predicate,
                unique,
                definition,
            })
        }
        KIND_ALTER_INDEX_DEFINITION => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let definition = decode_index_definition(payload, &mut at)?;
            (at == payload.len()).then_some(WalOp::AlterIndexDefinition {
                schema,
                name,
                definition,
            })
        }
        KIND_CREATE_TABLESPACE => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let name = take_name(&mut at)?;
            let location_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let location = core::str::from_utf8(payload.get(at..at + location_len)?).ok()?;
            at += location_len;
            let options = decode_tablespace_options(payload, &mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::CreateTablespace {
                created_at,
                name,
                location,
                options,
                owner,
            })
        }
        KIND_ALTER_TABLESPACE => {
            let name = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            let options = decode_tablespace_options(payload, &mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::AlterTablespace {
                name,
                new_name,
                options,
                owner,
            })
        }
        KIND_DROP_TABLESPACE => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropTablespace { name })
        }
        KIND_CREATE_DATABASE => {
            let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let template_oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let definition = decode_database_definition(payload, &mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::CreateDatabase {
                oid,
                template_oid,
                definition,
                owner,
            })
        }
        KIND_ALTER_DATABASE => {
            let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let definition = decode_database_definition(payload, &mut at)?;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            (at == payload.len()).then_some(WalOp::AlterDatabase {
                oid,
                definition,
                owner,
            })
        }
        KIND_DROP_DATABASE => {
            let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            (at == payload.len()).then_some(WalOp::DropDatabase { oid })
        }
        KIND_DROP_INDEX => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
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
            let schema = take_name(&mut at)?;
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
            if *payload.get(at)? != DOMAIN_PAYLOAD_WITH_BASE_SLOT {
                return None;
            }
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
            let base_domain = match (base_domain, base_domain_schema) {
                (None, None) => None,
                (Some(name), Some(schema)) => Some(crate::storage::UserTypeName { schema, name }),
                _ => return None,
            };
            let base_name = take_name(&mut at)?;
            let base_schema = take_name(&mut at)?;
            let base_user_type = match (base_name.is_empty(), base_schema.is_empty()) {
                (true, true) => None,
                (false, false) => Some(crate::storage::UserTypeName {
                    schema: SqlName::parse(base_schema).ok()?,
                    name: SqlName::parse(base_name).ok()?,
                }),
                _ => return None,
            };
            let base = crate::sql::types::ColType::from_code(*payload.get(at)?)?;
            at += 1;
            let base_slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let base = match (base_user_type, base, base_slot) {
                (Some(_), crate::sql::types::ColType::Enum(_), slot)
                    if slot != NO_DOMAIN_BASE_SLOT =>
                {
                    crate::sql::types::ColType::Enum(slot)
                }
                (Some(_), crate::sql::types::ColType::Composite(_), slot)
                    if slot != NO_DOMAIN_BASE_SLOT =>
                {
                    crate::sql::types::ColType::Composite(slot)
                }
                (None, base, NO_DOMAIN_BASE_SLOT) => base,
                _ => return None,
            };
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
                    validation: crate::storage::ConstraintValidation::EnforcedValidated,
                };
            }
            (at == payload.len()).then_some(WalOp::CreateDomain(crate::storage::DomainDef {
                database: crate::storage::DatabaseOid::POSTGRES,
                created_at: 0,
                schema: SqlName::parse(schema).ok()?,
                name: SqlName::parse(name).ok()?,
                ownership: crate::storage::Ownership::BOOTSTRAP,
                base_domain,
                base_user_type,
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
                database: crate::storage::DatabaseOid::POSTGRES,
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
        KIND_CREATE_COMPOSITE => {
            let slot = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let n_fields = *payload.get(at)? as usize;
            at += 1;
            if n_fields > crate::storage::MAX_COMPOSITE_FIELDS {
                return None;
            }
            let mut fields =
                [crate::storage::CompositeFieldDef::EMPTY; crate::storage::MAX_COMPOSITE_FIELDS];
            for field in fields.iter_mut().take(n_fields) {
                let field_name = take_name(&mut at)?;
                let attribute_number =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
                at += 2;
                let dropped = match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                at += 1;
                let not_null = match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                at += 1;
                let code = *payload.get(at)?;
                at += 1;
                let type_mod = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                let collation = crate::sql::ast::Collation::from_code(*payload.get(at)?)?;
                at += 1;
                let has_user_type = *payload.get(at)?;
                at += 1;
                let user_type = match has_user_type {
                    0 => None,
                    1 => Some(crate::storage::UserTypeName {
                        schema: SqlName::parse(take_name(&mut at)?).ok()?,
                        name: SqlName::parse(take_name(&mut at)?).ok()?,
                    }),
                    _ => return None,
                };
                *field = crate::storage::CompositeFieldDef {
                    attribute_number,
                    name: SqlName::parse(field_name).ok()?,
                    ctype: crate::sql::types::ColType::from_code(code)?,
                    type_mod,
                    collation,
                    user_type,
                    dropped,
                    not_null,
                };
            }
            (at == payload.len()).then_some(WalOp::CreateComposite {
                slot,
                definition: crate::storage::CompositeDef {
                    database: crate::storage::DatabaseOid::POSTGRES,
                    created_at: 0,
                    schema: SqlName::parse(schema).ok()?,
                    name: SqlName::parse(name).ok()?,
                    ownership: crate::storage::Ownership::BOOTSTRAP,
                    fields,
                    n_fields,
                    pending_definition: None,
                    ddl_state: crate::storage::CatalogDdlState::Absent,
                },
            })
        }
        KIND_DROP_COMPOSITE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropComposite { schema, name })
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
                let user_type = match *payload.get(at)? {
                    0 => None,
                    1 => {
                        at += 1;
                        Some(crate::storage::UserTypeName {
                            schema: SqlName::parse(take_name(&mut at)?).ok()?,
                            name: SqlName::parse(take_name(&mut at)?).ok()?,
                        })
                    }
                    _ => return None,
                };
                if user_type.is_none() {
                    at += 1;
                }
                *argument = crate::storage::RoutineArgumentDef {
                    name: SqlName::parse(argument_name).ok()?,
                    ctype,
                    user_type,
                };
            }
            let parameter_count = *payload.get(at)? as usize;
            at += 1;
            if parameter_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                return None;
            }
            let mut parameters =
                [crate::storage::RoutineParameterDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
            for parameter in parameters.iter_mut().take(parameter_count) {
                let parameter_name = take_name(&mut at)?;
                let ctype = ColType::from_code(*payload.get(at)?)?;
                at += 1;
                let user_type = match *payload.get(at)? {
                    0 => None,
                    1 => {
                        at += 1;
                        Some(crate::storage::UserTypeName {
                            schema: SqlName::parse(take_name(&mut at)?).ok()?,
                            name: SqlName::parse(take_name(&mut at)?).ok()?,
                        })
                    }
                    _ => return None,
                };
                if user_type.is_none() {
                    at += 1;
                }
                let mode_code = *payload.get(at)?;
                at += 1;
                let default_len =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 2;
                if default_len > crate::storage::ROUTINE_DEFAULT_MAX {
                    return None;
                }
                let default_text = core::str::from_utf8(payload.get(at..at + default_len)?).ok()?;
                at += default_len;
                let default =
                    (default_len != 0).then(|| crate::util::StackStr::from_str(default_text));
                *parameter = crate::storage::RoutineParameterDef {
                    name: SqlName::parse(parameter_name).ok()?,
                    ctype,
                    user_type,
                    mode: crate::storage::RoutineParameterMode::from_code(mode_code, default)?,
                };
            }
            let result_code = *payload.get(at)?;
            at += 1;
            let result_user_type = match *payload.get(at)? {
                0 => None,
                1 => {
                    at += 1;
                    Some(crate::storage::UserTypeName {
                        schema: SqlName::parse(take_name(&mut at)?).ok()?,
                        name: SqlName::parse(take_name(&mut at)?).ok()?,
                    })
                }
                _ => return None,
            };
            if result_user_type.is_none() {
                at += 1;
            }
            let result = crate::storage::RoutineResult {
                ctype: ColType::from_code(result_code)?,
                user_type: result_user_type,
            };
            let body_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            if body_len > crate::storage::ROUTINE_SQL_MAX {
                return None;
            }
            let body = core::str::from_utf8(payload.get(at..at + body_len)?).ok()?;
            at += body_len;
            let attributes = crate::storage::RoutineAttributes {
                strict: match *payload.get(at)? {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
                volatility: crate::storage::RoutineVolatility::from_code(*payload.get(at + 1)?)?,
                parallel: crate::storage::RoutineParallel::from_code(*payload.get(at + 2)?)?,
                security_definer: false,
                leakproof: false,
                cost_bits: None,
                rows_bits: None,
            };
            at += 3;
            let body_kind = crate::storage::RoutineBodyKind::from_code(*payload.get(at)?)?;
            let language = crate::storage::RoutineLanguage::from_code(*payload.get(at + 1)?)?;
            let security_definer = match *payload.get(at + 2)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            let leakproof = match *payload.get(at + 3)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 4;
            let mut estimates = [None; 2];
            for estimate in &mut estimates {
                *estimate = match *payload.get(at)? {
                    0 => {
                        at += 1;
                        None
                    }
                    1 => {
                        at += 1;
                        let bits = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
                        at += 8;
                        let value = f64::from_bits(bits);
                        Some((value.is_finite() && value > 0.0).then_some(bits)?)
                    }
                    _ => return None,
                };
            }
            let attributes = crate::storage::RoutineAttributes {
                security_definer,
                leakproof,
                cost_bits: estimates[0],
                rows_bits: estimates[1],
                ..attributes
            };
            let config_count = *payload.get(at)? as usize;
            at += 1;
            if config_count > crate::storage::MAX_ROUTINE_CONFIGS {
                return None;
            }
            let mut configs =
                [crate::storage::RoutineConfig::EMPTY; crate::storage::MAX_ROUTINE_CONFIGS];
            for config in &mut configs[..config_count] {
                let name = SqlName::parse(take_name(&mut at)?).ok()?;
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 2;
                if len > crate::storage::ROUTINE_CONFIG_VALUE_MAX {
                    return None;
                }
                let value = core::str::from_utf8(payload.get(at..at + len)?).ok()?;
                at += len;
                let value = crate::util::StackStr::from_str(value);
                if value.is_truncated() {
                    return None;
                }
                *config = crate::storage::RoutineConfig { name, value };
            }
            let mut result_columns =
                [crate::storage::RoutineArgumentDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut result_column_count = 0;
            let code = *payload.get(at)?;
            at += 1;
            let kind = if matches!(code, 3 | 6 | 7) {
                result_column_count = *payload.get(at)? as usize;
                at += 1;
                if result_column_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                    return None;
                }
                for column in result_columns.iter_mut().take(result_column_count) {
                    let name = take_name(&mut at)?;
                    let ctype = ColType::from_code(*payload.get(at)?)?;
                    at += 1;
                    let user_type = match *payload.get(at)? {
                        0 => None,
                        1 => {
                            at += 1;
                            Some(crate::storage::UserTypeName {
                                schema: SqlName::parse(take_name(&mut at)?).ok()?,
                                name: SqlName::parse(take_name(&mut at)?).ok()?,
                            })
                        }
                        _ => return None,
                    };
                    if user_type.is_none() {
                        at += 1;
                    }
                    *column = crate::storage::RoutineArgumentDef {
                        name: SqlName::parse(name).ok()?,
                        ctype,
                        user_type,
                    };
                }
                crate::storage::RoutineKind::from_wire_code(code, result)?
            } else if code == 5 {
                crate::storage::RoutineKind::Aggregate(
                    crate::storage::AggregateRoutine::decode_wire(body)?,
                )
            } else {
                crate::storage::RoutineKind::from_wire_code(code, result)?
            };
            let creation_path = crate::util::StackStr::from_str(take_name(&mut at)?);
            if creation_path.is_truncated() {
                return None;
            }
            let encoded_dependencies = payload.get(at..)?;
            if !validate_stored_query_dependencies(encoded_dependencies) {
                return None;
            }
            at = payload.len();
            (at == payload.len()).then_some(WalOp::CreateRoutine {
                definition: crate::storage::RoutineDef {
                    database: crate::storage::DatabaseOid::POSTGRES,
                    created_at,
                    schema: SqlName::parse(schema).ok()?,
                    name: SqlName::parse(name).ok()?,
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
                    body: crate::util::StackStr::from_str(body),
                    creation_path,
                    ownership: crate::storage::Ownership {
                        owner,
                        pending: None,
                    },
                    ddl_state: crate::storage::CatalogDdlState::Absent,
                },
                dependencies: WalStoredQueryDependencies::Encoded(encoded_dependencies),
            })
        }
        KIND_SET_CAST => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let source = decode_routine_result(payload, &mut at)?;
            let target = decode_routine_result(payload, &mut at)?;
            let method = match *payload.get(at)? {
                b'f' => {
                    at += 1;
                    let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                    at += 4;
                    if oid <= 0 {
                        return None;
                    }
                    crate::storage::CastMethod::Function(oid)
                }
                b'b' => {
                    at += 1;
                    crate::storage::CastMethod::Binary
                }
                b'i' => {
                    at += 1;
                    crate::storage::CastMethod::InOut
                }
                _ => return None,
            };
            let context = crate::storage::CastContext::from_code(*payload.get(at)?)?;
            at += 1;
            (at == payload.len()).then_some(WalOp::SetCast(crate::storage::CastDef {
                database: crate::storage::DatabaseOid::POSTGRES,
                created_at,
                source,
                target,
                method,
                context,
                ddl_state: crate::storage::CatalogDdlState::Absent,
            }))
        }
        KIND_DROP_CAST => {
            let source = decode_routine_result(payload, &mut at)?;
            let target = decode_routine_result(payload, &mut at)?;
            (at == payload.len()).then_some(WalOp::DropCast { source, target })
        }
        KIND_SET_OPERATOR => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let signature = decode_operator_signature(payload, &mut at)?;
            let result = decode_routine_result(payload, &mut at)?;
            let function = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let commutator = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let negator = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if function < 0 || commutator < 0 || negator < 0 {
                return None;
            }
            let flags = *payload.get(at)?;
            at += 1;
            if flags & !3 != 0 {
                return None;
            }
            let owner = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if owner <= 0 {
                return None;
            }
            (at == payload.len()).then_some(WalOp::SetOperator {
                created_at,
                definition: crate::storage::OperatorDefinition {
                    schema,
                    name,
                    signature,
                    implementation: if function == 0 {
                        crate::storage::OperatorImplementation::Shell
                    } else {
                        crate::storage::OperatorImplementation::Function {
                            routine: function,
                            result,
                        }
                    },
                    commutator: (commutator != 0).then_some(commutator),
                    negator: (negator != 0).then_some(negator),
                    hashes: flags & 1 != 0,
                    merges: flags & 2 != 0,
                    owner,
                },
            })
        }
        KIND_DROP_OPERATOR => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let signature = decode_operator_signature(payload, &mut at)?;
            (at == payload.len()).then_some(WalOp::DropOperator {
                schema,
                name,
                signature,
            })
        }
        KIND_SET_COLLATION => {
            let slot = *payload.get(at)?;
            at += 1;
            if usize::from(slot) >= crate::storage::MAX_COLLATIONS {
                return None;
            }
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let owner = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if owner <= 0 {
                return None;
            }
            let provider = match *payload.get(at)? {
                b'd' => crate::storage::CollationProvider::Default,
                b'b' => crate::storage::CollationProvider::Builtin,
                b'c' => crate::storage::CollationProvider::Libc,
                b'i' => crate::storage::CollationProvider::Icu,
                _ => return None,
            };
            at += 1;
            let deterministic = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let encoding = match *payload.get(at)? {
                u8::MAX => None,
                code => Some(crate::storage::PgEncoding::from_code(code)?),
            };
            at += 1;
            let fixed128 = |value: &str| {
                let value = StackStr::<128>::from_str(value);
                (!value.is_truncated()).then_some(value)
            };
            let collate = fixed128(take_name(&mut at)?)?;
            let ctype = fixed128(take_name(&mut at)?)?;
            let locale = fixed128(take_name(&mut at)?)?;
            let rules = fixed128(take_name(&mut at)?)?;
            let version_text = take_name(&mut at)?;
            let version = StackStr::<64>::from_str(version_text);
            if version.is_truncated() {
                return None;
            }
            let behavior = match *payload.get(at)? {
                0 => crate::storage::CollationBehavior::Bytewise,
                1 => crate::storage::CollationBehavior::Database,
                _ => return None,
            };
            at += 1;
            (at == payload.len()).then_some(WalOp::SetCollation {
                slot,
                created_at,
                definition: crate::storage::CollationDefinition {
                    schema,
                    name,
                    owner,
                    provider,
                    deterministic,
                    encoding,
                    collate,
                    ctype,
                    locale,
                    rules,
                    version,
                    behavior,
                },
            })
        }
        KIND_DROP_COLLATION | KIND_DROP_CONVERSION => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            if at != payload.len() {
                return None;
            }
            Some(if kind == KIND_DROP_COLLATION {
                WalOp::DropCollation { schema, name }
            } else {
                WalOp::DropConversion { schema, name }
            })
        }
        KIND_SET_CONVERSION => {
            let slot = *payload.get(at)?;
            at += 1;
            if usize::from(slot) >= crate::storage::MAX_CONVERSIONS {
                return None;
            }
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let owner = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if owner <= 0 {
                return None;
            }
            let source = crate::storage::PgEncoding::from_code(*payload.get(at)?)?;
            at += 1;
            let destination = crate::storage::PgEncoding::from_code(*payload.get(at)?)?;
            at += 1;
            let procedure = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let default = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            (at == payload.len()).then_some(WalOp::SetConversion {
                slot,
                created_at,
                definition: crate::storage::ConversionDefinition {
                    schema,
                    name,
                    owner,
                    source,
                    destination,
                    procedure,
                    default,
                },
            })
        }
        KIND_SET_TEXT_SEARCH => {
            let slot = *payload.get(at)?;
            at += 1;
            if usize::from(slot) >= crate::storage::MAX_TEXT_SEARCH_OBJECTS {
                return None;
            }
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let object_kind = *payload.get(at)?;
            at += 1;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            let read_i32 = |at: &mut usize| {
                let value = i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
                *at += 4;
                Some(value)
            };
            let read_behavior = |at: &mut usize| {
                let behavior = match (*payload.get(*at)?, *payload.get(*at + 1)?) {
                    (0, accept @ 0..=1) => crate::storage::TextSearchDictionaryBehavior::Simple {
                        accept: accept != 0,
                    },
                    (1, 0) => crate::storage::TextSearchDictionaryBehavior::EnglishStem,
                    _ => return None,
                };
                *at += 2;
                Some(behavior)
            };
            let definition = match object_kind {
                0 => crate::storage::TextSearchDefinition::Parser {
                    schema,
                    name,
                    oid,
                    start: read_i32(&mut at)?,
                    gettoken: read_i32(&mut at)?,
                    end: read_i32(&mut at)?,
                    headline: read_i32(&mut at)?,
                    lextypes: read_i32(&mut at)?,
                },
                1 => crate::storage::TextSearchDefinition::Template {
                    schema,
                    name,
                    oid,
                    init: read_i32(&mut at)?,
                    lexize: read_i32(&mut at)?,
                    behavior: read_behavior(&mut at)?,
                },
                2 => {
                    let owner = read_i32(&mut at)?;
                    let template = read_i32(&mut at)?;
                    let options = StackStr::<512>::from_str(take_name(&mut at)?);
                    if options.is_truncated() {
                        return None;
                    }
                    crate::storage::TextSearchDefinition::Dictionary {
                        schema,
                        name,
                        oid,
                        owner,
                        template,
                        options,
                        behavior: read_behavior(&mut at)?,
                    }
                }
                3 => {
                    let owner = read_i32(&mut at)?;
                    let parser = read_i32(&mut at)?;
                    let mut mappings = crate::storage::TextSearchMappings::EMPTY;
                    for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
                        let count = usize::from(*payload.get(at)?);
                        at += 1;
                        if count > crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN {
                            return None;
                        }
                        mappings.counts[token] = count as u8;
                        for dictionary in mappings.dictionaries[token].iter_mut().take(count) {
                            *dictionary = read_i32(&mut at)?;
                        }
                    }
                    crate::storage::TextSearchDefinition::Configuration {
                        schema,
                        name,
                        oid,
                        owner,
                        parser,
                        mappings,
                    }
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetTextSearch {
                slot,
                created_at,
                definition,
            })
        }
        KIND_DROP_TEXT_SEARCH => {
            let kind = match *payload.get(at)? {
                0 => crate::sql::ast::TextSearchObjectKind::Parser,
                1 => crate::sql::ast::TextSearchObjectKind::Template,
                2 => crate::sql::ast::TextSearchObjectKind::Dictionary,
                3 => crate::sql::ast::TextSearchObjectKind::Configuration,
                _ => return None,
            };
            at += 1;
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropTextSearch { kind, schema, name })
        }
        KIND_SET_EVENT_TRIGGER => {
            let slot = *payload.get(at)?;
            at += 1;
            if usize::from(slot) >= crate::storage::MAX_EVENT_TRIGGERS {
                return None;
            }
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let event = crate::sql::ast::EventTriggerEvent::from_code(*payload.get(at)?)?;
            at += 1;
            let function = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let enabled = crate::storage::TriggerEnabled::from_code(*payload.get(at)?)?;
            at += 1;
            let owner = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let tag_count = usize::from(*payload.get(at)?);
            at += 1;
            if tag_count > crate::storage::MAX_EVENT_TRIGGER_TAGS {
                return None;
            }
            let mut tag_values = [""; crate::storage::MAX_EVENT_TRIGGER_TAGS];
            for tag in tag_values.iter_mut().take(tag_count) {
                *tag = take_name(&mut at)?;
            }
            let tags = crate::storage::EventTriggerTags::parse(&tag_values[..tag_count]).ok()?;
            (at == payload.len()).then_some(WalOp::SetEventTrigger {
                slot,
                created_at,
                definition: crate::storage::EventTriggerDefinition {
                    name,
                    event,
                    function,
                    tags,
                    enabled,
                    ownership: crate::storage::Ownership {
                        owner,
                        pending: None,
                    },
                },
            })
        }
        KIND_DROP_EVENT_TRIGGER => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropEventTrigger { name })
        }
        KIND_SET_OPERATOR_FAMILY => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let owner = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if owner <= 0 {
                return None;
            }
            let operator_count = usize::from(*payload.get(at)?);
            at += 1;
            if operator_count > crate::storage::MAX_OPERATOR_FAMILY_MEMBERS {
                return None;
            }
            let mut operators = [crate::storage::OperatorFamilyOperator::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS];
            for member in &mut operators[..operator_count] {
                let strategy =
                    crate::sql::ast::BtreeStrategy::from_number(u32::from(*payload.get(at)?))?;
                at += 1;
                let left = decode_routine_result(payload, &mut at)?;
                let right = decode_routine_result(payload, &mut at)?;
                let operator = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                if operator <= 0 {
                    return None;
                }
                *member = crate::storage::OperatorFamilyOperator {
                    used: true,
                    strategy,
                    left,
                    right,
                    operator,
                };
            }
            let function_count = usize::from(*payload.get(at)?);
            at += 1;
            if function_count > crate::storage::MAX_OPERATOR_FAMILY_MEMBERS {
                return None;
            }
            let mut functions = [crate::storage::OperatorFamilyFunction::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS];
            for member in &mut functions[..function_count] {
                let left = decode_routine_result(payload, &mut at)?;
                let right = decode_routine_result(payload, &mut at)?;
                let function = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                if function <= 0 {
                    return None;
                }
                *member = crate::storage::OperatorFamilyFunction {
                    used: true,
                    left,
                    right,
                    function,
                };
            }
            (at == payload.len()).then_some(WalOp::SetOperatorFamily {
                created_at,
                definition: crate::storage::OperatorFamilyDefinition {
                    schema,
                    name,
                    owner,
                    operators,
                    functions,
                },
            })
        }
        KIND_DROP_OPERATOR_FAMILY | KIND_DROP_OPERATOR_CLASS => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            if at != payload.len() {
                return None;
            }
            Some(if kind == KIND_DROP_OPERATOR_FAMILY {
                WalOp::DropOperatorFamily { schema, name }
            } else {
                WalOp::DropOperatorClass { schema, name }
            })
        }
        KIND_SET_OPERATOR_CLASS => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = SqlName::parse(take_name(&mut at)?).ok()?;
            let name = SqlName::parse(take_name(&mut at)?).ok()?;
            let owner = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if owner <= 0 {
                return None;
            }
            let family = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            if family <= 0 {
                return None;
            }
            let input = decode_routine_result(payload, &mut at)?;
            let storage = decode_routine_result(payload, &mut at)?;
            let default = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let operator_count = usize::from(*payload.get(at)?);
            at += 1;
            if operator_count > crate::storage::MAX_OPERATOR_FAMILY_MEMBERS {
                return None;
            }
            let mut operators = [crate::storage::OperatorFamilyOperator::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS];
            for member in &mut operators[..operator_count] {
                let strategy =
                    crate::sql::ast::BtreeStrategy::from_number(u32::from(*payload.get(at)?))?;
                at += 1;
                let left = decode_routine_result(payload, &mut at)?;
                let right = decode_routine_result(payload, &mut at)?;
                let operator = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                if operator <= 0 {
                    return None;
                }
                *member = crate::storage::OperatorFamilyOperator {
                    used: true,
                    strategy,
                    left,
                    right,
                    operator,
                };
            }
            let function_count = usize::from(*payload.get(at)?);
            at += 1;
            if function_count > crate::storage::MAX_OPERATOR_FAMILY_MEMBERS {
                return None;
            }
            let mut functions = [crate::storage::OperatorFamilyFunction::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS];
            for member in &mut functions[..function_count] {
                let left = decode_routine_result(payload, &mut at)?;
                let right = decode_routine_result(payload, &mut at)?;
                let function = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                if function <= 0 {
                    return None;
                }
                *member = crate::storage::OperatorFamilyFunction {
                    used: true,
                    left,
                    right,
                    function,
                };
            }
            (at == payload.len()).then_some(WalOp::SetOperatorClass {
                created_at,
                definition: crate::storage::OperatorClassDefinition {
                    schema,
                    name,
                    owner,
                    family,
                    input,
                    storage,
                    default,
                    operators,
                    functions,
                },
            })
        }
        KIND_DROP_ROUTINE => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let argument_signature = payload.get(at..)?;
            at = payload.len();
            if at != payload.len() {
                return None;
            }
            Some(WalOp::DropRoutine {
                schema,
                name,
                argument_signature,
            })
        }
        KIND_ALTER_ROUTINE_IDENTITY => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let signature_start = at;
            let count = *payload.get(at)? as usize;
            at += 1;
            for _ in 0..count {
                at += 2;
                match *payload.get(at - 1)? {
                    0 => {}
                    1 => {
                        let _ = take_name(&mut at)?;
                        let _ = take_name(&mut at)?;
                    }
                    _ => return None,
                }
            }
            let argument_signature = payload.get(signature_start..at)?;
            let new_schema = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::AlterRoutineIdentity {
                schema,
                name,
                argument_signature,
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
        KIND_ALTER_ENUM_IDENTITY => {
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let new_schema = take_name(&mut at)?;
            let new_name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::AlterEnumIdentity {
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
        KIND_UPSERT_EXTENSION => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let name = take_name(&mut at)?;
            let schema = take_name(&mut at)?;
            let version = take_name(&mut at)?;
            let relocatable = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            let owner = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::UpsertExtension {
                name,
                schema,
                version,
                relocatable,
                owner,
                created_at,
            })
        }
        KIND_DROP_EXTENSION => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropExtension { name })
        }
        KIND_SET_EXTENSION_DEPENDENCY => {
            let extension = take_name(&mut at)?;
            let class = *payload.get(at)?;
            at += 1;
            let class = crate::storage::AccessClass::from_u8(class)?;
            let object_oid = if class == crate::storage::AccessClass::Routine {
                let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                oid
            } else {
                0
            };
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let kind = crate::storage::ExtensionDependencyKind::from_u8(*payload.get(at)?)?;
            let exists = match *payload.get(at + 1)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 2;
            (at == payload.len()).then_some(WalOp::SetExtensionDependency {
                extension,
                class,
                object_oid,
                schema,
                name,
                kind,
                exists,
            })
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
        KIND_SET_SEQUENCE_SCHEMA => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let new_schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::SetSequenceSchema {
                schema,
                name,
                new_schema,
            })
        }
        KIND_SET_VIEW_SCHEMA => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let new_schema = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::SetViewSchema {
                schema,
                name,
                new_schema,
            })
        }
        KIND_SET_EXTENSION_CONFIG => {
            let extension = take_name(&mut at)?;
            let ordinal = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let relation_kind =
                crate::storage::ExtensionConfigRelationKind::from_u8(*payload.get(at)?)?;
            at += 1;
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let condition_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
            at += 2;
            let condition = core::str::from_utf8(payload.get(at..at + condition_len)?).ok()?;
            at += condition_len;
            let exists = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            (at == payload.len()).then_some(WalOp::SetExtensionConfig {
                extension,
                ordinal,
                relation_kind,
                schema,
                name,
                condition,
                exists,
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
        KIND_SET_EXTENDED_STATISTICS => {
            let created_at = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
            at += 8;
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let table_schema = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let target_raw = i16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let target = match target_raw {
                -1 => None,
                0..=10_000 => Some(target_raw as u16),
                _ => return None,
            };
            let kinds = *payload.get(at)?;
            at += 1;
            let expression_only = match *payload.get(at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            at += 1;
            if (expression_only && kinds != 0)
                || (!expression_only
                    && crate::sql::ast::StatisticsKinds::from_code(kinds).is_none())
            {
                return None;
            }
            let key_count = *payload.get(at)? as usize;
            at += 1;
            if key_count == 0 || key_count > crate::storage::MAX_EXTENDED_STATISTICS_KEYS {
                return None;
            }
            let mut keys =
                [WalExtendedStatisticsKey::EMPTY; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
            for key in &mut keys[..key_count] {
                *key = match *payload.get(at)? {
                    0 => {
                        at += 1;
                        let column = take_name(&mut at)?;
                        WalExtendedStatisticsKey::Column(column)
                    }
                    1 => {
                        at += 1;
                        let length =
                            u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                        at += 2;
                        let expression =
                            core::str::from_utf8(payload.get(at..at + length)?).ok()?;
                        at += length;
                        WalExtendedStatisticsKey::Expression(expression)
                    }
                    _ => return None,
                };
            }
            (at == payload.len()).then_some(WalOp::SetExtendedStatistics {
                created_at,
                schema,
                name,
                table_schema,
                table,
                target,
                kinds,
                expression_only,
                keys,
                key_count,
            })
        }
        KIND_DROP_EXTENDED_STATISTICS => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropExtendedStatistics { schema, name })
        }
        KIND_ANALYZE_EXTENDED_STATISTICS => {
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let encoded = payload.get(at..)?;
            decode_extended_statistics_data(encoded)?;
            Some(WalOp::AnalyzeExtendedStatistics {
                schema,
                name,
                statistics: WalExtendedStatisticsData::Encoded(encoded),
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
            let password_present = flags & (1 << 7) != 0;
            let valid_until_present = flags & (1 << 8) != 0;
            let password = crate::storage::RolePassword {
                salt,
                stored_key,
                server_key,
                iterations,
            };
            if at != payload.len()
                || valid_until.len() > crate::storage::ROLE_VALID_UNTIL_MAX
                || (password_present && iterations == 0)
                || (!password_present && password != crate::storage::RolePassword::EMPTY)
                || (valid_until_present == valid_until.is_empty())
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
                    password: password_present.then_some(password),
                    valid_until: valid_until_present
                        .then(|| crate::util::StackStr::from_str(valid_until)),
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
        KIND_SET_ROLE_SETTING => {
            let flags = *payload.get(at)?;
            at += 1;
            if flags & !0x07 != 0 || flags & 0x02 == 0 && flags & 0x01 == 0 {
                return None;
            }
            let role = if flags & 0x02 != 0 {
                Some(take_name(&mut at)?)
            } else {
                None
            };
            let database = if flags & 0x01 != 0 {
                let oid = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                Some(oid)
            } else {
                None
            };
            let name = take_name(&mut at)?;
            let value = if flags & 0x04 != 0 {
                let len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                at += 2;
                let value = core::str::from_utf8(payload.get(at..at + len)?).ok()?;
                at += len;
                Some(value)
            } else {
                None
            };
            (at == payload.len()).then_some(WalOp::SetRoleSetting {
                role,
                database,
                name,
                value,
            })
        }
        KIND_SET_SYSTEM_SETTING => {
            let name = take_name(&mut at)?;
            let present = *payload.get(at)?;
            at += 1;
            let value = match present {
                0 => None,
                1 => {
                    let len =
                        u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
                    at += 2;
                    let value = core::str::from_utf8(payload.get(at..at + len)?).ok()?;
                    at += len;
                    Some(value)
                }
                _ => return None,
            };
            (at == payload.len()).then_some(WalOp::SetSystemSetting { name, value })
        }
        KIND_SET_OBJECT_OWNER => {
            let class = *payload.get(at)?;
            at += 1;
            crate::storage::AccessClass::from_u8(class)?;
            let object_oid = if access_class_has_oid(class) {
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
            let class = crate::storage::AccessClass::from_u8(class)?;
            let object_oid = if access_class_has_oid(class as u8) {
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
            if privileges & !crate::storage::all_object_privileges(class).0 != 0
                || grant_options & !privileges != 0
            {
                return None;
            }
            (at == payload.len()).then_some(WalOp::SetObjectAcl {
                class: class as u8,
                object_oid,
                schema,
                name,
                grantee,
                grantor,
                privileges: crate::storage::PrivilegeSet(privileges),
                grant_options: crate::storage::PrivilegeSet(grant_options),
            })
        }
        KIND_SET_COLUMN_ACL => {
            let class = *payload.get(at)?;
            at += 1;
            let class = crate::storage::AccessClass::from_u8(class)?;
            if !matches!(
                class,
                crate::storage::AccessClass::Table | crate::storage::AccessClass::MaterializedView
            ) {
                return None;
            }
            let schema = take_name(&mut at)?;
            let name = take_name(&mut at)?;
            let column = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let grantee = take_name(&mut at)?;
            let grantor = take_name(&mut at)?;
            let privileges = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let grant_options = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let allowed = crate::storage::PrivilegeSet::SELECT
                .union(crate::storage::PrivilegeSet::INSERT)
                .union(crate::storage::PrivilegeSet::UPDATE)
                .union(crate::storage::PrivilegeSet::REFERENCES);
            if privileges & !allowed.0 != 0 || grant_options & !privileges != 0 {
                return None;
            }
            (at == payload.len()).then_some(WalOp::SetColumnAcl {
                class: class as u8,
                schema,
                name,
                column,
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
            let typed_class = crate::storage::DefaultPrivilegeClass::from_u8(class)?;
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
            if privileges & !typed_class.all_privileges().0 != 0 || grant_options & !privileges != 0
            {
                return None;
            }
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
        Some(OwnedDatum::Int4(_)) | Some(OwnedDatum::Oid(_)) => 4,
        Some(OwnedDatum::Int8(_)) | Some(OwnedDatum::Float8(_)) => 8,
        Some(OwnedDatum::Regtype { len, .. }) => 5 + *len as usize,
        Some(OwnedDatum::RegObject { len, .. }) => 9 + *len as usize,
        Some(OwnedDatum::Date(_)) => 4,
        Some(OwnedDatum::Timestamp(_))
        | Some(OwnedDatum::Timestamptz(_))
        | Some(OwnedDatum::Time(_)) => 8,
        Some(OwnedDatum::Timetz(..)) => 12,
        Some(OwnedDatum::Interval(_)) | Some(OwnedDatum::Uuid(_)) => 16,
        Some(OwnedDatum::Text { len, .. }) => 1 + *len as usize,
        Some(OwnedDatum::TextSearch { len, .. }) => 2 + *len as usize,
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
        Some(OwnedDatum::Oid(v)) => {
            out[0] = 27;
            out[1..5].copy_from_slice(&v.to_le_bytes());
            5
        }
        Some(OwnedDatum::Int8(v)) => {
            out[0] = 4;
            out[1..9].copy_from_slice(&v.to_le_bytes());
            9
        }
        Some(OwnedDatum::Regtype {
            referenced_oid,
            len,
            bytes,
        }) => {
            out[0] = 25;
            out[1..5].copy_from_slice(&referenced_oid.to_le_bytes());
            out[5] = *len;
            out[6..6 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            6 + *len as usize
        }
        Some(OwnedDatum::RegObject {
            type_oid,
            referenced_oid,
            len,
            bytes,
        }) => {
            out[0] = 26;
            out[1..5].copy_from_slice(&type_oid.to_le_bytes());
            out[5..9].copy_from_slice(&referenced_oid.to_le_bytes());
            out[9] = *len;
            out[10..10 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            10 + *len as usize
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
        Some(OwnedDatum::TextSearch { query, len, bytes }) => {
            out[0] = 28;
            out[1] = u8::from(*query);
            out[2] = *len;
            out[3..3 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            3 + *len as usize
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
            let (base_code, base_user_slot) = match element {
                crate::sql::types::ArrElem::Domain {
                    base_code,
                    base_user_slot,
                    ..
                } => (*base_code, *base_user_slot),
                _ => (0, crate::sql::types::ColType::ENUM_SLOT_UNRESOLVED),
            };
            out[2] = base_code;
            out[3..5].copy_from_slice(&base_user_slot.to_le_bytes());
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
        27 => {
            let b = payload.get(*at..*at + 4)?;
            *at += 4;
            Some(OwnedDatum::Oid(u32::from_le_bytes(b.try_into().unwrap())))
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
            let base_user_slot =
                u16::from_le_bytes(payload.get(*at + 2..*at + 4)?.try_into().unwrap());
            let len = *payload.get(*at + 4)? as usize;
            *at += 5;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            let mut element = crate::sql::types::ArrElem::from_code(code)?;
            if let crate::sql::types::ArrElem::Domain { slot, .. } = element {
                crate::sql::types::ColType::from_code(base_code)?;
                element = crate::sql::types::ArrElem::Domain {
                    slot,
                    base_code,
                    base_user_slot,
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
        25 => {
            let referenced_oid = i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().unwrap());
            let len = *payload.get(*at + 4)? as usize;
            *at += 5;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::Regtype {
                referenced_oid,
                len: len as u8,
                bytes,
            })
        }
        26 => {
            let type_oid = i32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().unwrap());
            let referenced_oid =
                i32::from_le_bytes(payload.get(*at + 4..*at + 8)?.try_into().unwrap());
            let len = *payload.get(*at + 8)? as usize;
            *at += 9;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::RegObject {
                type_oid,
                referenced_oid,
                len: len as u8,
                bytes,
            })
        }
        28 => {
            let query = match *payload.get(*at)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            let len = *payload.get(*at + 1)? as usize;
            *at += 2;
            let bytes = decode_bounded_default_bytes(payload, at, len)?;
            core::str::from_utf8(&bytes[..len]).ok()?;
            Some(OwnedDatum::TextSearch {
                query,
                len: len as u8,
                bytes,
            })
        }
        _ => return None,
    })
}

fn decode_partition(payload: &[u8], at: &mut usize) -> Option<PartitionDef> {
    let flags = *payload.get(*at)?;
    *at += 1;
    if flags & !3 != 0 {
        return None;
    }
    let scheme = if flags & 1 != 0 {
        let strategy = match *payload.get(*at)? {
            0 => PartitionStrategy::Range,
            1 => PartitionStrategy::List,
            2 => PartitionStrategy::Hash,
            _ => return None,
        };
        *at += 1;
        let n_keys = *payload.get(*at)?;
        *at += 1;
        if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
            return None;
        }
        let mut keys = [0u16; crate::storage::MAX_PARTITION_KEYS];
        for key in &mut keys[..usize::from(n_keys)] {
            *key = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
            *at += 2;
        }
        Some(crate::storage::PartitionScheme {
            strategy,
            keys,
            n_keys,
        })
    } else {
        None
    };
    let attachment = if flags & 2 != 0 {
        let parent = u16::from_le_bytes(payload.get(*at..*at + 2)?.try_into().ok()?);
        *at += 2;
        let tag = *payload.get(*at)?;
        *at += 1;
        let bound = match tag {
            0 => PartitionBound::Default,
            1 => {
                let n_keys = *payload.get(*at)?;
                *at += 1;
                if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
                    return None;
                }
                let mut lower = [PartitionBoundValue::MinValue; crate::storage::MAX_PARTITION_KEYS];
                let mut upper = lower;
                for i in 0..usize::from(n_keys) {
                    lower[i] = decode_bound_value(payload, at)?;
                    upper[i] = decode_bound_value(payload, at)?;
                }
                PartitionBound::Range {
                    lower,
                    upper,
                    n_keys,
                }
            }
            2 => {
                let n_values = *payload.get(*at)?;
                *at += 1;
                if usize::from(n_values) > crate::storage::MAX_PARTITION_LIST_VALUES {
                    return None;
                }
                let mut values = [OwnedDatum::Null; crate::storage::MAX_PARTITION_LIST_VALUES];
                for value in &mut values[..usize::from(n_values)] {
                    *value = decode_default(payload, at)??;
                }
                PartitionBound::List { values, n_values }
            }
            3 => {
                let modulus = u32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
                *at += 4;
                let remainder = u32::from_le_bytes(payload.get(*at..*at + 4)?.try_into().ok()?);
                *at += 4;
                PartitionBound::Hash { modulus, remainder }
            }
            _ => return None,
        };
        Some(crate::storage::PartitionAttachment { parent, bound })
    } else {
        None
    };
    Some(PartitionDef { scheme, attachment })
}

fn decode_bound_value(payload: &[u8], at: &mut usize) -> Option<PartitionBoundValue> {
    let tag = *payload.get(*at)?;
    *at += 1;
    match tag {
        0 => Some(PartitionBoundValue::MinValue),
        1 => Some(PartitionBoundValue::Value(decode_default(payload, at)??)),
        2 => Some(PartitionBoundValue::MaxValue),
        _ => None,
    }
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

    fn encode_catalog_operation<'a>(operation: &WalOp<'_>, bytes: &'a mut [u8; 4096]) -> &'a [u8] {
        let mut budget = Budget::new(bytes.len());
        let mut payload = FixedBuf::new(
            &mut budget,
            "catalog operation payload",
            encoded_payload_len(operation),
        )
        .unwrap();
        assert!(append_payload(&mut payload, operation));
        assert_eq!(payload.len(), encoded_payload_len(operation));
        let len = payload.len();
        bytes[..len].copy_from_slice(payload.readable());
        &bytes[..len]
    }

    #[test]
    fn cast_and_operator_catalog_payloads_round_trip_strictly() {
        let public = SqlName::parse("public").unwrap();
        let custom = crate::storage::RoutineResult {
            ctype: ColType::Enum(7),
            user_type: Some(crate::storage::UserTypeName {
                schema: public,
                name: SqlName::parse("mood").unwrap(),
            }),
        };
        let binary_signature = crate::storage::OperatorSignature {
            left: Some(custom),
            right: Some(custom),
        };
        let operator = crate::storage::OperatorDefinition {
            schema: public,
            name: SqlName::parse("===").unwrap(),
            signature: binary_signature,
            implementation: crate::storage::OperatorImplementation::Function {
                routine: 701_003,
                result: crate::storage::RoutineResult::builtin(ColType::Bool),
            },
            commutator: Some(620_011),
            negator: None,
            hashes: true,
            merges: true,
            owner: 10,
        };
        let mut family = crate::storage::OperatorFamilyDefinition {
            schema: public,
            name: SqlName::parse("mood_ops").unwrap(),
            owner: 10,
            operators: [crate::storage::OperatorFamilyOperator::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
            functions: [crate::storage::OperatorFamilyFunction::EMPTY;
                crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
        };
        family.operators[0] = crate::storage::OperatorFamilyOperator {
            used: true,
            strategy: crate::sql::ast::BtreeStrategy::Equal,
            left: custom,
            right: custom,
            operator: 620_011,
        };
        family.functions[0] = crate::storage::OperatorFamilyFunction {
            used: true,
            left: custom,
            right: custom,
            function: 701_004,
        };
        let class = crate::storage::OperatorClassDefinition {
            schema: public,
            name: SqlName::parse("mood_ops").unwrap(),
            owner: 10,
            family: 640_012,
            input: custom,
            storage: custom,
            default: true,
            operators: family.operators,
            functions: family.functions,
        };
        let collation = crate::storage::CollationDefinition {
            schema: public,
            name: SqlName::parse("byte_order").unwrap(),
            owner: 10,
            provider: crate::storage::CollationProvider::Libc,
            deterministic: true,
            encoding: None,
            collate: StackStr::from_str("C"),
            ctype: StackStr::from_str("C"),
            locale: StackStr::from_str("C"),
            rules: StackStr::new(),
            version: StackStr::from_str("1"),
            behavior: crate::storage::CollationBehavior::Bytewise,
        };
        let conversion = crate::storage::ConversionDefinition {
            schema: public,
            name: SqlName::parse("latin1_to_utf8").unwrap(),
            owner: 10,
            source: crate::storage::PgEncoding::LATIN1,
            destination: crate::storage::PgEncoding::UTF8,
            procedure: 4374,
            default: true,
        };
        let event_trigger = crate::storage::EventTriggerDefinition {
            name: SqlName::parse("audit_ddl").unwrap(),
            event: crate::sql::ast::EventTriggerEvent::DdlCommandEnd,
            function: 7,
            tags: crate::storage::EventTriggerTags::parse(&["CREATE TABLE", "ALTER TABLE"])
                .unwrap(),
            enabled: crate::storage::TriggerEnabled::Always,
            ownership: crate::storage::Ownership {
                owner: 0,
                pending: None,
            },
        };
        let operations = [
            WalOp::SetCast(crate::storage::CastDef {
                database: crate::storage::DatabaseOid::POSTGRES,
                created_at: 9,
                source: custom,
                target: crate::storage::RoutineResult::TEXT,
                method: crate::storage::CastMethod::Function(701_002),
                context: crate::storage::CastContext::Assignment,
                ddl_state: crate::storage::CatalogDdlState::Present,
            }),
            WalOp::DropCast {
                source: custom,
                target: crate::storage::RoutineResult::TEXT,
            },
            WalOp::SetOperator {
                created_at: 11,
                definition: operator,
            },
            WalOp::DropOperator {
                schema: "public",
                name: "===",
                signature: binary_signature,
            },
            WalOp::SetOperatorFamily {
                created_at: 12,
                definition: family,
            },
            WalOp::DropOperatorFamily {
                schema: "public",
                name: "mood_ops",
            },
            WalOp::SetOperatorClass {
                created_at: 13,
                definition: class,
            },
            WalOp::DropOperatorClass {
                schema: "public",
                name: "mood_ops",
            },
            WalOp::SetCollation {
                slot: 7,
                created_at: 14,
                definition: collation,
            },
            WalOp::DropCollation {
                schema: "public",
                name: "byte_order",
            },
            WalOp::SetConversion {
                slot: 9,
                created_at: 15,
                definition: conversion,
            },
            WalOp::DropConversion {
                schema: "public",
                name: "latin1_to_utf8",
            },
            WalOp::SetEventTrigger {
                slot: 3,
                created_at: 16,
                definition: event_trigger,
            },
            WalOp::DropEventTrigger { name: "audit_ddl" },
        ];
        for operation in operations {
            let mut bytes = [0; 4096];
            let payload = encode_catalog_operation(&operation, &mut bytes);
            let kind = op_kind(&operation);
            assert!(decode_op(kind, payload).is_some());
            assert!(decode_op(kind, &payload[..payload.len() - 1]).is_none());
            let mut trailing = [0; 4097];
            trailing[..payload.len()].copy_from_slice(payload);
            assert!(decode_op(kind, &trailing[..payload.len() + 1]).is_none());
        }
    }

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
            Some(OwnedDatum::Regtype {
                referenced_oid: 23,
                len: 3,
                bytes: text,
            }),
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
            .serialized_push(SerializedStoredQueryDependency {
                class: DependencyClass::Table,
                identity: StoredDependencyIdentity::Name,
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("items").unwrap(),
                referenced_schema: SqlName::parse("").unwrap(),
                referenced_name: SqlName::parse("items").unwrap(),
                referenced_columns: 0b101,
            })
            .unwrap();
        dependencies
            .serialized_push(SerializedStoredQueryDependency {
                class: DependencyClass::Routine,
                identity: StoredDependencyIdentity::RoutineOid(
                    crate::storage::ROUTINE_OID_BASE + 7,
                ),
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("expanded").unwrap(),
                referenced_schema: SqlName::parse("").unwrap(),
                referenced_name: SqlName::parse("original_function").unwrap(),
                referenced_columns: 0,
            })
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
    fn subscription_advance_codec_retains_its_complete_stream_identity() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "subscription advance wal", 1024).unwrap();
        append_record(
            &mut buffer,
            9,
            &WalOp::AdvanceSubscription {
                name: "apply_changes",
                created_at: 41,
                definition_generation: 7,
                confirmed_lsn: 99,
            },
        )
        .unwrap();
        let WalOp::AdvanceSubscription {
            name,
            created_at,
            definition_generation,
            confirmed_lsn,
        } = decode_record(&buffer.readable()[16..]).unwrap()
        else {
            panic!("expected subscription advance WAL operation");
        };
        assert_eq!(name, "apply_changes");
        assert_eq!(created_at, 41);
        assert_eq!(definition_generation, 7);
        assert_eq!(confirmed_lsn, 99);
    }

    #[test]
    fn object_acl_codec_keeps_routine_identity() {
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

        let mut invalid_budget = Budget::new(1024);
        let mut invalid =
            FixedBuf::new(&mut invalid_budget, "invalid object acl wal", 1024).unwrap();
        assert!(append_payload(
            &mut invalid,
            &WalOp::SetObjectAcl {
                class: crate::storage::AccessClass::Table as u8,
                object_oid: 0,
                schema: "public",
                name: "documents",
                grantee: "reader",
                grantor: "postgres",
                privileges: crate::storage::PrivilegeSet::EXECUTE,
                grant_options: crate::storage::PrivilegeSet::NONE,
            }
        ));
        assert!(decode_op(KIND_SET_OBJECT_ACL, invalid.readable()).is_none());
    }

    #[test]
    fn large_object_codec_keeps_unsigned_identity_and_catalog_order() {
        let mut budget = Budget::new(2048);
        let mut buffer = FixedBuf::new(&mut budget, "large-object WAL", 2048).unwrap();
        append_record(
            &mut buffer,
            9,
            &WalOp::CreateLargeObject {
                oid: u32::MAX,
                created_at: 73,
                allocated: true,
            },
        )
        .unwrap();
        let WalOp::CreateLargeObject {
            oid,
            created_at,
            allocated,
        } = decode_record(&buffer.readable()[16..]).unwrap()
        else {
            panic!("expected large-object creation WAL operation");
        };
        assert_eq!((oid, created_at), (u32::MAX, 73));
        assert!(allocated);

        buffer.clear();
        append_record(
            &mut buffer,
            10,
            &WalOp::SetObjectAcl {
                class: crate::storage::AccessClass::LargeObject as u8,
                object_oid: -1,
                schema: "",
                name: "4294967295",
                grantee: "PUBLIC",
                grantor: "postgres",
                privileges: crate::storage::PrivilegeSet::SELECT,
                grant_options: crate::storage::PrivilegeSet::NONE,
            },
        )
        .unwrap();
        let WalOp::SetObjectAcl { object_oid, .. } =
            decode_record(&buffer.readable()[16..]).unwrap()
        else {
            panic!("expected large-object ACL WAL operation");
        };
        assert_eq!(object_oid as u32, u32::MAX);
    }

    #[test]
    fn newest_role_and_column_acl_records_replay_from_the_journal() {
        let dir = temp_dir("role-column-acl-replay");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.append_committed(
                1,
                &WalOp::SetRoleSetting {
                    role: Some("reader"),
                    database: Some(5),
                    name: "application_name",
                    value: Some("durable"),
                },
            )
            .unwrap();
            wal.append_committed(
                2,
                &WalOp::SetColumnAcl {
                    class: crate::storage::AccessClass::Table as u8,
                    schema: "public",
                    name: "documents",
                    column: 3,
                    grantee: "reader",
                    grantor: "postgres",
                    privileges: crate::storage::PrivilegeSet::SELECT,
                    grant_options: crate::storage::PrivilegeSet::NONE,
                },
            )
            .unwrap();
            wal.append_committed(3, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            wal.commit();
        }
        let mut replay_budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut replay_budget).unwrap();
        let seen = collect_replay_operations(&mut wal, 0);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].contains("SetRoleSetting"), "{}", seen[0]);
        assert!(seen[1].contains("SetColumnAcl"), "{}", seen[1]);
        assert!(seen[1].contains("column: 3"), "{}", seen[1]);
    }

    #[test]
    fn column_acl_codec_rejects_unexecutable_relation_classes() {
        let mut budget = Budget::new(1024);
        let mut payload = FixedBuf::new(&mut budget, "view column acl wal", 1024).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::SetColumnAcl {
                class: crate::storage::AccessClass::View as u8,
                schema: "public",
                name: "documents_view",
                column: 0,
                grantee: "reader",
                grantor: "postgres",
                privileges: crate::storage::PrivilegeSet::SELECT,
                grant_options: crate::storage::PrivilegeSet::NONE,
            }
        ));
        assert!(decode_op(KIND_SET_COLUMN_ACL, payload.readable()).is_none());
    }

    #[test]
    fn extension_catalog_wal_round_trips_typed_dependencies() {
        let mut budget = Budget::new(4096);
        let mut buffer = FixedBuf::new(&mut budget, "extension wal", 4096).unwrap();
        append_record(
            &mut buffer,
            1,
            &WalOp::UpsertExtension {
                name: "typed_ext",
                schema: "extensions",
                version: "2.0",
                relocatable: true,
                owner: "postgres",
                created_at: 47,
            },
        )
        .unwrap();
        let Some(WalOp::UpsertExtension {
            name,
            schema,
            version,
            relocatable,
            owner,
            created_at,
        }) = decode_record(&buffer.readable()[16..])
        else {
            panic!("expected extension definition WAL operation");
        };
        assert_eq!(
            (name, schema, version, relocatable, owner, created_at),
            ("typed_ext", "extensions", "2.0", true, "postgres", 47)
        );

        buffer.clear();
        append_record(
            &mut buffer,
            2,
            &WalOp::SetExtensionDependency {
                extension: "typed_ext",
                class: crate::storage::AccessClass::Routine,
                object_oid: 100_007,
                schema: "extensions",
                name: "typed_identity",
                kind: crate::storage::ExtensionDependencyKind::Automatic,
                exists: true,
            },
        )
        .unwrap();
        let Some(WalOp::SetExtensionDependency {
            extension,
            class,
            object_oid,
            schema,
            name,
            kind,
            exists,
        }) = decode_record(&buffer.readable()[16..])
        else {
            panic!("expected extension dependency WAL operation");
        };
        assert_eq!(extension, "typed_ext");
        assert_eq!(class, crate::storage::AccessClass::Routine);
        assert_eq!(object_oid, 100_007);
        assert_eq!((schema, name), ("extensions", "typed_identity"));
        assert_eq!(kind, crate::storage::ExtensionDependencyKind::Automatic);
        assert!(exists);

        buffer.clear();
        append_record(
            &mut buffer,
            3,
            &WalOp::SetExtensionConfig {
                extension: "typed_ext",
                ordinal: 4,
                relation_kind: crate::storage::ExtensionConfigRelationKind::Table,
                schema: "extensions",
                name: "typed_config",
                condition: "WHERE NOT built_in",
                exists: true,
            },
        )
        .unwrap();
        let Some(WalOp::SetExtensionConfig {
            extension,
            ordinal,
            relation_kind,
            schema,
            name,
            condition,
            exists,
        }) = decode_record(&buffer.readable()[16..])
        else {
            panic!("expected extension configuration WAL operation");
        };
        assert_eq!(extension, "typed_ext");
        assert_eq!(ordinal, 4);
        assert_eq!(
            relation_kind,
            crate::storage::ExtensionConfigRelationKind::Table
        );
        assert_eq!((schema, name), ("extensions", "typed_config"));
        assert_eq!(condition, "WHERE NOT built_in");
        assert!(exists);

        buffer.clear();
        append_record(&mut buffer, 4, &WalOp::DropExtension { name: "typed_ext" }).unwrap();
        assert!(matches!(
            decode_record(&buffer.readable()[16..]),
            Some(WalOp::DropExtension { name: "typed_ext" })
        ));
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
                collation: crate::sql::ast::Collation::None,
                not_null: crate::storage::NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type: None,
                statistics_target: -1,
            }; MAX_COLUMNS],
            n_columns: 2,
            ..TableDef::empty()
        };
        def.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
            not_null: crate::storage::NotNullOrigin::Local,
            unique: true,
            primary: true,
            auto_increment: false,
            default: ColumnDefault::NONE,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type: None,
            statistics_target: -1,
        };
        def.columns[1] = ColumnMeta {
            name: SqlName::parse("v").unwrap(),
            ctype: ColType::Text,
            type_mod: -1,
            collation: crate::sql::ast::Collation::C,
            not_null: crate::storage::NotNullOrigin::Nullable,
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
            statistics_target: -1,
        };
        // Exercise every durable constraint state in one table record.
        let mut uk = UniqueKey::EMPTY;
        uk.name = SqlName::parse("t_id_v_key").unwrap();
        uk.columns[0] = 0;
        uk.columns[1] = 1;
        uk.n_cols = 2;
        uk.timing = crate::storage::ConstraintTiming::DeferrableDeferred;
        def.uniques[0] = uk;
        def.n_uniques = 1;
        let mut check = CheckConstraint::EMPTY;
        check.name = SqlName::parse("t_check").unwrap();
        core::fmt::Write::write_str(&mut check.expression, "id > 0").unwrap();
        check.validation = crate::storage::ConstraintValidation::NotEnforced;
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
        fk.timing = crate::storage::ConstraintTiming::DeferrableImmediate;
        fk.validation = crate::storage::ConstraintValidation::EnforcedNotValid;
        def.fkeys[0] = fk;
        def.n_fkeys = 1;
        let mut exclusion = crate::storage::ExclusionConstraint::EMPTY;
        exclusion.name = SqlName::parse("t_id_excl").unwrap();
        exclusion.columns[0] = 0;
        exclusion.operators[0] = crate::storage::ExclusionOperator::Adjacent;
        exclusion.n_cols = 1;
        exclusion.predicate = Some(crate::util::StackStr::from_str("id > 0"));
        exclusion.timing = crate::storage::ConstraintTiming::DeferrableDeferred;
        def.exclusions[0] = exclusion;
        def.n_exclusions = 1;
        def
    }

    fn collect_replay(wal: &mut Wal) -> Vec<String> {
        collect_replay_from(wal, 0)
    }

    fn collect_replay_from(wal: &mut Wal, floor: u64) -> Vec<String> {
        let mut seen = Vec::new();
        wal.replay(floor, |lsn, record| {
            let operation = decode_record(record).unwrap();
            seen.push(format!("{lsn}:{operation:?}"));
            Ok(())
        })
        .unwrap();
        seen
    }

    fn collect_replay_operations(wal: &mut Wal, floor: u64) -> Vec<String> {
        collect_replay_from(wal, floor)
            .into_iter()
            .filter(|record| !record.contains(":Commit {"))
            .collect()
    }

    #[test]
    fn truncated_table_payload_is_rejected() {
        let definition = sample_def();
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(&mut budget, "truncated table payload", 4096).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTable(definition)
        ));
        let truncated_len = payload.len() - definition.n_columns;
        assert!(decode_op(KIND_CREATE, &payload.readable()[..truncated_len]).is_none());
    }

    #[test]
    fn table_payload_round_trip_preserves_typed_partition_bounds() {
        let mut definition = sample_def();
        definition.columns[0].not_null = crate::storage::NotNullOrigin::LocalAndInherited;
        definition.partition = PartitionDef::child(
            4,
            PartitionBound::Range {
                lower: [PartitionBoundValue::Value(OwnedDatum::Int4(10));
                    crate::storage::MAX_PARTITION_KEYS],
                upper: [PartitionBoundValue::Value(OwnedDatum::Int4(20));
                    crate::storage::MAX_PARTITION_KEYS],
                n_keys: 1,
            },
        );
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(&mut budget, "partition table payload", 4096).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTable(definition)
        ));
        let Some(WalOp::CreateTable(restored)) = decode_op(KIND_CREATE, payload.readable()) else {
            panic!("partition table definition did not decode")
        };
        let Some(crate::storage::PartitionAttachment {
            parent,
            bound:
                PartitionBound::Range {
                    lower,
                    upper,
                    n_keys,
                },
        }) = restored.partition.attachment
        else {
            panic!("partition metadata lost")
        };
        assert_eq!((parent, n_keys), (4, 1));
        assert_eq!(
            restored.columns[0].not_null,
            crate::storage::NotNullOrigin::LocalAndInherited
        );
        assert_eq!(restored.n_exclusions, 1);
        assert_eq!(
            (
                restored.exclusions[0].operators[0],
                restored.exclusions[0].timing,
                restored.exclusions[0]
                    .predicate
                    .as_ref()
                    .map(|predicate| predicate.as_str()),
            ),
            (
                crate::storage::ExclusionOperator::Adjacent,
                crate::storage::ConstraintTiming::DeferrableDeferred,
                Some("id > 0"),
            )
        );
        assert_eq!(
            restored.uniques[0].timing,
            crate::storage::ConstraintTiming::DeferrableDeferred
        );
        assert_eq!(
            restored.checks[0].validation,
            crate::storage::ConstraintValidation::NotEnforced
        );
        assert_eq!(
            (restored.fkeys[0].timing, restored.fkeys[0].validation,),
            (
                crate::storage::ConstraintTiming::DeferrableImmediate,
                crate::storage::ConstraintValidation::EnforcedNotValid,
            )
        );
        assert!(matches!(
            lower[0],
            PartitionBoundValue::Value(OwnedDatum::Int4(10))
        ));
        assert!(matches!(
            upper[0],
            PartitionBoundValue::Value(OwnedDatum::Int4(20))
        ));
    }

    #[test]
    fn create_trigger_payload_requires_typed_qualification() {
        let mut budget = Budget::new(1024);
        let mut payload = FixedBuf::new(&mut budget, "trigger payload", 1024).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTrigger {
                name: "audit_row",
                target: TriggerTargetKind::Table,
                table_schema: "public",
                table: "orders",
                function_schema: "public",
                function: "audit_order",
                or_replace: false,
                constraint: false,
                constraint_timing: crate::storage::ConstraintTiming::NotDeferrable.code(),
                referenced_schema: None,
                referenced_table: None,
                timing: 0,
                level: crate::sql::ast::TriggerLevel::Row,
                events: crate::sql::ast::TriggerEvents::from_bits(1).unwrap(),
                update_columns: 0,
                old_table: None,
                new_table: None,
                when: None,
                arguments: [""; crate::storage::MAX_TRIGGER_ARGUMENTS],
                argument_count: 0,
            },
        ));
        let incomplete_payload = &payload.readable()[..payload.len() - 10];
        assert!(decode_op(KIND_CREATE_TRIGGER, incomplete_payload).is_none());
    }

    #[test]
    fn create_trigger_payload_retains_transition_table_aliases() {
        let mut budget = Budget::new(1024);
        let mut payload = FixedBuf::new(&mut budget, "transition trigger payload", 1024).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTrigger {
                name: "audit_statement",
                target: TriggerTargetKind::Table,
                table_schema: "public",
                table: "orders",
                function_schema: "public",
                function: "audit_orders",
                or_replace: false,
                constraint: false,
                constraint_timing: crate::storage::ConstraintTiming::NotDeferrable.code(),
                referenced_schema: None,
                referenced_table: None,
                timing: 1,
                level: crate::sql::ast::TriggerLevel::Statement,
                events: crate::sql::ast::TriggerEvents::from_bits(2).unwrap(),
                update_columns: 0,
                old_table: Some("old_orders"),
                new_table: Some("new_orders"),
                when: None,
                arguments: [""; crate::storage::MAX_TRIGGER_ARGUMENTS],
                argument_count: 0,
            },
        ));
        let Some(WalOp::CreateTrigger {
            old_table,
            new_table,
            ..
        }) = decode_op(KIND_CREATE_TRIGGER, payload.readable())
        else {
            panic!("transition trigger did not decode");
        };
        assert_eq!(old_table, Some("old_orders"));
        assert_eq!(new_table, Some("new_orders"));
    }

    #[test]
    fn create_view_trigger_payload_retains_its_relation_kind() {
        let mut budget = Budget::new(1024);
        let mut payload = FixedBuf::new(&mut budget, "view trigger payload", 1024).unwrap();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTrigger {
                name: "write_view",
                target: TriggerTargetKind::View,
                table_schema: "public",
                table: "orders_view",
                function_schema: "public",
                function: "write_orders",
                or_replace: false,
                constraint: false,
                constraint_timing: crate::storage::ConstraintTiming::NotDeferrable.code(),
                referenced_schema: None,
                referenced_table: None,
                timing: 2,
                level: crate::sql::ast::TriggerLevel::Row,
                events: crate::sql::ast::TriggerEvents::from_bits(1).unwrap(),
                update_columns: 0,
                old_table: None,
                new_table: None,
                when: None,
                arguments: [""; crate::storage::MAX_TRIGGER_ARGUMENTS],
                argument_count: 0,
            },
        ));
        let Some(WalOp::CreateTrigger { target, timing, .. }) =
            decode_op(KIND_CREATE_TRIGGER, payload.readable())
        else {
            panic!("view trigger did not decode");
        };
        assert_eq!(target, TriggerTargetKind::View);
        assert_eq!(timing, 2);
    }

    #[test]
    fn policy_and_view_security_payloads_roundtrip() {
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(&mut budget, "security payload", 4096).unwrap();
        let mut roles = [SqlName::EMPTY; crate::storage::MAX_POLICY_ROLES];
        roles[0] = SqlName::parse("reader").unwrap();
        let dependencies = crate::storage::StoredQueryDependencies::EMPTY;
        assert!(append_payload(
            &mut payload,
            &WalOp::SetPolicy {
                schema: "public",
                table: "protected",
                name: "reader_rows",
                command: crate::storage::PolicyCommandKind::Update.code(),
                permissive: false,
                roles,
                role_count: 1,
                using: Some("tenant = 'reader'"),
                with_check: Some("tenant = current_user"),
                dependencies: WalStoredQueryDependencies::Captured(&dependencies),
            },
        ));
        let Some(WalOp::SetPolicy {
            command,
            permissive,
            roles,
            role_count,
            using,
            with_check,
            ..
        }) = decode_op(KIND_SET_POLICY, payload.readable())
        else {
            panic!("policy payload did not decode");
        };
        assert_eq!(command, crate::storage::PolicyCommandKind::Update.code());
        assert!(!permissive);
        assert_eq!(role_count, 1);
        assert_eq!(roles[0].as_str(), "reader");
        assert_eq!(using, Some("tenant = 'reader'"));
        assert_eq!(with_check, Some("tenant = current_user"));

        payload.clear();
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateView {
                schema: "public",
                name: "reader_view",
                sql: "SELECT * FROM protected",
                path: "public",
                security_invoker: true,
                dependencies: WalStoredQueryDependencies::Captured(&dependencies),
            },
        ));
        assert!(matches!(
            decode_op(KIND_CREATE_VIEW, payload.readable()),
            Some(WalOp::CreateView {
                security_invoker: true,
                ..
            })
        ));
    }

    #[test]
    fn create_trigger_payload_retains_arguments() {
        let mut budget = Budget::new(1024);
        let mut payload = FixedBuf::new(&mut budget, "trigger argument payload", 1024).unwrap();
        let mut arguments = [""; crate::storage::MAX_TRIGGER_ARGUMENTS];
        arguments[0] = "audit";
        arguments[1] = "v1";
        assert!(append_payload(
            &mut payload,
            &WalOp::CreateTrigger {
                name: "audit_row",
                target: TriggerTargetKind::Table,
                table_schema: "public",
                table: "orders",
                function_schema: "public",
                function: "audit_order",
                or_replace: false,
                constraint: false,
                constraint_timing: crate::storage::ConstraintTiming::NotDeferrable.code(),
                referenced_schema: None,
                referenced_table: None,
                timing: 0,
                level: crate::sql::ast::TriggerLevel::Row,
                events: crate::sql::ast::TriggerEvents::from_bits(1).unwrap(),
                update_columns: 0,
                old_table: None,
                new_table: None,
                when: None,
                arguments,
                argument_count: 2,
            },
        ));
        let Some(WalOp::CreateTrigger {
            arguments,
            argument_count,
            ..
        }) = decode_op(KIND_CREATE_TRIGGER, payload.readable())
        else {
            panic!("trigger arguments did not decode");
        };
        assert_eq!(argument_count, 2);
        assert_eq!(&arguments[..argument_count], ["audit", "v1"]);
    }

    #[test]
    fn composite_lifecycle_payload_keeps_slot_and_physical_attributes() {
        let mut fields =
            [crate::storage::CompositeFieldDef::EMPTY; crate::storage::MAX_COMPOSITE_FIELDS];
        fields[0] = crate::storage::CompositeFieldDef {
            attribute_number: 1,
            name: crate::storage::SqlName::parse("retained").unwrap(),
            ctype: ColType::Composite(3),
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
            user_type: Some(crate::storage::UserTypeName {
                schema: crate::storage::SqlName::parse("public").unwrap(),
                name: crate::storage::SqlName::parse("root").unwrap(),
            }),
            dropped: false,
            not_null: true,
        };
        fields[1] = crate::storage::CompositeFieldDef {
            attribute_number: 2,
            name: crate::storage::SqlName::parse("........pg.dropped.2........").unwrap(),
            ctype: ColType::Text,
            type_mod: -1,
            collation: crate::sql::ast::Collation::Default,
            user_type: None,
            dropped: true,
            not_null: false,
        };
        let op = WalOp::CreateComposite {
            slot: 7,
            definition: crate::storage::CompositeDef {
                database: crate::storage::DatabaseOid::POSTGRES,
                created_at: 0,
                schema: crate::storage::SqlName::parse("public").unwrap(),
                name: crate::storage::SqlName::parse("evolving").unwrap(),
                ownership: crate::storage::Ownership::BOOTSTRAP,
                fields,
                n_fields: 2,
                pending_definition: None,
                ddl_state: crate::storage::CatalogDdlState::Absent,
            },
        };
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(&mut budget, "composite WAL", 2048).unwrap();
        assert!(append_payload(&mut payload, &op));
        let Some(WalOp::CreateComposite { slot, definition }) =
            decode_op(KIND_CREATE_COMPOSITE, payload.readable())
        else {
            panic!("composite WAL did not decode");
        };
        assert_eq!(slot, 7);
        assert_eq!(definition.fields()[0].attribute_number, 1);
        assert!(definition.fields()[0].not_null);
        assert_eq!(
            definition.fields()[0]
                .user_type
                .expect("composite field identity")
                .name
                .as_str(),
            "root"
        );
        assert_eq!(definition.fields()[1].attribute_number, 2);
        assert!(definition.fields()[1].dropped);
        assert_eq!(
            definition.fields()[1].collation,
            crate::sql::ast::Collation::Default
        );

        payload.clear();
        let drop = WalOp::DropComposite {
            schema: "public",
            name: "evolving",
        };
        assert!(append_payload(&mut payload, &drop));
        assert!(matches!(
            decode_op(KIND_DROP_COMPOSITE, payload.readable()),
            Some(WalOp::DropComposite {
                schema: "public",
                name: "evolving"
            })
        ));
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
                    behavior: crate::storage::ReplicationSlotBehavior::DEFAULT,
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
                    table_column_masks: [0; crate::storage::MAX_PUBLICATION_TABLES],
                    table_filter_sql: [StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES],
                    table_count: 2,
                    schemas: [u8::MAX; crate::storage::MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: true,
                    publish_delete: true,
                    publish_truncate: true,
                    publish_via_partition_root: false,
                    publish_generated_columns: crate::storage::PublishGeneratedColumns::None,
                },
            )
            .unwrap();
            wal.append_committed(
                14,
                &WalOp::AlterPublication {
                    name: "changes",
                    all_tables: false,
                    tables: publication_tables,
                    table_column_masks: [0; crate::storage::MAX_PUBLICATION_TABLES],
                    table_filter_sql: [StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES],
                    table_count: 2,
                    schemas: [u8::MAX; crate::storage::MAX_SCHEMAS],
                    schema_count: 0,
                    publish_insert: true,
                    publish_update: false,
                    publish_delete: true,
                    publish_truncate: false,
                    publish_via_partition_root: true,
                    publish_generated_columns: crate::storage::PublishGeneratedColumns::Stored,
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
                    argument_signature: &[1, 23, 0],
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
            wal.append_committed(
                20,
                &WalOp::CreateTrigger {
                    name: "audit_row",
                    target: TriggerTargetKind::Table,
                    table_schema: "public",
                    table: "orders",
                    function_schema: "public",
                    function: "audit_order",
                    or_replace: false,
                    constraint: false,
                    constraint_timing: crate::storage::ConstraintTiming::NotDeferrable.code(),
                    referenced_schema: None,
                    referenced_table: None,
                    timing: 0,
                    level: crate::sql::ast::TriggerLevel::Row,
                    events: crate::sql::ast::TriggerEvents::from_bits(3).unwrap(),
                    update_columns: 3,
                    old_table: None,
                    new_table: None,
                    when: Some("NEW.total > OLD.total"),
                    arguments: [""; crate::storage::MAX_TRIGGER_ARGUMENTS],
                    argument_count: 0,
                },
            )
            .unwrap();
            wal.append_committed(
                21,
                &WalOp::AlterTrigger {
                    name: "audit_row",
                    target: TriggerTargetKind::Table,
                    table_schema: "public",
                    table: "orders",
                    new_name: "audit_row_disabled",
                    enabled: b'D',
                },
            )
            .unwrap();
            wal.append_committed(
                22,
                &WalOp::DropTrigger {
                    name: "audit_row_disabled",
                    target: TriggerTargetKind::Table,
                    table_schema: "public",
                    table: "orders",
                },
            )
            .unwrap();
            let mut publications =
                [crate::storage::SqlName::EMPTY; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
            publications[0] = crate::storage::SqlName::parse("sales").unwrap();
            publications[1] = crate::storage::SqlName::parse("inventory").unwrap();
            wal.append_committed(
                23,
                &WalOp::AlterSubscription {
                    name: "apply_changes",
                    connection: "host=127.0.0.2 port=5433 user=repl dbname=publisher application_name=apply_changes sslmode=disable",
                    publications,
                    publication_count: 2,
                    slot: crate::storage::SubscriptionSlot::External(
                        crate::storage::ReplicationSlotName::parse("apply_changes").unwrap(),
                    ),
                    behavior: crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                },
            )
            .unwrap();
            wal.append_committed(
                24,
                &WalOp::AlterEnumIdentity {
                    schema: "public",
                    name: "mood",
                    new_schema: "types",
                    new_name: "state",
                },
            )
            .unwrap();
            wal.append_committed(25, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay_operations(&mut wal, 0);
        assert_eq!(seen.len(), 24);
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
        assert!(seen[19].contains("CreateTrigger"), "{}", seen[19]);
        assert!(seen[20].contains("AlterTrigger"), "{}", seen[20]);
        assert!(seen[21].contains("DropTrigger"), "{}", seen[21]);
        assert!(seen[22].contains("AlterSubscription"), "{}", seen[22]);
        assert!(
            seen[23].contains("AlterEnumIdentity")
                && seen[23].contains("new_schema: \"types\"")
                && seen[23].contains("new_name: \"state\""),
            "enum identity: {}",
            seen[23]
        );
        assert_eq!(wal.last_lsn(), 25);
        // Appending continues after the replayed tail.
        wal.append_committed(
            26,
            &WalOp::DropTable {
                schema: "public",
                name: "u",
            },
        )
        .unwrap();
        wal.append_committed(27, &WalOp::Commit { transaction_id: 2 })
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
                    preserve_rows: true,
                    column_mapping,
                },
            )
            .unwrap();
            wal.append_committed(2, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            wal.commit();
        }
        let mut replay_budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut replay_budget).unwrap();
        let mut seen = false;
        wal.replay(0, |lsn, record| {
            let operation = decode_record(record).unwrap();
            if matches!(operation, WalOp::Commit { .. }) {
                return Ok(());
            }
            let WalOp::BeginTableRewrite {
                previous_schema,
                previous_name,
                preserve_rows,
                column_mapping,
            } = operation
            else {
                panic!("expected table rewrite");
            };
            assert_eq!(lsn, 1);
            assert_eq!(previous_schema, "public");
            assert_eq!(previous_name, "t");
            assert!(preserve_rows);
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
    fn partial_index_payload_round_trips_without_name_length_limits() {
        let operation = WalOp::CreateIndex {
            created_at: 42,
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
            collations: [crate::sql::ast::Collation::Default; MAX_INDEX_COLS],
            explicit_collations: [false; MAX_INDEX_COLS],
            operator_classes: [None; MAX_INDEX_COLS],
            resolved_operator_classes: [Some(crate::storage::IndexOperatorClass::Builtin(
                BtreeOperatorClass::Text,
            )); MAX_INDEX_COLS],
            descending: [false; MAX_INDEX_COLS],
            nulls_first: [false; MAX_INDEX_COLS],
            n_cols: 1,
            n_include_cols: 1,
            nulls_not_distinct: true,
            predicate: Some("active AND value IS NOT NULL"),
            unique: true,
            definition: crate::storage::IndexMutableDefinition {
                clustered: true,
                ..crate::storage::IndexMutableDefinition::DEFAULT
            },
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
            definition,
            ..
        }) = decode_op(KIND_CREATE_INDEX, payload.readable())
        else {
            panic!("partial index WAL payload must decode");
        };
        assert_eq!(n_include_cols, 1);
        assert_eq!(include_columns[0], 2);
        assert!(nulls_not_distinct);
        assert!(definition.clustered);
        assert_eq!(predicate, Some("active AND value IS NOT NULL"));
    }

    #[test]
    fn clustered_index_definition_payload_round_trips() {
        let operation = WalOp::AlterIndexDefinition {
            schema: "public",
            name: "clustered_rows",
            definition: crate::storage::IndexMutableDefinition {
                clustered: true,
                ..crate::storage::IndexMutableDefinition::DEFAULT
            },
        };
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(
            &mut budget,
            "clustered index definition WAL payload",
            encoded_payload_len(&operation),
        )
        .unwrap();
        assert!(append_payload(&mut payload, &operation));
        assert_eq!(payload.len(), encoded_payload_len(&operation));
        let Some(WalOp::AlterIndexDefinition {
            schema,
            name,
            definition,
        }) = decode_op(KIND_ALTER_INDEX_DEFINITION, payload.readable())
        else {
            panic!("clustered index definition WAL payload must decode");
        };
        assert_eq!(schema, "public");
        assert_eq!(name, "clustered_rows");
        assert!(definition.clustered);
    }

    #[test]
    fn domain_payload_without_durable_base_slot_is_rejected() {
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

        assert!(decode_op(KIND_CREATE_DOMAIN, &payload).is_none());
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
        payload.push(DOMAIN_PAYLOAD_WITH_BASE_SLOT);
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
    fn routine_payload_rejects_corrupt_dependency_suffix() {
        let definition = crate::storage::RoutineDef {
            created_at: 1,
            schema: crate::storage::SqlName::parse("public").unwrap(),
            name: crate::storage::SqlName::parse("durable_routine").unwrap(),
            body: crate::util::StackStr::from_str("SELECT 1"),
            creation_path: crate::util::StackStr::from_str("public"),
            ddl_state: crate::storage::CatalogDdlState::Present,
            ..crate::storage::RoutineDef::EMPTY
        };
        let operation = WalOp::CreateRoutine {
            definition,
            dependencies: WalStoredQueryDependencies::Captured(
                &crate::storage::StoredQueryDependencies::EMPTY,
            ),
        };
        let mut budget = Budget::new(4096);
        let mut payload = FixedBuf::new(
            &mut budget,
            "routine WAL payload",
            encoded_payload_len(&operation),
        )
        .unwrap();
        assert!(append_payload(&mut payload, &operation));
        assert!(decode_op(KIND_CREATE_ROUTINE, payload.readable()).is_some());
        let last = payload.len() - 1;
        payload.filled_mut()[last] = 0xff;
        assert!(decode_op(KIND_CREATE_ROUTINE, payload.readable()).is_none());
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

            assert_eq!(wal.commit_stage(22, 50).unwrap(), 53);
            wal.commit();
            wal.discard_stage(33);
            assert_eq!(wal.commit_stage(11, 53).unwrap(), 56);
            wal.commit();
        }

        let mut replay_budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut replay_budget).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(seen.len(), 6);
        assert!(seen[0].starts_with("51:DatabaseScope"));
        assert!(seen[1].starts_with("52:") && seen[1].contains("middle"));
        assert!(seen[2].starts_with("53:Commit"));
        assert!(seen[3].starts_with("54:DatabaseScope"));
        assert!(seen[4].starts_with("55:") && seen[4].contains("late"));
        assert!(seen[5].starts_with("56:Commit"));
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
    fn corrupt_record_discards_its_transaction_without_hiding_a_prior_commit() {
        let dir = temp_dir("corrupt");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.append_committed(
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
            wal.append_committed(2, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            for lsn in 3..=5 {
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
            wal.append_committed(6, &WalOp::Commit { transaction_id: 2 })
                .unwrap();
            wal.commit();
        }
        // Flip one byte in the second transaction's second record.
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
        let commit_len = HEADER_LEN + encoded_payload_len(&WalOp::Commit { transaction_id: 1 });
        bytes[record_len + commit_len + record_len + HEADER_LEN] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay_operations(&mut wal, 0);
        assert_eq!(
            seen.len(),
            1,
            "the prior commit survives and the corrupt transaction is atomic"
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
            wal.append_committed(6, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay_operations(&mut wal, 3);
        assert_eq!(seen.len(), 2, "only records above the floor apply");
        assert!(seen[0].starts_with("4:"));
        assert_eq!(wal.last_lsn(), 6, "scan still tracks the true tail");
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
            wal.append_committed(11, &WalOp::Commit { transaction_id: 1 })
                .unwrap();
            wal.commit();
            // Checkpoint at lsn 11; journal restarts with two tail records.
            wal.reset_after_checkpoint();
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
            wal.append_committed(
                13,
                &WalOp::Delete {
                    schema: "public",
                    table: "t",
                    rowid: 13,
                    old_row: None,
                    command_id: 0,
                },
            )
            .unwrap();
            wal.append_committed(14, &WalOp::Commit { transaction_id: 2 })
                .unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        // The checkpoint says floor = 11; stale records 4..11 still sit in
        // the file beyond the new tail but must not replay.
        let seen = collect_replay_operations(&mut wal, 11);
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("12:"));
        assert!(seen[1].starts_with("13:"));
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
            assert_eq!(wal.commit_stage(1, 16).unwrap(), 34);
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
        let mut seen = [(0u64, 0u8); 3];
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
        assert_eq!(count, 3);
        assert_eq!(seen[0].1, KIND_DATABASE_SCOPE);
        assert_eq!(seen[1].1, KIND_DELETE);
        assert_eq!(seen[2].1, KIND_COMMIT);
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
