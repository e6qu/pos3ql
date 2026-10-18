//! Bounded heap preparation and copying; row versions coordinate publication.

use core::num::NonZeroUsize;

mod sort;
use sort::RelocationSort;

use super::row_heap::{HeapCopy, RelocationValidation};
use super::row_versions::RowVersionState;
use super::*;
use crate::mem::fixed_vec::FixedVec;

/// Cache locations, never durable row identities.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeapRelocationEntry {
    source: RowLoc,
    destination: u32,
}

// All row-map locations are included until retirement clears them. Avoid
// acquiring definitions after shared version ownership.
fn visit_heap_locations(
    tables: &[Table],
    versions: &RowVersionState,
    mut visit: impl FnMut(RowLoc) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    for table in tables {
        for (_, state) in table.rows.iter() {
            if let Some(RowHome::Heap(location)) = state.committed {
                visit(location)?;
            }
            let mut slot = state.history.tail;
            while let Some(index) = slot {
                let entry = &versions.committed_row_versions[index];
                if let Some(RowHome::Heap(location)) = entry.version.home {
                    visit(location)?;
                }
                slot = entry.previous;
            }
            let mut slot = state.pending.tail;
            while let Some(index) = slot {
                let entry = &versions.pending_row_versions[index];
                if let Some(location) = entry.change.loc {
                    visit(location)?;
                }
                slot = entry.previous;
            }
        }
    }
    Ok(())
}

struct RelocationPlan<'scratch> {
    entries: &'scratch mut FixedVec<HeapRelocationEntry>,
    packed_bytes: usize,
}

impl<'scratch> RelocationPlan<'scratch> {
    fn capture(
        tables: &[Table],
        versions: &RowVersionState,
        entries: &'scratch mut FixedVec<HeapRelocationEntry>,
    ) -> Result<Self, SqlError> {
        entries.clear();
        visit_heap_locations(tables, versions, |source| {
            entries
                .push(HeapRelocationEntry {
                    source,
                    destination: 0,
                })
                .map_err(|error| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "heap compaction scratch overflow: {}",
                        error
                    )
                })
        })?;
        Ok(Self {
            entries,
            packed_bytes: 0,
        })
    }

    fn sort(&mut self) {
        self.entries
            .as_mut_slice()
            .sort_unstable_by_key(|entry| (entry.source.offset, entry.source.len));
    }

    fn pack(&mut self) -> Result<(), SqlError> {
        let mut prior = None;
        let mut destination = 0;
        for entry in self.entries.iter_mut() {
            if prior != Some(entry.source) {
                destination = self.packed_bytes;
                self.packed_bytes = self
                    .packed_bytes
                    .checked_add(entry.source.len as usize)
                    .filter(|&bytes| bytes <= u32::MAX as usize)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "heap relocation address range exhausted"
                        )
                    })?;
                prior = Some(entry.source);
            }
            entry.destination = destination as u32;
        }
        Ok(())
    }

    fn relocated(
        &self,
        source: RowLoc,
        generation: u64,
        tail: Option<(usize, usize)>,
    ) -> Result<RowLoc, SqlError> {
        if source.generation.checked_add(1) != Some(generation) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "heap relocation encountered a replaced row allocation"
            ));
        }
        let destination = if let Some((frontier, copied_tail)) = tail
            && source.offset as usize >= frontier
        {
            let end = (source.offset as usize).checked_add(source.len as usize);
            if end.is_none_or(|end| end > frontier + copied_tail) {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "heap relocation row exceeds its copied tail"
                ));
            }
            self.packed_bytes + (source.offset as usize - frontier)
        } else {
            let index = self
                .entries
                .binary_search_by_key(&(source.offset, source.len), |entry| {
                    (entry.source.offset, entry.source.len)
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "heap relocation row was not retained during capture"
                    )
                })?;
            self.entries[index].destination as usize
        };
        Ok(RowLoc {
            offset: destination as u32,
            len: source.len,
            generation,
        })
    }

    fn preflight(
        &self,
        tables: &[Table],
        versions: &RowVersionState,
        generation: u64,
        tail: Option<(usize, usize)>,
    ) -> Result<(), SqlError> {
        visit_heap_locations(tables, versions, |source| {
            self.relocated(source, generation, tail).map(|_| ())
        })
    }

    fn apply(
        &self,
        tables: &[Table],
        versions: &mut RowVersionState,
        generation: u64,
        tail: Option<(usize, usize)>,
    ) {
        let relocate = |location| {
            self.relocated(location, generation, tail)
                .expect("preflighted heap row")
        };
        for table in tables {
            let mut rows = table.rows.write();
            for (_, state) in rows.iter_mut() {
                if let Some(RowHome::Heap(location)) = state.committed {
                    state.committed = Some(RowHome::Heap(relocate(location)));
                }
                let mut slot = state.history.tail;
                while let Some(index) = slot {
                    let entry = &mut versions.committed_row_versions[index];
                    if let Some(RowHome::Heap(location)) = entry.version.home {
                        entry.version.home = Some(RowHome::Heap(relocate(location)));
                    }
                    slot = entry.previous;
                }
                let mut slot = state.pending.tail;
                while let Some(index) = slot {
                    let entry = &mut versions.pending_row_versions[index];
                    if let Some(location) = entry.change.loc {
                        entry.change.loc = Some(relocate(location));
                    }
                    slot = entry.previous;
                }
            }
        }
    }
}

