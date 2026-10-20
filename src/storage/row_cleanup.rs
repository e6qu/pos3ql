//! Cache cleanup uses current table ownership and preserves reader boundaries.

use super::row_ownership::{acquire, try_mutex};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceMode {
    Background,
    Finish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceStep {
    Advanced,
    WaitingForOwner,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeapCleanup {
    AllSaved,
    Durable,
    PressuredDurable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryCleanup {
    AllSaved,
    Pressured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowCleanupTask {
    SavedMarkers { through_lsn: u64, generation: u64 },
    DurableHistory,
    SnapshotHistory,
    HeapRows(HeapCleanup),
    SavedEntries(EntryCleanup),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RowCleanupTarget {
    AllTables,
    Table(usize),
}

#[derive(Clone, Copy, Debug)]
struct RowCleanupPosition {
    table: usize,
    slot: usize,
    selected: Option<bool>,
}

/// A pass owns positions, never table or allocator guards between turns.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowCleanup {
    task: RowCleanupTask,
    target: RowCleanupTarget,
    position: RowCleanupPosition,
    retry: Option<RowCleanupPosition>,
    changed: bool,
}

impl RowCleanup {
    pub(crate) fn new(task: RowCleanupTask, target: RowCleanupTarget) -> Self {
        Self {
            task,
            target,
            position: RowCleanupPosition {
                table: match target {
                    RowCleanupTarget::AllTables => 0,
                    RowCleanupTarget::Table(table) => table,
                },
                slot: 0,
                selected: None,
            },
            retry: None,
            changed: false,
        }
    }

    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) fn task(&self) -> RowCleanupTask {
        self.task
    }
}

struct CleanedSlots {
    next: usize,
    visited: usize,
    complete: bool,
    selected: bool,
    changed: bool,
}

impl Storage {
    /// Retain before scanning row metadata. Row publication observes this owner
    /// under the same row lock, so a deletion cannot disappear after capture.
    pub(crate) fn retain_checkpoint_deletions(&self, slot: usize) -> std::sync::Arc<()> {
        std::sync::Arc::clone(&self.tables[slot].checkpoint_deletions)
    }

    pub(crate) fn advance_row_cleanup(
        &self,
        cleanup: &mut RowCleanup,
        work: core::num::NonZeroUsize,
        mode: MaintenanceMode,
    ) -> MaintenanceStep {
        let end = match cleanup.target {
            RowCleanupTarget::AllTables => self.tables.len(),
            RowCleanupTarget::Table(table) => table + 1,
        };
        let mut remaining = work.get();
        while remaining != 0 && cleanup.position.table < end {
            let position = cleanup.position;
            let batch = self.clean_row_slots(
                cleanup.task,
                position,
                core::num::NonZeroUsize::new(remaining).unwrap(),
                mode,
            );
            match batch {
                Some(batch) => {
                    remaining -= batch.visited.max(1);
                    cleanup.changed |= batch.changed;
                    if !batch.complete {
                        cleanup.position.slot = batch.next;
                        cleanup.position.selected = Some(batch.selected);
                        continue;
                    }
                }
                None => {
                    cleanup.retry.get_or_insert(position);
                    remaining -= 1;
                }
            }
            cleanup.position = RowCleanupPosition {
                table: position.table + 1,
                slot: 0,
                selected: None,
            };
        }
        if cleanup.position.table != end {
            return MaintenanceStep::Advanced;
        }
        if let Some(retry) = cleanup.retry.take() {
            cleanup.position = retry;
            MaintenanceStep::WaitingForOwner
        } else {
            MaintenanceStep::Complete
        }
    }

    fn clean_row_slots(
        &self,
        task: RowCleanupTask,
        position: RowCleanupPosition,
        work: core::num::NonZeroUsize,
        mode: MaintenanceMode,
    ) -> Option<CleanedSlots> {
        let table = &self.tables[position.table];
        let attempt = || {
            let snapshots = matches!(task, RowCleanupTask::SnapshotHistory)
                .then(|| try_mutex(&self.snapshots, "snapshot state lock poisoned"))
                .transpose()?;
            let definition = table.definition.try_read_for_batch()?;
            let publication = matches!(task, RowCleanupTask::SavedMarkers { .. })
                .then(|| try_mutex(&table.publication, "table publication lock poisoned"))
                .transpose()?;
            let generations = table.spill_generations.try_read_for_batch()?;
            let versions = matches!(
                task,
                RowCleanupTask::DurableHistory | RowCleanupTask::SnapshotHistory
            )
            .then(|| self.row_versions.try_write_for_batch())
            .transpose()?;
            let rows = table.rows.try_write_for_batch()?;
            Ok((
                snapshots,
                definition,
                publication,
                generations,
                versions,
                rows,
            ))
        };
        let (snapshots, definition, publication, generations, mut versions, mut rows) = match mode {
            MaintenanceMode::Finish => acquire(attempt),
            MaintenanceMode::Background => attempt().ok()?,
        };
        let live = definition.identity.live();
        let temporary = definition.committed.persistence == RelationPersistence::Temporary;
        let through_lsn = generations.published_through();
        let eligible = match task {
            RowCleanupTask::DurableHistory => live && !temporary && through_lsn.is_some(),
            RowCleanupTask::SnapshotHistory | RowCleanupTask::SavedMarkers { .. } => live,
            RowCleanupTask::HeapRows(selection) => {
                live && !generations.is_empty()
                    && match selection {
                        HeapCleanup::Durable | HeapCleanup::PressuredDurable => !temporary,
                        HeapCleanup::AllSaved => true,
                    }
            }
            RowCleanupTask::SavedEntries(_) => live && !generations.is_empty(),
        };
        let pressure_only = matches!(
            task,
            RowCleanupTask::HeapRows(HeapCleanup::PressuredDurable)
                | RowCleanupTask::SavedEntries(EntryCleanup::Pressured)
        );
        let selected = position
            .selected
            .unwrap_or_else(|| !pressure_only || rows.len() * 100 >= rows.capacity() * 50);
        if !eligible || !selected || rows.is_empty() {
            return Some(CleanedSlots {
                next: rows.scan_end(),
                visited: 0,
                complete: true,
                selected,
                changed: false,
            });
        }
        let start = position.slot;
        assert!(
            start <= rows.scan_end(),
            "cleanup cursor exceeds used row slots"
        );
        let end = start.saturating_add(work.get()).min(rows.scan_end());
        let mut changed = false;
        for slot in start..end {
            let Some((&rowid, state)) = rows.entry_at_slot_mut(slot) else {
                continue;
            };
            match task {
                RowCleanupTask::SavedMarkers {
                    through_lsn,
                    generation,
                } => {
                    let saved = if temporary {
                        state
                            .committed_generation
                            .is_some_and(|revision| revision <= generation)
                    } else {
                        publication.as_ref().unwrap().generation == generation
                    };
                    if saved && state.checkpoint_change_lsn <= through_lsn {
                        state.checkpoint_change_lsn = 0;
                    }
                    if state.committed.is_none()
                        && (!temporary || saved)
                        && state.committed_lsn <= through_lsn
                        && state.history.is_empty()
                        && state.pending.is_none()
                    {
                        rows.remove(&rowid).expect("selected row exists");
                        changed = true;
                    }
                }
                RowCleanupTask::DurableHistory => {
                    changed |= release_committed_history_through(
                        &mut versions.as_mut().unwrap().committed_row_versions,
                        &mut state.history,
                        through_lsn.unwrap(),
                    );
                }
                RowCleanupTask::SnapshotHistory => {
                    changed |= prune_committed_history(
                        &mut versions.as_mut().unwrap().committed_row_versions,
                        &mut state.history,
                        snapshots.as_ref().unwrap().oldest(),
                    );
                }
                RowCleanupTask::HeapRows(_) => {
                    if let Some(RowHome::Heap(location)) = state.committed
                        && let Some(sst) = generations.eviction_member(
                            state.committed_lsn,
                            state
                                .committed_generation
                                .expect("committed heap image has a revision"),
                        )
                    {
                        state.committed = Some(RowHome::Spilled {
                            len: location.len,
                            sst,
                            commit_lsn: state.committed_lsn,
                        });
                        changed = true;
                    }
                }
                RowCleanupTask::SavedEntries(_) => {
                    if Self::redundant_spilled_row_state(state) {
                        rows.remove(&rowid).expect("selected row exists");
                        changed = true;
                    }
                }
            }
        }
        Some(CleanedSlots {
            next: end,
            visited: end - start,
            complete: end == rows.scan_end(),
            selected,
            changed,
        })
    }

    #[cfg(test)]
    pub(crate) fn finish_row_cleanup(&self, task: RowCleanupTask, target: RowCleanupTarget) {
        let mut cleanup = RowCleanup::new(task, target);
        loop {
            if self.advance_row_cleanup(
                &mut cleanup,
                core::num::NonZeroUsize::new(64).unwrap(),
                MaintenanceMode::Finish,
            ) == MaintenanceStep::Complete
            {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};

    fn fixture(persistence: RelationPersistence, table_rows: usize) -> (Storage, usize, usize) {
        let mut config = test_config();
        config.table_rows = table_rows;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def("cleanup_owned", &[("id", ColType::Int4, false)]);
        definition.persistence = persistence;
        let first = storage.create_table(definition).unwrap();
        let second = storage
            .create_table(make_def("cleanup_other", &[("id", ColType::Int4, false)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
        storage.tables[first]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        (storage, first, second)
    }

    fn saved_handle(tag: u8) -> crate::store::SstHandle {
        crate::store::SstHandle {
            index: crate::store::BlockId([tag; 32]),
            filter: crate::store::BlockId([tag + 1; 32]),
            roster: crate::store::BlockId([tag + 2; 32]),
            format: crate::store::RowSstFormat::PackedV4,
        }
    }

    fn update(storage: &Storage, table: usize, rowid: u64, value: i32, txid: u32, lsn: u64) {
        let image = storage
            .heap
            .append_row_image(&[Datum::Int4(value)])
            .unwrap();
        storage
            .write_pending_image_untracked(table, rowid, txid, 1, Some(image))
            .unwrap();
        storage.commit_row(table, rowid, txid, lsn).unwrap();
        storage.observe_lsn(lsn);
    }

    fn selected(storage: &Storage, table: usize, snapshot: u64) -> VisibleRowHome {
        let state = storage.resident_row_state(table, 1).unwrap().unwrap();
        storage
            .visible_row_home_at(table, 1, state, 99, SNAPSHOT_ALL, snapshot)
            .unwrap()
            .unwrap()
    }

    fn assert_value(storage: &Storage, table: usize, selected: VisibleRowHome, expected: i32) {
        storage
            .with_row_bytes(table, 1, selected, |bytes| {
                let mut values = [Datum::Null];
                rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                assert_eq!(values[0], Datum::Int4(expected));
                Ok(())
            })
            .unwrap();
    }

    fn bounded_fixture(count: usize) -> (Storage, usize, usize) {
        let mut config = test_config();
        config.table_rows = count * 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let first = storage
            .create_table(make_def("bounded_first", &[("id", ColType::Int4, false)]))
            .unwrap();
        let second = storage
            .create_table(make_def("bounded_second", &[("id", ColType::Int4, false)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
        for table in [first, second] {
            for rowid in 1..=count as u64 {
                storage.tables[table]
                    .rows
                    .insert(rowid, RowState::committed_only_at(location, 7))
                    .unwrap();
            }
            storage
                .append_spill_through(
                    &storage
                        .reserve_spill_generation(
                            table,
                            storage.table(table).created_at(),
                            storage.table(table).rewrite_generation(),
                        )
                        .unwrap(),
                    saved_handle(table as u8 * 4 + 1),
                    7,
                )
                .unwrap();
            let generation = storage.tables[table].generation();
            storage.finish_row_cleanup(
                RowCleanupTask::SavedMarkers {
                    through_lsn: 7,
                    generation,
                },
                RowCleanupTarget::Table(table),
            );
            storage.clear_table_dirty_at_generation(table, generation);
        }
        storage.set_lsn(7);
        (storage, first, second)
    }

    fn remaining_rows(storage: &Storage, table: usize, task: RowCleanupTask) -> usize {
        let rows = storage.tables[table].rows.read();
        rows.iter()
            .filter(|(_, state)| match task {
                RowCleanupTask::DurableHistory | RowCleanupTask::SnapshotHistory => {
                    !state.history.is_empty()
                }
                RowCleanupTask::HeapRows(_) => matches!(state.committed, Some(RowHome::Heap(_))),
                RowCleanupTask::SavedEntries(_) | RowCleanupTask::SavedMarkers { .. } => true,
            })
            .count()
    }

    fn prepare_task(
        storage: &Storage,
        first: usize,
        second: usize,
        count: usize,
        task: RowCleanupTask,
    ) {
        match task {
            RowCleanupTask::SavedMarkers { .. } => {
                for table in [first, second] {
                    for (_, state) in storage.tables[table].rows.write().iter_mut() {
                        state.committed = None;
                    }
                }
            }
            RowCleanupTask::DurableHistory | RowCleanupTask::SnapshotHistory => {
                storage.register_snapshot(99, 7).unwrap();
                for table in [first, second] {
                    for rowid in 1..=count as u64 {
                        update(storage, table, rowid, 20, 8, 9);
                    }
                }
                if matches!(task, RowCleanupTask::SnapshotHistory) {
                    storage.end_transaction_snapshots(99, SnapshotCleanup::Schedule);
                }
            }
            RowCleanupTask::SavedEntries(_) => {
                storage.finish_row_cleanup(
                    RowCleanupTask::HeapRows(HeapCleanup::Durable),
                    RowCleanupTarget::AllTables,
                );
            }
            _ => {}
        }
    }

    const BOUNDED_TASKS: [RowCleanupTask; 8] = [
        RowCleanupTask::SavedMarkers {
            through_lsn: 7,
            generation: 0,
        },
        RowCleanupTask::DurableHistory,
        RowCleanupTask::SnapshotHistory,
        RowCleanupTask::HeapRows(HeapCleanup::Durable),
        RowCleanupTask::HeapRows(HeapCleanup::PressuredDurable),
        RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
        RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
        RowCleanupTask::SavedEntries(EntryCleanup::Pressured),
    ];

    #[test]
    fn bounded_row_cleanup_limits_every_walk_and_keeps_pressure_selection() {
        const ROWS: usize = 129;
        for task in BOUNDED_TASKS {
            let (storage, first, second) = bounded_fixture(ROWS);
            prepare_task(&storage, first, second, ROWS, task);
            let mut cleanup = RowCleanup::new(task, RowCleanupTarget::Table(first));
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(remaining_rows(&storage, first, task), ROWS, "{task:?}");
                assert_eq!(
                    storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    ),
                    MaintenanceStep::Advanced
                );
                assert_eq!(remaining_rows(&storage, first, task), ROWS - 1, "{task:?}");
                assert_eq!(remaining_rows(&storage, second, task), ROWS);
                let mut finished = false;
                for _ in 0..ROWS {
                    let before = remaining_rows(&storage, first, task);
                    let step = storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    );
                    let after = remaining_rows(&storage, first, task);
                    assert!(before - after <= 1, "{task:?}");
                    if step == MaintenanceStep::Complete {
                        finished = true;
                        break;
                    }
                    assert_eq!(step, MaintenanceStep::Advanced);
                }
                assert!(finished, "{task:?}");
                assert_eq!(remaining_rows(&storage, first, task), 0, "{task:?}");
                assert_eq!(remaining_rows(&storage, second, task), ROWS);
            });
        }
    }

    #[test]
    fn bounded_row_cleanup_skips_busy_tables_and_releases_every_prefix_owner() {
        for task in BOUNDED_TASKS {
            let (storage, first, second) = bounded_fixture(3);
            prepare_task(&storage, first, second, 3, task);
            let held = storage.tables[first].rows.write();
            let mut cleanup = RowCleanup::new(task, RowCleanupTarget::AllTables);
            crate::mem::guard::forbid_alloc(|| {
                let mut waited = false;
                for _ in 0..storage.tables.len() * 4 + 1 {
                    let step = storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    );
                    assert!(storage.snapshots.try_lock().is_ok());
                    assert!(storage.row_versions.test_write().is_ok());
                    assert!(storage.tables[first].publication.try_lock().is_ok());
                    if step == MaintenanceStep::WaitingForOwner {
                        waited = true;
                        break;
                    }
                    assert_eq!(step, MaintenanceStep::Advanced);
                }
                assert!(waited, "{task:?}");
                assert_eq!(remaining_rows(&storage, second, task), 0, "{task:?}");
                assert_eq!(held.len(), 3);
                assert_eq!(cleanup.position.table, first);
                assert_eq!(cleanup.position.slot, 0);
            });
            drop(held);
            crate::mem::guard::forbid_alloc(|| {
                let mut finished = false;
                for _ in 0..storage.tables.len() * 4 + 1 {
                    match storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    ) {
                        MaintenanceStep::Complete => {
                            finished = true;
                            break;
                        }
                        MaintenanceStep::Advanced => {}
                        MaintenanceStep::WaitingForOwner => panic!("uncontended cleanup waited"),
                    }
                }
                assert!(finished, "{task:?}");
                assert_eq!(remaining_rows(&storage, first, task), 0, "{task:?}");
            });
        }
    }

    #[test]
    fn bounded_row_cleanup_rechecks_later_writes_and_retained_bytes() {
        let (storage, first, second) = bounded_fixture(4);
        let selected = selected(&storage, first, 7);
        let mut cleanup = RowCleanup::new(
            RowCleanupTask::HeapRows(HeapCleanup::Durable),
            RowCleanupTarget::Table(first),
        );
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut cleanup,
                    core::num::NonZeroUsize::new(1).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Advanced
            );
            update(&storage, first, 2, 30, 8, 9);
            update(&storage, second, 1, 50, 10, 11);
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut cleanup,
                    core::num::NonZeroUsize::new(64).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Complete
            );
            assert!(matches!(
                storage.tables[first].rows.get(&2).unwrap().committed,
                Some(RowHome::Heap(_))
            ));
            assert_eq!(remaining_rows(&storage, first, cleanup.task()), 1);
            assert_value(&storage, first, selected, 10);
            assert_eq!(
                storage.tables[second].rows.get(&1).unwrap().committed_lsn,
                11
            );
        });
        let (storage, first, _) = bounded_fixture(4);
        for (_, state) in storage.tables[first].rows.write().iter_mut() {
            state.checkpoint_change_lsn = 7;
        }
        let generation = storage.tables[first].generation();
        let mut markers = RowCleanup::new(
            RowCleanupTask::SavedMarkers {
                through_lsn: 7,
                generation,
            },
            RowCleanupTarget::Table(first),
        );
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut markers,
                    core::num::NonZeroUsize::new(1).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Advanced
            );
            assert_eq!(
                storage.tables[first]
                    .rows
                    .get(&1)
                    .unwrap()
                    .checkpoint_change_lsn,
                0
            );
            update(&storage, first, 1, 30, 8, 9);
            update(&storage, first, 2, 40, 10, 11);
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut markers,
                    core::num::NonZeroUsize::new(64).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Complete
            );
            for (rowid, expected) in [(1, 9), (2, 11), (3, 7), (4, 7)] {
                assert_eq!(
                    storage.tables[first]
                        .rows
                        .get(&rowid)
                        .unwrap()
                        .checkpoint_change_lsn,
                    expected
                );
            }
            storage.finish_row_cleanup(
                RowCleanupTask::SavedMarkers {
                    through_lsn: 11,
                    generation: storage.tables[first].generation(),
                },
                RowCleanupTarget::Table(first),
            );
            assert!(
                storage.tables[first]
                    .rows
                    .read()
                    .iter()
                    .all(|(_, state)| state.checkpoint_change_lsn == 0)
            );
        });
    }

    #[test]
    fn bounded_row_cleanup_survives_free_slot_reuse() {
        let (storage, first, _) = bounded_fixture(4);
        let mut cleanup = RowCleanup::new(
            RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
            RowCleanupTarget::Table(first),
        );
        storage.finish_row_cleanup(
            RowCleanupTask::HeapRows(HeapCleanup::Durable),
            RowCleanupTarget::Table(first),
        );
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut cleanup,
                    core::num::NonZeroUsize::new(1).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Advanced
            );
            let location = storage.heap.append_row(&[Datum::Int4(90)]).unwrap();
            storage.tables[first]
                .rows
                .write()
                .insert(99, RowState::committed_only_at(location, 9))
                .unwrap();
            assert_eq!(
                storage.advance_row_cleanup(
                    &mut cleanup,
                    core::num::NonZeroUsize::new(64).unwrap(),
                    MaintenanceMode::Background,
                ),
                MaintenanceStep::Complete
            );
            assert_eq!(storage.tables[first].rows.len(), 1);
            assert!(storage.tables[first].rows.get(&99).is_some());
            assert!(storage.row_versions.test_write().is_ok());
            update(&storage, first, 99, 100, 8, 10);
        });
    }

    #[test]
    fn bounded_row_cleanup_temporary_eviction_rechecks_generation_between_batches() {
        for delete_after_save in [false, true] {
            let (mut storage, table, _) = fixture(RelationPersistence::Temporary, 4);
            let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
            for rowid in [2, 3] {
                storage.tables[table]
                    .rows
                    .insert(rowid, RowState::committed_only_at(location, 7))
                    .unwrap();
            }
            for (_, row) in storage.tables[table].rows.write().iter_mut() {
                row.committed_lsn = 0;
                row.checkpoint_change_lsn = 0;
            }
            storage.set_lsn(0);
            let selected = selected(&storage, table, 0);
            let generation = storage.tables[table].generation();
            storage
                .install_temporary_spill(table, saved_handle(1), generation, false)
                .unwrap();
            let mut cleanup = RowCleanup::new(
                RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
                RowCleanupTarget::Table(table),
            );
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    ),
                    MaintenanceStep::Advanced
                );
                // Temporary commits can share a journal position; revisions must differ.
                update(&storage, table, 2, 30, 8, 0);
                assert_eq!(
                    storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    ),
                    MaintenanceStep::Advanced
                );
                assert!(matches!(
                    storage.tables[table].rows.get(&2).unwrap().committed,
                    Some(RowHome::Heap(_))
                ));
                assert_eq!(
                    storage.advance_row_cleanup(
                        &mut cleanup,
                        core::num::NonZeroUsize::new(1).unwrap(),
                        MaintenanceMode::Background,
                    ),
                    MaintenanceStep::Complete
                );
                for rowid in [1, 3] {
                    assert!(matches!(
                        storage.tables[table].rows.get(&rowid).unwrap().committed,
                        Some(RowHome::Spilled { .. })
                    ));
                }
                if delete_after_save {
                    storage
                        .write_pending_image_untracked(table, 2, 10, 1, None)
                        .unwrap();
                    storage.commit_row(table, 2, 10, 0).unwrap();
                    let rejected = storage.heap.append_row_image(&[Datum::Int4(99)]).unwrap();
                    let undo = storage
                        .write_pending_image_untracked(table, 2, 12, 1, Some(rejected))
                        .unwrap();
                    storage.restore_pending(table, 2, 12, undo);
                }
                storage.finish_row_cleanup(
                    RowCleanupTask::SavedMarkers {
                        through_lsn: 0,
                        generation,
                    },
                    RowCleanupTarget::Table(table),
                );
                storage.finish_row_cleanup(
                    RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
                    RowCleanupTarget::Table(table),
                );
                assert_eq!(storage.tables[table].rows.len(), 1);
                let current = storage.tables[table].rows.get(&2).unwrap();
                assert_eq!(current.committed_lsn, 0);
                assert_eq!(current.committed.is_none(), delete_after_save);
                assert!(
                    current
                        .committed_generation
                        .is_some_and(|revision| revision > generation)
                );
                assert_eq!(current.checkpoint_change_lsn, 0);
                assert_value(&storage, table, selected, 10);
            });
        }
    }

    #[test]
    fn row_lock_owners_shared_cleanup_preserves_retained_bytes_and_later_writes() {
        let (mut storage, first, second) = fixture(RelationPersistence::Permanent, 2);
        let location = storage.heap.append_row(&[Datum::Int4(40)]).unwrap();
        storage.tables[first]
            .rows
            .insert(2, RowState::committed_only_at(location, 7))
            .unwrap();
        storage
            .append_spill_through(
                &storage
                    .reserve_spill_generation(
                        first,
                        storage.table(first).created_at(),
                        storage.table(first).rewrite_generation(),
                    )
                    .unwrap(),
                saved_handle(1),
                7,
            )
            .unwrap();
        let generation = storage.tables[first].generation();
        storage.finish_row_cleanup(
            RowCleanupTask::SavedMarkers {
                through_lsn: 7,
                generation,
            },
            RowCleanupTarget::Table(first),
        );
        storage.clear_table_dirty_at_generation(first, generation);
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            update(&storage, first, 1, 20, 8, 9);
            update(&storage, first, 1, 30, 10, 11);
        });
        let old = selected(&storage, first, 7);
        let current = selected(&storage, first, u64::MAX);
        let definition = storage.tables[first].definition();
        std::thread::scope(|scope| {
            let shared = &storage;
            let cleanup = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    shared.finish_row_cleanup(
                        RowCleanupTask::DurableHistory,
                        RowCleanupTarget::AllTables,
                    );
                    shared.finish_row_cleanup(
                        RowCleanupTask::HeapRows(HeapCleanup::Durable),
                        RowCleanupTarget::AllTables,
                    );
                    shared.finish_row_cleanup(
                        RowCleanupTask::SavedEntries(EntryCleanup::Pressured),
                        RowCleanupTarget::AllTables,
                    );
                    shared.finish_row_cleanup(
                        RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
                        RowCleanupTarget::Table(first),
                    );
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                update(&storage, second, 1, 50, 12, 13);
                assert_value(&storage, first, old, 10);
                assert_value(&storage, first, current, 30);
            });
            cleanup.join().unwrap();
        });
        crate::mem::guard::forbid_alloc(|| {
            let row = storage.resident_row_state(first, 1).unwrap().unwrap();
            assert_eq!(row.history.len(), 1);
            assert!(matches!(row.committed, Some(RowHome::Heap(_))));
            drop(row);
            assert!(storage.tables[first].rows.get(&2).is_none());
            assert_value(&storage, first, selected(&storage, first, 9), 20);
            assert_value(&storage, first, selected(&storage, first, u64::MAX), 30);
            assert_eq!(
                storage.tables[second].rows.get(&1).unwrap().committed_lsn,
                13
            );
            storage.release_snapshot(99);
        });
        drop(definition);
    }

    #[test]
    fn row_lock_owners_local_history_rechecks_readers_and_temporary_generation() {
        for matching_generation in [true, false] {
            let (mut storage, table, _) = fixture(RelationPersistence::Temporary, 2);
            crate::mem::guard::forbid_alloc(|| {
                storage.register_snapshot(99, 7).unwrap();
                update(&storage, table, 1, 20, 8, 9);
            });
            let old = selected(&storage, table, 7);
            let generation = storage.tables[table].generation();
            storage
                .install_temporary_spill(table, saved_handle(1), generation, false)
                .unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let shared = &storage;
                shared.finish_row_cleanup(
                    RowCleanupTask::SnapshotHistory,
                    RowCleanupTarget::Table(table),
                );
                assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 1);
                shared.finish_row_cleanup(
                    RowCleanupTask::DurableHistory,
                    RowCleanupTarget::AllTables,
                );
                assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 1);

                shared.finish_row_cleanup(
                    RowCleanupTask::HeapRows(HeapCleanup::Durable),
                    RowCleanupTarget::AllTables,
                );
                assert!(matches!(
                    shared.tables[table].rows.get(&1).unwrap().committed,
                    Some(RowHome::Heap(_))
                ));
                if !matching_generation {
                    update(shared, table, 1, 30, 10, 11);
                    shared.finish_row_cleanup(
                        RowCleanupTask::SnapshotHistory,
                        RowCleanupTarget::Table(table),
                    );
                    assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 2);
                }
                shared.finish_row_cleanup(
                    RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
                    RowCleanupTarget::Table(table),
                );
                let state = shared.tables[table].rows.get(&1).unwrap();
                if matching_generation {
                    assert!(matches!(
                        state.committed,
                        Some(RowHome::Spilled { commit_lsn: 9, .. })
                    ));
                } else {
                    assert!(matches!(state.committed, Some(RowHome::Heap(_))));
                    assert_value(shared, table, selected(shared, table, u64::MAX), 30);
                }
                shared.release_snapshot(99);
                shared.finish_row_cleanup(
                    RowCleanupTask::SnapshotHistory,
                    RowCleanupTarget::Table(table),
                );
                assert!(
                    shared.tables[table]
                        .rows
                        .get(&1)
                        .unwrap()
                        .history
                        .is_empty()
                );
                assert_value(shared, table, old, 10);
            });
        }
    }

    #[test]
    fn row_lock_owners_tombstone_cleanup_keeps_live_pending_and_historical_states() {
        const DELETED: usize = 1536;
        let mut config = test_config();
        config.table_rows = 2048;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("cleanup_deleted", &[("id", ColType::Int4, false)]))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
            let mut deleted = RowState::committed_only_at(location, 7);
            deleted.committed = None;
            let mut retained = [deleted; 4];
            retained[0].committed = Some(RowHome::Heap(location));
            retained[1].committed = Some(RowHome::Spilled {
                len: location.len,
                sst: crate::storage::SpillGenerationId::fixture(0),
                commit_lsn: 7,
            });
            {
                let versions = storage.row_versions.exclusive();
                push_committed_version(
                    &mut versions.committed_row_versions,
                    &mut retained[2].history,
                    config.max_row_versions_per_row,
                    CommittedVersion {
                        home: Some(RowHome::Heap(location)),
                        lsn: 6,
                    },
                )
                .unwrap();
                push_pending_version(
                    &mut versions.pending_row_versions,
                    &mut retained[3].pending,
                    config.max_row_versions_per_row,
                    PendingChange {
                        txid: 9,
                        cid: 1,
                        loc: Some(location),
                        changed_columns: ColumnSet::EMPTY,
                        changes_existence: false,
                    },
                )
                .unwrap();
            }
            for rowid in 0..DELETED {
                storage.tables[table]
                    .rows
                    .insert(rowid as u64, deleted)
                    .unwrap();
            }
            for (index, state) in retained.iter().enumerate() {
                storage.tables[table]
                    .rows
                    .insert((DELETED + index) as u64, *state)
                    .unwrap();
            }
            let mut later_delete = deleted;
            later_delete.committed_lsn = 8;
            later_delete.checkpoint_change_lsn = 8;
            storage.tables[table]
                .rows
                .insert((DELETED + retained.len()) as u64, later_delete)
                .unwrap();
            storage.finish_row_cleanup(
                RowCleanupTask::SavedMarkers {
                    through_lsn: 7,
                    generation: 0,
                },
                RowCleanupTarget::Table(table),
            );
            assert_eq!(storage.tables[table].rows.len(), retained.len() + 1);
            for rowid in 0..DELETED {
                assert!(storage.tables[table].rows.get(&(rowid as u64)).is_none());
            }
            for (index, state) in retained.iter().enumerate() {
                assert_eq!(
                    storage.tables[table].rows.get(&((DELETED + index) as u64)),
                    Some(*state)
                );
            }
            storage.finish_row_cleanup(
                RowCleanupTask::SavedMarkers {
                    through_lsn: 7,
                    generation: 0,
                },
                RowCleanupTarget::Table(table),
            );
            assert_eq!(storage.tables[table].rows.len(), retained.len() + 1);
            assert_eq!(
                storage.tables[table]
                    .rows
                    .get(&((DELETED + retained.len()) as u64)),
                Some(later_delete)
            );
            storage.finish_row_cleanup(
                RowCleanupTask::SavedMarkers {
                    through_lsn: 8,
                    generation: 0,
                },
                RowCleanupTarget::Table(table),
            );
            assert_eq!(storage.tables[table].rows.len(), retained.len());
        });
    }
}
