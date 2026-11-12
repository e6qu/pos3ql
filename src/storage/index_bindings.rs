//! Index columns retain their table lifetime and the definition they selected.

use super::catalog_sources::{CatalogObjectIdentity, CatalogObjectSlot};
use super::*;
use crate::sql::types::BtreeOperatorClass;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexRelationKind {
    Table,
    Toast,
}

/// TOAST keys describe the virtual large-value relation, never its owner's columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexRelationSource {
    Table,
    Toast(CatalogPosition),
}

impl IndexRelationSource {
    pub(crate) const ENCODED_BYTES: usize = 1 + CatalogPosition::ENCODED_BYTES;

    pub(crate) fn encode(self) -> [u8; Self::ENCODED_BYTES] {
        let mut bytes = [0; Self::ENCODED_BYTES];
        if let Self::Toast(owner) = self {
            bytes[0] = 1;
            bytes[1..].copy_from_slice(&owner.encode());
        }
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::ENCODED_BYTES {
            return None;
        }
        match bytes[0] {
            0 if bytes[1..].iter().all(|byte| *byte == 0) => Some(Self::Table),
            1 => {
                let owner = CatalogPosition::decode(&bytes[1..])?;
                RelationIdentity::parse(owner.created_at())?;
                Some(Self::Toast(owner))
            }
            _ => None,
        }
    }