/// Dropping an unfinished job releases the private buffer and capture boundary.
pub(crate) struct HeapRelocationJob<'storage, 'scratch> {
    storage: &'storage Storage,
    entries: &'scratch mut FixedVec<HeapRelocationEntry>,
    progress: HeapRelocationProgress,
}

enum HeapRelocationPreparation {
    Sorting(RelocationSort),
    Packing {
        entry: usize,
        validation: RelocationValidation,
    },
    Ready,
}

struct HeapRelocationProgress {
    copy: HeapCopy,
    preparation: HeapRelocationPreparation,
    packed_bytes: usize,
    entry: usize,
    entry_at: usize,
    copied_tail: usize,
}

pub(crate) enum HeapRelocationPublication<'storage, 'scratch> {
    More(HeapRelocationJob<'storage, 'scratch>),
    Published,
}

impl HeapRelocationProgress {
    fn prepare_step(
        &mut self,
        entries: &mut FixedVec<HeapRelocationEntry>,
        remaining: &mut usize,
    ) -> Result<bool, SqlError> {
        loop {
            match &mut self.preparation {
                HeapRelocationPreparation::Sorting(sort) => {
                    if !sort.step(entries.as_mut_slice(), remaining) {
                        return Ok(false);
                    }
                    self.preparation = HeapRelocationPreparation::Packing {
                        entry: 0,
                        validation: RelocationValidation::default(),
                    };
                }
                HeapRelocationPreparation::Packing { entry, validation } => {
                    while *entry < entries.len() && *remaining != 0 {
                        let source = entries[*entry].source;
                        self.copy.validate_location(validation, source)?;
                        if (source.offset as usize)
                            .checked_add(source.len as usize)
                            .is_none_or(|end| end > self.copy.frontier())
                        {
                            return Err(sql_err!(
                                sqlstate::SERIALIZATION_FAILURE,
                                "heap relocation row exceeds its captured prefix"
                            ));
                        }
                        let destination = if *entry == 0 || entries[*entry - 1].source != source {
                            let destination = self.packed_bytes;
                            self.packed_bytes = self
                                .packed_bytes
                                .checked_add(source.len as usize)
                                .filter(|&bytes| bytes <= u32::MAX as usize)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                        "heap relocation address range exhausted"
                                    )
                                })?;
                            destination as u32
                        } else {
                            entries[*entry - 1].destination
                        };
                        entries[*entry].destination = destination;
                        *entry += 1;
                        *remaining -= 1;
                    }
                    if *entry != entries.len() {
                        return Ok(false);
                    }
                    self.preparation = HeapRelocationPreparation::Ready;
                }
                HeapRelocationPreparation::Ready => return Ok(true),
            }
        }
    }

    /// Work is bounded by bytes and by zero-length or aliased entry visits.
    fn copy_step(
        &mut self,
        entries: &FixedVec<HeapRelocationEntry>,
        budget: NonZeroUsize,
    ) -> Result<bool, SqlError> {
        let mut remaining = budget.get();
        while self.entry < entries.len() && remaining != 0 {
            let entry = entries[self.entry];
            if (self.entry > 0
                && self.entry_at == 0
                && entries[self.entry - 1].source == entry.source)
                || entry.source.len == 0
            {
                self.entry += 1;
                remaining -= 1;
                continue;
            }
            let count = remaining.min(entry.source.len as usize - self.entry_at);
            self.copy.copy_bytes(
                entry.source,
                self.entry_at,
                entry.destination as usize + self.entry_at,
                count,
            )?;
            self.entry_at += count;
            remaining -= count;
            if self.entry_at == entry.source.len as usize {
                self.entry += 1;
                self.entry_at = 0;
            }
        }
        if self.entry != entries.len() {
            return Ok(false);
        }
        let tail = self.copy.source_used() - self.copy.frontier();
        let count = remaining.min(tail - self.copied_tail);
        if count != 0 {
            let source = RowLoc {
                offset: (self.copy.frontier() + self.copied_tail) as u32,
                len: count as u32,
                generation: self.copy.generation() - 1,
            };
            self.copy
                .copy_bytes(source, 0, self.packed_bytes + self.copied_tail, count)?;
            self.copied_tail += count;
        }
        Ok(self.copied_tail == tail)
    }
    fn try_publish(
        &mut self,
        storage: &Storage,
        entries: &mut FixedVec<HeapRelocationEntry>,
    ) -> Result<bool, SqlError> {
        if !self.copy.belongs_to(&storage.heap) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap relocation belongs to another storage instance"
            ));
        }
        if !matches!(self.preparation, HeapRelocationPreparation::Ready)
            || self.entry != entries.len()
        {
            return Ok(false);
        }
        let plan = RelocationPlan {
            entries,
            packed_bytes: self.packed_bytes,
        };
        {
            let mut versions = storage.row_versions.write();
            let tail = Some((self.copy.frontier(), self.copied_tail));
            self.copy
                .try_publish(self.packed_bytes, self.copied_tail, |generation| {
                    plan.preflight(&storage.tables, &versions, generation, tail)?;
                    plan.apply(&storage.tables, &mut versions, generation, tail);
                    Ok(())
                })
        }
    }
}

impl HeapRelocationJob<'_, '_> {
    pub(crate) fn copy_step(&mut self, budget: NonZeroUsize) -> Result<bool, SqlError> {
        self.progress.copy_step(self.entries, budget)
    }
}

impl<'storage, 'scratch> HeapRelocationJob<'storage, 'scratch> {
    pub(crate) fn try_publish(
        mut self,
    ) -> Result<HeapRelocationPublication<'storage, 'scratch>, SqlError> {
        if self.progress.try_publish(self.storage, self.entries)? {
            Ok(HeapRelocationPublication::Published)
        } else {
            Ok(HeapRelocationPublication::More(self))
        }
    }
}

