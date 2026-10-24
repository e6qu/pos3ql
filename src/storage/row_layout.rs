//! A row encoding is selected by its table publication, independently of LSNs.

use super::{CommittedHistory, RowState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowLayout {
    first_generation: u64,
}

impl RowLayout {
    pub(super) const fn initial() -> Self {
        Self {
            first_generation: 0,
        }
    }

    pub(super) fn installed_at(generation: u64) -> Self {
        Self {
            first_generation: generation,
        }
    }

    pub(super) fn accepts(self, generation: u64) -> bool {
        generation >= self.first_generation
    }

    pub(super) fn obsolete(self, state: &RowState) -> bool {
        state
            .committed_generation
            .is_some_and(|generation| !self.accepts(generation))
    }

    pub(super) fn visible_state(self, mut state: RowState) -> RowState {
        if self.obsolete(&state) {
            state.committed = None;
            state.history = CommittedHistory::empty();
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{arena::Arena, budget::Budget};
    use crate::sql::eval::sqlstate;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{HistoryReclamation, RelationPersistence, SNAPSHOT_ALL, Storage};
    use core::num::NonZeroUsize;

    fn fixture(persistence: RelationPersistence, count: usize) -> (Storage, usize, usize) {
        let mut config = test_config();
        config.table_rows = count * 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def("layout_orders", &[("id", ColType::Int4, false)]);
        definition.persistence = persistence;
        let table = storage.create_table(definition).unwrap();
        let other = storage
            .create_table(make_def("layout_invoices", &[("id", ColType::Int4, false)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(11)]).unwrap();
        let generation = storage.table(table).generation();
        for rowid in 1..=count as u64 {
            storage.tables[table]
                .rows
                .insert(
                    rowid,
                    RowState::committed_in_generation(location, 7, generation),
                )
                .unwrap();
        }
        storage.set_lsn(7);
        (storage, table, other)
    }

    fn change(
        storage: &Storage,
        table: usize,
        ty: ColType,
        rewrite: bool,
    ) -> crate::storage::TableDefinitionChange {
        let mut definition = *storage.table_def(table, 0);
        definition.columns[0].ctype = ty;
        let mut mapping = [None; crate::storage::MAX_COLUMNS];
        mapping[0] = Some(definition.columns[0].name);
        storage
            .write_table_def(
                storage.select_table_definition(table, 8).0,
                definition,
                &mapping,
                rewrite,
            )
            .unwrap()
    }

    fn histories(storage: &Storage, table: usize) -> usize {
        storage.tables[table]
            .rows
            .read()
            .iter()
            .filter(|(_, state)| !state.history.is_empty())
            .count()
    }

    #[test]
    fn row_layout_rewrite_defers_history_work_and_preserves_owned_bytes() {
        for persistence in [
            RelationPersistence::Permanent,
            RelationPersistence::Temporary,
        ] {
            let (mut storage, table, other) = fixture(persistence, 129);
            let mut budget = Budget::new(1 << 18);
            let arena = Arena::new(&mut budget, "layout scan", 1 << 18).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                storage.register_snapshot(99, 7).unwrap();
                let original = storage
                    .visible_row_home_at(
                        table,
                        1,
                        storage.resident_row_state(table, 1).unwrap().unwrap(),
                        99,
                        SNAPSHOT_ALL,
                        7,
                    )
                    .unwrap()
                    .unwrap();
                let identity = original.snapshot();
                let scan = storage.captured_row_scan_in(table, 99, &arena).unwrap();
                let (target, retained_ids) = storage.retain_metadata_walk_rows(table).unwrap();
                for rowid in 1..=129 {
                    storage.write_pending(table, rowid, 8, 1, None).unwrap();
                    storage.commit_row(table, rowid, 8, 9).unwrap();
                }
                assert_eq!(histories(&storage, table), 129);
                let rewrite = change(&storage, table, ColType::Int8, true);
                assert_eq!(storage.commit_table_def(rewrite).unwrap(), Some(true));
                storage.finish_table_def_commit(table, true);
                assert_eq!(
                    histories(&storage, table),
                    129,
                    "rewrite finalization walked row history"
                );
                assert!(storage.history_cleanup_pending());
                assert_eq!(
                    storage
                        .capture_resident_row_metadata(target, 1)
                        .err()
                        .unwrap()
                        .sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                assert_eq!(
                    storage
                        .read_row_snapshot(table, 1, identity)
                        .err()
                        .unwrap()
                        .sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                for commit in [7, 9] {
                    let mut visible = 0;
                    storage
                        .for_each_row_state(table, &mut |rowid, state| {
                            visible += usize::from(
                                storage
                                    .visible_row_home_at(
                                        table,
                                        rowid,
                                        state,
                                        99,
                                        SNAPSHOT_ALL,
                                        commit,
                                    )?
                                    .is_some(),
                            );
                            Ok(core::ops::ControlFlow::Continue(()))
                        })
                        .unwrap();
                    assert_eq!(visible, 0, "new reader decoded a previous row layout");
                }
                let mut previous = 129;
                for _ in 0..storage.tables.len() + 16 {
                    if !storage.history_cleanup_pending() {
                        break;
                    }
                    assert_ne!(
                        storage.advance_history_cleanup(NonZeroUsize::new(64).unwrap()),
                        HistoryReclamation::WaitingForOwner
                    );
                    let remaining = histories(&storage, table);
                    assert!(remaining <= previous && previous - remaining <= 64);
                    previous = remaining;
                    let image = storage.heap.append_row(&[Datum::Int4(22)]).unwrap();
                    let undo = storage.write_pending(other, 1, 10, 1, Some(image)).unwrap();
                    storage.restore_pending(other, 1, 10, undo);
                    assert!(storage.tables[table].publication.try_lock().is_ok());
                    assert!(storage.row_versions.test_write().is_ok());
                }
                assert!(!storage.history_cleanup_pending());
                assert_eq!(histories(&storage, table), 0);
                let mut count = 0;
                assert!(
                    scan.for_each_home(&mut |rowid, home| {
                        storage.with_row_bytes(table, rowid, home, |bytes| {
                            let mut values = [Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values[0], Datum::Int4(11));
                            Ok(())
                        })?;
                        count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap()
                    .is_continue()
                );
                assert_eq!(count, 129);
                drop(retained_ids);
                storage
                    .with_row_bytes(table, 1, original, |bytes| {
                        let mut values = [Datum::Null];
                        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(11));
                        Ok(())
                    })
                    .unwrap();
                storage.release_snapshot(99);
            });
        }
    }

    #[test]
    fn row_layout_rollback_and_metadata_edits_preserve_retained_admission() {
        let (mut storage, table, _) = fixture(RelationPersistence::Permanent, 2);
        crate::mem::guard::forbid_alloc(|| {
            let (target, _ids) = storage.retain_metadata_walk_rows(table).unwrap();
            let original = storage.table(table).publication().row_layout;
            let rewrite = change(&storage, table, ColType::Int8, true);
            storage.rollback_table_def(rewrite);
            assert_eq!(storage.table(table).publication().row_layout, original);
            let metadata = change(&storage, table, ColType::Int4, false);
            assert_eq!(storage.commit_table_def(metadata).unwrap(), Some(false));
            storage.finish_table_def_commit(table, false);
            assert!(
                storage
                    .capture_resident_row_metadata(target, 1)
                    .unwrap()
                    .is_some()
            );
            let location = storage.heap.append_row(&[Datum::Int4(12)]).unwrap();
            storage
                .write_pending(table, 2, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 2, 8, 9).unwrap();
            assert!(
                storage
                    .capture_resident_row_metadata(target, 1)
                    .unwrap()
                    .is_some()
            );
        });
    }

    #[test]
    fn row_layout_definition_exhaustion_precedes_catalog_mutation() {
        let (storage, table, _) = fixture(RelationPersistence::Permanent, 2);
        crate::mem::guard::forbid_alloc(|| {
            let rewrite = change(&storage, table, ColType::Int8, true);
            let original = storage.select_table_definition(table, 0).0;
            storage.tables[table].publication().generation = u64::MAX;
            let error = storage.commit_table_def(rewrite).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "table publication generation exhausted"
            );
            assert_eq!(storage.table_def(table, 0).columns[0].ctype, ColType::Int4);
            assert!(storage.tables[table].publication().row_layout.accepts(0));
            assert_eq!(
                storage
                    .select_table_definition(table, 0)
                    .0
                    .committed_revision,
                original.committed_revision
            );
            storage.rollback_table_def(rewrite);
        });
    }

    #[test]
    fn row_layout_temporary_same_lsn_cannot_redirect_a_selected_version() {
        let (storage, table, _) = fixture(RelationPersistence::Temporary, 2);
        crate::mem::guard::forbid_alloc(|| {
            let original = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    7,
                )
                .unwrap()
                .unwrap();
            let snapshot = original.snapshot();
            let location = storage.heap.append_row(&[Datum::Int4(12)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 7).unwrap();
            let error = storage.read_row_snapshot(table, 1, snapshot).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            storage
                .with_row_bytes(table, 1, original, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(11));
                    Ok(())
                })
                .unwrap();
            let current = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    7,
                )
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, current, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(12));
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn row_layout_slot_reuse_resets_layout_without_rebinding_old_readers() {
        let (mut storage, table, _) = fixture(RelationPersistence::Permanent, 2);
        crate::mem::guard::forbid_alloc(|| {
            let original = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    7,
                )
                .unwrap()
                .unwrap();
            let snapshot = original.snapshot();
            let rewrite = change(&storage, table, ColType::Int8, true);
            assert_eq!(storage.commit_table_def(rewrite).unwrap(), Some(true));
            storage.finish_table_def_commit(table, true);
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def(
                    "layout_replacement",
                    &[("note", ColType::Text, false)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            let location = storage
                .heap
                .append_row(&[Datum::Text("replacement")])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(location, 9))
                .unwrap();
            let selected = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    9,
                )
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, selected, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Text], &mut values)?;
                    assert_eq!(values[0], Datum::Text("replacement"));
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                storage
                    .read_row_snapshot(table, 1, snapshot)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage
                .with_row_bytes(table, 1, original, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(11));
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn row_layout_rebinding_exhaustion_precedes_all_catalog_changes() {
        for dependent_exhausted in [false, true] {
            let (mut storage, table, other) = fixture(RelationPersistence::Permanent, 2);
            let destination = crate::storage::SqlName::parse("layout_destination").unwrap();
            storage.create_schema(destination).unwrap();
            let revision = storage.reserve_table_definition_revision().unwrap();
            {
                let original = *storage.table_def(table, 0);
                let mut dependent = storage.tables[other].definition_mut(revision);
                dependent.n_fkeys = 1;
                dependent.fkeys[0] = crate::storage::ForeignKey {
                    parent_schema: original.schema,
                    parent: original.name,
                    n_cols: 1,
                    n_parent_cols: 1,
                    ..crate::storage::ForeignKey::EMPTY
                };
            }
            crate::mem::guard::forbid_alloc(|| {
                let (target, mut definition) = storage.select_table_definition(table, 8);
                let original_name = definition.name;
                let original_schema = definition.schema;
                definition.schema = destination;
                definition.name = crate::storage::SqlName::parse("layout_renamed").unwrap();
                definition.columns[0].ctype = ColType::Int8;
                let mut mapping = [None; crate::storage::MAX_COLUMNS];
                mapping[0] = Some(definition.columns[0].name);
                let change = storage
                    .write_table_def(target, definition, &mapping, true)
                    .unwrap();
                let exhausted = if dependent_exhausted { other } else { table };
                storage.tables[exhausted].publication().generation = u64::MAX;
                let error = storage.commit_table_def(change).err().unwrap();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                let committed = storage.table_def(table, 0);
                assert_eq!(
                    (committed.schema, committed.name, committed.columns[0].ctype),
                    (original_schema, original_name, ColType::Int4)
                );
                drop(committed);
                let dependent = storage.table_def(other, 0);
                assert_eq!(
                    (dependent.fkeys[0].parent_schema, dependent.fkeys[0].parent),
                    (original_schema, original_name)
                );
                drop(dependent);
                storage.rollback_table_def(change);
            });
        }
    }

    #[test]
    fn row_layout_byte_sources_accept_compatible_updates_and_reject_rewrite_images() {
        let (mut storage, table, _) = fixture(RelationPersistence::Permanent, 2);
        crate::mem::guard::forbid_alloc(|| {
            let source = storage.table_row_snapshot(table, None).unwrap();
            let original = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    7,
                )
                .unwrap()
                .unwrap();
            let selected = source.retain(1, &original).unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(12)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            let compatible = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    9,
                )
                .unwrap()
                .unwrap();
            let updated = source.retain(1, &compatible).unwrap();
            let metadata = change(&storage, table, ColType::Int4, false);
            assert_eq!(storage.commit_table_def(metadata).unwrap(), Some(false));
            assert!(source.retain(1, &compatible).is_ok());
            let rewrite = change(&storage, table, ColType::Int8, true);
            let location = storage.heap.append_row(&[Datum::Int8(13)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            assert_eq!(storage.commit_table_def(rewrite).unwrap(), Some(true));
            storage.commit_rewritten_row(table, 1, 8, 11).unwrap();
            storage.finish_table_def_commit(table, true);
            let replacement = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.resident_row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    11,
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                source.retain(1, &replacement).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let current = storage.table_row_snapshot(table, None).unwrap();
            assert_eq!(
                current.retain(1, &original).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert!(current.retain(1, &replacement).is_ok());
            for (image, expected) in [
                (selected.read().unwrap(), 11),
                (updated.read().unwrap(), 12),
            ] {
                storage
                    .with_row_bytes(table, 1, image, |bytes| {
                        let mut values = [Datum::Null];
                        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(expected));
                        Ok(())
                    })
                    .unwrap();
            }
        });
    }
}
