//! Transaction-private table shapes and their comment identities share one edit owner.

use crate::mem::mutex::MutexGuard;

use super::table_names::DefinitionWrite;
use super::{
    CommentCatalog, CommentClass, CommentEntry, DatabaseOid, MAX_COLUMNS, PendingCommentIdentity,
    PendingTableDef, SqlError, SqlName, Storage, TableDef,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableDefinitionTarget {
    pub(super) source: super::catalog_sources::CatalogObjectIdentity,
    pub(super) transaction: u32,
    pub(super) committed_revision: super::TableDefinitionRevision,
    pub(super) pending_revision: u64,
}

impl TableDefinitionTarget {
    pub(super) fn matches_definition(
        self,
        storage: &Storage,
        definition: &super::table_definitions::DefinitionState,
    ) -> bool {
        self.source.matches(
            storage,
            definition.identity.database,
            definition.identity.created_at,
            definition.identity.existence,
        ) && definition.committed_revision == self.committed_revision
            && storage
                .pending_table_defs
                .selected_revision(definition, self.transaction)
                == self.pending_revision
    }

    pub(crate) fn validate(self, storage: &Storage) -> Result<(), SqlError> {
        let table = storage.tables.get(self.table()).ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "selected table source is no longer present"
            )
        })?;
        let definition = table.definition.read();
        if self.matches_definition(storage, &definition)
            && definition.identity.existence.visible_to(self.transaction)
        {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table definition changed while acquiring its relation lock"
            ))
        }
    }

    pub(crate) fn transaction(self) -> u32 {
        self.transaction
    }

    pub(crate) fn table(self) -> usize {
        match self.source.slot() {
            super::catalog_sources::CatalogObjectSlot::Table(table) => table,
            _ => unreachable!("table definition targets contain table sources"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableDefinitionChange {
    pub(super) source: super::catalog_sources::CatalogObjectIdentity,
    pub(super) transaction: u32,
    pub(super) revision: u64,
    pub(super) committed_revision: super::TableDefinitionRevision,
}

impl TableDefinitionChange {
    pub(crate) fn row_target(self) -> TableDefinitionTarget {
        TableDefinitionTarget {
            source: self.source,
            transaction: self.transaction,
            committed_revision: self.committed_revision,
            pending_revision: self.revision,
        }
    }

    pub(crate) fn table(self) -> usize {
        match self.source.slot() {
            super::catalog_sources::CatalogObjectSlot::Table(table) => table,
            _ => unreachable!("table definition changes contain table sources"),
        }
    }
}

pub(super) struct TableDefinitionEdit<'storage> {
    storage: &'storage Storage,
    table: usize,
    definition: DefinitionWrite<'storage>,
    // Release the definition before allowing retirement or slot reuse.
    _lifecycle: MutexGuard<'storage, ()>,
}

impl<'storage> TableDefinitionEdit<'storage> {
    fn matches_target(&self, target: TableDefinitionTarget) -> bool {
        target.matches_definition(self.storage, &self.definition)
    }

    fn after_wait(self, wait: super::row_ownership::LockWait<'storage>) -> Self {
        let storage = self.storage;
        let table = self.table;
        drop(self);
        #[cfg(test)]
        storage
            .definition_publication
            .waits
            .fetch_add(1, core::sync::atomic::Ordering::Release);
        wait.wait();
        Self::new(storage, table)
    }

    pub(super) fn new(storage: &'storage Storage, table: usize) -> Self {
        let lifecycle = storage.tables[table]
            .lifecycle
            .lock()
            .expect("table lifecycle lock poisoned");
        let definition = storage.tables[table].definition.write();
        Self {
            storage,
            table,
            definition,
            _lifecycle: lifecycle,
        }
    }

    pub(super) fn stage(
        mut self,
        target: TableDefinitionTarget,
        mut definition: TableDef,
        column_mapping: &[Option<SqlName>; MAX_COLUMNS],
        rewrites_rows: bool,
    ) -> Result<TableDefinitionChange, SqlError> {
        let transaction = target.transaction;
        let (mut comments, _source, mut indexes) = loop {
            if !self.matches_target(target) {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "table definition source changed before publication"
                ));
            }
            let comments = self.storage.comment_catalog();
            let source = if !definition.requires_type_catalog()
                && definition.schema == self.definition.committed().schema
                && definition.name == self.definition.committed().name
                && self.definition.pending.is_none()
            {
                None
            } else {
                match super::row_ownership::try_mutex(
                    &self.storage.type_catalog,
                    "type catalog lock poisoned",
                ) {
                    Ok(source) => {
                        self.storage.require_row_type_name(
                            &source,
                            definition.schema,
                            definition.name,
                            transaction,
                            Some(super::RuleTarget::Table(self.table as u16)),
                            None,
                        )?;
                        source.bind_table_definition(&mut definition, transaction)?;
                        Some(source)
                    }
                    Err(wait) => {
                        drop(comments);
                        self = self.after_wait(wait);
                        continue;
                    }
                }
            };
            let indexes = match super::row_ownership::try_mutex(
                &self.storage.index_catalog,
                "index catalog lock poisoned",
            ) {
                Ok(indexes) => indexes,
                Err(wait) => {
                    drop(source);
                    drop(comments);
                    self = self.after_wait(wait);
                    continue;
                }
            };
            break (comments, source, indexes);
        };
        if let Some(owner) = self.definition.locked_by_other(transaction) {
            let name = self.definition.committed().name;
            let storage = self.storage;
            drop(indexes);
            drop(_source);
            drop(comments);
            drop(self);
            return Err(storage.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        if !self.definition.identity.existence.visible_to(transaction) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "pending definition belongs to a retired table incarnation"
            ));
        }
        let prior = self
            .storage
            .pending_table_defs
            .latest_locked(&self.definition);
        let current = prior
            .as_ref()
            .map_or(self.definition.committed(), |version| &version.def);
        definition.has_toast |= current.has_toast
            || definition
                .columns()
                .iter()
                .any(|column| column.ctype.typlen() == -1);
        definition.has_rules |= current.has_rules;
        super::constraint_names::table(&mut definition)?;
        // Column-only edits replace pg_attribute rows. Keep track of edits
        // that also fork pg_class, including a rename followed by renaming back.
        let class_tuple_private = prior
            .as_ref()
            .is_some_and(|version| version.class_tuple_private)
            || current.schema != definition.schema
            || current.name != definition.name
            || current.kind != definition.kind
            || current.persistence != definition.persistence
            || current.access_method != definition.access_method
            || current.storage_options != definition.storage_options
            || current.tablespace != definition.tablespace
            || current.n_columns != definition.n_columns
            || current.n_checks != definition.n_checks
            || current.has_toast != definition.has_toast
            || current.row_level_security.enabled != definition.row_level_security.enabled
            || current.row_level_security.forced != definition.row_level_security.forced
            || current.replica_identity != definition.replica_identity
            || current.type_membership != definition.type_membership
            || current.partition.is_partitioned() != definition.partition.is_partitioned()
            || current.partition.is_attached() != definition.partition.is_attached();
        let mut composed = [None; MAX_COLUMNS];
        for (committed_column, target) in composed
            .iter_mut()
            .enumerate()
            .take(self.definition.committed().n_columns)
        {
            let current_name = prior.as_ref().map_or_else(
                || Some(self.definition.committed().columns()[committed_column].name),
                |version| version.column_mapping[committed_column],
            );
            let Some(current_name) = current_name else {
                continue;
            };
            let Some(current_column) = current.column_index(current_name.as_str()) else {
                continue;
            };
            *target = column_mapping[current_column];
        }
        let comment_identity = TableCommentIdentity::new(
            self.table,
            self.definition.identity.database,
            self.definition.committed(),
            &definition,
            transaction,
        );
        // Acquire the selected definition before the shared comment catalog
        // or version allocator. Waiting for its readers never reserves either.
        comment_identity.ensure_available(&comments)?;
        let source = self.storage.catalog_object_identity(
            super::catalog_sources::CatalogObjectSlot::Table(self.table),
            self.definition.identity.database,
            self.definition.identity.created_at,
        );
        Storage::check_index_table_comments(
            &indexes,
            &comments,
            source,
            definition.schema,
            transaction,
        )?;
        if let Err(conflict) =
            Storage::check_index_table_names(&indexes, source, definition.schema, transaction)
        {
            let storage = self.storage;
            drop(indexes);
            drop(_source);
            drop(comments);
            drop(self);
            return Err(match conflict {
                super::index_bindings::IndexNamespaceConflict::Duplicate(name) => sql_err!(
                    sqlstate::DUPLICATE_TABLE,
                    "relation \"{}\" already exists",
                    name.as_str()
                ),
                super::index_bindings::IndexNamespaceConflict::Wait(owner, name) => {
                    storage.catalog_ddl_wait_error(transaction, owner, name.as_str())
                }
            });
        }
        let mut transition = [u16::MAX; MAX_COLUMNS];
        for (column, target) in column_mapping.iter().enumerate().take(current.n_columns) {
            if let Some(target) = target {
                transition[column] = definition.column_index(target.as_str()).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "table column mapping references an absent target"
                    )
                })? as u16;
            }
        }
        let revision = self.storage.catalog_sequence.next()?;
        self.storage.pending_table_defs.stage_locked(
            &mut self.definition,
            PendingTableDef {
                txid: transaction,
                revision,
                def: definition,
                transition,
                column_mapping: composed,
                rewrites_rows: rewrites_rows
                    || prior.as_ref().is_some_and(|version| version.rewrites_rows),
                class_tuple_private,
            },
            self.storage.max_catalog_versions_per_object,
        )?;
        Storage::stage_index_table_names(
            &mut indexes,
            &mut comments,
            source,
            definition.schema,
            definition.name,
            transaction,
        );
        comment_identity.apply(&mut comments);
        self.definition.publish_names();
        Ok(TableDefinitionChange {
            source: target.source,
            transaction,
            revision,
            committed_revision: target.committed_revision,
        })
    }

    pub(super) fn rollback(mut self, change: TableDefinitionChange) {
        let transaction = change.transaction;
        let (mut comments, mut indexes) = loop {
            if !change.source.matches(
                self.storage,
                self.definition.identity.database,
                self.definition.identity.created_at,
                self.definition.identity.existence,
            ) || self
                .storage
                .pending_table_defs
                .selected_revision(&self.definition, transaction)
                != change.revision
            {
                return;
            }
            let comments = self.storage.comment_catalog();
            match super::row_ownership::try_mutex(
                &self.storage.index_catalog,
                "index catalog lock poisoned",
            ) {
                Ok(indexes) => break (comments, indexes),
                Err(wait) => {
                    drop(comments);
                    self = self.after_wait(wait);
                }
            }
        };
        if !self
            .storage
            .pending_table_defs
            .rollback_locked(&mut self.definition, transaction)
        {
            return;
        }
        let prior = self
            .storage
            .pending_table_defs
            .latest_locked(&self.definition);
        let visible = prior
            .as_ref()
            .map_or(self.definition.committed(), |version| &version.def);
        let identity = TableCommentIdentity::new(
            self.table,
            self.definition.identity.database,
            self.definition.committed(),
            visible,
            transaction,
        );
        let source = self.storage.catalog_object_identity(
            super::catalog_sources::CatalogObjectSlot::Table(self.table),
            self.definition.identity.database,
            self.definition.identity.created_at,
        );
        Storage::stage_index_table_names(
            &mut indexes,
            &mut comments,
            source,
            visible.schema,
            visible.name,
            transaction,
        );
        identity.apply(&mut comments);
    }
}

