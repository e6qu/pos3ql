//! Materialized-view lifecycle undo retains its catalog and backing-table lifetimes.

use super::catalog_sources::{CatalogDdlOperation, CatalogEditTarget, CatalogObjectSlot};
use super::type_contracts::{QueryTableTarget, QueryTargetUse};
use super::*;

#[derive(Clone, Copy)]
pub(crate) struct MatviewDdlUndo {
    catalog: CatalogDdlUndo,
    backing: QueryTableTarget,
}

impl core::fmt::Debug for MatviewDdlUndo {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        output
            .debug_struct("MatviewDdlUndo")
            .field("catalog", &self.catalog)
            .field("backing_table", &self.backing.slot())
            .field("backing_created_at", &self.backing.created_at())
            .finish()
    }
}

impl MatviewDdlUndo {
    pub(crate) fn slot(self) -> usize {
        self.catalog.slot()
    }
    pub(crate) fn is_creation(self) -> bool {
        self.catalog.operation == CatalogDdlOperation::Create
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MatviewEditTarget {
    catalog: CatalogEditTarget,
    backing: QueryTableTarget,
    usage: QueryTargetUse,
    query_version: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingMatviewQuery {
    pub(super) transaction: u32,
    version: u64,
    pub(super) sql: StackStr<VIEW_SQL_MAX>,
    pub(super) creation_path: StackStr<128>,
    pub(super) dependency_slot: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct MatviewQueryUndo {
    target: CatalogEditTarget,
    prior: Option<PendingMatviewQuery>,
}

impl core::fmt::Debug for MatviewQueryUndo {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        output
            .debug_struct("MatviewQueryUndo")
            .field("target", &self.target)
            .finish()
    }
}

impl Storage {
    pub(crate) fn matview_position(&self, slot: usize) -> CatalogPosition {
        CatalogPosition::new(slot, self.matview(slot).created_at)
            .expect("materialized-view position selects a live object")
    }

    pub(crate) fn matview_replay_slot(
        &self,
        position: CatalogPosition,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let view = self
            .matview_catalog()
            .get(position.slot())
            .copied()
            .filter(|view| {
                view.database == current_database()
                    && view.visible_to(transaction)
                    && view.created_at == position.created_at()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "journal materialized-view source identity does not match"
                )
            })?;
        let backing = usize::from(view.backing_table);
        if backing < self.table_count()
            && self.table(backing).created_at() == view.backing_created_at
            && {
                let definition = self.table_def(backing, transaction);
                definition.identity().database == current_database()
                    && definition.schema.as_str() == schema
                    && definition.name.as_str() == name
            }
        {
            Ok(position.slot())
        } else {
            Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal materialized-view backing identity or name does not match"
            ))
        }
    }