    pub(crate) fn kind(self) -> IndexRelationKind {
        match self {
            Self::Table => IndexRelationKind::Table,
            Self::Toast(_) => IndexRelationKind::Toast,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexTableBinding {
    pub(super) source: CatalogObjectIdentity,
    revision: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingIndexTable {
    pub(super) transaction: u32,
    pub(super) schema: SqlName,
    pub(super) table: SqlName,
}

impl IndexDef {
    pub(crate) fn relation_kind(&self) -> IndexRelationKind {
        self.relation.kind()
    }

    pub(crate) fn relation_oid(&self) -> i32 {
        let first = match self.relation {
            IndexRelationSource::Table => FIRST_EXPLICIT_INDEX_OID,
            IndexRelationSource::Toast(_) => FIRST_TOAST_INDEX_OID,
        };
        RelationIdentity::parse(self.created_at)
            .expect("admitted index has a durable identity")
            .companion_oid(first)
    }

    pub(super) fn pending_owner_other_than(&self, transaction: u32) -> Option<u32> {
        self.ddl_state
            .pending_txid()
            .into_iter()
            .chain(self.pending_table.map(|pending| pending.transaction))
            .chain(self.pending_name.map(|pending| pending.txid))
            .chain(self.pending_definition.map(|pending| pending.txid))
            .find(|owner| *owner != transaction)
    }

    pub(super) fn schema_for(&self, transaction: u32) -> SqlName {
        self.pending_table
            .filter(|pending| pending.transaction == transaction)
            .map_or(self.schema, |pending| pending.schema)
    }

    pub(super) fn table_for(&self, transaction: u32) -> SqlName {
        self.pending_table
            .filter(|pending| pending.transaction == transaction)
            .map_or(self.table, |pending| pending.table)
    }

    pub(super) fn namespace_blocker(
        &self,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
    ) -> Option<u32> {
        if self.ddl_state == CatalogDdlState::Absent {
            return None;
        }
        let owner = self
            .pending_table
            .map(|pending| pending.transaction)
            .or(self.pending_name.map(|pending| pending.txid))
            .or(self.ddl_state.pending_txid())?;
        (owner != transaction
            && self.visible_to(owner)
            && self.schema_for(owner) == schema
            && self.name_for(owner) == name
            && (!self.ddl_state.committed_exists() || self.schema != schema || self.name != name))
            .then_some(owner)
    }
}

pub(super) enum IndexNamespaceConflict {
    Duplicate(SqlName),
    Wait(u32, SqlName),
}

impl Storage {
    pub(super) fn check_index_table_names(
        indexes: &[IndexDef],
        source: CatalogObjectIdentity,
        schema: SqlName,
        transaction: u32,
    ) -> Result<(), IndexNamespaceConflict> {
        for (slot, index) in indexes.iter().enumerate().filter(|(_, index)| {
            matches!(index.relation, IndexRelationSource::Table)
                && index.ddl_state != CatalogDdlState::Absent
                && index
                    .table_binding
                    .is_some_and(|binding| binding.source == source)
        }) {
            if let Some(owner) = index.pending_owner_other_than(transaction) {
                return Err(IndexNamespaceConflict::Wait(owner, index.name));
            }
            if !index.visible_to(transaction) {
                continue;
            }
            let name = index.name_for(transaction);
            for (_, candidate) in indexes
                .iter()
                .enumerate()
                .filter(|(other, candidate)| *other != slot && candidate.database == index.database)
            {
                if let Some(owner) = candidate.namespace_blocker(schema, name, transaction) {
                    return Err(IndexNamespaceConflict::Wait(owner, name));
                }
                if candidate.visible_to(transaction)
                    && candidate.schema_for(transaction) == schema
                    && candidate.name_for(transaction) == name
                {
                    return Err(IndexNamespaceConflict::Duplicate(name));
                }
            }
        }
        Ok(())
    }

    pub(super) fn move_index_comments(
        comments: &mut CommentCatalog,
        before: IndexDef,
        after: IndexDef,
        transaction: u32,
    ) {
        for comment in comments.entries.iter_mut().filter(|comment| {
            comment.matches_to(
                Some(before.database),
                CommentClass::Relation,
                before.schema_for(transaction).as_str(),
                before.name_for(transaction).as_str(),
                0,
                transaction,
            )
        }) {
            comment.pending_identity = (comment.schema != after.schema_for(transaction)
                || comment.name != after.name_for(transaction))
            .then_some(PendingCommentIdentity {
                txid: transaction,
                schema: after.schema_for(transaction),
                name: after.name_for(transaction),
            });
        }
    }

    pub(super) fn check_index_comment_move(
        comments: &CommentCatalog,
        before: IndexDef,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
    ) -> Result<(), SqlError> {
        for comment in comments.entries.iter().filter(|comment| {
            comment.matches_to(
                Some(before.database),
                CommentClass::Relation,
                before.schema_for(transaction).as_str(),
                before.name_for(transaction).as_str(),
                0,
                transaction,
            )
        }) {
            if (schema != before.schema_for(transaction) || name != before.name_for(transaction))
                && comment
                    .pending_identity
                    .is_some_and(|pending| pending.txid != transaction)
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "index comment identity is owned by another transaction"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn check_index_table_comments(
        indexes: &[IndexDef],
        comments: &CommentCatalog,
        source: CatalogObjectIdentity,
        schema: SqlName,
        transaction: u32,
    ) -> Result<(), SqlError> {
        for index in indexes.iter().filter(|index| {
            matches!(index.relation, IndexRelationSource::Table)
                && index.visible_to(transaction)
                && index
                    .table_binding
                    .is_some_and(|binding| binding.source == source)
        }) {
            Self::check_index_comment_move(
                comments,
                *index,
                schema,
                index.name_for(transaction),
                transaction,
            )?;
        }
        Ok(())
    }

    pub(super) fn stage_index_table_names(
        indexes: &mut [IndexDef],
        comments: &mut CommentCatalog,
        source: CatalogObjectIdentity,
        schema: SqlName,
        table: SqlName,
        transaction: u32,
    ) {
        for index in indexes.iter_mut().filter(|index| {
            matches!(index.relation, IndexRelationSource::Table)
                && index.visible_to(transaction)
                && index
                    .table_binding
                    .is_some_and(|binding| binding.source == source)
        }) {
            let before = *index;
            index.pending_table =
                (index.schema != schema || index.table != table).then_some(PendingIndexTable {
                    transaction,
                    schema,
                    table,
                });
            Self::move_index_comments(comments, before, *index, transaction);
        }
    }

    pub(super) fn commit_index_table_names_locked(
        indexes: &mut [IndexDef],
        comments: &mut CommentCatalog,
        source: CatalogObjectIdentity,
        transaction: u32,
    ) {
        for index in indexes.iter_mut().filter(|index| {
            index
                .table_binding
                .is_some_and(|binding| binding.source == source)
        }) {
            for comment in comments.entries.iter_mut().filter(|comment| {
                comment.matches_to(
                    Some(index.database),
                    CommentClass::Relation,
                    index.schema_for(transaction).as_str(),
                    index.name_for(transaction).as_str(),
                    0,
                    transaction,
                ) && comment
                    .pending_identity
                    .is_some_and(|pending| pending.txid == transaction)
            }) {
                let pending = comment
                    .pending_identity
                    .take()
                    .expect("selected pending index comment");
                comment.schema = pending.schema;
                comment.name = pending.name;
            }
            if let Some(pending) = index
                .pending_table
                .filter(|pending| pending.transaction == transaction)
            {
                index.schema = pending.schema;
                index.table = pending.table;
                index.pending_table = None;
            }
        }
    }
}

impl IndexTableBinding {
    pub(super) fn matches_selected(
        self,
        storage: &Storage,
        state: &table_definitions::DefinitionState,
        transaction: u32,
    ) -> bool {
        self.source.matches(
            storage,
            state.identity.database,
            state.identity.created_at,
            state.identity.existence,
        ) && self.revision
            == storage
                .pending_table_defs
                .selected_revision(state, transaction)
    }
}

impl Storage {
    pub(super) fn resolve_index_source_table(
        &self,
        index: IndexDef,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        match index.relation {
            IndexRelationSource::Table => self
                .find_visible(index.schema.as_str(), index.table.as_str(), transaction)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "relation \"{}\" does not exist",
                        index.table.as_str()
                    )
                }),
            IndexRelationSource::Toast(position) => {
                let table = self.tables.get(position.slot()).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "TOAST index owner position is invalid"
                    )
                })?;
                let identity = table.identity();
                let owner_oid = RelationIdentity::parse(position.created_at())
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "TOAST index owner identity is invalid"
                        )
                    })?
                    .oid();
                if index.created_at != 0 && index.created_at <= position.created_at() {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "TOAST index creation identity precedes its owner"
                    ));
                }
                let name = crate::stack_format!(64, "pg_toast_{owner_oid}");
                if identity.database != current_database()
                    || identity.created_at != position.created_at()
                    || !identity.existence.visible_to(transaction)
                    || !self.table_def(position.slot(), transaction).has_toast
                    || index.schema.as_str() != "pg_toast"
                    || index.table.as_str() != name.as_str()
                {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "TOAST index owner changed before installation"
                    ));
                }
                if index.method != crate::sql::ast::IndexAccessMethod::Btree
                    || !index.unique
                    || !index.mutable.immediate
                    || index.n_cols != 2
                    || index.columns[..2] != [0, 1]
                    || index.n_include_cols != 0
                    || index.predicate.is_some()
                    || index.expressions[..2].iter().any(Option::is_some)
                    || index.collations[..2]
                        .iter()
                        .any(|collation| *collation != Collation::None)
                    || index.resolved_operator_classes[..2]
                        != [
                            Some(IndexOperatorClass::Btree(BtreeOperatorClass::Oid)),
                            Some(IndexOperatorClass::Btree(BtreeOperatorClass::Int4)),
                        ]
                    || index.mutable.kind.is_partitioned()
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "invalid object-native TOAST index definition"
                    ));
                }
                Ok(position.slot())
            }
        }
    }

    /// A table drop retains its exact pending index drops for `sql_drop`.
    pub(crate) fn dropped_table_index_for_event(
        &self,
        index_slot: usize,
        table_slot: usize,
        transaction: u32,
    ) -> Option<IndexDef> {
        let mut index = self.index_catalog().get(index_slot).copied()?;
        if index.database != current_database()
            || index.ddl_state.pending_drop_txid() != Some(transaction)
        {
            return None;
        }
        let source = index.table_binding?.source;
        let table = self.tables.get(table_slot)?.identity();
        if source.slot() != CatalogObjectSlot::Table(table_slot)
            || !source.matches(self, table.database, table.created_at, table.existence)
        {
            return None;
        }
        index = index.definition_for(transaction);
        if index.relation_kind() == IndexRelationKind::Table
            && let Some(pending) = index
                .pending_table
                .filter(|pending| pending.transaction == transaction)
        {
            index.schema = pending.schema;
            index.table = pending.table;
        }
        index.name = index.name_for(transaction);
        Some(index)
    }

    pub(super) fn index_binding_table(&self, index: IndexDef, transaction: u32) -> Option<usize> {
        let binding = index.table_binding?;
        let CatalogObjectSlot::Table(table) = binding.source.slot() else {
            return None;
        };
        let identity = self.tables.get(table)?.identity();
        (identity.existence.visible_to(transaction)
            && binding.source.matches(
                self,
                identity.database,
                identity.created_at,
                identity.existence,
            ))
        .then_some(table)
    }

    pub(super) fn capture_index_binding(
        &self,
        table: usize,
        transaction: u32,
    ) -> IndexTableBinding {
        let state = self.tables[table].definition.read();
        IndexTableBinding {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Table(table),
                state.identity.database,
                state.identity.created_at,
            ),
            revision: self
                .pending_table_defs
                .selected_revision(&state, transaction),
        }
    }

    pub(super) fn project_index_binding(
        &self,
        index: IndexDef,
        transaction: u32,
    ) -> Option<IndexDef> {
        if index.ddl_state.pending_drop_txid() == Some(transaction) {
            return Some(index);
        }
        let binding = index.table_binding?;
        let CatalogObjectSlot::Table(table) = binding.source.slot() else {
            return None;
        };
        let state = self.tables.get(table)?.definition.read();
        self.project_index_binding_locked(index, &state, transaction)
    }

    pub(super) fn project_index_binding_locked(
        &self,
        mut index: IndexDef,
        state: &table_definitions::DefinitionState,
        transaction: u32,
    ) -> Option<IndexDef> {
        index = index.definition_for(transaction);
        if index.ddl_state.pending_drop_txid() == Some(transaction) {
            return Some(index);
        }
        let binding = index.table_binding?;
        if !binding.source.matches(
            self,
            state.identity.database,
            state.identity.created_at,
            state.identity.existence,
        ) {
            return None;
        }
        if matches!(index.relation, IndexRelationSource::Toast(_)) {
            if !state.committed().has_toast
                && !state
                    .pending
                    .is_some_and(|head| head.transaction == transaction)
            {
                return None;
            }
            index.table_binding = Some(IndexTableBinding {
                source: binding.source,
                revision: self
                    .pending_table_defs
                    .selected_revision(state, transaction),
            });
            return Some(index);
        }
        let mut columns = [0; MAX_INDEX_COLS * 2];
        let mut count = 0;
        for key in 0..index.n_cols {
            if index.expressions[key].is_none() {
                columns[count] = index.columns[key];
                count += 1;
            }
        }
        columns[count..count + index.n_include_cols]
            .copy_from_slice(&index.include_columns[..index.n_include_cols]);
        count += index.n_include_cols;
        assert!(
            self.pending_table_defs.project_columns(
                state,
                transaction,
                binding.revision,
                &mut columns[..count]
            ),
            "visible index lost its selected table definition or a referenced column"
        );
        let mut cursor = 0;
        for key in 0..index.n_cols {
            if index.expressions[key].is_none() {
                index.columns[key] = columns[cursor];
                cursor += 1;
            }
        }
        index.include_columns[..index.n_include_cols].copy_from_slice(&columns[cursor..count]);
        let (schema, name) = state
            .pending
            .filter(|head| head.transaction == transaction)
            .map_or((state.committed().schema, state.committed().name), |head| {
                (head.schema, head.name)
            });
        index.schema = schema;
        index.table = name;
        index.table_binding = Some(IndexTableBinding {
            source: binding.source,
            revision: self
                .pending_table_defs
                .selected_revision(state, transaction),
        });
        Some(index)
    }

    pub(super) fn promote_private_index_bindings_locked(
        &self,
        indexes: &mut [IndexDef],
        source: CatalogObjectIdentity,
        state: &table_definitions::DefinitionState,
        transaction: u32,
    ) {
        for index in indexes.iter_mut().filter(|index| {
            index.database == state.identity.database
                && index.ddl_state == (CatalogDdlState::PendingCreate { txid: transaction })
                && index
                    .table_binding
                    .is_some_and(|binding| binding.source == source)
        }) {
            let mut promoted = self
                .project_index_binding_locked(*index, state, transaction)
                .expect("private index retains its table lifetime through commit");
            promoted
                .table_binding
                .as_mut()
                .expect("projected binding exists")
                .revision = 0;
            *index = promoted;
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};

    #[test]
    fn index_relation_source_codec_rejects_ambiguous_or_unbounded_owners() {
        crate::mem::guard::forbid_alloc(|| {
            let owner = CatalogPosition::new(7, 42).unwrap();
            for source in [
                IndexRelationSource::Table,
                IndexRelationSource::Toast(owner),
            ] {
                assert_eq!(IndexRelationSource::decode(&source.encode()), Some(source));
            }
            let mut malformed = IndexRelationSource::Toast(owner).encode();
            malformed[0] = 0;
            assert!(IndexRelationSource::decode(&malformed).is_none());
            malformed[0] = 2;
            assert!(IndexRelationSource::decode(&malformed).is_none());
            malformed[0] = 1;
            malformed[3..].copy_from_slice(&(MAX_INDEX_OID_GENERATION + 1).to_le_bytes());
            assert!(IndexRelationSource::decode(&malformed).is_none());
            malformed[3..].fill(0);
            assert!(IndexRelationSource::decode(&malformed).is_none());
        });
    }

    #[test]
    fn registered_toast_indexes_keep_owner_identity_and_never_enter_main_table_keys() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_indexes = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let owner = storage
                .create_table(make_def("toast_owner", &[("value", ColType::Text, false)]))
                .unwrap();
            let position = storage.table_position(owner);
            let anchor = storage.table_def(owner, 0).toast_index_identity.unwrap();
            let owner_oid = RelationIdentity::parse(position.created_at())
                .unwrap()
                .oid();
            let table_name = crate::stack_format!(64, "pg_toast_{owner_oid}");
            let index_name = crate::stack_format!(64, "pg_toast_{owner_oid}_index");
            let mut definition = IndexDef {
                relation: IndexRelationSource::Toast(position),
                created_at: anchor.created_at(),
                schema: SqlName::parse("pg_toast").unwrap(),
                table: SqlName::parse(table_name.as_str()).unwrap(),
                name: SqlName::parse(index_name.as_str()).unwrap(),
                n_cols: 2,
                unique: true,
                collations: [Collation::None; MAX_INDEX_COLS],
                ..IndexDef::EMPTY
            };
            definition.columns[1] = 1;
            definition.resolved_operator_classes[0] =
                Some(IndexOperatorClass::Btree(BtreeOperatorClass::Oid));
            definition.resolved_operator_classes[1] =
                Some(IndexOperatorClass::Btree(BtreeOperatorClass::Int4));
            let slot = storage.create_index(definition, 0).unwrap();
            storage.commit_index_create(slot).unwrap();
            assert_eq!(storage.index_table_slot(slot), Some(owner));
            assert_eq!(
                storage.index_visible_to(slot, 0).unwrap().relation_oid(),
                anchor.companion_oid(FIRST_TOAST_INDEX_OID)
            );
            assert_eq!(storage.indexes_for("public", "toast_owner", 0).count(), 0);
            assert_eq!(
                storage
                    .unique_indexes_for("public", "toast_owner", 0)
                    .count(),
                0
            );
            storage
                .rebuild_value_caches(owner, ValueCacheRebuild::Committed)
                .unwrap();
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
            let generation = storage.table(owner).generation();
            for kind in [IndexKind::OrdinaryUnbuilt, IndexKind::Ordinary] {
                let mut changed = storage.index_visible_to(slot, 0).unwrap().mutable;
                changed.kind = kind;
                assert!(
                    storage
                        .alter_index_definition(slot, changed, 7)
                        .unwrap()
                        .is_none()
                );
                storage
                    .prepare_index_definition_enforcers(slot, 7, IndexBindingOperation::State)
                    .unwrap();
                storage.publish_index_definition(slot, 7).unwrap();
                assert_eq!(
                    storage.index_visible_to(slot, 0).unwrap().mutable.kind,
                    kind
                );
                assert_eq!(storage.table(owner).generation(), generation);
                assert_eq!(storage.indexes_for("public", "toast_owner", 0).count(), 0);
            }
            for name in [SqlName::parse("toast_retired").unwrap(), definition.name] {
                assert!(storage.rename_index(slot, name, 7).unwrap().is_none());
                storage.publish_index_rename(slot, 7).unwrap();
                assert_eq!(storage.index_visible_to(slot, 0).unwrap().name, name);
                assert_eq!(storage.table(owner).generation(), generation);
            }
            let mut stale = definition;
            stale.created_at = 0;
            stale.name = SqlName::parse("stale_toast").unwrap();
            stale.relation = IndexRelationSource::Toast(
                CatalogPosition::new(owner, position.created_at() + 1).unwrap(),
            );
            assert_eq!(
                storage.create_index(stale, 0).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(storage.indexes_for("public", "toast_owner", 0).count(), 0);
        });
    }

    #[test]
    fn index_admission_uses_the_selected_table_owner_for_live_and_replayed_definitions() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_indexes = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let (owner, _) = storage
                .create_role(
                    SqlName::parse("selected_index_owner").unwrap(),
                    RoleAttributes::ORDINARY,
                    7,
                )
                .unwrap();
            storage.commit_role_change(owner);
            let table = storage
                .create_table(make_def(
                    "owned_index_source",
                    &[("value", ColType::Text, false)],
                ))
                .unwrap();
            let table_object = AccessObject {
                class: AccessClass::Table,
                slot: table as u16,
            };
            assert!(storage.set_object_owner(table_object, owner, 7).is_none());
            let ordinary = IndexDef {
                schema: SqlName::parse("public").unwrap(),
                table: SqlName::parse("owned_index_source").unwrap(),
                name: SqlName::parse("owned_index").unwrap(),
                n_cols: 1,
                ..IndexDef::EMPTY
            };
            let ordinary_slot = storage.create_index(ordinary, 7).unwrap();
            let admitted = storage.index_visible_to(ordinary_slot, 7).unwrap();
            assert_eq!(admitted.ownership.owner_to(7), owner as u16);
            assert_eq!(admitted.ownership.pending, None);
            storage.commit_index_create(ordinary_slot).unwrap();
            storage.commit_object_owner(table_object, 7);
            assert_eq!(
                storage
                    .index_visible_to(ordinary_slot, 0)
                    .unwrap()
                    .ownership
                    .owner,
                owner as u16
            );

            let position = storage.table_position(table);
            let anchor = storage.table_def(table, 0).toast_index_identity.unwrap();
            let main_oid = RelationIdentity::parse(position.created_at())
                .unwrap()
                .oid();
            let table_name = crate::stack_format!(64, "pg_toast_{main_oid}");
            let index_name = crate::stack_format!(64, "pg_toast_{main_oid}_index");
            let mut replayed = IndexDef {
                relation: IndexRelationSource::Toast(position),
                created_at: anchor.created_at(),
                schema: SqlName::parse("pg_toast").unwrap(),
                table: SqlName::parse(table_name.as_str()).unwrap(),
                name: SqlName::parse(index_name.as_str()).unwrap(),
                unique: true,
                n_cols: 2,
                collations: [Collation::None; MAX_INDEX_COLS],
                ..IndexDef::EMPTY
            };
            replayed.columns[1] = 1;
            replayed.resolved_operator_classes[0] =
                Some(IndexOperatorClass::Btree(BtreeOperatorClass::Oid));
            replayed.resolved_operator_classes[1] =
                Some(IndexOperatorClass::Btree(BtreeOperatorClass::Int4));
            let toast = storage.create_index(replayed, 0).unwrap();
            storage.commit_index_create(toast).unwrap();
            assert_eq!(
                storage.index_visible_to(toast, 0).unwrap().ownership.owner,
                owner as u16
            );
            assert_eq!(
                storage
                    .indexes_for("public", "owned_index_source", 0)
                    .count(),
                1
            );
        });
    }

    fn index(name: &str, table: &str, column: u16, included: u16) -> IndexDef {
        let mut definition = IndexDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse(name).unwrap(),
            table: SqlName::parse(table).unwrap(),
            n_cols: 1,
            n_include_cols: 1,
            ..IndexDef::EMPTY
        };
        definition.columns[0] = column;
        definition.include_columns[0] = included;
        definition
    }

    #[test]
    fn private_index_names_reserve_schema_moves_and_restore_comments_without_partial_edits() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let target_schema = SqlName::parse("binding_destination").unwrap();
        storage.create_schema(target_schema).unwrap();
        let source = make_def(
            "binding_source",
            &[
                ("key", ColType::Int4, false),
                ("payload", ColType::Int4, false),
            ],
        );
        let table = storage.create_table(source.clone()).unwrap();
        let mut target = make_def(
            "binding_target",
            &[
                ("key", ColType::Int4, false),
                ("payload", ColType::Int4, false),
            ],
        );
        target.schema = target_schema;
        storage.create_table(target).unwrap();
        storage
            .create_table(make_def(
                "binding_replacement",
                &[
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            ))
            .unwrap();
        let slot = storage
            .create_index(index("binding_index", "binding_source", 0, 1), 7)
            .unwrap();
        storage.commit_index_create(slot).unwrap();
        let mut occupied = index("binding_index", "binding_target", 0, 1);
        occupied.schema = target_schema;
        let occupied_slot = storage.create_index(occupied, 8).unwrap();
        storage.commit_index_create(occupied_slot).unwrap();
        storage
            .apply_comment(
                CommentClass::Relation,
                source.schema,
                SqlName::parse("binding_index").unwrap(),
                0,
                Some(StackStr::from_str("index comment")),
            )
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut moved = source;
            moved.schema = target_schema;
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(SqlName::parse("key").unwrap());
            mapping[1] = Some(SqlName::parse("payload").unwrap());
            assert_eq!(
                storage
                    .write_table_def(
                        storage.select_table_definition(table, 7).0,
                        moved.clone(),
                        &mapping,
                        false,
                        crate::storage::ColumnSet::EMPTY,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_TABLE
            );
            assert!(storage.pending_table_def(table).is_none());
            assert!(storage.index(slot).pending_table.is_none());
            assert_eq!(
                storage
                    .drop_index("binding_destination", "binding_index", 8)
                    .unwrap(),
                Some(occupied_slot)
            );
            storage.commit_index_drop(occupied_slot).unwrap();
            let change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    moved,
                    &mapping,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let mut second = index("second_binding_index", "binding_source", 0, 1);
            second.schema = target_schema;
            let second_slot = storage.create_index(second, 7).unwrap();
            for (clustered, replica_identity) in [(true, false), (false, true)] {
                let definition = IndexMutableDefinition {
                    clustered,
                    replica_identity,
                    ..IndexMutableDefinition::DEFAULT
                };
                storage.alter_index_definition(slot, definition, 7).unwrap();
                assert_eq!(
                    storage
                        .alter_index_definition(second_slot, definition, 7)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::INVALID_OBJECT_DEFINITION
                );
            }
            storage.rollback_index_definition(slot, None);
            storage.rollback_index_create(second_slot);
            assert!(
                storage
                    .comment_text(
                        CommentClass::Relation,
                        "binding_destination",
                        "binding_index",
                        0,
                        7
                    )
                    .is_some()
            );
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "binding_index", 0, 8)
                    .is_some()
            );
            assert_eq!(
                storage.create_index(occupied, 8).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.release_row_locks(8);
            let replacement = storage
                .create_index(index("binding_index", "binding_replacement", 0, 1), 7)
                .unwrap();
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "binding_index", 0, 7)
                    .is_none()
            );
            storage.rollback_index_create(replacement);
            storage.rollback_table_def(change);
            assert!(storage.index(slot).pending_table.is_none());
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "binding_index", 0, 7)
                    .is_some()
            );
            assert!(
                storage
                    .comment_text(
                        CommentClass::Relation,
                        "binding_destination",
                        "binding_index",
                        0,
                        7
                    )
                    .is_none()
            );
        });
    }

    #[test]
    fn private_index_bindings_do_not_select_a_reused_table_or_another_storage() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let mut other_budget = test_budget(&config);
        let mut other = Storage::new(&config, &mut other_budget, 0).unwrap();
        let definition = make_def(
            "binding_source",
            &[
                ("key", ColType::Int4, false),
                ("payload", ColType::Int4, false),
            ],
        );
        let table = storage.create_table(definition.clone()).unwrap();
        other.create_table(definition.clone()).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let slot = storage
                .create_index(index("binding_index", "binding_source", 0, 1), 7)
                .unwrap();
            storage.commit_index_create(slot).unwrap();
            let captured = storage.index(slot);
            assert!(other.project_index_binding(captured, 0).is_none());
            let selected_indexes = storage.indexes_for("public", "binding_source", 0);
            storage.commit_drop(table);
            let replacement = storage.create_table_in(definition, 9).unwrap();
            storage.commit_create(replacement);
            assert_eq!(replacement, table);
            assert!(storage.index_visible_to(slot, 0).is_none());
            assert!(storage.index_table_slot(slot).is_none());
            assert!(
                storage
                    .index_definition("public", "binding_index", 0)
                    .is_none()
            );
            assert_eq!(
                storage.indexes_for("public", "binding_source", 0).count(),
                0
            );
            storage
                .create_index(index("replacement_index", "binding_source", 0, 1), 0)
                .unwrap();
            assert_eq!(selected_indexes.count(), 0);
            assert_eq!(
                storage.indexes_for("public", "binding_source", 0).count(),
                1
            );
        });
    }

    #[test]
    fn private_index_bindings_revalidate_lifetime_and_shape_after_installation_waits() {
        for replace in [false, true] {
            let config = test_config();
            let mut budget = test_budget(&config);
            let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
            let definition = make_def(
                "binding_source",
                &[
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            );
            let table = storage.create_table(definition.clone()).unwrap();
            storage
                .create_table(make_def(
                    "unrelated_source",
                    &[
                        ("key", ColType::Int4, false),
                        ("payload", ColType::Int4, false),
                    ],
                ))
                .unwrap();
            let held = storage.tables[table].lifecycle();
            let waits = storage
                .definition_publication
                .waits
                .load(core::sync::atomic::Ordering::Acquire);
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            std::thread::scope(|scope| {
                let storage = &storage;
                let selected = scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        storage.create_index(index("selected_index", "binding_source", 0, 1), 7)
                    })
                });
                let deadline = crate::test_wait::deadline();
                while storage
                    .definition_publication
                    .waits
                    .load(core::sync::atomic::Ordering::Acquire)
                    == waits
                    && std::time::Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                let waited = storage
                    .definition_publication
                    .waits
                    .load(core::sync::atomic::Ordering::Acquire)
                    > waits;
                scope.spawn(move || {
                    let result = crate::mem::guard::forbid_alloc(|| {
                        storage.create_index(index("unrelated_index", "unrelated_source", 0, 1), 8)
                    });
                    send.send(result).unwrap();
                });
                let progress = receive.recv_timeout(crate::test_wait::HANG);
                if replace {
                    storage.tables[table].definition.write().identity.created_at += 1;
                } else {
                    let mut mapping = [None; MAX_COLUMNS];
                    mapping[0] = Some(SqlName::parse("key").unwrap());
                    mapping[1] = Some(SqlName::parse("payload").unwrap());
                    storage
                        .pending_table_defs
                        .stage(
                            &storage.tables[table].definition,
                            PendingTableDef {
                                txid: 7,
                                revision: 999,
                                def: definition.freeze(&storage.metadata_pool).unwrap(),
                                transition: storage.metadata_pool.copy_slice(&[0u16, 1]).unwrap(),
                                column_mapping: storage
                                    .metadata_pool
                                    .copy_slice(&mapping[..2])
                                    .unwrap(),
                                rewrites_rows: false,
                                class_tuple_private: false,
                            },
                            storage.max_catalog_versions_per_object,
                        )
                        .unwrap();
                }
                drop(held);
                let selected = selected.join().unwrap();
                assert!(
                    waited,
                    "index installation did not reach its selected lifecycle wait"
                );
                assert!(
                    progress.is_ok(),
                    "an unrelated index waited for the selected table"
                );
                assert!(progress.unwrap().is_ok());
                assert_eq!(
                    selected.unwrap_err().sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                assert!(storage.index_slot("public", "selected_index", 7).is_none());
            });
        }
    }

    #[test]
    fn private_index_bindings_follow_their_selected_definition_through_rollback_and_commit() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "binding_source",
                &[
                    ("discarded", ColType::Int4, false),
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let before = storage
                .create_index(index("before_shape", "binding_source", 1, 2), 7)
                .unwrap();
            let first = make_def(
                "binding_source",
                &[
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                    ("added", ColType::Int4, false),
                ],
            );
            let mut first_mapping = [None; MAX_COLUMNS];
            first_mapping[1] = Some(SqlName::parse("key").unwrap());
            first_mapping[2] = Some(SqlName::parse("payload").unwrap());
            let _first_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    first,
                    &first_mapping,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let during = storage
                .create_index(index("during_shape", "binding_source", 2, 1), 7)
                .unwrap();
            let second = make_def(
                "binding_renamed",
                &[
                    ("payload", ColType::Int4, false),
                    ("key", ColType::Int4, false),
                    ("added_renamed", ColType::Int4, false),
                ],
            );
            let mut second_mapping = [None; MAX_COLUMNS];
            second_mapping[0] = Some(SqlName::parse("key").unwrap());
            second_mapping[1] = Some(SqlName::parse("payload").unwrap());
            second_mapping[2] = Some(SqlName::parse("added_renamed").unwrap());
            for commit in [false, true] {
                let second_change = storage
                    .write_table_def(
                        storage.select_table_definition(table, 7).0,
                        second.clone(),
                        &second_mapping,
                        false,
                        crate::storage::ColumnSet::EMPTY,
                    )
                    .unwrap();
                for (slot, expected) in [(before, (1, 0)), (during, (2, 0))] {
                    let projected = storage.index_visible_to(slot, 7).unwrap();
                    assert_eq!(
                        (projected.columns[0], projected.include_columns[0]),
                        expected
                    );
                    assert_eq!(projected.table.as_str(), "binding_renamed");
                    let repeated = storage.project_index_binding(projected, 7).unwrap();
                    assert_eq!((repeated.columns[0], repeated.include_columns[0]), expected);
                    assert!(storage.index_visible_to(slot, 8).is_none());
                }
                assert_eq!(
                    storage.indexes_for("public", "binding_renamed", 7).count(),
                    2
                );
                if commit {
                    assert_eq!(
                        crate::storage::tests::publish_table_definition(&storage, second_change)
                            .unwrap(),
                        Some(false)
                    );
                } else {
                    storage.rollback_table_def(second_change);
                    for (slot, expected) in [(before, (0, 1)), (during, (2, 1))] {
                        let reverted = storage.index_visible_to(slot, 7).unwrap();
                        assert_eq!((reverted.columns[0], reverted.include_columns[0]), expected);
                    }
                }
            }
            for (slot, expected) in [(before, (1, 0)), (during, (2, 0))] {
                storage.commit_index_create(slot).unwrap();
                let committed = storage.index_visible_to(slot, 8).unwrap();
                assert_eq!(
                    (committed.columns[0], committed.include_columns[0]),
                    expected
                );
                assert_eq!(committed.table_binding.unwrap().revision, 0);
            }
        });
    }

    #[test]
    fn prepared_bindings_project_committed_indexes_through_table_moves_and_reordering() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let destination = SqlName::parse("binding_destination").unwrap();
        storage.create_schema(destination).unwrap();
        let table = storage
            .create_table(make_def(
                "binding_source",
                &[
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut definition = index("committed_key", "binding_source", 0, 1);
            definition.collations = [Collation::None; MAX_INDEX_COLS];
            let committed = storage.create_index(definition, 0).unwrap();
            storage.commit_index_create(committed).unwrap();
            storage.refresh_enforcers(table).unwrap();
            assert_eq!(storage.value_binding_count(table), 1);
            assert_eq!(storage.value_binding_columns(table, 0).0[0], 0);
            assert!(storage.value_binding_include_mask(table, 0).contains(1));

            let mut replacement = make_def(
                "binding_renamed",
                &[
                    ("payload", ColType::Int4, false),
                    ("key", ColType::Int4, false),
                ],
            );
            replacement.schema = destination;
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(SqlName::parse("key").unwrap());
            mapping[1] = Some(SqlName::parse("payload").unwrap());
            let change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    replacement,
                    &mapping,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            assert_eq!(storage.value_binding_count(table), 1);
            assert_eq!(storage.value_binding_columns(table, 0).0[0], 1);
            assert!(storage.value_binding_include_mask(table, 0).contains(0));
            assert!(!storage.value_binding_include_mask(table, 0).contains(1));

            let mut redundant = index("private_key", "binding_renamed", 1, 0);
            redundant.schema = destination;
            redundant.collations = [Collation::None; MAX_INDEX_COLS];
            redundant.n_include_cols = 0;
            let private = storage.create_index(redundant, 7).unwrap();
            storage
                .prepare_index_definition_enforcers(
                    private,
                    7,
                    super::super::IndexBindingOperation::Create,
                )
                .unwrap();
            assert_eq!(storage.value_binding_count(table), 1);
            assert_eq!(storage.value_binding_columns(table, 0).0[0], 1);
            assert!(storage.value_binding_include_mask(table, 0).contains(0));

            storage.rollback_index_create(private);
            storage.rollback_table_def(change);
            assert_eq!(storage.value_binding_count(table), 1);
            assert_eq!(storage.value_binding_columns(table, 0).0[0], 0);
            assert!(storage.value_binding_include_mask(table, 0).contains(1));
            assert!(!storage.value_binding_include_mask(table, 0).contains(0));
        });
    }
}
