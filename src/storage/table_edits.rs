//! Transaction-private table shapes and their comment identities share one edit owner.

use std::sync::MutexGuard;

use super::table_names::DefinitionWrite;
use super::{
    CommentCatalog, CommentClass, CommentEntry, DatabaseOid, MAX_COLUMNS, PendingCommentIdentity,
    PendingTableDef, SqlError, SqlName, Storage, TableDef,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

pub(super) struct TableDefinitionEdit<'storage> {
    storage: &'storage Storage,
    table: usize,
    definition: DefinitionWrite<'storage>,
    // Release the definition before allowing retirement or slot reuse.
    _lifecycle: MutexGuard<'storage, ()>,
}

impl<'storage> TableDefinitionEdit<'storage> {
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
        transaction: u32,
        mut definition: TableDef,
        column_mapping: &[Option<SqlName>; MAX_COLUMNS],
        rewrites_rows: bool,
    ) -> Result<(), SqlError> {
        let (mut comments, _source) = loop {
            let comments = self.storage.comment_catalog();
            if !definition.requires_type_catalog() {
                break (comments, None);
            }
            match super::row_ownership::try_mutex(
                &self.storage.type_catalog,
                "type catalog lock poisoned",
            ) {
                Ok(source) => {
                    source.bind_table_definition(&mut definition, transaction)?;
                    break (comments, Some(source));
                }
                Err(wait) => {
                    let storage = self.storage;
                    let table = self.table;
                    let identity = self.definition.identity;
                    drop(comments);
                    drop(self);
                    #[cfg(test)]
                    storage
                        .type_publication
                        .waits
                        .fetch_add(1, core::sync::atomic::Ordering::Release);
                    wait.wait();
                    self = Self::new(storage, table);
                    if self.definition.identity.database != identity.database
                        || self.definition.identity.created_at != identity.created_at
                    {
                        return Err(sql_err!(
                            sqlstate::SERIALIZATION_FAILURE,
                            "pending definition belongs to a retired table incarnation"
                        ));
                    }
                }
            }
        };
        if let Some(owner) = self.definition.locked_by_other(transaction) {
            let name = self.definition.committed.name;
            let storage = self.storage;
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
            .map_or(&self.definition.committed, |version| &version.def);
        definition.has_toast |= current.has_toast
            || definition
                .columns()
                .iter()
                .any(|column| column.ctype.typlen() == -1);
        definition.has_rules |= current.has_rules;
        let mut composed = [None; MAX_COLUMNS];
        for (committed_column, target) in composed
            .iter_mut()
            .enumerate()
            .take(self.definition.committed.n_columns)
        {
            let current_name = prior.as_ref().map_or_else(
                || Some(self.definition.committed.columns()[committed_column].name),
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
            &self.definition.committed,
            &definition,
            transaction,
        );
        // Acquire the selected definition before the shared comment catalog
        // or version allocator. Waiting for its readers never reserves either.
        comment_identity.ensure_available(&comments)?;
        self.storage.pending_table_defs.stage_locked(
            &mut self.definition,
            PendingTableDef {
                txid: transaction,
                def: definition,
                column_mapping: composed,
                rewrites_rows: rewrites_rows
                    || prior.as_ref().is_some_and(|version| version.rewrites_rows),
            },
            self.storage.max_catalog_versions_per_object,
        )?;
        comment_identity.apply(&mut comments);
        Ok(())
    }

    pub(super) fn rollback(mut self, transaction: u32) {
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
            .map_or(&self.definition.committed, |version| &version.def);
        let identity = TableCommentIdentity::new(
            self.table,
            self.definition.identity.database,
            &self.definition.committed,
            visible,
            transaction,
        );
        identity.apply(&mut self.storage.comment_catalog());
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
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
        std::thread::scope(|scope| {
            let storage = &storage;
            scope.spawn(move || {
                let result = crate::mem::guard::forbid_alloc(|| {
                    storage.write_table_def(first, 7, updated, &mapping("id"), false)
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
                        storage
                            .write_table_def(other, 8, changed, &mapping("id"), false)
                            .unwrap();
                        assert_eq!(storage.table_def(other, 8).name.as_str(), "other_pending");
                        storage.rollback_table_def(other, 8);
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
                .unwrap();
        });
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(storage.table_def(first, 7).name.as_str(), "first_pending");
            storage.rollback_table_def(first, 7);
            assert_eq!(storage.table_def(first, 7).name.as_str(), "first_edit");
        });
    }

    #[test]
    fn pending_table_definition_composition_exhaustion_and_rollback_are_atomic() {
        let mut config = test_config();
        config.max_catalog_versions_per_object = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
            storage
                .write_table_def(table, 7, first, &mapping("renamed_id"), true)
                .unwrap();
            let mut second = *storage.table_def(table, 7);
            second.schema = SqlName::parse("moved").unwrap();
            second.columns[0].name = SqlName::parse("final_id").unwrap();
            storage
                .write_table_def(table, 7, second, &mapping("final_id"), false)
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
                    .write_table_def(table, 7, rejected, &mapping("final_id"), false)
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
            storage.rollback_table_def(table, 8);
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "final_id"
            );
            storage.rollback_table_def(table, 7);
            assert_eq!(
                storage.table_def(table, 7).columns()[0].name.as_str(),
                "renamed_id"
            );
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "first_shape", 0, 7)
                    .is_some()
            );
            storage.rollback_table_def(table, 7);
            assert_eq!(storage.table_def(table, 7).name.as_str(), "composed_edit");
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", "composed_edit", 0, 7)
                    .is_some()
            );
            assert!(storage.pending_table_def(table).is_none());
            storage
                .write_table_def(table, 9, first, &mapping("renamed_id"), false)
                .unwrap();
            storage.rollback_table_def(table, 9);
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
                    .write_table_def(first, 8, changed, &mapping("id"), false)
                    .is_err()
            );
            // Exercise the common publication check as well as public admission.
            let error = storage
                .pending_table_defs
                .stage(
                    &storage.tables[first].definition,
                    PendingTableDef {
                        txid: 8,
                        def: changed,
                        column_mapping: mapping("id"),
                        rewrites_rows: false,
                    },
                    storage.max_catalog_versions_per_object,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.pending_table_def(first).is_none());
            storage
                .write_table_def(first, 7, changed, &mapping("id"), false)
                .unwrap();
            storage.rollback_table_def(first, 8);
            assert_eq!(storage.pending_table_def(first).unwrap().txid, 7);
            assert!(
                storage
                    .begin_rule_history(RuleTarget::Table(first as u16), 8)
                    .is_err()
            );
            storage.rollback_table_def(first, 7);
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
                    .write_table_def(first, 7, definition, &mapping("id"), false)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert!(storage.pending_table_def(first).is_none());
            let next = storage.create_table_in(definition, 9).unwrap();
            assert_eq!(next, first);
            storage
                .write_table_def(next, 9, definition, &mapping("id"), false)
                .unwrap();
            storage.rollback_table_def(next, 9);
            storage.drop_table_in(next, 9);
            assert_eq!(
                storage.tables[next].existence(),
                CatalogDdlState::PendingCreateDrop { txid: 9 }
            );
            assert_eq!(
                storage
                    .write_table_def(next, 9, definition, &mapping("id"), false)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage.rollback_drop(next);
            storage
                .write_table_def(next, 9, definition, &mapping("id"), false)
                .unwrap();
            storage.rollback_table_def(next, 9);
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
                .write_table_def(first, 9, renamed, &mapping("id"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.pending_table_def(first).is_none());
            assert_eq!(storage.table_def(first, 9).schema.as_str(), "public");
            storage.rollback_table_def(first, 9);
            assert_eq!(
                storage.comment_catalog().entries[slot]
                    .pending_identity
                    .unwrap()
                    .txid,
                8
            );
            storage.restore_comment_identity_pending(slot, None);
            storage
                .write_table_def(first, 9, renamed, &mapping("id"), false)
                .unwrap();
            storage.rollback_table_def(first, 9);
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
}