    pub(crate) fn prepare_matview_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<MatviewEditTarget, SqlError> {
        let view = self
            .matview_catalog()
            .get(slot)
            .copied()
            .filter(|view| view.database == current_database() && view.visible_to(transaction))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "materialized-view edit source no longer exists"
                )
            })?;
        if let Some(owner) = view
            .pending_population
            .map(|pending| pending.transaction)
            .or(view.pending_query.map(|pending| pending.transaction))
            .or_else(|| view.ddl_state.pending_txid())
            .filter(|owner| *owner != transaction)
        {
            return Err(self.catalog_ddl_wait_error(transaction, owner, "materialized view"));
        }
        let backing = self.query_table_target(
            usize::from(view.backing_table),
            transaction,
            QueryTargetUse::Retirement,
        )?;
        if backing.created_at() != view.backing_created_at {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "materialized-view backing relation has been replaced"
            ));
        }
        Ok(MatviewEditTarget {
            catalog: CatalogEditTarget {
                source: self.catalog_object_identity(
                    CatalogObjectSlot::MaterializedView(slot),
                    view.database,
                    view.created_at,
                ),
                transaction,
                version: view.ddl_version,
            },
            backing,
            usage: QueryTargetUse::Retirement,
            query_version: view
                .pending_query
                .map_or(view.query_version, |pending| pending.version),
        })
    }

    pub(crate) fn stage_matview_query_schema(
        &self,
        target: MatviewEditTarget,
        old: SqlName,
        new: SqlName,
        transaction: u32,
    ) -> Result<MatviewQueryUndo, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::StoredQueryEdit,
            Some(target.backing),
        );
        owners.validate_target(transaction, QueryTargetUse::Read)?;
        let slot = target.catalog.slot();
        let view = owners
            .catalog
            .get(slot)
            .copied()
            .filter(|view| {
                matches!(
                    target.catalog.source.slot(),
                    CatalogObjectSlot::MaterializedView(_)
                ) && target.catalog.transaction == transaction
                    && target.catalog.source.matches(
                        self,
                        view.database,
                        view.created_at,
                        view.ddl_state,
                    )
                    && view.visible_to(transaction)
                    && view.ddl_version == target.catalog.version
                    && view
                        .pending_query
                        .map_or(view.query_version, |pending| pending.version)
                        == target.query_version
                    && usize::from(view.backing_table) == target.backing.slot()
                    && view.backing_created_at == target.backing.created_at()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "materialized query changed after edit preparation"
                )
            })?;
        if let Some(owner) = view
            .pending_query
            .map(|pending| pending.transaction)
            .filter(|owner| *owner != transaction)
        {
            drop(owners);
            return Err(self.catalog_ddl_wait_error(transaction, owner, "materialized query"));
        }
        let selected = view.definition_for(transaction);
        let mut sql = selected.sql;
        let mut creation_path = selected.creation_path;
        rename_schema_path(&mut creation_path, old, new)?;
        rename_schema_qualified_sql(&mut sql, &mut [], old, new)?;
        let version = self.catalog_sequence.next()?;
        let dependency_slot = owners.allocate_preserved_query_image(
            self,
            StoredQueryDependencyOwner::MaterializedView(slot as u16),
            view.pending_query.map(|pending| pending.dependency_slot),
            transaction,
        )?;
        let count = owners.dependencies.pending[dependency_slot as usize].count;
        rename_dependency_schema(
            owners.dependencies.image_mut(
                self.stored_query_dependency_pending_base + dependency_slot as usize,
                count,
                self.stored_query_dependencies_per_image,
            ),
            old,
            new,
        );
        owners.catalog[slot].pending_query = Some(PendingMatviewQuery {
            transaction,
            version,
            sql,
            creation_path,
            dependency_slot,
        });
        Ok(MatviewQueryUndo {
            target: CatalogEditTarget {
                version,
                ..target.catalog
            },
            prior: view.pending_query,
        })
    }

    pub(crate) fn finish_matview_query(
        &self,
        undo: MatviewQueryUndo,
        decision: CatalogDdlDecision,
    ) {
        let mut owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let slot = undo.target.slot();
        let Some(view) = owners.catalog.get(slot).copied().filter(|view| {
            matches!(
                undo.target.source.slot(),
                CatalogObjectSlot::MaterializedView(_)
            ) && undo
                .target
                .source
                .matches(self, view.database, view.created_at, view.ddl_state)
        }) else {
            return;
        };
        let Some(pending) = view.pending_query.filter(|pending| {
            pending.transaction == undo.target.transaction && pending.version == undo.target.version
        }) else {
            return;
        };
        match decision {
            CatalogDdlDecision::Commit => {
                self.commit_pending_dependencies_in(
                    &mut owners.dependencies,
                    StoredQueryDependencyOwner::MaterializedView(slot as u16),
                    pending.dependency_slot,
                );
                let view = &mut owners.catalog[slot];
                view.sql = pending.sql;
                view.creation_path = pending.creation_path;
                view.query_version = pending.version;
                view.pending_query = None;
            }
            CatalogDdlDecision::Rollback => {
                owners.dependencies.pending[pending.dependency_slot as usize] =
                    PendingStoredQueryDependencies::EMPTY;
                owners.catalog[slot].pending_query = undo.prior;
            }
        }
    }

    pub(crate) fn drop_matview(
        &self,
        target: MatviewEditTarget,
        transaction: u32,
    ) -> Result<MatviewDdlUndo, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::CatalogRetirement,
            Some(target.backing),
        );
        owners.validate_target(transaction, target.usage)?;
        let slot = target.catalog.slot();
        if !matches!(
            target.catalog.source.slot(),
            CatalogObjectSlot::MaterializedView(_)
        ) || target.catalog.transaction != transaction
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "materialized-view edit belongs to another transaction"
            ));
        }
        let view = owners
            .catalog
            .get(slot)
            .copied()
            .filter(|view| {
                target
                    .catalog
                    .source
                    .matches(self, view.database, view.created_at, view.ddl_state)
                    && view.visible_to(transaction)
                    && view.ddl_version == target.catalog.version
                    && usize::from(view.backing_table) == target.backing.slot()
                    && view.backing_created_at == target.backing.created_at()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "materialized view changed after edit preparation"
                )
            })?;
        if let Some(owner) = view
            .pending_population
            .map(|pending| pending.transaction)
            .or_else(|| view.ddl_state.pending_txid())
            .filter(|owner| *owner != transaction)
        {
            return Err(self.catalog_ddl_wait_error(transaction, owner, "materialized view"));
        }
        let version = self.catalog_sequence.next()?;
        let (schema, name) = owners
            .target_names(transaction)
            .expect("materialized-view retirement owns backing relation");
        self.stage_catalog_comment_drop(
            owners
                .comments
                .as_mut()
                .expect("materialized-view retirement owns comments"),
            target.catalog.source,
            transaction,
            |comment| {
                matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                    && comment.matches_to(
                        Some(current_database()),
                        comment.class,
                        schema.as_str(),
                        name.as_str(),
                        comment.subid,
                        transaction,
                    )
            },
        )?;
        owners.catalog[slot].ddl_state = view.ddl_state.drop_by(transaction);
        owners.catalog[slot].ddl_version = version;
        Ok(MatviewDdlUndo {
            catalog: CatalogDdlUndo {
                source: target.catalog.source,
                transaction,
                operation: CatalogDdlOperation::Drop,
                version,
                prior_version: view.ddl_version,
            },
            backing: target.backing,
        })
    }

    pub(crate) fn replay_drop_matview(
        &mut self,
        position: CatalogPosition,
        schema: &str,
        name: &str,
    ) -> Result<(), SqlError> {
        let slot = self.matview_replay_slot(position, schema, name, 0)?;
        let view = self.matview(slot);
        let backing = self.query_table_target(
            usize::from(view.backing_table),
            0,
            QueryTargetUse::JournalRetirement,
        )?;
        if backing.created_at() != view.backing_created_at {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal materialized-view backing lifetime does not match"
            ));
        }
        let target = MatviewEditTarget {
            catalog: CatalogEditTarget {
                source: self.catalog_object_identity(
                    CatalogObjectSlot::MaterializedView(slot),
                    view.database,
                    view.created_at,
                ),
                transaction: 0,
                version: view.ddl_version,
            },
            backing,
            usage: QueryTargetUse::JournalRetirement,
            query_version: view.query_version,
        };
        let change = self.drop_matview(target, 0)?;
        self.finish_matview_change(change, CatalogDdlDecision::Commit);
        Ok(())
    }

    fn matview_ddl_slot(
        &self,
        views: &FixedVec<MatviewDef>,
        undo: MatviewDdlUndo,
    ) -> Option<usize> {
        let CatalogObjectSlot::MaterializedView(slot) = undo.catalog.source.slot() else {
            return None;
        };
        let view = views.get(slot)?;
        if !undo
            .catalog
            .source
            .matches(self, view.database, view.created_at, view.ddl_state)
            || usize::from(view.backing_table) != undo.backing.slot()
            || view.backing_created_at != undo.backing.created_at()
        {
            return None;
        }
        let matched = match undo.catalog.operation {
            CatalogDdlOperation::Create => matches!(view.ddl_state,
                CatalogDdlState::PendingCreate { txid } | CatalogDdlState::PendingCreateDrop { txid }
                    if txid == undo.catalog.transaction),
            CatalogDdlOperation::Drop => {
                view.ddl_state.pending_drop_txid() == Some(undo.catalog.transaction)
                    && view.ddl_version == undo.catalog.version
            }
        };
        matched.then_some(slot)
    }

    pub(crate) fn validate_matview_change(
        &self,
        undo: MatviewDdlUndo,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::MetadataCommit,
            Some(undo.backing),
        );
        if undo.catalog.transaction != transaction
            || self.matview_ddl_slot(&owners.catalog, undo).is_none()
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "materialized-view change no longer belongs to its selected source"
            ));
        }
        owners.validate_target(transaction, QueryTargetUse::Retirement)
    }

    pub(crate) fn finish_matview_change(&self, undo: MatviewDdlUndo, decision: CatalogDdlDecision) {
        let mut owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::CatalogRetirement,
            None,
        );
        let Some(slot) = self.matview_ddl_slot(&owners.catalog, undo) else {
            return;
        };
        match (undo.catalog.operation, decision) {
            (CatalogDdlOperation::Create, CatalogDdlDecision::Commit) => {
                let view = &mut owners.catalog[slot];
                if matches!(view.ddl_state, CatalogDdlState::PendingCreateDrop { .. }) {
                    return;
                }
                if let Some(pending) = view
                    .pending_population
                    .filter(|pending| pending.transaction == undo.catalog.transaction)
                {
                    view.populated = pending.populated;
                    view.population_version = pending.version;
                    view.pending_population = None;
                }
                view.ddl_state = view.ddl_state.commit_create();
                view.ownership = view.ownership.committed();
            }
            (CatalogDdlOperation::Drop, CatalogDdlDecision::Rollback) => {
                self.finish_catalog_comment_drop(
                    owners
                        .comments
                        .as_mut()
                        .expect("materialized-view rollback owns comments"),
                    undo.catalog.source,
                    undo.catalog.transaction,
                    decision,
                );
                let view = &mut owners.catalog[slot];
                view.ddl_state = view.ddl_state.rollback_drop(undo.catalog.transaction);
                view.ddl_version = undo.catalog.prior_version;
            }
            (CatalogDdlOperation::Create, CatalogDdlDecision::Rollback)
            | (CatalogDdlOperation::Drop, CatalogDdlDecision::Commit) => {
                self.finish_catalog_comment_drop(
                    owners
                        .comments
                        .as_mut()
                        .expect("materialized-view retirement owns comments"),
                    undo.catalog.source,
                    undo.catalog.transaction,
                    CatalogDdlDecision::Commit,
                );
                owners.dependencies.counts[self.matview_dependency_image(slot)] = 0;
                let object = AccessObject {
                    class: AccessClass::MaterializedView,
                    slot: slot as u16,
                };
                Self::clear_object_acl_entries_locked(
                    owners
                        .permissions
                        .as_mut()
                        .expect("materialized-view retirement owns permissions"),
                    object,
                );
                Self::clear_extension_dependencies_for_object_locked(
                    owners
                        .extensions
                        .as_mut()
                        .expect("materialized-view retirement owns extension references"),
                    object,
                );
                owners.catalog[slot] = MatviewDef::EMPTY;
            }
        }
    }
}

