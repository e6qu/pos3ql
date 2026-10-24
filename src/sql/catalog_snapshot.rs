//! Catalog row keys, committed before-images, and transaction-private overlays.

use core::hash::Hasher;
use core::num::{NonZeroU32, NonZeroU64};

use crate::mem::arena::Arena;
use crate::mem::fixed_map::Fnv1aHasher;
use crate::sql_err;
use crate::storage::{
    CatalogHistoryBatch, CatalogRelationIdentity, CatalogRowImage, HistoricalCatalogRow,
    MAX_COLUMNS,
};

use super::catalog::SynthTable;
use super::eval::{SqlError, arena_full, sqlstate};
use super::exec::{decode_projected_col_record, encode_projected_pub};
use super::types::{ColType, Datum};

#[derive(Clone, Copy)]
pub(crate) enum CatalogRowKey<'a> {
    Columns(&'a [&'a str]),
    /// Dependency catalogs can contain repeated identical tuples.
    Tuple,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogScan {
    Index,
    Sequential,
}

#[derive(Clone, Copy)]
pub(crate) enum CatalogImageSource {
    Committed,
    Private,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRowState {
    pub(crate) root: Option<NonZeroU64>,
    pub(crate) private: bool,
}

impl CatalogRowState {
    pub(crate) const UNTRACKED: Self = Self {
        root: None,
        private: false,
    };
}

pub(crate) struct DescriptionKey {
    pub(crate) object: u32,
    pub(crate) class: u32,
    pub(crate) column: i32,
}

impl DescriptionKey {
    pub(crate) fn decode(bytes: &[u8], shared: bool) -> Result<Self, SqlError> {
        // Canonical tagged rows: oid, oid, and (for local comments) int4.
        let count = if shared { 2u16 } else { 3u16 };
        let valid = bytes.len() == 2 + usize::from(count) * 5
            && bytes[..2] == count.to_le_bytes()
            && bytes[2] == 35
            && bytes[7] == 35
            && (shared || bytes[12] == 2);
        if !valid {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid description catalog key"
            ));
        }
        let object = u32::from_le_bytes(bytes[3..7].try_into().unwrap());
        let class = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let column = if shared {
            0
        } else {
            i32::from_le_bytes(bytes[13..17].try_into().unwrap())
        };
        if object == 0 || class == 0 || column < 0 {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid description catalog key identity"
            ));
        }
        Ok(Self {
            object,
            class,
            column,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CatalogSnapshotRelation {
    pub(crate) name: &'static str,
    pub(crate) oid: NonZeroU32,
    pub(crate) shared: bool,
    pub(crate) key: CatalogRowKey<'static>,
}

impl CatalogSnapshotRelation {
    pub(crate) fn identity(self, database: crate::storage::DatabaseOid) -> CatalogRelationIdentity {
        CatalogRelationIdentity {
            database: (!self.shared).then_some(database),
            relation: self.oid,
        }
    }
}

fn affected_catalogs(change: &super::txn::DdlUndo) -> &'static [&'static str] {
    use super::txn::DdlUndo::*;
    match change {
        Created(_)
        | Dropped(_)
        | TableAltered(_)
        | IndexCreated(_)
        | IndexDropped(_)
        | IndexRenamed { .. }
        | IndexAltered { .. }
        | MatviewChanged(_)
        | MatviewPopulationChanged(_) => &[
            "pg_class",
            "pg_attribute",
            "pg_attrdef",
            "pg_constraint",
            "pg_index",
            "pg_inherits",
            "pg_partitioned_table",
            "pg_sequence",
            "pg_type",
            "pg_statistic_ext",
            "pg_statistic_ext_data",
            "pg_trigger",
            "pg_rewrite",
            "pg_publication_rel",
            "pg_foreign_table",
            "pg_policy",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        ViewChanged(_) | ViewDropped(_) | RuleChanged { .. } | RuleAltered(_) | RuleDropped(_) => {
            &[
                "pg_class",
                "pg_attribute",
                "pg_type",
                "pg_rewrite",
                "pg_depend",
                "pg_shdepend",
                "pg_description",
            ]
        }
        RoutineChanged(_) => &[
            "pg_proc",
            "pg_aggregate",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        DomainCreated(_) | DomainDropped(_) | DomainAltered(_) | EnumCreated(_)
        | EnumDropped(_) | EnumAltered(_) | CompositeCreated(_) | CompositeDropped(_)
        | CompositeAltered(_) => &[
            "pg_type",
            "pg_enum",
            "pg_range",
            "pg_attribute",
            "pg_class",
            "pg_constraint",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        CastCreated(_) | CastDropped(_) => &["pg_cast", "pg_depend", "pg_description"],
        OperatorCreated(_) | OperatorAltered { .. } | OperatorDropped(_) => {
            &["pg_operator", "pg_depend", "pg_shdepend", "pg_description"]
        }
        OperatorFamilyCreated(_)
        | OperatorFamilyAltered { .. }
        | OperatorFamilyDropped(_)
        | OperatorClassCreated(_)
        | OperatorClassAltered { .. }
        | OperatorClassDropped(_) => &[
            "pg_opfamily",
            "pg_opclass",
            "pg_amop",
            "pg_amproc",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        CollationCreated(_) | CollationAltered { .. } | CollationDropped(_) => {
            &["pg_collation", "pg_depend", "pg_shdepend", "pg_description"]
        }
        ConversionCreated(_) | ConversionAltered { .. } | ConversionDropped(_) => &[
            "pg_conversion",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        TextSearchCreated(_) | TextSearchAltered { .. } | TextSearchDropped(_) => &[
            "pg_ts_config",
            "pg_ts_config_map",
            "pg_ts_dict",
            "pg_ts_parser",
            "pg_ts_template",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        EventTriggerCreated(_) | EventTriggerAltered { .. } | EventTriggerDropped(_) => &[
            "pg_event_trigger",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        TriggerCreated(_)
        | TriggerDropped(_)
        | TriggerAltered { .. }
        | PartitionTriggerAltered { .. } => &[
            "pg_trigger",
            "pg_class",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        PolicyCreated(_) | PolicyDropped(_) | PolicyAltered { .. } => &[
            "pg_policy",
            "pg_class",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        StatisticsCreated(_)
        | StatisticsDropped(_)
        | StatisticsAltered { .. }
        | StatisticsKeysAltered { .. } => &[
            "pg_statistic_ext",
            "pg_statistic_ext_data",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        PublicationCreated(_)
        | PublicationDropped(_)
        | PublicationAltered { .. }
        | PublicationOwnerChanged { .. }
        | PublicationRenamed { .. } => &[
            "pg_publication",
            "pg_publication_namespace",
            "pg_publication_rel",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        ForeignDataWrapperCreated(_)
        | ForeignDataWrapperAltered { .. }
        | ForeignDataWrapperDropped(_)
        | ForeignServerCreated(_)
        | ForeignServerAltered { .. }
        | ForeignServerDropped(_)
        | UserMappingCreated(_)
        | UserMappingAltered { .. }
        | UserMappingDropped(_)
        | ForeignTableCreated(_)
        | ForeignTableAltered { .. }
        | ForeignTableDropped(_)
        | ForeignOwnerChanged { .. } => &[
            "pg_foreign_data_wrapper",
            "pg_foreign_server",
            "pg_user_mapping",
            "pg_foreign_table",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        SubscriptionCreated(_)
        | SubscriptionDropped(_)
        | SubscriptionEnabled { .. }
        | SubscriptionBootstrapChanged { .. }
        | SubscriptionRelationsChanged
        | SubscriptionDefinitionChanged { .. }
        | SubscriptionOwnerChanged { .. }
        | SubscriptionRenamed { .. } => &[
            "pg_subscription",
            "pg_subscription_rel",
            "pg_shdepend",
            "pg_description",
        ],
        SequenceChanged(_) => &[
            "pg_sequence",
            "pg_class",
            "pg_attribute",
            "pg_type",
            "pg_depend",
            "pg_shdepend",
            "pg_description",
        ],
        LargeObjectCreated(_) | LargeObjectDropped(_) => &[
            "pg_largeobject",
            "pg_largeobject_metadata",
            "pg_shdepend",
            "pg_description",
        ],
        TablespaceCreated(_) | TablespaceAltered { .. } | TablespaceDropped(_) => {
            &["pg_tablespace", "pg_shdepend", "pg_shdescription"]
        }
        AccessMethodCreated(_) | AccessMethodDropped(_) => {
            &["pg_am", "pg_depend", "pg_description"]
        }
        DatabaseCreated(_) | DatabaseAltered { .. } | DatabaseDropped(_) => &[
            "pg_database",
            "pg_db_role_setting",
            "pg_shdepend",
            "pg_shdescription",
        ],
        SchemaCreated(_) | SchemaDropped(_) | SchemaRenamed { .. } => {
            &["pg_namespace", "pg_depend", "pg_shdepend", "pg_description"]
        }
        ExtensionCreated(_)
        | ExtensionDropped(_)
        | ExtensionAltered { .. }
        | ExtensionDependencyChanged { .. }
        | ExtensionConfigChanged { .. } => {
            &["pg_extension", "pg_depend", "pg_shdepend", "pg_description"]
        }
        RoleChanged { .. } | RoleMembershipChanged { .. } => &[
            "pg_authid",
            "pg_auth_members",
            "pg_shdepend",
            "pg_shdescription",
        ],
        RoleSettingChanged { .. } => &["pg_db_role_setting"],
        ObjectOwnerChanged { .. } | ObjectAclChanged { .. } => &[
            "pg_class",
            "pg_type",
            "pg_proc",
            "pg_namespace",
            "pg_database",
            "pg_tablespace",
            "pg_language",
            "pg_largeobject_metadata",
            "pg_foreign_data_wrapper",
            "pg_foreign_server",
            "pg_operator",
            "pg_opclass",
            "pg_opfamily",
            "pg_collation",
            "pg_conversion",
            "pg_ts_config",
            "pg_ts_dict",
            "pg_publication",
            "pg_subscription",
            "pg_shdepend",
        ],
        ColumnAclChanged { .. } => &["pg_attribute", "pg_shdepend"],
        DefaultAclChanged { .. } => &["pg_default_acl", "pg_shdepend"],
        ParameterAclChanged { .. } => &["pg_parameter_acl", "pg_shdepend"],
        CommentSet { .. } | ConstraintCommentRenamed { .. } => {
            &["pg_description", "pg_shdescription"]
        }
        SequenceReset { .. } | SystemSettingChanged { .. } => &[],
    }
}

pub(crate) fn capture_transaction(
    storage: &crate::storage::Storage,
    transaction: &super::txn::TxnState,
    publication: crate::storage::CatalogPublication,
    arena: &Arena,
) -> Result<Option<CatalogHistoryBatch>, SqlError> {
    if transaction.ddl().is_empty() {
        return Ok(None);
    }
    let Some(mut batch) = storage.reserve_catalog_history(transaction.txid, publication)? else {
        return Ok(None);
    };
    for relation in super::catalog_snapshot_keys::RELATIONS {
        if !transaction
            .ddl()
            .iter()
            .any(|change| affected_catalogs(change).contains(&relation.name))
        {
            continue;
        }
        let mark = arena.mark();
        let result = (|| {
            let committed = super::catalog::synthesize_current(
                storage,
                Some("pg_catalog"),
                relation.name,
                0,
                arena,
            )?;
            let private = super::catalog::synthesize_current(
                storage,
                Some("pg_catalog"),
                relation.name,
                transaction.txid,
                arena,
            )?;
            capture_rows(
                relation.identity(storage.current_database_oid()),
                relation.key,
                &committed,
                &private,
                &mut batch,
                |key, source| {
                    super::catalog::catalog_row_state(
                        storage,
                        match source {
                            CatalogImageSource::Committed => 0,
                            CatalogImageSource::Private => transaction.txid,
                        },
                        relation.name,
                        key,
                    )
                },
                arena,
            )
        })();
        // record copies keys and row images into the startup-sized history owner.
        unsafe { arena.rewind_to(mark) };
        result?;
    }
    Ok(batch.has_rows().then_some(batch))
}

#[cfg(test)]
mod key_definitions {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::guard::forbid_alloc;
    use crate::storage::{DatabaseOid, TableDef};

    #[test]
    fn catalog_row_keys_match_the_current_catalog_shapes_and_database_scope() {
        let capacity = core::mem::size_of::<TableDef>() + 4096;
        let mut budget = Budget::new(capacity);
        let mut arena = Arena::new(&mut budget, "catalog key definition test", capacity).unwrap();
        let relations = super::super::catalog_snapshot_keys::RELATIONS;
        assert_eq!(relations.len(), 64);
        assert_eq!(
            relations.iter().filter(|relation| relation.shared).count(),
            11
        );
        let mut matched = 0;
        let mut errors = Vec::new();
        for relation in relations {
            let identity = relation.identity(DatabaseOid::POSTGRES);
            assert_eq!(identity.database.is_none(), relation.shared);
            let baseline = super::super::catalog_metadata::RELATIONS
                .iter()
                .find(|definition| definition.name == relation.name)
                .unwrap();
            assert_eq!(baseline.oid as u32, identity.relation.get());
            if !super::super::catalog::is_catalog_relation(None, relation.name) {
                continue;
            }
            arena.reset();
            let result = forbid_alloc(|| {
                let definition = super::super::catalog::synthesize_definition(
                    Some("pg_catalog"),
                    relation.name,
                    &arena,
                )?;
                let indexed = KeyedRows::from_catalog(&definition, relation.key, &arena)?;
                Ok::<_, SqlError>(indexed.len)
            });
            match result {
                Ok(count) => assert_eq!(count, 0),
                Err(error) => errors.push(format!("{}: {}", relation.name, error.message.as_str())),
            }
            matched += 1;
        }
        assert!(
            errors.is_empty(),
            "catalog key contract failures: {errors:#?}"
        );
        assert!(matched > 40);
    }

    #[test]
    fn catalog_row_keys_accept_every_materialized_base_catalog_without_allocations() {
        let mut config = crate::config::Config::default_dev();
        config.max_connections = 2;
        config.max_prepared_transactions = 0;
        config.max_ddl_per_transaction = 1;
        config.max_catalog_versions_per_object = 2;
        config.memtable_bytes = 64 * 1024;
        config.max_tables = 4;
        config.table_rows = 8;
        config.max_routines = 2;
        config.max_views = 2;
        config.max_materialized_views = 2;
        config.max_enums = 2;
        config.max_enum_labels_per_type = 4;
        let mut budget = Budget::new(
            crate::storage::Storage::extra_budget_bytes(&config)
                + config.memtable_bytes
                + 65 * 1024 * 1024,
        );
        let storage = crate::storage::Storage::new(&config, &mut budget, 0).unwrap();
        let mut arena = Arena::new(
            &mut budget,
            "materialized catalog keys test",
            64 * 1024 * 1024,
        )
        .unwrap();
        let mut matched = 0;
        let mut errors = Vec::new();
        for relation in super::super::catalog_snapshot_keys::RELATIONS {
            if !super::super::catalog::is_catalog_relation(None, relation.name) {
                continue;
            }
            arena.reset();
            let result = forbid_alloc(|| {
                let catalog = super::super::catalog::synthesize(
                    &storage,
                    Some("pg_catalog"),
                    relation.name,
                    0,
                    CatalogScan::Sequential,
                    &arena,
                )?;
                let indexed = KeyedRows::from_catalog(&catalog, relation.key, &arena)?;
                Ok::<_, SqlError>((
                    indexed
                        .iter()
                        .map(|row| row.image.unwrap().count.get() as usize)
                        .sum::<usize>(),
                    catalog.rows.len(),
                ))
            });
            match result {
                Ok((indexed, source)) => assert_eq!(indexed, source),
                Err(error) => errors.push(format!("{}: {}", relation.name, error.message.as_str())),
            }
            matched += 1;
        }
        assert!(
            errors.is_empty(),
            "materialized catalog key failures: {errors:#?}"
        );
        assert!(matched > 40);
    }
}

#[derive(Clone, Copy)]
struct KeyedRow<'a> {
    key: &'a [u8],
    image: Option<CatalogRowImage<'a>>,
}

struct KeyedRows<'a> {
    slots: &'a mut [Option<KeyedRow<'a>>],
    len: usize,
}

impl<'a> KeyedRows<'a> {
    fn new(arena: &'a Arena, count: usize) -> Result<Self, SqlError> {
        let capacity = count
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or_else(arena_full)?
            .max(8);
        Ok(Self {
            slots: arena
                .alloc_slice_with(capacity, |_| None)
                .map_err(|_| arena_full())?,
            len: 0,
        })
    }

    fn slot(&self, key: &[u8]) -> usize {
        let mut hash = Fnv1aHasher::default();
        hash.write(key);
        let mask = self.slots.len() - 1;
        let mut slot = hash.finish() as usize & mask;
        while self.slots[slot].is_some_and(|row| row.key != key) {
            slot = (slot + 1) & mask;
        }
        slot
    }

    fn get(&self, key: &[u8]) -> Option<KeyedRow<'a>> {
        self.slots[self.slot(key)]
    }

    fn iter(&self) -> impl Iterator<Item = KeyedRow<'a>> + '_ {
        self.slots.iter().flatten().copied()
    }

    fn from_catalog(
        table: &SynthTable<'_>,
        key: CatalogRowKey<'_>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let mut output = Self::new(arena, table.rows.len())?;
        let mut key_columns = [0; MAX_COLUMNS];
        let key_count = match key {
            CatalogRowKey::Columns(names) => {
                if names.is_empty() || names.len() > key_columns.len() {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "invalid catalog row key columns"
                    ));
                }
                for (index, name) in names.iter().enumerate() {
                    key_columns[index] = table
                        .def
                        .columns()
                        .iter()
                        .position(|column| column.name.as_str() == *name)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::INTERNAL_ERROR,
                                "catalog row key column {} is missing from {}",
                                name,
                                table.def.name.as_str()
                            )
                        })?;
                }
                names.len()
            }
            CatalogRowKey::Tuple => 0,
        };
        for row in table.rows {
            if row.len() != table.def.n_columns + table.hidden_columns || row.len() > MAX_COLUMNS {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "catalog row does not match its definition"
                ));
            }
            let mut values = [Datum::Null; MAX_COLUMNS];
            for (column, value) in row.iter().copied().enumerate() {
                values[column] = if column < table.def.n_columns
                    && table.def.columns[column].ctype == ColType::Oid
                {
                    super::eval::cast_to(value, ColType::Oid, arena)?
                } else {
                    value
                };
            }
            let bytes = encode_projected_pub(&values[..row.len()], arena)?;
            let key = match key {
                CatalogRowKey::Tuple => bytes,
                CatalogRowKey::Columns(_) => {
                    let mut selected = [Datum::Null; MAX_COLUMNS];
                    for index in 0..key_count {
                        selected[index] = values[key_columns[index]];
                    }
                    encode_projected_pub(&selected[..key_count], arena)?
                }
            };
            let slot = output.slot(key);
            match output.slots[slot] {
                Some(mut previous) => {
                    if key_count != 0 {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "catalog {} contains duplicate unique row keys",
                            table.def.name.as_str()
                        ));
                    }
                    let previous_image = previous
                        .image
                        .as_mut()
                        .expect("a materialized catalog row exists");
                    assert_eq!(previous_image.bytes, bytes);
                    previous_image.count =
                        previous_image.count.checked_add(1).ok_or_else(|| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "catalog row multiplicity exceeds u32"
                            )
                        })?;
                    output.slots[slot] = Some(previous);
                }
                None => {
                    output.slots[slot] = Some(KeyedRow {
                        key,
                        image: Some(CatalogRowImage {
                            bytes,
                            count: NonZeroU32::new(1).unwrap(),
                            root: None,
                        }),
                    });
                    output.len += 1;
                }
            }
        }
        Ok(output)
    }

    fn from_history(
        history: &[HistoricalCatalogRow<'a>],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let mut output = Self::new(arena, history.len())?;
        for row in history {
            let slot = output.slot(row.key);
            if output.slots[slot].is_some() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "catalog snapshot contains a repeated history key"
                ));
            }
            output.slots[slot] = Some(KeyedRow {
                key: row.key,
                image: row.before,
            });
            output.len += 1;
        }
        Ok(output)
    }
}