struct TableCommentIdentity {
    table: u32,
    database: Option<DatabaseOid>,
    committed_schema: SqlName,
    committed_name: SqlName,
    visible_schema: SqlName,
    visible_name: SqlName,
    transaction: u32,
}

impl TableCommentIdentity {
    fn new(
        table: usize,
        database: DatabaseOid,
        committed: &TableDef,
        visible: &TableDef,
        transaction: u32,
    ) -> Self {
        Self {
            table: table as u32,
            database: Some(database),
            committed_schema: committed.schema,
            committed_name: committed.name,
            visible_schema: visible.schema,
            visible_name: visible.name,
            transaction,
        }
    }

    fn matches(&self, comment: &CommentEntry) -> bool {
        comment.used
            && comment.database == self.database
            && comment.schema == self.committed_schema
            && match comment.class {
                CommentClass::Relation | CommentClass::Type => comment.name == self.committed_name,
                CommentClass::Constraint => comment.subid == self.table,
                _ => false,
            }
    }

    fn replacement(&self, comment: &CommentEntry) -> Option<PendingCommentIdentity> {
        let name = if comment.class == CommentClass::Constraint {
            comment
                .pending_identity
                .filter(|identity| identity.txid == self.transaction)
                .map_or(comment.name, |identity| identity.name)
        } else {
            self.visible_name
        };
        (self.visible_schema != comment.schema || name != comment.name).then_some(
            PendingCommentIdentity {
                txid: self.transaction,
                schema: self.visible_schema,
                name,
            },
        )
    }