impl Storage {
    /// Registers a materialized view as an uncommitted CREATE owned by `txid`.
    /// Unlike `create_view`, the name-vs-table collision is owned by the backing
    /// table's own creation, so no `find_table`/`or_replace` handling is needed.
    pub(crate) fn create_matview(
        &self,
        allocation: CatalogAllocation,
        backing_table: usize,
        query: StoredQueryDefinition,
        populated: bool,
        txid: u32,
    ) -> Result<MatviewDdlUndo, SqlError> {
        let target =
            self.query_table_target(backing_table, txid, type_contracts::QueryTargetUse::Read)?;
        let name = self.table_def(backing_table, txid).name;
        let ownership = self.initial_ownership(txid)?;
        let mut owners = self.catalog_dependency_owners(
            &self.matviews,
            type_contracts::CatalogPublicationInput::QueryCreation(query.dependencies.view()),
            Some(target),
        );
        owners.validate_target(txid, type_contracts::QueryTargetUse::Read)?;
        if let Some(existing) = owners.catalog.iter().find(|m| {
            m.database == current_database()
                && m.ddl_state != CatalogDdlState::Absent
                && usize::from(m.backing_table) == backing_table
        }) {
            if let Some(blocker) = existing
                .ddl_state
                .pending_txid()
                .filter(|&owner| owner != txid)
            {
                drop(owners);
                return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
            }
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "materialized view \"{}\" already exists",
                name.as_str()
            ));
        }
        let (new, created_at) = match allocation {
            CatalogAllocation::New => {
                let slot = owners
                    .catalog
                    .iter()
                    .position(|view| view.ddl_state == CatalogDdlState::Absent)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "too many materialized views (limit {})",
                            owners.catalog.len()
                        )
                    })?;
                (slot, self.catalog_sequence.next()?)
            }
            CatalogAllocation::Restore(position) => {
                if owners
                    .catalog
                    .get(position.slot())
                    .is_none_or(|view| view.ddl_state != CatalogDdlState::Absent)
                    || owners.catalog.iter().any(|view| {
                        view.database == current_database()
                            && view.ddl_state != CatalogDdlState::Absent
                            && view.created_at == position.created_at()
                    })
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "saved materialized-view position is unavailable or its creation identity is already used"
                    ));
                }
                self.catalog_sequence.observe(position.created_at());
                (position.slot(), position.created_at())
            }
        };
        let image = self.matview_dependency_image(new);
        owners.write_query_image(self, image, query.dependencies.view(), txid)?;
        owners.catalog[new] = MatviewDef {
            database: current_database(),
            created_at,
            backing_table: backing_table as u16,
            backing_created_at: target.created_at(),
            ddl_version: created_at,
            sql: query.sql,
            creation_path: query.creation_path,
            ownership,
            populated,
            population_version: created_at,
            pending_population: None,
            query_version: created_at,
            pending_query: None,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Self::clear_object_acl_entries_locked(
            owners
                .permissions
                .as_mut()
                .expect("materialized-view creation owns permissions"),
            AccessObject {
                class: AccessClass::MaterializedView,
                slot: new as u16,
            },
        );
        Ok(MatviewDdlUndo {
            catalog: CatalogDdlUndo {
                source: self.catalog_object_identity(
                    CatalogObjectSlot::MaterializedView(new),
                    current_database(),
                    created_at,
                ),
                transaction: txid,
                operation: CatalogDdlOperation::Create,
                version: created_at,
                prior_version: 0,
            },
            backing: target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config, test_view_definition};

    #[test]
    fn matview_lifecycle_undo_rejects_reused_catalog_and_backing_lifetimes() {
        let mut config = test_config();
        config.max_materialized_views = 1;
        config.max_tables = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "original_materialized",
                &[("value", ColType::Int4, true)],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let original = storage
                .create_matview(
                    crate::storage::CatalogAllocation::New,
                    table,
                    test_view_definition(StoredQueryDependencies::EMPTY).query,
                    false,
                    7,
                )
                .unwrap();
            let selected = storage.prepare_matview_edit(original.slot(), 7).unwrap();
            storage.finish_matview_change(original, CatalogDdlDecision::Rollback);
            let replacement = storage
                .create_matview(
                    crate::storage::CatalogAllocation::New,
                    table,
                    test_view_definition(StoredQueryDependencies::EMPTY).query,
                    false,
                    9,
                )
                .unwrap();
            assert_eq!(original.slot(), replacement.slot());
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_matview_change(original, decision);
            }
            assert_eq!(
                storage.drop_matview(selected, 7).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage.finish_matview_change(replacement, CatalogDdlDecision::Commit);
            let first = storage
                .drop_matview(
                    storage
                        .prepare_matview_edit(replacement.slot(), 11)
                        .unwrap(),
                    11,
                )
                .unwrap();
            storage.finish_matview_change(first, CatalogDdlDecision::Rollback);
            let second = storage
                .drop_matview(
                    storage
                        .prepare_matview_edit(replacement.slot(), 11)
                        .unwrap(),
                    11,
                )
                .unwrap();
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_matview_change(first, decision);
            }
            assert!(!storage.matview(replacement.slot()).visible_to(11));
            storage.finish_matview_change(second, CatalogDdlDecision::Rollback);
            storage.commit_drop(table);
            let reused = storage
                .create_table(make_def(
                    "replacement_table",
                    &[("value", ColType::Int4, true)],
                ))
                .unwrap();
            assert_eq!(table, reused);
            assert_eq!(
                storage
                    .prepare_matview_edit(replacement.slot(), 13)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                storage.table(reused).definition().name.as_str(),
                "replacement_table"
            );
        });
    }
}