/// Store only keys whose committed row will change. Unrelated catalog rows
/// do not consume history bytes or overwrite another transaction's history.
pub(crate) fn capture_rows(
    relation: CatalogRelationIdentity,
    key: CatalogRowKey<'_>,
    committed: &SynthTable<'_>,
    private: &SynthTable<'_>,
    batch: &mut CatalogHistoryBatch,
    row_state: impl Fn(&[u8], CatalogImageSource) -> Result<CatalogRowState, SqlError>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let committed = KeyedRows::from_catalog(committed, key, arena)?;
    let private = KeyedRows::from_catalog(private, key, arena)?;
    for row in committed.iter() {
        if row_state(row.key, CatalogImageSource::Private)?.private
            || private
                .get(row.key)
                .is_none_or(|after| after.image != row.image)
        {
            let mut before = row.image;
            if let Some(image) = &mut before {
                image.root = row_state(row.key, CatalogImageSource::Committed)?.root;
            }
            batch.record(relation, row.key, before)?;
        }
    }
    for row in private.iter() {
        if committed.get(row.key).is_none() {
            batch.record(relation, row.key, None)?;
        }
    }
    Ok(())
}

fn selected_image<'a>(
    key: CatalogRowKey<'_>,
    encoded_key: &'a [u8],
    current: Option<CatalogRowImage<'a>>,
    private: Option<CatalogRowImage<'a>>,
    historical: Option<KeyedRow<'a>>,
    state: CatalogRowState,
    scan: CatalogScan,
) -> Result<[Option<CatalogRowImage<'a>>; 2], SqlError> {
    let snapshot = historical.map_or(current, |row| row.image);
    match key {
        CatalogRowKey::Columns(_) => Ok(match historical {
            Some(KeyedRow {
                image: Some(before),
                ..
            }) => {
                let own = (state.private || current != private)
                    && (scan == CatalogScan::Sequential
                        || state.root.is_some_and(|root| before.root != Some(root)));
                [Some(before), if own { private } else { None }]
            }
            _ if state.private || current != private => [private, None],
            _ => [snapshot, None],
        }),
        CatalogRowKey::Tuple => {
            if state.private {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "multiset catalog rows require counted private changes"
                ));
            }
            let count = i64::from(snapshot.map_or(0, |row| row.count.get()))
                + i64::from(private.map_or(0, |row| row.count.get()))
                - i64::from(current.map_or(0, |row| row.count.get()));
            if count <= 0 {
                return Ok([None, None]);
            }
            let count = u32::try_from(count)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "catalog row multiplicity exceeds u32"
                    )
                })?;
            Ok([
                Some(CatalogRowImage {
                    bytes: encoded_key,
                    count,
                    root: None,
                }),
                None,
            ])
        }
    }
}

