//! Bounded heap copy plans; row versions coordinate capture and publication.

use core::num::NonZeroUsize;

use super::row_heap::HeapCopy;
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
    copy: HeapCopy<'storage>,
    plan: RelocationPlan<'scratch>,
    entry: usize,
    entry_at: usize,
    copied_tail: usize,
}

pub(crate) enum HeapRelocationPublication<'storage, 'scratch> {
    More(HeapRelocationJob<'storage, 'scratch>),
    Published,
}

impl HeapRelocationJob<'_, '_> {
    /// Work is bounded by bytes and by zero-length or aliased entry visits.
    pub(crate) fn copy_step(&mut self, budget: NonZeroUsize) -> Result<bool, SqlError> {
        let mut remaining = budget.get();
        while self.entry < self.plan.entries.len() && remaining != 0 {
            let entry = self.plan.entries[self.entry];
            if (self.entry > 0
                && self.entry_at == 0
                && self.plan.entries[self.entry - 1].source == entry.source)
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
        if self.entry != self.plan.entries.len() {
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
                .copy_bytes(source, 0, self.plan.packed_bytes + self.copied_tail, count)?;
            self.copied_tail += count;
        }
        Ok(self.copied_tail == tail)
    }
}

impl<'storage, 'scratch> HeapRelocationJob<'storage, 'scratch> {
    pub(crate) fn try_publish(
        mut self,
    ) -> Result<HeapRelocationPublication<'storage, 'scratch>, SqlError> {
        if self.entry != self.plan.entries.len() {
            return Ok(HeapRelocationPublication::More(self));
        }
        let published = {
            let mut versions = self.storage.row_versions.write();
            let tail = Some((self.copy.frontier(), self.copied_tail));
            self.copy
                .try_publish(self.plan.packed_bytes, self.copied_tail, |generation| {
                    self.plan
                        .preflight(&self.storage.tables, &versions, generation, tail)?;
                    self.plan
                        .apply(&self.storage.tables, &mut versions, generation, tail);
                    Ok(())
                })?
        };
        if published {
            Ok(HeapRelocationPublication::Published)
        } else {
            Ok(HeapRelocationPublication::More(self))
        }
    }
}

impl Storage {
    pub(crate) fn begin_heap_relocation<'storage, 'scratch>(
        &'storage self,
        entries: &'scratch mut FixedVec<HeapRelocationEntry>,
    ) -> Result<HeapRelocationJob<'storage, 'scratch>, SqlError> {
        let (copy, mut plan) = {
            let versions = self.row_versions.write();
            let copy = self.heap.begin_copy()?;
            let plan = RelocationPlan::capture(&self.tables, &versions, entries)?;
            (copy, plan)
        };
        plan.sort();
        copy.validate_locations(plan.entries.iter().map(|entry| entry.source))?;
        if plan
            .entries
            .iter()
            .any(|entry| entry.source.offset as usize + entry.source.len as usize > copy.frontier())
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "heap relocation row exceeds its captured prefix"
            ));
        }
        plan.pack()?;
        Ok(HeapRelocationJob {
            storage: self,
            copy,
            plan,
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
            let frontier = job.copy.frontier();
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
            assert!(storage.heap.test_write().is_ok());
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
