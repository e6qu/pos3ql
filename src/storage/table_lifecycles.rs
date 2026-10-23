//! Table drop undo and comment retirement retain the selected lifetime and revision.

use super::catalog_sources::{CatalogDdlOperation, CatalogObjectSlot};
use super::*;

impl Storage {
    pub(crate) fn table_replay_slot(
        &self,
        position: CatalogPosition,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        if position.slot() < self.table_count()
            && self.table(position.slot()).created_at() == position.created_at()
            && self.find_visible(schema, name, transaction) == Some(position.slot())
        {
            Ok(position.slot())
        } else {
            Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal table source identity or name does not match"
            ))
        }
    }

    pub(crate) fn drop_table_in(
        &self,
        index: usize,
        transaction: u32,
    ) -> Result<CatalogDdlUndo, SqlError> {
        let table = self.tables.get(index).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "table drop source no longer exists"
            )
        })?;
        loop {
            let lifecycle = table.lifecycle();
            let mut state = table.definition.write();
            if state.identity.database != current_database()
                || !state.identity.existence.visible_to(transaction)
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "table drop source no longer exists"
                ));
            }
            if let Some(owner) = state
                .identity
                .existence
                .pending_txid()
                .filter(|owner| *owner != transaction)
            {
                let name = state.committed().name;
                drop(state);
                drop(lifecycle);
                return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
            }
            let mut comments =
                match row_ownership::try_mutex(&self.comments, "comment catalog lock poisoned") {
                    Ok(comments) => comments,
                    Err(wait) => {
                        drop(state);
                        drop(lifecycle);
                        wait.wait();
                        continue;
                    }
                };
            let mut rules = match row_ownership::try_mutex(
                &self.view_rule_catalog,
                "view/rule catalog lock poisoned",
            ) {
                Ok(rules) => rules,
                Err(wait) => {
                    drop(comments);
                    drop(state);
                    drop(lifecycle);
                    wait.wait();
                    continue;
                }
            };
            let source = self.catalog_object_identity(
                CatalogObjectSlot::Table(index),
                state.identity.database,
                state.identity.created_at,
            );
            let (schema, name) = state
                .pending
                .filter(|pending| pending.transaction == transaction)
                .map_or((state.committed().schema, state.committed().name), |pending| {
                    (pending.schema, pending.name)
                });
            let version = self.catalog_sequence.next()?;
            self.stage_catalog_comment_drop(&mut comments, source, transaction, |comment| {
                if comment.class == CommentClass::Constraint {
                    comment.used
                        && comment.database == Some(current_database())
                        && comment.subid == index as u32
                        && comment
                            .pending_drop
                            .is_none_or(|pending| pending.txid != transaction)
                } else {
                    matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                        && comment.matches_to(
                            Some(current_database()),
                            comment.class,
                            schema.as_str(),
                            name.as_str(),
                            comment.subid,
                            transaction,
                        )
                }
            })?;
            let prior_version = state.identity.ddl_version;
            state.identity.ddl_version = version;
            state.identity.existence = state.identity.existence.drop_by(transaction);
            for rule in rules.rules.iter_mut().filter(|rule| {
                rule.database == current_database()
                    && rule.visible_to(transaction)
                    && rule.definition_for(transaction).target == RuleTarget::Table(index as u16)
            }) {
                rule.ddl_state = rule.ddl_state.drop_by(transaction);
                rule.pending_parent_drop_txid = Some(transaction);
            }
            table.mark_dirty();
            return Ok(CatalogDdlUndo {
                source,
                transaction,
                operation: CatalogDdlOperation::Drop,
                version,
                prior_version,
            });
        }
    }

    fn table_drop_matches(
        &self,
        identity: table_definitions::TableIdentity,
        undo: CatalogDdlUndo,
    ) -> bool {
        undo.operation == CatalogDdlOperation::Drop
            && undo.source.matches(
                self,
                identity.database,
                identity.created_at,
                identity.existence,
            )
            && identity.existence.pending_drop_txid() == Some(undo.transaction)
            && identity.ddl_version == undo.version
    }

    pub(crate) fn validate_table_drop(
        &self,
        undo: CatalogDdlUndo,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let CatalogObjectSlot::Table(slot) = undo.source.slot() else {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "drop source is not a table"
            ));
        };
        if undo.transaction == transaction
            && self
                .tables
                .get(slot)
                .is_some_and(|table| self.table_drop_matches(table.identity(), undo))
        {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table drop no longer belongs to its selected source"
            ))
        }
    }

    pub(crate) fn finish_table_drop(
        &self,
        undo: CatalogDdlUndo,
        decision: CatalogDdlDecision,
    ) -> Option<(SqlName, SqlName)> {
        let CatalogObjectSlot::Table(slot) = undo.source.slot() else {
            return None;
        };
        let table = self.tables.get(slot)?;
        match decision {
            CatalogDdlDecision::Commit => {
                let (selected, names) = {
                    let state = table.definition.read();
                    if !self.table_drop_matches(state.identity, undo) {
                        return None;
                    }
                    (
                        state.identity,
                        (state.committed().schema, state.committed().name),
                    )
                };
                let _lifecycle = self.retire_table_names(slot, selected)?;
                self.retire_table_data(slot);
                Some(names)
            }
            CatalogDdlDecision::Rollback => loop {
                let lifecycle = table.lifecycle();
                let mut state = table.definition.write();
                if !self.table_drop_matches(state.identity, undo) {
                    return None;
                }
                let mut comments =
                    match row_ownership::try_mutex(&self.comments, "comment catalog lock poisoned")
                    {
                        Ok(comments) => comments,
                        Err(wait) => {
                            drop(state);
                            drop(lifecycle);
                            wait.wait();
                            continue;
                        }
                    };
                self.finish_catalog_comment_drop(
                    &mut comments,
                    undo.source,
                    undo.transaction,
                    decision,
                );
                state.identity.existence = state.identity.existence.rollback_drop(undo.transaction);
                state.identity.ddl_version = undo.prior_version;
                let names = (state.committed().schema, state.committed().name);
                drop(state);
                drop(comments);
                self.view_rule_catalog()
                    .rollback_target_rules(RuleTarget::Table(slot as u16), undo.transaction);
                return Some(names);
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};

    #[test]
    fn table_lifecycle_drop_undo_cannot_finalize_later_drop_or_reused_slot() {
        let mut config = test_config();
        config.max_tables = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let slot = storage
            .create_table(make_def("original_table", &[]))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = storage.drop_table_in(slot, 7).unwrap();
            assert!(
                storage
                    .finish_table_drop(first, CatalogDdlDecision::Rollback)
                    .is_some()
            );
            let second = storage.drop_table_in(slot, 7).unwrap();
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                assert!(storage.finish_table_drop(first, decision).is_none());
            }
            assert!(!storage.table(slot).visible_to(7));
            assert!(
                storage
                    .finish_table_drop(second, CatalogDdlDecision::Commit)
                    .is_some()
            );
            let replacement = storage
                .create_table(make_def("replacement_table", &[]))
                .unwrap();
            assert_eq!(slot, replacement);
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                assert!(storage.finish_table_drop(second, decision).is_none());
            }
            assert!(storage.table(replacement).visible_to(0));
            assert_eq!(
                storage.table(replacement).definition().name.as_str(),
                "replacement_table"
            );
            assert_eq!(
                storage.validate_table_drop(second, 7).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
        });
    }
}