    fn ensure_available(&self, comments: &CommentCatalog) -> Result<(), SqlError> {
        for comment in comments.entries.iter() {
            if self.matches(comment)
                && self.replacement(comment).is_some()
                && comment
                    .pending_identity
                    .is_some_and(|identity| identity.txid != self.transaction)
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "table comment identity is owned by another transaction"
                ));
            }
        }
        Ok(())
    }

    fn apply(&self, comments: &mut CommentCatalog) {
        for comment in comments.entries.iter_mut() {
            if self.matches(comment)
                && comment
                    .pending_identity
                    .is_none_or(|identity| identity.txid == self.transaction)
            {
                comment.pending_identity = self.replacement(comment);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config};
    use super::super::{CatalogDdlState, RuleTarget, comment_stackstr};
    use super::*;
    use crate::mem::budget::Budget;
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let first = storage
            .create_table(make_def(
                "first_edit",
                &[("id", crate::sql::types::ColType::Int4, false)],
            ))
            .unwrap();
        let other = storage
            .create_table(make_def(
                "other_edit",
                &[("id", crate::sql::types::ColType::Int4, false)],
            ))
            .unwrap();
        (storage, first, other)
    }

    fn mapping(name: &str) -> [Option<SqlName>; MAX_COLUMNS] {
        let mut mapping = [None; MAX_COLUMNS];
        mapping[0] = Some(SqlName::parse(name).unwrap());
        mapping
    }

    #[test]
    fn pending_table_definition_wait_keeps_unrelated_edits_and_comments_available() {
        let (storage, first, other) = fixture();
        let held = storage.table_def(first, 7);
        let mut updated = *held;
        updated.name = SqlName::parse("first_pending").unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let first_change = std::thread::scope(|scope| {
            let storage = &storage;
            scope.spawn(move || {
                let result = crate::mem::guard::forbid_alloc(|| {
                    storage.write_table_def(
                        storage.select_table_definition(first, 7).0,
                        updated,
                        &mapping("id"),
                        false,
                    )
                });
                send.send(result).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let waiting = loop {
                match storage.tables[first].lifecycle.try_lock() {
                    Err(std::sync::TryLockError::WouldBlock) => break true,
                    Ok(guard) => drop(guard),
                    Err(std::sync::TryLockError::Poisoned(_)) => break false,
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::yield_now();
            };
            let (other_send, other_receive) = std::sync::mpsc::sync_channel(1);
            if waiting {
                scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        let mut changed = *storage.table_def(other, 8);
                        changed.name = SqlName::parse("other_pending").unwrap();
                        let change = storage
                            .write_table_def(
                                storage.select_table_definition(other, 8).0,
                                changed,
                                &mapping("id"),
                                false,
                            )
                            .unwrap();
                        assert_eq!(storage.table_def(other, 8).name.as_str(), "other_pending");
                        storage.rollback_table_def(change);
                        storage
                            .apply_comment(
                                CommentClass::Relation,
                                SqlName::parse("public").unwrap(),
                                SqlName::parse("other_edit").unwrap(),
                                0,
                                Some(
                                    comment_stackstr("available while another edit waits").unwrap(),
                                ),
                            )
                            .unwrap();
                        assert!(storage.pending_table_defs.test_write().is_ok());
                    });
                    other_send.send(()).unwrap();
                });
            }
            let progress = other_receive.recv_timeout(Duration::from_secs(5));
            // Release before assertions and joins so a blocked regression can
            // finish without leaving either worker stranded.
            drop(held);
            assert!(
                waiting,
                "the selected edit never acquired its lifecycle owner"
            );
            assert!(
                progress.is_ok(),
                "the selected edit reserved unrelated metadata"
            );
            receive
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
        });
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(storage.table_def(first, 7).name.as_str(), "first_pending");
            storage.rollback_table_def(first_change);
            assert_eq!(storage.table_def(first, 7).name.as_str(), "first_edit");
        });
    }

    #[test]
    fn pending_table_definition_composition_exhaustion_and_rollback_are_atomic() {
        let mut config = test_config();
        config.max_catalog_versions_per_object = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "composed_edit",
                &[("id", crate::sql::types::ColType::Int4, false)],
            ))
            .unwrap();
        let schema = SqlName::parse("public").unwrap();
        storage
            .apply_comment(
                CommentClass::Relation,
                schema,
                SqlName::parse("composed_edit").unwrap(),
                0,
                Some(comment_stackstr("retained relation").unwrap()),
            )
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut first = *storage.table_def(table, 7);
            first.name = SqlName::parse("first_shape").unwrap();
            first.columns[0].name = SqlName::parse("renamed_id").unwrap();
            let first_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    first,
                    &mapping("renamed_id"),
                    true,
                )
                .unwrap();
            let mut second = *storage.table_def(table, 7);
            second.schema = SqlName::parse("moved").unwrap();
            second.columns[0].name = SqlName::parse("final_id").unwrap();
            let second_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    second,
                    &mapping("final_id"),
                    false,
                )
                .unwrap();
            {
                let pending = storage.pending_table_def(table).unwrap();
                assert_eq!(pending.column_mapping[0].unwrap().as_str(), "final_id");
                assert!(pending.rewrites_rows);
            }
            let mut rejected = second;
            rejected.name = SqlName::parse("must_not_publish").unwrap();
            assert_eq!(
                storage
                    .write_table_def(
                        storage.select_table_definition(table, 7).0,
                        rejected,
                        &mapping("final_id"),
                        false
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.table_def(table, 7).name.as_str(), "first_shape");
            assert_eq!(
                storage
                    .comment_text(CommentClass::Relation, "moved", "first_shape", 0, 7)
                    .unwrap()
                    .as_str(),
                "retained relation"
            );
            storage.rollback_table_def(TableDefinitionChange {
                transaction: 8,
                ..second_change
            });
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "final_id"
            );
            storage.rollback_table_def(second_change);
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "renamed_id"
            );
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "first_shape", 0, 7)
                    .is_some()
            );
            storage.rollback_table_def(first_change);
            assert_eq!(storage.table_def(table, 7).name.as_str(), "composed_edit");
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "composed_edit", 0, 7)
                    .is_some()
            );
            assert!(storage.pending_table_def(table).is_none());
            let third_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 9).0,
                    first,
                    &mapping("renamed_id"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(third_change);
        });
    }

    #[test]
    fn pending_table_definition_and_rule_history_share_foreign_owner_admission() {
        let (storage, first, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage
                .begin_rule_history(RuleTarget::Table(first as u16), 7)
                .unwrap();
            let changed = *storage.table_def(first, 7);
            assert!(
                storage
                    .write_table_def(
                        storage.select_table_definition(first, 8).0,
                        changed,
                        &mapping("id"),
                        false
                    )
                    .is_err()
            );
            // Exercise the common publication check as well as public admission.
            let error = storage
                .pending_table_defs
                .stage(
                    &storage.tables[first].definition,
                    PendingTableDef {
                        txid: 8,
                        revision: 1,
                        def: changed,
                        transition: [u16::MAX; MAX_COLUMNS],
                        column_mapping: mapping("id"),
                        rewrites_rows: false,
                        class_tuple_private: false,
                    },
                    storage.max_catalog_versions_per_object,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.pending_table_def(first).is_none());
            let change = storage
                .write_table_def(
                    storage.select_table_definition(first, 7).0,
                    changed,
                    &mapping("id"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(TableDefinitionChange {
                transaction: 8,
                ..change
            });
            assert_eq!(storage.pending_table_def(first).unwrap().txid, 7);
            assert!(
                storage
                    .begin_rule_history(RuleTarget::Table(first as u16), 8)
                    .is_err()
            );
            storage.rollback_table_def(change);
            assert_eq!(
                storage.tables[first]
                    .definition
                    .read()
                    .pending_has_rules_txid,
                Some(7)
            );
        });
    }

    #[test]
    fn pending_table_definition_rejects_retired_slots_and_preserves_pending_create() {
        let (storage, first, _) = fixture();
        let definition = *storage.table_def(first, 0);
        crate::mem::guard::forbid_alloc(|| {
            storage.commit_drop(first);
            assert_eq!(
                storage
                    .write_table_def(
                        storage.select_table_definition(first, 7).0,
                        definition,
                        &mapping("id"),
                        false
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert!(storage.pending_table_def(first).is_none());
            let next = storage.create_table_in(definition, 9).unwrap();
            assert_eq!(next, first);
            let first_change = storage
                .write_table_def(
                    storage.select_table_definition(next, 9).0,
                    definition,
                    &mapping("id"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(first_change);
            let dropped = storage.drop_table_in(next, 9).unwrap();
            assert_eq!(
                storage.tables[next].existence(),
                CatalogDdlState::PendingCreateDrop { txid: 9 }
            );
            assert_eq!(
                storage
                    .write_table_def(
                        storage.select_table_definition(next, 9).0,
                        definition,
                        &mapping("id"),
                        false
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
            let second_change = storage
                .write_table_def(
                    storage.select_table_definition(next, 9).0,
                    definition,
                    &mapping("id"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(second_change);
            storage.rollback_create(next);
            assert!(storage.pending_table_def(next).is_none());
        });
    }

    #[test]
    fn pending_table_definition_comment_ownership_survives_foreign_text_commit() {
        let (storage, first, _) = fixture();
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("commented_check").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let (slot, _) = storage
                .set_comment(
                    CommentClass::Constraint,
                    schema,
                    name,
                    first as u32,
                    Some(comment_stackstr("text from seven").unwrap()),
                    7,
                )
                .unwrap()
                .unwrap();
            storage
                .stage_constraint_comment_rename(
                    first,
                    schema,
                    name,
                    SqlName::parse("owned_by_eight").unwrap(),
                    8,
                )
                .unwrap();
            assert!(storage.commit_comment(slot, 7).is_some());
            {
                let comments = storage.comment_catalog();
                assert_eq!(comments.entries[slot].pending_identity.unwrap().txid, 8);
                assert_eq!(comments.entries[slot].name, name);
            }
            let mut renamed = *storage.table_def(first, 9);
            renamed.schema = SqlName::parse("moved").unwrap();
            let error = storage
                .write_table_def(
                    storage.select_table_definition(first, 9).0,
                    renamed,
                    &mapping("id"),
                    false,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.pending_table_def(first).is_none());
            assert_eq!(storage.table_def(first, 9).schema.as_str(), "public");
            assert_eq!(
                storage.comment_catalog().entries[slot]
                    .pending_identity
                    .unwrap()
                    .txid,
                8
            );
            storage.restore_comment_identity_pending(slot, None);
            let change = storage
                .write_table_def(
                    storage.select_table_definition(first, 9).0,
                    renamed,
                    &mapping("id"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(change);
            assert!(
                storage
                    .comment_text(
                        CommentClass::Constraint,
                        "public",
                        "commented_check",
                        first as u32,
                        0,
                    )
                    .is_some()
            );
        });
    }

    #[test]
    fn table_definition_sources_reject_stale_versions_without_touching_other_tables() {
        let (storage, table, other) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let (original_target, mut first) = storage.select_table_definition(table, 7);
            let (other_target, mut other_definition) = storage.select_table_definition(other, 7);
            first.columns[0].name = SqlName::parse("first_column").unwrap();
            let first_change = storage
                .write_table_def(original_target, first, &mapping("first_column"), false)
                .unwrap();
            let error = storage
                .write_table_def(original_target, first, &mapping("first_column"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            let (next_target, mut second) = storage.select_table_definition(table, 7);
            second.columns[0].name = SqlName::parse("second_column").unwrap();
            let second_change = storage
                .write_table_def(next_target, second, &mapping("second_column"), false)
                .unwrap();
            storage.rollback_table_def(first_change);
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "second_column"
            );
            storage.rollback_table_def(second_change);
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "first_column"
            );
            storage.rollback_table_def(first_change);
            storage.rollback_table_def(first_change);
            assert!(storage.pending_table_def(table).is_none());
            let (committed_target, committed) = storage.select_table_definition(table, 7);
            let revision = storage.reserve_table_definition_revision().unwrap();
            storage
                .move_table_schema(table, SqlName::parse("moved").unwrap(), revision)
                .unwrap();
            let error = storage
                .write_table_def(committed_target, committed, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(storage.table_def(table, 7).schema.as_str(), "moved");
            other_definition.columns[0].name = SqlName::parse("unrelated_column").unwrap();
            let other_change = storage
                .write_table_def(
                    other_target,
                    other_definition,
                    &mapping("unrelated_column"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(other_change);
            assert_eq!(storage.table_def(other, 7).columns()[0].name.as_str(), "id");
        });
    }

    #[test]
    fn table_definition_undo_rejects_foreign_storage_and_reused_lifetimes() {
        let (storage, table, _) = fixture();
        let (foreign, foreign_table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let (target, mut definition) = storage.select_table_definition(table, 7);
            definition.name = SqlName::parse("selected_pending").unwrap();
            let change = storage
                .write_table_def(target, definition, &mapping("id"), false)
                .unwrap();
            let error = foreign
                .write_table_def(target, definition, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            foreign.rollback_table_def(change);
            assert!(foreign.pending_table_def(foreign_table).is_none());
            storage.rollback_table_def(change);
            storage.commit_drop(table);
            let replacement = storage.create_table_in(definition, 7).unwrap();
            assert_eq!(replacement, table);
            let (replacement_target, mut replacement_definition) =
                storage.select_table_definition(replacement, 7);
            replacement_definition.columns[0].name = SqlName::parse("replacement_column").unwrap();
            let replacement_change = storage
                .write_table_def(
                    replacement_target,
                    replacement_definition,
                    &mapping("replacement_column"),
                    false,
                )
                .unwrap();
            storage.rollback_table_def(change);
            assert_eq!(
                storage.table_def(replacement, 7).columns()[0].name.as_str(),
                "replacement_column"
            );
            let error = storage
                .write_table_def(target, definition, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            storage.rollback_table_def(replacement_change);
            storage.rollback_create(replacement);
        });
    }

    #[test]
    fn table_definition_wait_rejects_a_newer_private_shape() {
        let (storage, table, other) = fixture();
        let (target, mut proposed) = storage.select_table_definition(table, 7);
        proposed.name = SqlName::parse("stale_proposal").unwrap();
        let held = storage.type_catalog.lock().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    storage.write_table_def(target, proposed, &mapping("id"), false)
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let waiting = loop {
                if storage
                    .definition_publication
                    .waits
                    .load(core::sync::atomic::Ordering::Acquire)
                    != 0
                {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::yield_now();
            };
            let installed = if waiting {
                Some(crate::mem::guard::forbid_alloc(|| {
                    let (new_target, mut newer) = storage.select_table_definition(table, 7);
                    newer.columns[0].name = SqlName::parse("newer_column").unwrap();
                    let change = storage
                        .write_table_def(new_target, newer, &mapping("newer_column"), false)
                        .unwrap();
                    let (other_target, other_definition) =
                        storage.select_table_definition(other, 8);
                    let other_change = storage
                        .write_table_def(other_target, other_definition, &mapping("id"), false)
                        .unwrap();
                    storage.rollback_table_def(other_change);
                    change
                }))
            } else {
                None
            };
            drop(held);
            let result = worker.join().unwrap();
            assert!(
                waiting,
                "edit did not release its owners for the type catalog wait"
            );
            assert_eq!(
                result.unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(storage.table_def(table, 7).name.as_str(), "first_edit");
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "newer_column"
            );
            storage.rollback_table_def(installed.unwrap());
            assert!(storage.pending_table_def(table).is_none());
        });
    }

    #[test]
    fn table_definition_transaction_log_exhaustion_restores_only_the_new_change() {
        let mut config = test_config();
        config.max_ddl_per_transaction = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "owned_edit",
                &[("id", crate::sql::types::ColType::Int4, false)],
            ))
            .unwrap();
        let mut transaction_budget =
            Budget::new(crate::sql::txn::TxnState::budget_bytes_with_config(&config));
        let mut transaction =
            crate::sql::txn::TxnState::new_with_config(&mut transaction_budget, &config).unwrap();
        transaction.txid = 7;
        crate::mem::guard::forbid_alloc(|| {
            let (target, mut first) = storage.select_table_definition(table, 7);
            first.name = SqlName::parse("kept_change").unwrap();
            transaction
                .write_table_definition(&storage, target, first, &mapping("id"), false)
                .unwrap();
            let (target, mut second) = storage.select_table_definition(table, 7);
            second.name = SqlName::parse("unrecorded_change").unwrap();
            let error = transaction
                .write_table_definition(&storage, target, second, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.table_def(table, 7).name.as_str(), "kept_change");
            assert_eq!(transaction.ddl().len(), 1);
            let (foreign_target, unchanged) = storage.select_table_definition(table, 8);
            let error = transaction
                .write_table_definition(&storage, foreign_target, unchanged, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(storage.table_def(table, 7).name.as_str(), "kept_change");
            let crate::sql::txn::DdlUndo::TableAltered(change) = transaction.ddl()[0] else {
                panic!("table definition change was not registered");
            };
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
            assert_eq!(storage.table_def(table, 0).name.as_str(), "kept_change");
        });
    }

    #[test]
    fn table_definition_revision_exhaustion_leaves_no_pending_change() {
        let (storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let (target, mut definition) = storage.select_table_definition(table, 7);
            definition.name = SqlName::parse("cannot_reserve").unwrap();
            storage
                .catalog_sequence
                .0
                .store(u64::MAX, core::sync::atomic::Ordering::Relaxed);
            let error = storage
                .write_table_def(target, definition, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(storage.pending_table_def(table).is_none());
            assert_eq!(storage.table_def(table, 7).name.as_str(), "first_edit");
            assert!(storage.tables[table].lifecycle.try_lock().is_ok());
            assert!(storage.tables[table].definition.test_write().is_ok());
            assert!(storage.index_catalog.try_lock().is_ok());
            assert!(storage.comments.try_lock().is_ok());
        });
    }
}