/// A private write to a newer committed tuple does not replace the older
/// catalog tuple selected by the statement. Own creations remain visible.
pub(crate) fn restore_rows<'a>(
    key: CatalogRowKey<'_>,
    scan: CatalogScan,
    committed: &SynthTable<'a>,
    private: &SynthTable<'a>,
    history: &[HistoricalCatalogRow<'a>],
    row_state: impl Fn(&[u8]) -> Result<CatalogRowState, SqlError>,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let committed_rows = KeyedRows::from_catalog(committed, key, arena)?;
    let private_rows = KeyedRows::from_catalog(private, key, arena)?;
    let historical_rows = KeyedRows::from_history(history, arena)?;
    let capacity = private_rows
        .len
        .checked_add(historical_rows.len)
        .ok_or_else(arena_full)?;
    let chosen = arena
        .alloc_slice_with(capacity, |_| [None::<CatalogRowImage<'a>>; 2])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for row in private_rows.iter() {
        chosen[count] = selected_image(
            key,
            row.key,
            committed_rows.get(row.key).and_then(|row| row.image),
            row.image,
            historical_rows.get(row.key),
            row_state(row.key)?,
            scan,
        )?;
        count += 1;
    }
    for row in historical_rows.iter() {
        if private_rows.get(row.key).is_none() {
            chosen[count] = selected_image(
                key,
                row.key,
                committed_rows.get(row.key).and_then(|row| row.image),
                None,
                Some(row),
                row_state(row.key)?,
                scan,
            )?;
            count += 1;
        }
    }
    let row_count =
        chosen[..count]
            .iter()
            .flatten()
            .flatten()
            .try_fold(0usize, |total, image| {
                total
                    .checked_add(image.count.get() as usize)
                    .ok_or_else(arena_full)
            })?;
    let output = arena
        .alloc_slice_with(row_count, |_| &[] as &[Datum<'a>])
        .map_err(|_| arena_full())?;
    let columns = private.def.n_columns + private.hidden_columns;
    let mut output_index = 0;
    for image in chosen[..count].iter().flatten().flatten() {
        if image
            .bytes
            .get(..2)
            .map(|header| u16::from_le_bytes(header.try_into().unwrap()) as usize)
            != Some(columns)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog history row has a different result shape"
            ));
        }
        let values = arena
            .alloc_slice_with(columns, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (index, value) in values.iter_mut().enumerate() {
            *value = decode_projected_col_record(image.bytes, index, arena)?;
        }
        for _ in 0..image.count.get() {
            output[output_index] = values;
            output_index += 1;
        }
    }
    Ok(SynthTable {
        def: private.def,
        rows: output,
        hidden_columns: private.hidden_columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::guard::forbid_alloc;
    use crate::storage::{CatalogHistory, DatabaseOid};
    use core::num::NonZeroU64;

    const KEY: CatalogRowKey<'static> = CatalogRowKey::Columns(&["objoid", "classoid", "objsubid"]);

    fn table<'a>(arena: &'a Arena, values: &[[Datum<'a>; 4]]) -> SynthTable<'a> {
        let definition =
            super::super::catalog::synthesize_definition(None, "pg_description", arena).unwrap();
        let rows = arena
            .alloc_slice_with(values.len(), |_| &[] as &[Datum<'a>])
            .unwrap();
        for (target, source) in rows.iter_mut().zip(values) {
            *target = arena.alloc_slice_copy(source).unwrap();
        }
        SynthTable {
            def: definition.def,
            rows,
            hidden_columns: 0,
        }
    }

    fn description(oid: u32, value: &str) -> [Datum<'_>; 4] {
        [
            Datum::Oid(oid),
            Datum::Oid(1259),
            Datum::Int4(0),
            Datum::Text(value),
        ]
    }

    fn comment<'a>(table: &SynthTable<'a>, oid: u32) -> Option<&'a str> {
        table.rows.iter().find_map(|row| match (row[0], row[3]) {
            (Datum::Oid(candidate), Datum::Text(value)) if candidate == oid => Some(value),
            _ => None,
        })
    }

    fn relation() -> CatalogRelationIdentity {
        CatalogRelationIdentity {
            database: Some(DatabaseOid::POSTGRES),
            relation: NonZeroU32::new(2609).unwrap(),
        }
    }

    #[test]
    fn catalog_snapshot_changed_keys_restore_old_rows_and_hide_new_objects() {
        let mut budget = Budget::new(32 * 1024 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog snapshot test", 16 * 1024 * 1024).unwrap();
        forbid_alloc(|| {
            let before = table(
                &arena,
                &[
                    description(1, "old"),
                    description(2, "same"),
                    description(4, "deleted"),
                ],
            );
            let current = table(
                &arena,
                &[
                    description(1, "new"),
                    description(2, "same"),
                    description(3, "created"),
                ],
            );
            let mut batch = history.reserve().unwrap();
            capture_rows(
                relation(),
                KEY,
                &before,
                &current,
                &mut batch,
                |_, _| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            let changes = history.rows_at(relation(), 6, &arena).unwrap();
            assert_eq!(changes.len(), 3);
            let restored = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &current,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 3);
            assert_eq!(comment(&restored, 1), Some("old"));
            assert_eq!(comment(&restored, 2), Some("same"));
            assert_eq!(comment(&restored, 3), None);
            assert_eq!(comment(&restored, 4), Some("deleted"));
            let fresh = history.rows_at(relation(), 7, &arena).unwrap();
            let restored = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &current,
                fresh,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(comment(&restored, 1), Some("new"));
            assert_eq!(comment(&restored, 3), Some("created"));
            assert_eq!(comment(&restored, 4), None);
        });
    }

    #[test]
    fn catalog_snapshot_private_changes_preserve_the_selected_committed_tuple() {
        let mut budget = Budget::new(32 * 1024 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog snapshot test", 16 * 1024 * 1024).unwrap();
        forbid_alloc(|| {
            let before = table(
                &arena,
                &[description(1, "old"), description(3, "old deleted object")],
            );
            let current = table(
                &arena,
                &[description(1, "new"), description(2, "new other object")],
            );
            let mut batch = history.reserve().unwrap();
            capture_rows(
                relation(),
                KEY,
                &before,
                &current,
                &mut batch,
                |_, _| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            let changes = history.rows_at(relation(), 6, &arena).unwrap();
            let private = table(
                &arena,
                &[description(1, "my change"), description(4, "my new object")],
            );
            let restored = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &private,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 3);
            assert_eq!(comment(&restored, 1), Some("old"));
            assert_eq!(comment(&restored, 2), None);
            assert_eq!(comment(&restored, 3), Some("old deleted object"));
            assert_eq!(comment(&restored, 4), Some("my new object"));
            let deleted = table(&arena, &[description(2, "new other object")]);
            let restored = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &deleted,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 2);
            assert_eq!(comment(&restored, 1), Some("old"));
            assert_eq!(comment(&restored, 2), None);
            assert_eq!(comment(&restored, 3), Some("old deleted object"));
            let rolled_back = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &current,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(rolled_back.rows.len(), 2);
            assert_eq!(comment(&rolled_back, 1), Some("old"));
            assert_eq!(comment(&rolled_back, 3), Some("old deleted object"));
            let overwritten =
                encode_projected_pub(&[Datum::Oid(1), Datum::Oid(1259), Datum::Int4(0)], &arena)
                    .unwrap();
            let removed =
                encode_projected_pub(&[Datum::Oid(3), Datum::Oid(1259), Datum::Int4(0)], &arena)
                    .unwrap();
            let own_equal_values = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &current,
                changes,
                |key| {
                    Ok(CatalogRowState {
                        root: None,
                        private: key == overwritten || key == removed,
                    })
                },
                &arena,
            )
            .unwrap();
            assert_eq!(own_equal_values.rows.len(), 2);
            assert_eq!(comment(&own_equal_values, 1), Some("old"));
            assert_eq!(comment(&own_equal_values, 3), Some("old deleted object"));
            let selected_tuple = restore_rows(
                KEY,
                CatalogScan::Index,
                &before,
                &private,
                &[],
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(selected_tuple.rows.len(), 2);
            assert_eq!(comment(&selected_tuple, 1), Some("my change"));
            assert_eq!(comment(&selected_tuple, 3), None);
            assert_eq!(comment(&selected_tuple, 4), Some("my new object"));
            let scan = restore_rows(
                KEY,
                CatalogScan::Sequential,
                &current,
                &private,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(scan.rows.len(), 4);
            assert!(scan.rows.iter().any(|row| row[3] == Datum::Text("old")));
            assert!(
                scan.rows
                    .iter()
                    .any(|row| row[3] == Datum::Text("my change"))
            );
            // A separate insertion has its own index root and remains visible
            // even when an older tuple satisfies the same logical key.
            let reinserted = restore_rows(
                KEY,
                CatalogScan::Index,
                &current,
                &private,
                changes,
                |key| {
                    Ok(CatalogRowState {
                        root: NonZeroU64::new(19),
                        private: key == overwritten,
                    })
                },
                &arena,
            )
            .unwrap();
            assert_eq!(reinserted.rows.len(), 4);
            assert!(
                reinserted
                    .rows
                    .iter()
                    .any(|row| row[3] == Datum::Text("old"))
            );
            assert!(
                reinserted
                    .rows
                    .iter()
                    .any(|row| row[3] == Datum::Text("my change"))
            );
        });
    }

    #[test]
    fn catalog_snapshot_normalizes_oid_keys_and_rejects_duplicate_unique_rows() {
        let mut budget = Budget::new(32 * 1024 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog snapshot test", 16 * 1024 * 1024).unwrap();
        forbid_alloc(|| {
            let before = table(
                &arena,
                &[[
                    Datum::Int4(1),
                    Datum::Int4(1259),
                    Datum::Int4(0),
                    Datum::Text("same"),
                ]],
            );
            let current = table(&arena, &[description(1, "same")]);
            let mut batch = history.reserve().unwrap();
            capture_rows(
                relation(),
                KEY,
                &before,
                &current,
                &mut batch,
                |_, _| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            assert!(history.rows_at(relation(), 6, &arena).unwrap().is_empty());
            let duplicate = table(&arena, &[description(1, "same"), description(1, "same")]);
            let error = match KeyedRows::from_catalog(&duplicate, KEY, &arena) {
                Ok(_) => panic!("a catalog unique key was accepted twice"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert!(error.message.as_str().contains("duplicate unique row keys"));
            let mut batch = history.reserve().unwrap();
            capture_rows(
                relation(),
                KEY,
                &current,
                &current,
                &mut batch,
                |_, _| {
                    Ok(CatalogRowState {
                        root: None,
                        private: true,
                    })
                },
                &arena,
            )
            .unwrap();
            assert!(batch.has_rows());
            batch.publish(NonZeroU64::new(8).unwrap());
            let physical_update = history.rows_at(relation(), 7, &arena).unwrap();
            assert_eq!(physical_update.len(), 1);
            let encoded =
                encode_projected_pub(&[Datum::Oid(1), Datum::Oid(1259), Datum::Int4(0)], &arena)
                    .unwrap();
            let decoded = DescriptionKey::decode(encoded, false).unwrap();
            assert_eq!(
                (decoded.object, decoded.class, decoded.column),
                (1, 1259, 0)
            );
            for end in 0..encoded.len() {
                assert!(DescriptionKey::decode(&encoded[..end], false).is_err());
            }
            assert!(DescriptionKey::decode(encoded, true).is_err());
            let mut invalid = [0; 17];
            invalid.copy_from_slice(encoded);
            invalid[2] = 2;
            assert!(DescriptionKey::decode(&invalid, false).is_err());
            invalid.copy_from_slice(encoded);
            invalid[3..7].fill(0);
            assert!(DescriptionKey::decode(&invalid, false).is_err());
            invalid.copy_from_slice(encoded);
            invalid[13..17].copy_from_slice(&(-1i32).to_le_bytes());
            assert!(DescriptionKey::decode(&invalid, false).is_err());
        });
    }

    #[test]
    fn catalog_snapshot_tuple_keys_preserve_identical_dependency_multiplicity() {
        let mut budget = Budget::new(32 * 1024 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog snapshot test", 16 * 1024 * 1024).unwrap();
        forbid_alloc(|| {
            let before = table(
                &arena,
                &[
                    description(1, "same"),
                    description(1, "same"),
                    description(1, "same"),
                ],
            );
            let current = table(&arena, &[description(1, "same")]);
            let mut batch = history.reserve().unwrap();
            capture_rows(
                relation(),
                CatalogRowKey::Tuple,
                &before,
                &current,
                &mut batch,
                |_, _| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            let changes = history.rows_at(relation(), 6, &arena).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].before.unwrap().count.get(), 3);
            let restored = restore_rows(
                CatalogRowKey::Tuple,
                CatalogScan::Sequential,
                &current,
                &current,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 3);
            assert!(
                restored
                    .rows
                    .iter()
                    .all(|row| row[3] == Datum::Text("same"))
            );
            let added = table(&arena, &[description(1, "same"), description(1, "same")]);
            let restored = restore_rows(
                CatalogRowKey::Tuple,
                CatalogScan::Sequential,
                &current,
                &added,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 4);
            let removed = table(&arena, &[]);
            let restored = restore_rows(
                CatalogRowKey::Tuple,
                CatalogScan::Sequential,
                &current,
                &removed,
                changes,
                |_| Ok(CatalogRowState::UNTRACKED),
                &arena,
            )
            .unwrap();
            assert_eq!(restored.rows.len(), 2);
        });
    }
}
