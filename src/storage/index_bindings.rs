//! Index columns retain their table lifetime and the definition they selected.

use super::catalog_sources::{CatalogObjectIdentity, CatalogObjectSlot};
use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexTableBinding {
    source: CatalogObjectIdentity,
    revision: u64,
}

impl Storage {
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

    pub(super) fn index_binding_matches(&self, index: IndexDef, table: usize) -> bool {
        let Some(binding) = index.table_binding else {
            return false;
        };
        let identity = self.tables[table].identity();
        binding.source.slot() == CatalogObjectSlot::Table(table)
            && binding.source.matches(
                self,
                identity.database,
                identity.created_at,
                identity.existence,
            )
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
        mut index: IndexDef,
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
        if !binding.source.matches(
            self,
            state.identity.database,
            state.identity.created_at,
            state.identity.existence,
        ) {
            return None;
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
                &state,
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
            .map_or((state.committed.schema, state.committed.name), |head| {
                (head.schema, head.name)
            });
        index.schema = schema;
        index.table = name;
        index.table_binding = Some(IndexTableBinding {
            source: binding.source,
            revision: self
                .pending_table_defs
                .selected_revision(&state, transaction),
        });
        Some(index)
    }

    pub(super) fn promote_private_index_bindings(&self, table: usize, transaction: u32) {
        let mut indexes = self.index_catalog();
        for index in indexes.iter_mut() {
            if index.database != current_database()
                || index.ddl_state != (CatalogDdlState::PendingCreate { txid: transaction })
                || !index
                    .table_binding
                    .is_some_and(|binding| binding.source.slot() == CatalogObjectSlot::Table(table))
            {
                continue;
            }
            let mut promoted = self
                .project_index_binding(*index, transaction)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};

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
    fn private_index_bindings_do_not_select_a_reused_table_or_another_storage() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut other_budget = test_budget(&config);
        let mut other = Storage::new(&config, &mut other_budget).unwrap();
        let definition = make_def(
            "binding_source",
            &[
                ("key", ColType::Int4, false),
                ("payload", ColType::Int4, false),
            ],
        );
        let table = storage.create_table(definition).unwrap();
        other.create_table(definition).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let slot = storage
                .create_index(index("binding_index", "binding_source", 0, 1), 7)
                .unwrap();
            storage.commit_index_create(slot);
            let captured = storage.index(slot);
            assert!(other.project_index_binding(captured, 0).is_none());
            storage.commit_drop(table);
            let replacement = storage.create_table(definition).unwrap();
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
        });
    }

    #[test]
    fn private_index_bindings_follow_their_selected_definition_through_rollback_and_commit() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
            storage
                .write_table_def(table, 7, first, &first_mapping, false)
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
                storage
                    .write_table_def(table, 7, second, &second_mapping, false)
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
                    storage.commit_table_def(table, 7);
                } else {
                    storage.rollback_table_def(table, 7);
                    for (slot, expected) in [(before, (0, 1)), (during, (2, 1))] {
                        let reverted = storage.index_visible_to(slot, 7).unwrap();
                        assert_eq!((reverted.columns[0], reverted.include_columns[0]), expected);
                    }
                }
            }
            for (slot, expected) in [(before, (1, 0)), (during, (2, 0))] {
                storage.commit_index_create(slot);
                let committed = storage.index_visible_to(slot, 8).unwrap();
                assert_eq!(
                    (committed.columns[0], committed.include_columns[0]),
                    expected
                );
                assert_eq!(committed.table_binding.unwrap().revision, 0);
            }
        });
    }
}