/// Owns startup scratch and preparation/copy controls across server turns.
pub(crate) struct HeapRelocationWorkspace {
    entries: FixedVec<HeapRelocationEntry>,
    progress: Option<HeapRelocationProgress>,
}

impl HeapRelocationWorkspace {
    pub(crate) fn budget_bytes(capacity: usize) -> usize {
        size_of::<Self>() + capacity.saturating_mul(size_of::<HeapRelocationEntry>())
    }

    pub(crate) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "heap relocation workspace controls")?;
        Ok(Self {
            entries: FixedVec::new(budget, "compact_scratch", capacity)?,
            progress: None,
        })
    }

    pub(crate) fn active(&self) -> bool {
        self.progress.is_some()
    }

    pub(crate) fn begin(&mut self, storage: &Storage) -> Result<(), SqlError> {
        if self.active() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation is already active"
            ));
        }
        self.progress = Some(storage.capture_heap_relocation(&mut self.entries)?);
        Ok(())
    }

    pub(crate) fn step(
        &mut self,
        storage: &Storage,
        budget: NonZeroUsize,
    ) -> Result<bool, SqlError> {
        let result = (|| {
            let progress = self.progress.as_mut().ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "heap relocation is not active")
            })?;
            if !progress.copy.belongs_to(&storage.heap) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "heap relocation belongs to another storage instance"
                ));
            }
            let mut remaining = budget.get();
            if !progress.prepare_step(&mut self.entries, &mut remaining)? {
                return Ok(false);
            }
            let Some(copy_budget) = NonZeroUsize::new(remaining) else {
                return Ok(false);
            };
            if !progress.copy_step(&self.entries, copy_budget)? {
                return Ok(false);
            }
            progress.try_publish(storage, &mut self.entries)
        })();
        if !matches!(&result, Ok(false)) {
            self.progress = None;
            self.entries.clear();
        }
        result
    }

    pub(crate) fn compact_exclusive(&mut self, storage: &mut Storage) -> Result<(), SqlError> {
        if self.active() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation is already active"
            ));
        }
        storage.compact_heap(&mut self.entries)
    }

    pub(crate) fn compact_step(
        &mut self,
        storage: &mut Storage,
        budget: NonZeroUsize,
        exclusive: bool,
    ) -> Result<bool, SqlError> {
        if !self.active() {
            if exclusive && !storage.heap.current_is_retained() {
                self.compact_exclusive(storage)?;
                return Ok(true);
            }
            self.begin(storage)?;
        }
        self.step(storage, budget)
    }
}

impl Storage {
    pub(crate) fn begin_heap_relocation<'storage, 'scratch>(
        &'storage self,
        entries: &'scratch mut FixedVec<HeapRelocationEntry>,
    ) -> Result<HeapRelocationJob<'storage, 'scratch>, SqlError> {
        let mut progress = self.capture_heap_relocation(entries)?;
        loop {
            let mut remaining = usize::MAX;
            if progress.prepare_step(entries, &mut remaining)? {
                break;
            }
        }
        Ok(HeapRelocationJob {
            storage: self,
            entries,
            progress,
        })
    }

    fn capture_heap_relocation(
        &self,
        entries: &mut FixedVec<HeapRelocationEntry>,
    ) -> Result<HeapRelocationProgress, SqlError> {
        let copy = {
            let versions = self.row_versions.write();
            let copy = self.heap.begin_copy()?;
            RelocationPlan::capture(&self.tables, &versions, entries)?;
            copy
        };
        Ok(HeapRelocationProgress {
            copy,
            preparation: HeapRelocationPreparation::Sorting(RelocationSort::new(entries.len())),
            packed_bytes: 0,
            entry: 0,
            entry_at: 0,
            copied_tail: 0,
        })
    }

    pub(crate) fn compact_heap_shared(
        &self,
        entries: &mut FixedVec<HeapRelocationEntry>,
    ) -> Result<(), SqlError> {
        let mut job = self.begin_heap_relocation(entries)?;
        loop {
            job.copy_step(NonZeroUsize::new(64 << 10).unwrap())?;
            match job.try_publish()? {
                HeapRelocationPublication::More(next) => job = next,
                HeapRelocationPublication::Published => return Ok(()),
            }
        }
    }

    /// Exclusive callers can reuse an unretained active buffer in place.
    /// Retained sources use the same private copy path as shared callers.
    pub(crate) fn compact_heap(
        &mut self,
        entries: &mut FixedVec<HeapRelocationEntry>,
    ) -> Result<(), SqlError> {
        if self.heap.current_is_retained() {
            return self.compact_heap_shared(entries);
        }
        let tables = &self.tables;
        let versions = self.row_versions.exclusive();
        self.heap.with_relocation(|heap| {
            let mut plan = RelocationPlan::capture(tables, versions, entries)?;
            plan.sort();
            heap.validate_relocation(plan.entries.iter().map(|entry| entry.source))?;
            let generation = heap.next_relocation_generation()?;
            plan.pack()?;
            plan.preflight(tables, versions, generation, None)?;
            let mut prior = None;
            for entry in plan.entries.iter() {
                if prior != Some(entry.source) {
                    heap.relocate(entry.source, entry.destination as usize);
                    prior = Some(entry.source);
                }
            }
            plan.apply(tables, versions, generation, None);
            Ok(heap.finish(plan.packed_bytes, generation))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize, RowLoc) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "heap_jobs",
                &[("id", ColType::Int4, false), ("note", ColType::Text, true)],
            ))
            .unwrap();
        storage.heap.append_bytes(b"unused prefix").unwrap();
        let original = storage
            .heap
            .append_row(&[Datum::Int4(10), Datum::Text("old")])
            .unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(original, 7))
            .unwrap();
        storage.set_lsn(7);
        (storage, table, original)
    }

    fn scratch(capacity: usize) -> FixedVec<HeapRelocationEntry> {
        FixedVec::new(&mut Budget::new(8192), "heap job test", capacity).unwrap()
    }

    fn workspace(capacity: usize) -> HeapRelocationWorkspace {
        let mut budget = Budget::new(HeapRelocationWorkspace::budget_bytes(capacity));
        let workspace = HeapRelocationWorkspace::new(&mut budget, capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        workspace
    }

    fn saved_handle(tag: u8) -> crate::store::SstHandle {
        crate::store::SstHandle {
            index: crate::store::BlockId([tag; 32]),
            filter: crate::store::BlockId([tag + 1; 32]),
            roster: crate::store::BlockId([tag + 2; 32]),
            format: crate::store::RowSstFormat::PackedV4,
        }
    }

    #[test]
    fn sliced_heap_cleanup_eviction_keeps_writes_after_installed_objects() {
        let (mut storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            for (rowid, lsn) in [(2, 11), (3, 13)] {
                let location = storage
                    .heap
                    .append_row(&[Datum::Int4(rowid as i32), Datum::Text("later")])
                    .unwrap();
                storage.tables[table]
                    .rows
                    .insert(rowid, RowState::committed_only_at(location, lsn))
                    .unwrap();
            }
            storage
                .append_spill_through(table, saved_handle(1), 7)
                .unwrap();
            storage
                .append_spill_through(table, saved_handle(5), 11)
                .unwrap();
            storage.evict_committed_table(table);
            for (rowid, member, lsn) in [(1, 0, 7), (2, 1, 11)] {
                let row = storage.resident_row_state(table, rowid).unwrap().unwrap();
                assert!(matches!(row.committed,
                    Some(RowHome::Spilled { sst, commit_lsn, .. }) if sst == member && commit_lsn == lsn));
            }
            assert_row(&storage, table, 3, 0, u64::MAX, 3, "later");
            let row = storage.resident_row_state(table, 3).unwrap().unwrap();
            assert!(matches!(row.committed, Some(RowHome::Heap(_))));
        });
    }

    #[test]
    fn sliced_heap_cleanup_recovered_roots_do_not_invent_eviction_coverage() {
        let (mut storage, table, original) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.set_spill_list(table, &[saved_handle(1)]);
            storage.evict_committed_table(table);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.committed, Some(RowHome::Heap(original)));
            drop(row);
            assert_row(&storage, table, 1, 0, 7, 10, "old");
            storage
                .append_spill_through(table, saved_handle(5), 9)
                .unwrap();
            storage.evict_committed_table(table);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert!(matches!(
                row.committed,
                Some(RowHome::Spilled {
                    sst: 1,
                    commit_lsn: 7,
                    ..
                })
            ));
        });
    }

    #[test]
    fn sliced_heap_cleanup_history_releases_only_published_versions() {
        let (mut storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            for (id, txid, lsn) in [(20, 8, 9), (30, 10, 11)] {
                let image = storage
                    .heap
                    .append_row_image(&[Datum::Int4(id), Datum::Text("later")])
                    .unwrap();
                storage
                    .write_pending_image_untracked(table, 1, txid, 1, Some(image))
                    .unwrap();
                storage.commit_row(table, 1, txid, lsn).unwrap();
            }
            storage.set_spill_list(table, &[saved_handle(1)]);
            storage.release_durable_histories();
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .len(),
                2
            );
            storage
                .append_spill_through(table, saved_handle(5), 7)
                .unwrap();
            storage.release_durable_histories();
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .len(),
                1
            );
            assert_row(&storage, table, 1, 99, 9, 20, "later");
            assert_row(&storage, table, 1, 0, u64::MAX, 30, "later");
            storage
                .append_spill_through(table, saved_handle(9), 11)
                .unwrap();
            storage.release_durable_histories();
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .is_empty()
            );
            storage.release_snapshot(99);
        });
    }

    fn add_preparation_rows(storage: &mut Storage, table: usize) {
        for rowid in 2..=64 {
            let source = storage
                .heap
                .append_row(&[Datum::Int4(rowid as i32), Datum::Text("kept")])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(rowid, RowState::committed_only_at(source, 7))
                .unwrap();
        }
    }

    #[test]
    fn sliced_heap_preparation_reads_and_late_writes_progress_between_sort_turns() {
        let (mut storage, table, original) = fixture();
        let other = storage
            .create_table(make_def(
                "during_preparation",
                &[("id", ColType::Int4, false), ("note", ColType::Text, true)],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| add_preparation_rows(&mut storage, table));
        let retained = storage.heap.get(original).unwrap();
        let mut workspace = workspace(128);
        crate::mem::guard::forbid_alloc(|| {
            workspace.begin(&storage).unwrap();
            assert!(!workspace.step(&storage, NonZeroUsize::MIN).unwrap());
            assert!(matches!(
                workspace.progress.as_ref().unwrap().preparation,
                HeapRelocationPreparation::Sorting(_)
            ));
            assert!(storage.row_versions.test_write().is_ok());
            assert!(matches!(
                storage.heap.test_exclusive_write(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            assert!(storage.heap.test_append_locks_available());
        });
        let undo = std::thread::scope(|scope| {
            let storage = &storage;
            scope
                .spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        assert_row(storage, table, 1, 0, 7, 10, "old");
                        let image = storage
                            .heap
                            .append_row_image(&[Datum::Int4(91), Datum::Text("late")])
                            .unwrap();
                        storage
                            .write_pending_image_untracked(other, 1, 77, 1, Some(image))
                            .unwrap()
                    })
                })
                .join()
                .unwrap()
        });
        crate::mem::guard::forbid_alloc(|| {
            let mut finished = false;
            for _ in 0..4096 {
                if workspace
                    .step(&storage, NonZeroUsize::new(5).unwrap())
                    .unwrap()
                {
                    finished = true;
                    break;
                }
                assert_row(&storage, table, 1, 0, 7, 10, "old");
            }
            assert!(finished, "preparation did not make bounded progress");
            for rowid in 2..=64 {
                assert_row(&storage, table, rowid, 0, 7, rowid as i32, "kept");
            }
            assert_row(&storage, other, 1, 77, u64::MAX, 91, "late");
            let mut values = [Datum::Null; 2];
            rowenc::decode(&retained, &[ColType::Int4, ColType::Text], &mut values).unwrap();
            assert_eq!(values, [Datum::Int4(10), Datum::Text("old")]);
            storage.restore_pending(other, 1, 77, undo);
        });
    }

    #[test]
    fn sliced_heap_preparation_cancellation_releases_sorting_buffers() {
        let (mut storage, table, original) = fixture();
        crate::mem::guard::forbid_alloc(|| add_preparation_rows(&mut storage, table));
        let retained = storage.heap.get(original).unwrap();
        let mut first = workspace(128);
        let mut second = workspace(128);
        crate::mem::guard::forbid_alloc(|| {
            first.begin(&storage).unwrap();
            assert!(!first.step(&storage, NonZeroUsize::MIN).unwrap());
            assert!(matches!(
                first.progress.as_ref().unwrap().preparation,
                HeapRelocationPreparation::Sorting(_)
            ));
            drop(first);
            second.begin(&storage).unwrap();
            while !second
                .step(&storage, NonZeroUsize::new(7).unwrap())
                .unwrap()
            {}
            assert!(!second.active());
            assert_row(&storage, table, 1, 0, 7, 10, "old");
            let mut values = [Datum::Null; 2];
            rowenc::decode(&retained, &[ColType::Int4, ColType::Text], &mut values).unwrap();
            assert_eq!(values, [Datum::Int4(10), Datum::Text("old")]);
        });
    }

    #[test]
    fn sliced_heap_preparation_overlap_across_zero_entries_releases_and_retries() {
        let (mut storage, table, original) = fixture();
        let retained = storage.heap.get(original).unwrap();
        let mut workspace = workspace(8);
        crate::mem::guard::forbid_alloc(|| {
            for (rowid, offset, len) in [(2, 1, 0), (3, 2, 1)] {
                storage.tables[table]
                    .rows
                    .insert(
                        rowid,
                        RowState::committed_only_at(
                            RowLoc {
                                offset: original.offset + offset,
                                len,
                                generation: original.generation,
                            },
                            7,
                        ),
                    )
                    .unwrap();
            }
            workspace.begin(&storage).unwrap();
            let mut error = None;
            for _ in 0..128 {
                match workspace.step(&storage, NonZeroUsize::MIN) {
                    Err(found) => {
                        error = Some(found);
                        break;
                    }
                    Ok(false) => {}
                    Ok(true) => panic!("overlapping rows were published"),
                }
            }
            let error = error.expect("overlap validation did not finish");
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert_eq!(
                error.message.as_str(),
                "heap compaction contains overlapping row locations"
            );
            assert!(!workspace.active());
            assert_eq!(location(&storage, table, 1, 0), original);
            assert_row(&storage, table, 1, 0, 7, 10, "old");
            for rowid in [2, 3] {
                storage.tables[table]
                    .rows
                    .insert(rowid, RowState::committed_only_at(original, 7))
                    .unwrap();
            }
            workspace.begin(&storage).unwrap();
            while !workspace.step(&storage, NonZeroUsize::MIN).unwrap() {}
            assert_eq!(storage.heap.used(), original.len as usize);
            for rowid in [1, 2, 3] {
                assert_row(&storage, table, rowid, 0, 7, 10, "old");
            }
            let mut values = [Datum::Null; 2];
            rowenc::decode(&retained, &[ColType::Int4, ColType::Text], &mut values).unwrap();
            assert_eq!(values, [Datum::Int4(10), Datum::Text("old")]);
        });
    }

    #[test]
    fn sliced_heap_cleanup_owned_workspace_allows_storage_moves_and_late_writes() {
        let (storage, table, original) = fixture();
        let retained = storage.heap.get(original).unwrap();
        let mut workspace = workspace(8);
        crate::mem::guard::forbid_alloc(|| workspace.begin(&storage)).unwrap();
        let mut storage = storage;
        crate::mem::guard::forbid_alloc(|| {
            assert!(!workspace.step(&storage, NonZeroUsize::MIN).unwrap());
            let error = workspace.compact_exclusive(&mut storage).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            let other = storage
                .create_table(make_def("late_table", &[("id", ColType::Int4, false)]))
                .unwrap();
            let image = storage.heap.append_row_image(&[Datum::Int4(91)]).unwrap();
            let undo = storage
                .write_pending_image_untracked(other, 1, 77, 1, Some(image))
                .unwrap();
            while !workspace
                .step(&storage, NonZeroUsize::new(3).unwrap())
                .unwrap()
            {}
            assert_row(&storage, table, 1, 0, 7, 10, "old");
            let row = storage.row_state(other, 1).unwrap().unwrap();
            let home = storage
                .visible_row_home_at(other, 1, row, 77, SNAPSHOT_ALL, u64::MAX)
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(other, 1, home, |bytes| {
                    let mut values = [Datum::Null];
                    rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert!(matches!(values, [Datum::Int4(91)]));
                    Ok(())
                })
                .unwrap();
            assert_eq!(retained.location(), original);
            let mut values = [Datum::Null; 2];
            rowenc::decode(&retained, &[ColType::Int4, ColType::Text], &mut values).unwrap();
            assert!(matches!(values, [Datum::Int4(10), Datum::Text("old")]));
            storage.restore_pending(other, 1, 77, undo);
        });
    }

    #[test]
    fn sliced_heap_cleanup_workspace_capacity_and_drop_release_capture() {
        let (storage, _, original) = fixture();
        let mut too_short = Budget::new(size_of::<HeapRelocationWorkspace>() - 1);
        assert!(HeapRelocationWorkspace::new(&mut too_short, 0).is_err());
        let mut empty = workspace(0);
        let mut first = workspace(8);
        let mut second = workspace(8);
        crate::mem::guard::forbid_alloc(|| {
            let error = empty.begin(&storage).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(!empty.active());
            first.begin(&storage).unwrap();
            let error = first.begin(&storage).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(first.active());
            assert!(!first.step(&storage, NonZeroUsize::MIN).unwrap());
            drop(first);
            assert_eq!(storage.heap.get(original).unwrap().location(), original);
            second.begin(&storage).unwrap();
            while !second
                .step(&storage, NonZeroUsize::new(7).unwrap())
                .unwrap()
            {}
        });
    }

    #[test]
    fn sliced_heap_cleanup_wrong_instance_releases_an_orphaned_copy() {
        let (storage, _, original) = fixture();
        let retained = storage.heap.get(original).unwrap();
        let mut workspace = workspace(8);
        crate::mem::guard::forbid_alloc(|| workspace.begin(&storage)).unwrap();
        drop(storage);
        let (replacement, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let error = workspace.step(&replacement, NonZeroUsize::MIN).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert!(!workspace.active());
            let mut values = [Datum::Null; 2];
            rowenc::decode(&retained, &[ColType::Int4, ColType::Text], &mut values).unwrap();
            assert!(matches!(values, [Datum::Int4(10), Datum::Text("old")]));
            workspace.begin(&replacement).unwrap();
            while !workspace
                .step(&replacement, NonZeroUsize::new(7).unwrap())
                .unwrap()
            {}
            assert_row(&replacement, table, 1, 0, 7, 10, "old");
        });
    }

    #[test]
    fn sliced_heap_cleanup_copy_can_resume_on_another_thread() {
        let (storage, table, _) = fixture();
        let mut workspace = workspace(8);
        crate::mem::guard::forbid_alloc(|| {
            workspace.begin(&storage).unwrap();
            assert!(!workspace.step(&storage, NonZeroUsize::MIN).unwrap());
        });
        let storage_ref = &storage;
        let workspace = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        while !workspace
                            .step(storage_ref, NonZeroUsize::new(3).unwrap())
                            .unwrap()
                        {}
                    });
                    workspace
                })
                .join()
                .unwrap()
        });
        assert!(!workspace.active());
        assert_row(&storage, table, 1, 0, 7, 10, "old");
    }

    fn finish(mut job: HeapRelocationJob<'_, '_>) {
        loop {
            job.copy_step(NonZeroUsize::new(7).unwrap()).unwrap();
            match job.try_publish().unwrap() {
                HeapRelocationPublication::More(next) => job = next,
                HeapRelocationPublication::Published => return,
            }
        }
    }

    fn assert_row(
        storage: &Storage,
        table: usize,
        rowid: u64,
        txid: u32,
        snapshot: u64,
        id: i32,
        note: &str,
    ) {
        let row = storage.resident_row_state(table, rowid).unwrap().unwrap();
        let home = storage
            .visible_row_home_at(table, rowid, row, txid, SNAPSHOT_ALL, snapshot)
            .unwrap()
            .unwrap();
        storage
            .with_row_bytes(table, rowid, home, |bytes| {
                let mut values = [Datum::Null; 2];
                rowenc::decode(bytes, &[ColType::Int4, ColType::Text], &mut values)?;
                assert_eq!(values, [Datum::Int4(id), Datum::Text(note)]);
                Ok(())
            })
            .unwrap();
    }

    fn location(storage: &Storage, table: usize, rowid: u64, txid: u32) -> RowLoc {
        let row = storage.resident_row_state(table, rowid).unwrap().unwrap();
        let Some(Some(RowHome::Heap(location))) = row.visible_at(txid, SNAPSHOT_ALL, u64::MAX)
        else {
            panic!("heap fixture")
        };
        location
    }

    #[test]
    fn shared_heap_relocation_private_copy_allows_readers_and_unrelated_writes() {
        let (mut storage, table, original) = fixture();
        let other = storage
            .create_table(make_def(
                "heap_job_other",
                &[("id", ColType::Int4, false), ("note", ColType::Text, true)],
            ))
            .unwrap();
        let mut entries = scratch(16);
        let reader = storage.heap.get(original).unwrap();
        let mut job = storage.begin_heap_relocation(&mut entries).unwrap();
        assert!(!crate::mem::guard::forbid_alloc(|| job.copy_step(NonZeroUsize::MIN)).unwrap());
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    assert_row(&storage, table, 1, 8, 7, 10, "old");
                    let image = storage
                        .heap
                        .append_row_image(&[Datum::Int4(20), Datum::Text("other")])
                        .unwrap();
                    let undo = storage
                        .write_pending_image(other, 1, 8, 1, Some(image))
                        .unwrap();
                    done.store(true, Ordering::Release);
                    undo
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let progressed = done.load(Ordering::Acquire);
            if !progressed {
                drop(job);
                writer.join().unwrap();
                panic!("private heap copying blocked an unrelated reader or writer");
            }
            let undo = writer.join().unwrap();
            crate::mem::guard::forbid_alloc(|| {
                finish(job);
                assert_row(&storage, table, 1, 8, 7, 10, "old");
                assert_row(&storage, other, 1, 8, u64::MAX, 20, "other");
                assert_ne!(
                    location(&storage, table, 1, 0).generation,
                    original.generation
                );
                assert_eq!(
                    &*reader,
                    &*storage.heap.get(location(&storage, table, 1, 0)).unwrap()
                );
                storage.restore_pending(other, 1, 8, undo);
            });
        });
    }

    #[test]
    fn shared_heap_relocation_unpublished_prefix_input_is_rehomed_into_the_tail() {
        let (storage, table, _) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let input = storage
                .heap
                .append_row_image(&[Datum::Int4(30), Datum::Text("prepared")])
                .unwrap();
            let prior = input.location();
            let job = storage.begin_heap_relocation(&mut entries).unwrap();
            let frontier = job.progress.copy.frontier();
            let undo = storage
                .write_pending_image(table, 2, 8, 1, Some(input))
                .unwrap();
            let current = location(&storage, table, 2, 8);
            assert_ne!(current, prior);
            assert!(current.offset as usize >= frontier);
            finish(job);
            assert_row(&storage, table, 2, 8, u64::MAX, 30, "prepared");
            storage.restore_pending(table, 2, 8, undo);
        });
    }

    #[test]
    fn shared_heap_relocation_write_comparison_retains_both_heap_allocations() {
        let (mut storage, table, _) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let committed = storage.committed_row_home(table, 1, row).unwrap().unwrap();
            storage.compact_heap(&mut entries).unwrap();
            let next = storage
                .heap
                .append_row_image(&[Datum::Int4(10), Datum::Text("new")])
                .unwrap();
            let changed = storage
                .pending_change_footprint(table, 1, committed, &next)
                .unwrap();
            assert!(!changed.contains(0));
            assert!(changed.contains(1));
            drop(next);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let committed = storage.committed_row_home(table, 1, row).unwrap().unwrap();
            storage.compact_heap(&mut entries).unwrap();
            let invalid = storage.heap.append_image(b"").unwrap();
            assert!(
                storage
                    .pending_change_footprint(table, 1, committed, &invalid)
                    .is_err()
            );
            // A failed comparison must release its old-buffer owner.
            storage.compact_heap(&mut entries).unwrap();
            drop(invalid);
            assert_row(&storage, table, 1, 8, 7, 10, "old");
        });
    }

    #[test]
    fn shared_heap_relocation_new_tail_requires_another_private_copy_turn() {
        let (storage, table, original) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let mut job = storage.begin_heap_relocation(&mut entries).unwrap();
            assert!(job.copy_step(NonZeroUsize::new(4096).unwrap()).unwrap());
            let input = storage
                .heap
                .append_row_image(&[Datum::Int4(40), Datum::Text("late")])
                .unwrap();
            let undo = storage
                .write_pending_image(table, 2, 8, 1, Some(input))
                .unwrap();
            let HeapRelocationPublication::More(job) = job.try_publish().unwrap() else {
                panic!("a new tail cannot be published before its bytes are copied")
            };
            assert_eq!(location(&storage, table, 1, 0), original);
            assert_row(&storage, table, 2, 8, u64::MAX, 40, "late");
            finish(job);
            assert_row(&storage, table, 2, 8, u64::MAX, 40, "late");
            storage.restore_pending(table, 2, 8, undo);
        });
    }

    #[test]
    fn shared_heap_relocation_cancellation_and_scratch_failure_release_capture() {
        let (storage, table, original) = fixture();
        let mut entries = scratch(16);
        let mut other = scratch(16);
        let mut empty = scratch(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut job = storage.begin_heap_relocation(&mut entries).unwrap();
            job.copy_step(NonZeroUsize::MIN).unwrap();
            let error = storage.begin_heap_relocation(&mut other).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "row heap relocation is already active"
            );
            drop(job);
            assert_eq!(location(&storage, table, 1, 0), original);
            assert!(storage.heap.test_exclusive_write().is_ok());
            assert!(storage.row_versions.test_write().is_ok());
            let image = storage
                .heap
                .append_row_image(&[Datum::Int4(50), Datum::Text("retry")])
                .unwrap();
            let used = storage.heap.used();
            let error = storage.begin_heap_relocation(&mut empty).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            let undo = storage
                .write_pending_image(table, 2, 8, 1, Some(image))
                .unwrap();
            assert_eq!(
                storage.heap.used(),
                used,
                "failed capture left its rehome boundary active"
            );
            finish(storage.begin_heap_relocation(&mut entries).unwrap());
            assert_row(&storage, table, 2, 8, u64::MAX, 50, "retry");
            storage.restore_pending(table, 2, 8, undo);
        });
    }

    #[test]
    fn shared_heap_relocation_two_retained_buffers_fail_and_resume_without_growth() {
        let (storage, table, original) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.heap.get(original).unwrap();
            finish(storage.begin_heap_relocation(&mut entries).unwrap());
            let current = storage.heap.get(location(&storage, table, 1, 0)).unwrap();
            let used = storage.heap.used();
            let capacity = storage.heap.capacity();
            let error = storage.begin_heap_relocation(&mut entries).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.heap.used(), used);
            assert_eq!(storage.heap.capacity(), capacity);
            assert_eq!(&*old, &*current);
            drop(old);
            finish(storage.begin_heap_relocation(&mut entries).unwrap());
            assert_row(&storage, table, 1, 8, 7, 10, "old");
            assert_eq!(
                &*current,
                &*storage.heap.get(location(&storage, table, 1, 0)).unwrap()
            );
        });
    }

    #[test]
    fn shared_heap_relocation_retirement_and_slot_reuse_preserve_new_rows() {
        let (storage, table, original) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.heap.get(original).unwrap();
            let stamp = storage.tables[table].created_at();
            let mut job = storage.begin_heap_relocation(&mut entries).unwrap();
            job.copy_step(NonZeroUsize::MIN).unwrap();
            storage.commit_drop(table);
            let replacement = storage
                .create_table_in(
                    make_def("heap_replacement", &[("active", ColType::Bool, false)]),
                    8,
                )
                .unwrap();
            assert_eq!(replacement, table);
            assert_ne!(storage.tables[table].created_at(), stamp);
            let image = storage.heap.append_row_image(&[Datum::Bool(true)]).unwrap();
            let undo = storage
                .write_pending_image(table, 1, 8, 1, Some(image))
                .unwrap();
            finish(job);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let home = storage
                .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, u64::MAX)
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, home, |bytes| {
                    let mut value = [Datum::Null; 1];
                    rowenc::decode(bytes, &[ColType::Bool], &mut value)?;
                    assert_eq!(value, [Datum::Bool(true)]);
                    Ok(())
                })
                .unwrap();
            let mut original_values = [Datum::Null; 2];
            rowenc::decode(&old, &[ColType::Int4, ColType::Text], &mut original_values).unwrap();
            assert_eq!(original_values, [Datum::Int4(10), Datum::Text("old")]);
            storage.restore_pending(table, 1, 8, undo);
            storage.rollback_create(table);
        });
    }

    #[test]
    fn shared_heap_relocation_aliases_history_and_reused_pending_slots_survive() {
        let (storage, table, original) = fixture();
        let mut entries = scratch(16);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.heap.get(original).unwrap();
            storage.register_snapshot(9, 7).unwrap();
            let image = storage
                .heap
                .append_row_image(&[Datum::Int4(20), Datum::Text("committed")])
                .unwrap();
            storage
                .write_pending_image_untracked(table, 1, 8, 1, Some(image))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            let image = storage
                .heap
                .append_row_image(&[Datum::Int4(30), Datum::Text("pending")])
                .unwrap();
            let second = storage
                .write_pending_image_untracked(table, 1, 8, 2, Some(image.clone()))
                .unwrap();
            let third = storage
                .write_pending_image_untracked(table, 1, 8, 3, Some(image))
                .unwrap();
            let mut job = storage.begin_heap_relocation(&mut entries).unwrap();
            job.copy_step(NonZeroUsize::MIN).unwrap();
            storage.restore_pending(table, 1, 8, third);
            let image = storage
                .heap
                .append_row_image(&[Datum::Int4(40), Datum::Text("new slot")])
                .unwrap();
            let fourth = storage
                .write_pending_image_untracked(table, 1, 8, 4, Some(image))
                .unwrap();
            finish(job);
            assert_row(&storage, table, 1, 9, 7, 10, "old");
            assert_row(&storage, table, 1, 8, u64::MAX, 40, "new slot");
            storage.restore_pending(table, 1, 8, fourth);
            assert_row(&storage, table, 1, 8, u64::MAX, 30, "pending");
            storage.restore_pending(table, 1, 8, second);
            assert_row(&storage, table, 1, 8, u64::MAX, 20, "committed");
            let mut original_values = [Datum::Null; 2];
            rowenc::decode(&old, &[ColType::Int4, ColType::Text], &mut original_values).unwrap();
            assert_eq!(original_values, [Datum::Int4(10), Datum::Text("old")]);
            storage.release_snapshot(9);
        });
    }

    #[test]
    fn shared_heap_relocation_does_not_wait_for_definition_owners() {
        let (mut storage, table, _) = fixture();
        let other = storage
            .create_table(make_def(
                "heap_definition_other",
                &[("id", ColType::Int4, false), ("note", ColType::Text, true)],
            ))
            .unwrap();
        let mut entries = scratch(16);
        let definition = storage.tables[table].definition.write();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let job = storage.begin_heap_relocation(&mut entries).unwrap();
                    let image = storage
                        .heap
                        .append_row_image(&[Datum::Int4(60), Datum::Text("other")])
                        .unwrap();
                    let undo = storage
                        .write_pending_image(other, 1, 8, 1, Some(image))
                        .unwrap();
                    finish(job);
                    storage.restore_pending(other, 1, 8, undo);
                    done.store(true, Ordering::Release);
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let progressed = done.load(Ordering::Acquire);
            drop(definition);
            worker.join().unwrap();
            assert!(
                progressed,
                "heap relocation waited for an unrelated definition owner"
            );
        });
    }
}
