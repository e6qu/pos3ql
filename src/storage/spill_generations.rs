//! Owned immutable SST roots retained until their readers release them.

use core::mem::size_of;
use core::num::NonZeroUsize;
use core::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{ColType, MAX_COLUMNS, RelationPersistence, SpillReader, Storage, table_slot_capacity};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::store::SstHandle;

/// Handles and their eviction boundaries are published as one complete list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpillGeneration {
    pub(crate) handle: SstHandle,
    /// Cold recovery leaves this zero: recovered rows already name their exact member.
    pub(crate) through_lsn: u64,
}

pub(crate) struct SpillGenerationList {
    members: FixedVec<SpillGeneration>,
    staging: FixedVec<SpillGeneration>,
}

impl SpillGenerationList {
    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.members.capacity()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &SpillGeneration> {
        self.members.iter()
    }

    pub(crate) fn handle(&self, index: usize) -> Option<SstHandle> {
        self.members.get(index).map(|member| member.handle)
    }

    pub(super) fn clear(&mut self) {
        self.members.clear();
        self.staging.clear();
    }

    pub(super) fn replace(
        &mut self,
        len: usize,
        handles: impl Iterator<Item = SstHandle>,
    ) -> Result<(), SqlError> {
        self.staging.clear();
        if len > self.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table SST generations exceed startup capacity ({})",
                self.capacity()
            ));
        }
        for handle in handles {
            if self.staging.len() == len {
                self.staging.clear();
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SST generation iterator exceeds declared length"
                ));
            }
            self.staging
                .push(SpillGeneration {
                    handle,
                    through_lsn: 0,
                })
                .expect("validated generation capacity");
        }
        if self.staging.len() != len {
            self.staging.clear();
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "SST generation iterator is shorter than declared length"
            ));
        }
        std::mem::swap(&mut self.members, &mut self.staging);
        self.staging.clear();
        Ok(())
    }

    pub(super) fn copy_from(&mut self, source: &Self) -> Result<(), SqlError> {
        if source.len() > self.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cloned SST generations exceed startup capacity ({})",
                self.capacity()
            ));
        }
        self.clear();
        for &member in source.iter() {
            self.members.push(member).expect("validated clone capacity");
        }
        Ok(())
    }

    pub(super) fn append(&mut self, handle: SstHandle, through_lsn: u64) -> Result<(), SqlError> {
        self.members
            .push(SpillGeneration {
                handle,
                through_lsn,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table SST generations exceed startup capacity ({})",
                    self.capacity()
                )
            })
    }

    pub(super) fn collapse(&mut self, handle: SstHandle, through_lsn: u64) -> Result<(), SqlError> {
        if self.capacity() == 0 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table SST generations exceed startup capacity (0)"
            ));
        }
        self.clear();
        self.append(handle, through_lsn)
    }

    pub(super) fn merge_pair(&mut self, at: usize, handle: Option<SstHandle>) {
        assert!(
            at.checked_add(1).is_some_and(|next| next < self.len()),
            "merge pair is outside the installed generation list"
        );
        let through_lsn = self.members[at]
            .through_lsn
            .max(self.members[at + 1].through_lsn);
        let removed = if let Some(handle) = handle {
            self.members[at] = SpillGeneration {
                handle,
                through_lsn,
            };
            1
        } else {
            2
        };
        let destination = at + 2 - removed;
        self.members
            .as_mut_slice()
            .copy_within(at + 2.., destination);
        for _ in 0..removed {
            self.members.pop();
        }
    }
}

pub(super) struct SpillGenerationRoster {
    state: RwLock<SpillGenerationList>,
}

impl SpillGenerationRoster {
    pub(super) fn member_budget_bytes(capacity: usize) -> usize {
        2 * capacity * size_of::<SpillGeneration>()
    }

    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            state: RwLock::new(SpillGenerationList {
                members: FixedVec::new(budget, "table SST generations", capacity)?,
                staging: FixedVec::new(budget, "table SST generation staging", capacity)?,
            }),
        })
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, SpillGenerationList> {
        self.state
            .read()
            .expect("table SST generation lock poisoned")
    }

    pub(super) fn write(&self) -> RwLockWriteGuard<'_, SpillGenerationList> {
        self.state
            .write()
            .expect("table SST generation lock poisoned")
    }
}

struct PinnedRoot {
    handle: SstHandle,
    durable: bool,
    readers: NonZeroUsize,
    garbage_retained: bool,
}

pub(super) struct SpillRootRegistry {
    roots: Mutex<FixedVec<PinnedRoot>>,
    reclamation_pending: AtomicBool,
}

impl SpillRootRegistry {
    pub(super) fn capacity(config: &Config) -> usize {
        table_slot_capacity(config)
            .saturating_mul(config.max_spill_generations_per_table)
            .saturating_mul(2)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<std::sync::atomic::AtomicUsize>()
            + Self::capacity(config) * size_of::<PinnedRoot>()
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(),
            "retained SST root owner",
        )?;
        let roots = FixedVec::new(budget, "retained SST roots", Self::capacity(config))?;
        Ok(Arc::new(Self {
            roots: Mutex::new(roots),
            reclamation_pending: AtomicBool::new(false),
        }))
    }

    fn pin(&self, handles: &[SstHandle], durable: bool) -> Result<(), SqlError> {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        let mut needed = 0;
        for (index, handle) in handles.iter().enumerate() {
            if !roots
                .iter()
                .any(|root| root.handle == *handle && root.durable == durable)
                && !handles[..index].contains(handle)
            {
                needed += 1;
            }
        }
        for root in roots.iter() {
            let count = handles
                .iter()
                .filter(|handle| **handle == root.handle && root.durable == durable)
                .count();
            if root.readers.checked_add(count).is_none() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained SST reader count exhausted"
                ));
            }
        }
        if needed > roots.capacity() - roots.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST roots exceed the startup capacity ({})",
                roots.capacity()
            ));
        }
        for handle in handles {
            if let Some(root) = roots
                .iter_mut()
                .find(|root| root.handle == *handle && root.durable == durable)
            {
                root.readers = root
                    .readers
                    .checked_add(1)
                    .expect("preflighted reader count");
            } else {
                roots
                    .push(PinnedRoot {
                        handle: *handle,
                        durable,
                        readers: NonZeroUsize::new(1).expect("nonzero"),
                        garbage_retained: false,
                    })
                    .expect("preflighted root capacity");
            }
        }
        Ok(())
    }

    fn release(&self, handles: &[SstHandle], durable: bool) {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        for handle in handles {
            let position = roots
                .iter()
                .position(|root| root.handle == *handle && root.durable == durable)
                .expect("reader retains its root");
            if let Some(remaining) = NonZeroUsize::new(roots[position].readers.get() - 1) {
                roots[position].readers = remaining;
            } else {
                let retired = roots.swap_remove(position);
                if retired.durable && retired.garbage_retained {
                    self.reclamation_pending.store(true, Ordering::Release);
                }
            }
        }
    }

    pub(super) fn copy_roots(
        &self,
        durable: bool,
        output: &mut Vec<SstHandle>,
    ) -> Result<(), SqlError> {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        output.clear();
        let count = roots.iter().filter(|root| root.durable == durable).count();
        if count > output.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST root snapshot exceeds garbage collection scratch ({})",
                output.capacity()
            ));
        }
        for root in roots.iter_mut().filter(|root| root.durable == durable) {
            root.garbage_retained = true;
            output.push(root.handle);
        }
        Ok(())
    }
    pub(super) fn reclamation_pending(&self) -> bool {
        self.reclamation_pending.load(Ordering::Acquire)
    }
    pub(super) fn take_reclamation_pending(&self) -> bool {
        self.reclamation_pending.swap(false, Ordering::AcqRel)
    }
}

pub(super) struct SpillRelation<'a> {
    pub(super) persistence: RelationPersistence,
    pub(super) schema: [ColType; MAX_COLUMNS],
    pub(super) n_columns: usize,
    pub(super) handles: &'a [SstHandle],
}

impl<'a> SpillRelation<'a> {
    pub(super) fn len(&self) -> usize {
        self.handles.len()
    }

    pub(super) fn handle(&self, member: usize) -> SstHandle {
        self.handles[member]
    }
}

struct QueryRootSlot {
    retained: AtomicBool,
    readers: AtomicUsize,
    snapshot: RwLock<SpillGenerationSnapshot>,
}

pub(super) struct QueryRootPool {
    slots: FixedVec<QueryRootSlot>,
    slots_per_use: usize,
}

pub(super) struct QueryRootLease {
    pool: Arc<QueryRootPool>,
    slot: usize,
}

impl QueryRootPool {
    fn slot_count(config: &Config) -> usize {
        config
            .query_workspace_slots
            .saturating_mul(super::MAX_ROW_WALK_NESTING)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<std::sync::atomic::AtomicUsize>()
            + 2 * Self::slot_count(config)
                * (size_of::<QueryRootSlot>()
                    + SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(),
            "query SST root owner",
        )?;
        let count = Self::slot_count(config);
        let mut slots = FixedVec::new(budget, "query SST root slots", 2 * count)?;
        for _ in 0..2 * count {
            budget.draw(
                SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table),
                "query SST root members",
            )?;
            slots
                .push(QueryRootSlot {
                    retained: AtomicBool::new(false),
                    readers: AtomicUsize::new(0),
                    snapshot: RwLock::new(SpillGenerationSnapshot::new(
                        config.max_spill_generations_per_table,
                    )),
                })
                .expect("sized query root slots");
        }
        Ok(Arc::new(Self {
            slots,
            slots_per_use: count,
        }))
    }

    pub(super) fn retain(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
    ) -> Result<QueryRootLease, SqlError> {
        self.retain_in(storage, table, lsn, 0)
    }

    pub(super) fn retain_point(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
    ) -> Result<QueryRootLease, SqlError> {
        self.retain_in(storage, table, lsn, self.slots_per_use)
    }

    fn retain_in(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
        start: usize,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(start, |snapshot| {
            snapshot.capture(storage, table)?;
            snapshot.commit_lsn = lsn;
            Ok(())
        })
    }

    fn retain_image(
        self: &Arc<Self>,
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(0, |snapshot| {
            snapshot.capture_image(storage, image, generations)
        })
    }

    fn retain_source(
        self: &Arc<Self>,
        source: &SpillSourceView<'_>,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(self.slots_per_use, |snapshot| snapshot.copy_source(source))
    }

    fn reserve_in(
        self: &Arc<Self>,
        start: usize,
        capture: impl FnOnce(&mut SpillGenerationSnapshot) -> Result<(), SqlError>,
    ) -> Result<QueryRootLease, SqlError> {
        for slot in start..start + self.slots_per_use {
            let candidate = &self.slots[slot];
            if candidate
                .retained
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            candidate.readers.store(1, Ordering::Relaxed);
            let lease = QueryRootLease {
                pool: Arc::clone(self),
                slot,
            };
            {
                let mut snapshot = candidate
                    .snapshot
                    .write()
                    .expect("query SST roots poisoned");
                capture(&mut snapshot)?;
            }
            return Ok(lease);
        }
        Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "query SST root slots exhausted ({})",
            self.slots_per_use
        ))
    }
}

impl QueryRootLease {
    pub(super) fn try_clone(&self) -> Result<Self, SqlError> {
        let readers = &self.pool.slots[self.slot].readers;
        let mut count = readers.load(Ordering::Relaxed);
        loop {
            let next = count.checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "query SST source reader count exhausted"
                )
            })?;
            match readers.compare_exchange_weak(count, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => count = current,
            }
        }
        Ok(Self {
            pool: Arc::clone(&self.pool),
            slot: self.slot,
        })
    }

    /// Only this source and its clones own the slot. Its guard excludes no unrelated reader,
    /// publisher, or retirement; registry ownership ends before object I/O.
    pub(super) fn with_snapshot<T>(
        &self,
        read: impl FnOnce(&SpillGenerationSnapshot) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        let snapshot = self.pool.slots[self.slot]
            .snapshot
            .read()
            .expect("query SST roots poisoned");
        read(&snapshot)
    }
}

impl Drop for QueryRootLease {
    fn drop(&mut self) {
        let slot = &self.pool.slots[self.slot];
        if slot.readers.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        slot.snapshot
            .write()
            .expect("query SST roots poisoned")
            .clear();
        slot.retained.store(false, Ordering::Release);
    }
}

/// A point verdict includes deletions so an older reader can still select
/// historical bytes from the same captured generation set.
pub(super) struct SpilledRowState {
    roots: QueryRootLease,
    rowid: u64,
    version: super::SpillVersion,
}

impl SpilledRowState {
    pub(super) fn new(roots: QueryRootLease, rowid: u64, version: super::SpillVersion) -> Self {
        Self {
            roots,
            rowid,
            version,
        }
    }

    pub(super) fn state(&self) -> super::RowState {
        super::RowState {
            committed: self.version.len.map(|len| super::RowHome::Spilled {
                len,
                sst: self.version.member,
                commit_lsn: self.version.commit_lsn,
            }),
            committed_lsn: self.version.commit_lsn,
            checkpoint_change_lsn: 0,
            history: super::CommittedHistory::empty(),
            pending: super::PendingVersions::empty(),
        }
    }

    pub(super) fn validate_request(&self, table: usize, rowid: u64) -> Result<(), SqlError> {
        self.roots.with_snapshot(|source| {
            if source.table_slot != table || self.rowid != rowid {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object state does not match the requested table and row"
                ));
            }
            Ok(())
        })
    }

    pub(super) fn visible_at(
        &self,
        commit_snapshot: u64,
    ) -> Result<Option<SpilledRowRead>, SqlError> {
        let version = self.roots.with_snapshot(|source| {
            if self.version.commit_lsn <= commit_snapshot && commit_snapshot <= source.commit_lsn {
                Ok(Some(self.version))
            } else {
                Storage::probe_spill_snapshot(source, self.rowid, commit_snapshot)
            }
        })?;
        let Some(super::SpillVersion {
            len: Some(len),
            member,
            commit_lsn,
        }) = version
        else {
            return Ok(None);
        };
        SpilledRowRead::new(self.roots.try_clone()?, self.rowid, len, member, commit_lsn).map(Some)
    }
}

/// The selected version and its complete immutable source share one lease.
/// Older snapshot probes never consult a replacement table's generation list.
pub(super) struct SpilledRowRead {
    roots: QueryRootLease,
    pub(super) table_slot: usize,
    pub(super) created_at: u64,
    pub(super) rowid: u64,
    pub(super) len: u32,
    pub(super) member: u32,
    pub(super) commit_lsn: u64,
}

impl SpilledRowRead {
    pub(super) fn new(
        roots: QueryRootLease,
        rowid: u64,
        len: u32,
        member: u32,
        commit_lsn: u64,
    ) -> Result<Self, SqlError> {
        let (table_slot, created_at) =
            roots.with_snapshot(|source| Ok((source.table_slot, source.created_at)))?;
        Ok(Self {
            roots,
            table_slot,
            created_at,
            rowid,
            len,
            member,
            commit_lsn,
        })
    }

    pub(super) fn snapshot(&self) -> super::RowSnapshot {
        super::RowSnapshot {
            created_at: self.created_at,
            version: super::row_images::RowVersionIdentity::Committed(self.commit_lsn),
        }
    }

    pub(super) fn validate_request(&self, table: usize, rowid: u64) -> Result<(), SqlError> {
        if table != self.table_slot || rowid != self.rowid {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained object row does not match the requested table and row"
            ));
        }
        Ok(())
    }

    pub(super) fn copy_bytes<'arena>(
        &self,
        arena: &'arena crate::mem::arena::Arena,
    ) -> Result<&'arena [u8], SqlError> {
        self.roots.with_snapshot(|source| {
            let reader = source.reader.as_ref().ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no reader"
                )
            })?;
            let handle = source.handles.get(self.member as usize).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no SST member"
                )
            })?;
            reader.copy_row_bytes(
                source.persistence,
                *handle,
                self.rowid,
                self.len,
                self.commit_lsn,
                arena,
            )
        })
    }

    pub(super) fn with_bytes<R>(
        &self,
        consume: impl FnOnce(&[u8]) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        self.roots.with_snapshot(|source| {
            let reader = source.reader.as_ref().ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no reader"
                )
            })?;
            let handle = source.handles.get(self.member as usize).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no SST member"
                )
            })?;
            reader.with_row_bytes(
                source.persistence,
                *handle,
                self.rowid,
                self.len,
                self.commit_lsn,
                consume,
            )
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct SpillGenerationImage {
    persistence: RelationPersistence,
    schema: [ColType; MAX_COLUMNS],
    n_columns: usize,
    table_slot: usize,
    pub(super) created_at: u64,
    data_generation: u64,
    pub(super) commit_lsn: u64,
}

impl SpillGenerationImage {
    const EMPTY: Self = Self {
        persistence: RelationPersistence::Permanent,
        schema: [ColType::Bool; MAX_COLUMNS],
        n_columns: 0,
        table_slot: 0,
        created_at: 0,
        data_generation: 0,
        commit_lsn: 0,
    };

    pub(super) fn capture(
        definition: &super::table_definitions::DefinitionState,
        generation: u64,
        table_slot: usize,
        commit_lsn: u64,
    ) -> Self {
        let mut image = Self::EMPTY;
        image.persistence = definition.committed.persistence;
        image.n_columns = definition.committed.schema(&mut image.schema);
        image.table_slot = table_slot;
        image.created_at = definition.identity.created_at;
        image.data_generation = generation;
        image.commit_lsn = commit_lsn;
        image
    }
}

pub(super) struct SpillSourceView<'a> {
    registry: &'a Arc<SpillRootRegistry>,
    pub(super) reader: Option<&'a Arc<SpillReader>>,
    handles: &'a [SstHandle],
    image: &'a SpillGenerationImage,
}

impl SpillSourceView<'_> {
    pub(super) fn relation(&self) -> SpillRelation<'_> {
        SpillRelation {
            persistence: self.image.persistence,
            schema: self.image.schema,
            n_columns: self.image.n_columns,
            handles: self.handles,
        }
    }

    pub(super) fn commit_lsn(&self) -> u64 {
        self.image.commit_lsn
    }
}

pub(super) struct StatementSpillRoots<'a> {
    registry: Arc<SpillRootRegistry>,
    reader: Option<Arc<SpillReader>>,
    image: &'a SpillGenerationImage,
    handles: &'a [SstHandle],
}

impl<'a> StatementSpillRoots<'a> {
    fn capture(
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<Self, SqlError> {
        let image = arena.alloc(image).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained scan schema exceeds the statement arena"
            )
        })?;
        let handles = arena
            .alloc_slice_with(generations.len(), |index| {
                generations.handle(index).expect("complete generation list")
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained scan SST roots exceed the statement arena"
                )
            })?;
        storage
            .spill_roots
            .pin(handles, image.persistence != RelationPersistence::Temporary)?;
        Ok(Self {
            registry: Arc::clone(&storage.spill_roots),
            reader: storage.spill.as_ref().map(Arc::clone),
            image,
            handles,
        })
    }

    fn view(&self) -> SpillSourceView<'_> {
        SpillSourceView {
            registry: &self.registry,
            reader: self.reader.as_ref(),
            handles: self.handles,
            image: self.image,
        }
    }
}

impl Drop for StatementSpillRoots<'_> {
    fn drop(&mut self) {
        self.registry.release(
            self.handles,
            self.image.persistence != RelationPersistence::Temporary,
        );
    }
}

pub(super) enum CapturedSpillRoots<'a> {
    Empty,
    Reserved(QueryRootLease),
    Statement(StatementSpillRoots<'a>),
}

impl<'a> CapturedSpillRoots<'a> {
    pub(super) fn capture(
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
        arena: Option<&'a crate::mem::arena::Arena>,
    ) -> Result<Self, SqlError> {
        if generations.is_empty() {
            return Ok(Self::Empty);
        }
        match arena {
            Some(arena) => StatementSpillRoots::capture(storage, image, generations, arena)
                .map(Self::Statement),
            None => storage
                .query_roots
                .retain_image(storage, image, generations)
                .map(Self::Reserved),
        }
    }

    pub(super) fn with_view<T>(
        &self,
        read: impl FnOnce(&SpillSourceView<'_>) -> Result<T, SqlError>,
    ) -> Result<Option<T>, SqlError> {
        match self {
            Self::Empty => Ok(None),
            Self::Reserved(lease) => {
                lease.with_snapshot(|snapshot| read(&snapshot.view()).map(Some))
            }
            Self::Statement(source) => read(&source.view()).map(Some),
        }
    }

    pub(super) fn owned_source(
        &self,
        storage: &Storage,
    ) -> Result<Option<QueryRootLease>, SqlError> {
        match self {
            Self::Empty => Ok(None),
            Self::Reserved(lease) => lease.try_clone().map(Some),
            Self::Statement(source) => storage.query_roots.retain_source(&source.view()).map(Some),
        }
    }
}

/// The buffers are constructed at startup. Capturing only clones existing Arc
/// owners and copies handles; registry ownership never spans object I/O.
pub(super) struct SpillGenerationSnapshot {
    registry: Option<Arc<SpillRootRegistry>>,
    pub(super) reader: Option<Arc<SpillReader>>,
    handles: Vec<SstHandle>,
    image: SpillGenerationImage,
}

impl Deref for SpillGenerationSnapshot {
    type Target = SpillGenerationImage;
    fn deref(&self) -> &Self::Target {
        &self.image
    }
}

impl DerefMut for SpillGenerationSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.image
    }
}

impl SpillGenerationSnapshot {
    pub(super) fn budget_bytes(max_generations: usize) -> usize {
        max_generations * size_of::<SstHandle>()
    }

    pub(super) fn new(max_generations: usize) -> Self {
        Self {
            registry: None,
            reader: None,
            handles: Vec::with_capacity(max_generations),
            image: SpillGenerationImage::EMPTY,
        }
    }

    pub(super) fn capture(&mut self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        self.clear();
        let table = storage.table(table_slot);
        let definition = table.definition.read();
        self.image = SpillGenerationImage::capture(
            &definition,
            table.generation(),
            table_slot,
            storage.lsn(),
        );
        // Retain roots before releasing list ownership. Replacement cannot
        // retire a member between copying its handle and registering the read.
        let generations = table.spill_generations();
        self.retain_members(&generations, &storage.spill_roots)?;
        self.reader = storage.spill.as_ref().map(Arc::clone);
        Ok(())
    }

    pub(super) fn capture_image(
        &mut self,
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
    ) -> Result<(), SqlError> {
        self.clear();
        self.image = image;
        self.retain_members(generations, &storage.spill_roots)?;
        self.reader = storage.spill.as_ref().map(Arc::clone);
        Ok(())
    }

    pub(super) fn view(&self) -> SpillSourceView<'_> {
        SpillSourceView {
            registry: self.registry.as_ref().expect("captured source registry"),
            reader: self.reader.as_ref(),
            handles: &self.handles,
            image: &self.image,
        }
    }

    fn copy_source(&mut self, source: &SpillSourceView<'_>) -> Result<(), SqlError> {
        self.clear();
        if source.handles.len() > self.handles.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST members exceed startup capacity ({})",
                self.handles.capacity()
            ));
        }
        self.image = *source.image;
        for &handle in source.handles {
            self.handles.push(handle);
        }
        if let Err(error) = source.registry.pin(
            &self.handles,
            self.persistence != RelationPersistence::Temporary,
        ) {
            self.handles.clear();
            return Err(error);
        }
        self.registry = Some(Arc::clone(source.registry));
        self.reader = source.reader.map(Arc::clone);
        Ok(())
    }

    fn retain_members(
        &mut self,
        generations: &SpillGenerationList,
        registry: &Arc<SpillRootRegistry>,
    ) -> Result<(), SqlError> {
        if generations.len() > self.handles.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST members exceed startup capacity ({})",
                self.handles.capacity()
            ));
        }
        for member in generations.iter() {
            self.handles.push(member.handle);
        }
        if let Err(error) = registry.pin(
            &self.handles,
            self.persistence != RelationPersistence::Temporary,
        ) {
            self.handles.clear();
            return Err(error);
        }
        self.registry = Some(Arc::clone(registry));
        Ok(())
    }

    pub(super) fn validate(&self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        if self
            .registry
            .as_ref()
            .is_none_or(|registry| !Arc::ptr_eq(registry, &storage.spill_roots))
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained SST reader belongs to different storage"
            ));
        }
        if table_slot != self.table_slot
            || table_slot >= storage.physical_table_count()
            || !storage.table(table_slot).live()
            || storage.table(table_slot).created_at() != self.created_at
            || storage.table(table_slot).generation() != self.data_generation
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source table identity changed while SST roots were retained"
            ));
        }
        let table = storage.table(table_slot);
        let definition = table.definition();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        if definition.persistence != self.persistence
            || definition.schema(&mut schema) != self.n_columns
            || schema[..self.n_columns] != self.schema[..self.n_columns]
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source schema changed while SST roots were retained"
            ));
        }
        Ok(())
    }

    pub(super) fn relation(&self) -> SpillRelation<'_> {
        SpillRelation {
            persistence: self.persistence,
            schema: self.schema,
            n_columns: self.n_columns,
            handles: &self.handles,
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.release(
                &self.handles,
                self.persistence != RelationPersistence::Temporary,
            );
        }
        self.handles.clear();
        self.reader = None;
    }
}

impl Drop for SpillGenerationSnapshot {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::store::{BlockId, RowSstFormat};

    fn handle(tag: u8) -> SstHandle {
        SstHandle {
            index: BlockId([tag; 32]),
            filter: BlockId([tag.wrapping_add(1); 32]),
            roster: BlockId([tag.wrapping_add(2); 32]),
            format: RowSstFormat::PackedV4,
        }
    }

    #[test]
    fn guarded_spill_generations_preserve_previous_list_on_failed_install() {
        let bytes = SpillGenerationRoster::member_budget_bytes(3);
        let mut budget = Budget::new(bytes);
        let roster = SpillGenerationRoster::new(&mut budget, 3).unwrap();
        assert_eq!(budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| {
            let mut list = roster.write();
            list.append(handle(1), 7).unwrap();
            list.append(handle(5), 9).unwrap();
            for (declared, members, state) in [
                (4, &[handle(9)][..], sqlstate::PROGRAM_LIMIT_EXCEEDED),
                (2, &[handle(9)][..], sqlstate::INTERNAL_ERROR),
                (1, &[handle(9), handle(13)][..], sqlstate::INTERNAL_ERROR),
                (0, &[handle(9)][..], sqlstate::INTERNAL_ERROR),
            ] {
                let error = list.replace(declared, members.iter().copied()).unwrap_err();
                assert_eq!(error.sqlstate, state);
                assert_eq!(list.len(), 2);
                assert_eq!(list.handle(0), Some(handle(1)));
                assert_eq!(list.handle(1), Some(handle(5)));
                assert_eq!(list.members[0].through_lsn, 7);
                assert_eq!(list.members[1].through_lsn, 9);
                assert!(list.staging.is_empty());
            }
            list.append(handle(9), 11).unwrap();
            let error = list.append(handle(17), 19).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(list.len(), 3);
            assert_eq!(
                list.members[2],
                SpillGeneration {
                    handle: handle(9),
                    through_lsn: 11
                }
            );
            list.merge_pair(0, Some(handle(13)));
            assert_eq!(list.len(), 2);
            assert_eq!(
                list.members[0],
                SpillGeneration {
                    handle: handle(13),
                    through_lsn: 9
                }
            );
            assert_eq!(
                list.members[1],
                SpillGeneration {
                    handle: handle(9),
                    through_lsn: 11
                }
            );
            list.merge_pair(0, None);
            assert!(list.is_empty());
            list.replace(3, [handle(1), handle(5), handle(9)].into_iter())
                .unwrap();
            list.merge_pair(1, None);
            assert_eq!(list.len(), 1);
            assert_eq!(list.handle(0), Some(handle(1)));
            list.replace(0, [].into_iter()).unwrap();
            assert!(list.is_empty());
            list.collapse(handle(17), 19).unwrap();
            assert_eq!(list.members[0].through_lsn, 19);
        });
    }

    #[test]
    fn guarded_spill_generations_retain_roots_before_releasing_list() {
        use std::time::{Duration, Instant};
        let config = test_config();
        let mut budget = Budget::new(
            SpillRootRegistry::budget_bytes(&config)
                + SpillGenerationRoster::member_budget_bytes(2),
        );
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let roster = SpillGenerationRoster::new(&mut budget, 2).unwrap();
        roster
            .write()
            .replace(2, [handle(1), handle(5)].into_iter())
            .unwrap();
        let mut snapshot = SpillGenerationSnapshot::new(2);
        let ready = AtomicBool::new(false);
        let mut copied = Vec::with_capacity(2);
        let pinned = registry.roots.lock().unwrap();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let list = roster.read();
                    ready.store(true, Ordering::Release);
                    snapshot.retain_members(&list, &registry).unwrap();
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let started = ready.load(Ordering::Acquire);
            let excluded = matches!(
                roster.state.try_write(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            // Release before assertions so a failing test cannot strand the reader.
            drop(pinned);
            reader.join().unwrap();
            assert!(started, "reader did not reach root registration");
            assert!(excluded, "replacement raced root registration");
        });
        crate::mem::guard::forbid_alloc(|| {
            roster.write().replace(1, [handle(9)].into_iter()).unwrap();
            assert_eq!(snapshot.handles, [handle(1), handle(5)]);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, [handle(1), handle(5)]);
            snapshot.clear();
            assert!(registry.take_reclamation_pending());
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            let list = roster.read();
            snapshot.retain_members(&list, &registry).unwrap();
            assert_eq!(snapshot.handles, [handle(9)]);
            drop(list);
            snapshot.clear();
        });
    }

    #[test]
    fn guarded_spill_generations_capture_complete_lists_during_replacement() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 3;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("coherent", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1), handle(5)]);
        let first = [handle(1), handle(5)];
        let second = [handle(9), handle(13), handle(17)];
        let mut snapshot = SpillGenerationSnapshot::new(3);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for index in 0..512 {
                        let members: &[SstHandle] = if index % 2 == 0 { &second } else { &first };
                        storage
                            .table(table)
                            .spill_generations
                            .write()
                            .replace(members.len(), members.iter().copied())
                            .unwrap();
                    }
                });
            });
            barrier.wait();
            crate::mem::guard::forbid_alloc(|| {
                for _ in 0..512 {
                    snapshot.capture(&storage, table).unwrap();
                    assert!(snapshot.handles == first || snapshot.handles == second);
                    assert_eq!(snapshot.created_at, storage.table(table).created_at());
                    assert_eq!(snapshot.schema[0], ColType::Int4);
                    snapshot.clear();
                }
            });
        });
        assert!(storage.spill_roots.roots.lock().unwrap().is_empty());
    }

    struct PointFixture {
        storage: Storage,
        table: usize,
        roots: [SstHandle; 3],
        arena: crate::mem::arena::Arena,
        path: std::path::PathBuf,
    }

    fn point_fixture(label: &str) -> PointFixture {
        use crate::sql::types::Datum;
        use crate::storage::rowenc;
        use crate::store::{EphemeralBlockStore, SstKey, SstWriter};
        let mut config = test_config();
        config.max_spill_generations_per_table = 3;
        let bytes = 32 * crate::store::BLOCK_SIZE;
        let mut budget = Budget::new(
            Storage::extra_budget_bytes(&config)
                + config.memtable_bytes
                + SpillReader::budget_bytes(false, 3)
                + EphemeralBlockStore::budget_bytes(bytes)
                + (1 << 20),
        );
        let path = std::env::temp_dir().join(format!(
            "pos3ql-retained-point-{label}-{}.blocks",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut blocks = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let mut writer = Box::new(SstWriter::new());
        let mut encoded = [0u8; 128];
        for (rowid, value) in [(1, 41), (2, 17)] {
            let values = [Datum::Int4(value)];
            let len = rowenc::encoded_len(&values);
            rowenc::encode(&values, &mut encoded[..len]);
            writer
                .append_version(&mut blocks, SstKey::at(rowid, 7), &encoded[..len])
                .unwrap();
        }
        let old = writer.finish(&mut blocks).unwrap().unwrap();
        writer.reset();
        let values = [Datum::Int4(88)];
        let len = rowenc::encoded_len(&values);
        rowenc::encode(&values, &mut encoded[..len]);
        writer
            .append_version(&mut blocks, SstKey::at(1, 9), &encoded[..len])
            .unwrap();
        writer
            .append_tombstone_version(&mut blocks, SstKey::at(2, 9))
            .unwrap();
        let current = writer.finish(&mut blocks).unwrap().unwrap();
        writer.reset();
        let values = [Datum::Text("replacement")];
        let len = rowenc::encoded_len(&values);
        rowenc::encode(&values, &mut encoded[..len]);
        writer
            .append_version(&mut blocks, SstKey::at(1, 11), &encoded[..len])
            .unwrap();
        let replacement = writer.finish(&mut blocks).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(blocks));
        let reader = SpillReader::new(&mut budget, 3, None, Some(blocks)).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def(label, &[("id", ColType::Int4, true)]);
        definition.persistence = RelationPersistence::Temporary;
        let table = storage.create_table(definition).unwrap();
        storage.set_spill_list(table, &[old, current]);
        let arena = crate::mem::arena::Arena::new(&mut budget, "point result", 1 << 18).unwrap();
        PointFixture {
            storage,
            table,
            roots: [old, current, replacement],
            arena,
            path,
        }
    }

    fn scan_fixture(label: &str) -> PointFixture {
        use crate::sql::types::Datum;
        use crate::store::SstWriter;
        let mut fixture = point_fixture(label);
        let values = [Datum::Int4(111)];
        let mut encoded = [0u8; 128];
        let len = crate::storage::rowenc::encoded_len(&values);
        crate::storage::rowenc::encode(&values, &mut encoded[..len]);
        let mut writer = Box::new(SstWriter::new());
        let spill = fixture.storage.spill.as_ref().unwrap();
        let mut blocks = spill.relation_blocks_for(RelationPersistence::Temporary);
        writer
            .append_version(
                &mut *blocks,
                crate::store::SstKey::at(1, 11),
                &encoded[..len],
            )
            .unwrap();
        fixture.roots[2] = writer.finish(&mut *blocks).unwrap().unwrap();
        drop(blocks);
        let location = fixture.storage.heap.append_row(&[Datum::Int4(99)]).unwrap();
        fixture.storage.tables[fixture.table]
            .rows
            .insert(0, super::super::RowState::committed_only_at(location, 7))
            .unwrap();
        fixture
    }

    #[test]
    fn captured_row_scans_metadata_keeps_files_and_boundary_before_callbacks() {
        use crate::sql::types::Datum;
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = scan_fixture("metadata-partition");
        let mut seen = [u64::MAX; 3];
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let mut count = 0;
            storage
                .for_each_row_state(table, &mut |rowid, state| {
                    seen[count] = rowid;
                    count += 1;
                    if rowid == 0 {
                        drop(state);
                        assert!(storage.tables[table].rows.test_write().is_ok());
                        assert!(storage.row_versions.test_write().is_ok());
                        storage
                            .table(table)
                            .spill_generations
                            .write()
                            .replace(1, [roots[2]].into_iter())?;
                        storage.set_commit_snapshot(u64::MAX);
                        storage.spill_roots.copy_roots(false, &mut copied)?;
                        assert_eq!(copied, roots[..2]);
                    } else {
                        assert_eq!(state.committed_lsn, 7);
                        let home = storage
                            .visible_row_home_at(
                                table,
                                rowid,
                                state,
                                8,
                                super::super::SNAPSHOT_ALL,
                                7,
                            )?
                            .unwrap();
                        storage.with_row_bytes(table, rowid, home, |bytes| {
                            let mut values = [Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values, [Datum::Int4(if rowid == 1 { 41 } else { 17 })]);
                            Ok(())
                        })?;
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert_eq!(seen, [0, 1, 2]);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            storage
                .table(table)
                .spill_generations
                .write()
                .replace(2, roots[..2].iter().copied())
                .unwrap();
            storage.set_commit_snapshot(7);
            let error = storage
                .for_each_row_state(table, &mut |_, state| {
                    drop(state);
                    Err(sql_err!(
                        sqlstate::QUERY_CANCELED,
                        "cancel captured overlay"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            assert!(storage.row_scan(table).is_ok());
            let _ = &arena;
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_statement_bytes_keep_partition_through_replacement() {
        use crate::sql::types::Datum;
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = scan_fixture("statement-partition");
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan_in(table, &arena).unwrap();
            assert!(scan.rowids.contains(0));
            let mut overlay = 0;
            scan.for_each_overlay(&mut |rowid, state| {
                assert_eq!(rowid, 0);
                drop(state);
                overlay += 1;
                storage
                    .table(table)
                    .spill_generations
                    .write()
                    .replace(1, [roots[2]].into_iter())?;
                storage.set_commit_snapshot(u64::MAX);
                Ok(core::ops::ControlFlow::Continue(()))
            })
            .unwrap();
            assert_eq!(overlay, 1);
            for columns in [None, Some(1)] {
                let mut ids = [0u64; 2];
                let mut count = 0;
                scan.for_each_spilled_row_batch(&arena, true, columns, &mut |batch| {
                    assert!(storage.tables[table].rows.test_write().is_ok());
                    for row in batch {
                        ids[count] = row.rowid;
                        count += 1;
                        let expected = Datum::Int4(if row.rowid == 1 { 41 } else { 17 });
                        match row.representation {
                            super::super::SpilledRowRepresentation::Encoded(bytes) => {
                                let mut values = [Datum::Null];
                                crate::storage::rowenc::decode(
                                    bytes,
                                    &[ColType::Int4],
                                    &mut values,
                                )?;
                                assert_eq!(values, [expected]);
                            }
                            super::super::SpilledRowRepresentation::Values(values) => {
                                assert_eq!(values, [expected])
                            }
                        }
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
                assert_eq!((count, ids), (2, [1, 2]));
            }
            let later = storage.row_scan_in(table, &arena).unwrap();
            later
                .roots
                .with_view(|source| {
                    assert_eq!(source.handles, [roots[2]]);
                    assert_eq!(source.commit_lsn(), u64::MAX);
                    Ok(())
                })
                .unwrap();
            drop(later);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, roots[..2]);
            let error = scan
                .for_each_spilled_row_batch(&arena, true, None, &mut |_| {
                    Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel captured bytes"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop(scan);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_point_source_survives_statement_memory_reset() {
        let PointFixture {
            storage,
            table,
            roots,
            mut arena,
            path,
        } = scan_fixture("owned-overlay");
        let mut state =
            super::super::RowState::committed_only_at(super::super::RowLoc::test(0, 0), 7);
        state.committed = Some(super::super::RowHome::Spilled {
            len: 7,
            sst: 0,
            commit_lsn: 7,
        });
        state.checkpoint_change_lsn = 7;
        storage.tables[table].rows.write().insert(2, state).unwrap();
        let mut saved = None;
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan_in(table, &arena).unwrap();
            assert!(scan.rowids.contains(2));
            storage.tables[table].rows.write().remove(&2);
            storage
                .table(table)
                .spill_generations
                .write()
                .replace(1, [roots[2]].into_iter())
                .unwrap();
            scan.for_each_overlay(&mut |rowid, state| {
                if rowid == 2 {
                    assert!(
                        state.committed.is_none(),
                        "captured newest deletion remains metadata"
                    );
                    let home = storage
                        .visible_row_home_at(table, rowid, state, 8, super::super::SNAPSHOT_ALL, 7)?
                        .unwrap();
                    let super::super::VisibleRowBytes::Spilled(source) = home.bytes else {
                        unreachable!()
                    };
                    saved = Some(source);
                }
                Ok(core::ops::ControlFlow::Continue(()))
            })
            .unwrap();
            drop(scan);
            arena.reset();
            let source = saved.take().unwrap();
            assert_point_value(&source, &arena, 17);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, roots[..2]);
            drop(source);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_statement_depth_and_failed_capture_release_are_bounded() {
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("scan-capacity");
        let count = super::super::MAX_ROW_WALK_NESTING + 1;
        let mut scans = Vec::with_capacity(count);
        let source_capacity = storage.query_roots.slots_per_use;
        let mut leases = Vec::with_capacity(source_capacity);
        let mut small_budget = Budget::new(size_of::<SpillGenerationImage>() + 64);
        let small = crate::mem::arena::Arena::new(
            &mut small_budget,
            "partial scan source",
            size_of::<SpillGenerationImage>() + 64,
        )
        .unwrap();
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..count {
                scans.push(storage.row_scan_in(table, &arena).unwrap());
            }
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire)),
                "statement sources must preserve metadata source capacity"
            );
            assert!(storage.row_scan(table).is_ok());
            let error = storage.row_scan_in(table, &small).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("SST roots"));
            scans.clear();
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(
                copied.is_empty(),
                "arena allocation failure cannot partially register roots"
            );
            for _ in 0..source_capacity {
                leases.push(storage.query_roots.retain(&storage, table, 7).unwrap());
            }
            assert_eq!(
                storage.row_scan(table).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            leases.clear();
            assert!(
                storage.row_scan(table).is_ok(),
                "failed source capture releases its row identity slot"
            );
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert_eq!(
                storage.table(table).spill_generations().handle(0),
                Some(roots[0])
            );
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    fn assert_point_value(source: &SpilledRowRead, arena: &crate::mem::arena::Arena, value: i32) {
        use crate::sql::types::Datum;
        let bytes = source.copy_bytes(arena).unwrap();
        let mut values = [Datum::Null];
        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values).unwrap();
        assert_eq!(values, [Datum::Int4(value)]);
        source
            .with_bytes(|bytes| {
                let mut values = [Datum::Null];
                crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                assert_eq!(values, [Datum::Int4(value)]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn retained_object_rows_keep_versions_schema_and_deletions_after_retirement() {
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("retirement");
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let state = storage
                .spill_row_state_at(table, 1, u64::MAX)
                .unwrap()
                .unwrap();
            let deleted = storage
                .spill_row_state_at(table, 2, u64::MAX)
                .unwrap()
                .unwrap();
            let current = state.visible_at(u64::MAX).unwrap().unwrap();
            let old = state.visible_at(7).unwrap().unwrap();
            let deleted_old = deleted.visible_at(7).unwrap().unwrap();
            assert!(deleted.visible_at(u64::MAX).unwrap().is_none());
            let deferred = storage
                .read_row_snapshot(
                    table,
                    2,
                    super::super::RowSnapshot {
                        created_at: storage.table(table).created_at(),
                        version: super::super::row_images::RowVersionIdentity::Committed(7),
                    },
                )
                .unwrap();
            assert_eq!(
                deferred.snapshot().version,
                super::super::row_images::RowVersionIdentity::Committed(7)
            );
            let bytes = storage.row_bytes(table, 2, deferred, &arena).unwrap();
            let mut values = [crate::sql::types::Datum::Null];
            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values).unwrap();
            assert_eq!(values, [crate::sql::types::Datum::Int4(17)]);
            let registry = Arc::clone(&storage.spill_roots);
            storage.set_spill_list(table, &[roots[2]]);
            storage.drop_table(table);
            let mut definition = make_def("retirement", &[("id", ColType::Text, true)]);
            definition.persistence = RelationPersistence::Temporary;
            let replacement = storage.create_table(definition).unwrap();
            storage.set_spill_list(replacement, &[roots[2]]);
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained.len(), 2);
            assert!(retained.contains(&roots[0]) && retained.contains(&roots[1]));
            state
                .roots
                .with_snapshot(|snapshot| {
                    assert_eq!(snapshot.relation().schema[0], ColType::Int4);
                    Ok(())
                })
                .unwrap();
            assert_point_value(&current, &arena, 88);
            assert_point_value(&old, &arena, 41);
            assert_point_value(&deleted_old, &arena, 17);
            assert!(deleted.visible_at(9).unwrap().is_none());
            current.with_bytes(|_| old.with_bytes(|_| Ok(()))).unwrap();
            assert_eq!(
                old.validate_request(table, 2).unwrap_err().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let error = current
                .with_bytes::<()>(|_| {
                    Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel retained row"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop((state, deleted, current, old, deleted_old));
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_use_captured_roots_for_latest_committed_validation() {
        let PointFixture {
            storage,
            table,
            arena,
            path,
            ..
        } = point_fixture("validation");
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            storage
                .for_each_row_state(table, &mut |rowid, state| {
                    assert_eq!(state.committed_lsn, 7);
                    let home = storage.committed_row_home(table, rowid, state)?;
                    if rowid == 1 {
                        let home = home.expect("newer committed row");
                        assert_eq!(
                            home.snapshot().version,
                            super::super::row_images::RowVersionIdentity::Committed(9)
                        );
                        let super::super::VisibleRowBytes::Spilled(source) = home.bytes else {
                            unreachable!()
                        };
                        assert_point_value(&source, &arena, 88);
                    } else {
                        assert!(home.is_none(), "latest deletion wins uniqueness validation");
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_allow_replacement_while_a_reader_consumes_old_bytes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let PointFixture {
            mut storage,
            table,
            roots,
            path,
            ..
        } = point_fixture("overlap");
        let state = storage
            .spill_row_state_at(table, 1, u64::MAX)
            .unwrap()
            .unwrap();
        let source = state.visible_at(7).unwrap().unwrap();
        let ready = AtomicBool::new(false);
        let replaced = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let finish = Release(&replaced);
            let reader = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    source
                        .with_bytes(|bytes| {
                            ready.store(true, Ordering::Release);
                            while !replaced.load(Ordering::Acquire) {
                                std::thread::yield_now();
                            }
                            let mut values = [crate::sql::types::Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values, [crate::sql::types::Datum::Int4(41)]);
                            Ok(())
                        })
                        .unwrap();
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let reading = ready.load(Ordering::Acquire);
            if reading {
                crate::mem::guard::forbid_alloc(|| {
                    storage.set_spill_list(table, &[roots[2]]);
                    storage.drop_table(table);
                    let mut definition = make_def("overlap", &[("id", ColType::Text, true)]);
                    definition.persistence = RelationPersistence::Temporary;
                    let replacement = storage.create_table(definition).unwrap();
                    storage.set_spill_list(replacement, &[roots[2]]);
                });
            }
            drop(finish);
            reader.join().unwrap();
            assert!(
                reading,
                "replacement must finish before the reader releases its bytes"
            );
        });
        drop((state, source, storage));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_bound_scratch_and_release_after_nested_errors() {
        let PointFixture {
            storage,
            table,
            arena,
            path,
            ..
        } = point_fixture("scratch");
        let mut tiny_budget = Budget::new(1);
        let tiny_arena = crate::mem::arena::Arena::new(&mut tiny_budget, "tiny result", 1).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let state = storage
                .spill_row_state_at(table, 1, u64::MAX)
                .unwrap()
                .unwrap();
            let row = state.visible_at(7).unwrap().unwrap();
            row.with_bytes(|_| {
                row.with_bytes(|_| {
                    let error = row.with_bytes(|_| Ok(())).unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                    Ok(())
                })
            })
            .unwrap();
            assert_point_value(&row, &arena, 41);
            let mut oversized = state.visible_at(7).unwrap().unwrap();
            oversized.len = u32::MAX;
            let error = oversized.with_bytes(|_| Ok(())).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_point_value(&row, &arena, 41);
            let error = state
                .visible_at(7)
                .unwrap()
                .unwrap()
                .copy_bytes(&tiny_arena)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn query_sst_roots_charge_exact_startup_capacity_and_reuse_released_slots() {
        let mut config = test_config();
        config.query_workspace_slots = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("query_roots", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1)]);
        let expected = QueryRootPool::budget_bytes(&config);
        let mut pool_budget = Budget::new(expected);
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        assert_eq!(pool_budget.used(), expected);
        let mut leases = Vec::with_capacity(pool.slots.len());
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..pool.slots_per_use {
                leases.push(pool.retain(&storage, table, 7).unwrap());
            }
            assert_eq!(
                pool.retain(&storage, table, 9).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(leases.pop());
            let replacement = pool.retain(&storage, table, 9).unwrap();
            replacement
                .with_snapshot(|snapshot| {
                    assert_eq!(snapshot.commit_lsn, 9);
                    assert_eq!(snapshot.relation().handle(0), handle(1));
                    replacement.with_snapshot(|nested| {
                        assert_eq!(nested.commit_lsn, snapshot.commit_lsn);
                        assert_eq!(nested.relation().handle(0), snapshot.relation().handle(0));
                        Ok(())
                    })?;
                    Ok(())
                })
                .unwrap();
            drop(replacement);
            leases.clear();
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
        });
    }

    #[test]
    fn retained_object_rows_preserve_scan_capacity_and_reuse_only_after_last_reader() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("source_capacity", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1)]);
        let pool = Arc::clone(&storage.query_roots);
        let mut scans = Vec::with_capacity(pool.slots_per_use);
        let mut points = Vec::with_capacity(pool.slots_per_use);
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..pool.slots_per_use {
                scans.push(pool.retain(&storage, table, 7).unwrap());
                points.push(pool.retain_point(&storage, table, 7).unwrap());
            }
            assert_eq!(
                pool.retain(&storage, table, 7).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let source = points.pop().unwrap();
            let copy = source.try_clone().unwrap();
            let slot = source.slot;
            drop(source);
            assert!(pool.slots[slot].retained.load(Ordering::Acquire));
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(copy);
            assert!(!pool.slots[slot].retained.load(Ordering::Acquire));
            let replacement = pool.retain_point(&storage, table, 7).unwrap();
            pool.slots[replacement.slot]
                .readers
                .store(usize::MAX, Ordering::Relaxed);
            assert_eq!(
                replacement.try_clone().err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            pool.slots[replacement.slot]
                .readers
                .store(1, Ordering::Relaxed);
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(replacement);
            scans.clear();
            points.clear();
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            assert!(storage.spill_roots.take_reclamation_pending());
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire)
                        && slot.readers.load(Ordering::Relaxed) == 0)
            );
        });
    }

    #[test]
    fn query_sst_roots_release_failed_capture_without_partial_pins() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "query_root_failure",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        storage.set_spill_list(table, &[handle(1), handle(2)]);
        config.max_spill_generations_per_table = 1;
        let mut pool_budget = Budget::new(QueryRootPool::budget_bytes(&config));
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        let mut retained = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                pool.retain(&storage, table, 7).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            storage.set_spill_list(table, &[handle(1)]);
            drop(pool.retain(&storage, table, 7).unwrap());
        });
    }

    #[test]
    fn query_sst_roots_read_captured_versions_after_table_and_storage_retirement() {
        use crate::sql::types::Datum;
        use crate::storage::row_walks::RowScanIds;
        use crate::storage::{SpillOverlayMode, SpilledRowRepresentation, rowenc};
        use crate::store::{EphemeralBlockStore, SstKey, SstWriter};
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let bytes = 32 * crate::store::BLOCK_SIZE;
        let mut budget = Budget::new(
            Storage::extra_budget_bytes(&config)
                + config.memtable_bytes
                + SpillReader::budget_bytes(false, 2)
                + EphemeralBlockStore::budget_bytes(bytes)
                + (1 << 20),
        );
        let path =
            std::env::temp_dir().join(format!("pos3ql-query-root-{}.blocks", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut blocks = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let mut writer = Box::new(SstWriter::new());
        let mut row = [0u8; 32];
        for (id, lsn, value) in [(1, 9, 88), (1, 7, 41), (2, 9, 99)] {
            let values = [Datum::Int4(value)];
            let len = rowenc::encoded_len(&values);
            rowenc::encode(&values, &mut row[..len]);
            writer
                .append_version(&mut blocks, SstKey::at(id, lsn), &row[..len])
                .unwrap();
        }
        let root = writer.finish(&mut blocks).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(blocks));
        let reader = SpillReader::new(&mut budget, 2, None, Some(blocks)).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def("retiring_query", &[("id", ColType::Int4, true)]);
        definition.persistence = RelationPersistence::Temporary;
        let table = storage.create_table(definition).unwrap();
        storage.set_spill_list(table, &[root]);
        let mut arena =
            crate::mem::arena::Arena::new(&mut budget, "query root result", 1 << 18).unwrap();
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan(table).unwrap();
            assert!(matches!(&scan.rowids, RowScanIds::Reserved(_)));
            let roots = scan.roots.owned_source(&storage).unwrap().unwrap();
            let mut metadata_rows = 0;
            storage
                .spill_merged_walk(&roots, &scan.rowids, &mut |rowid, source| {
                    let lsn = source.version.commit_lsn;
                    assert_eq!((rowid, lsn), (1, 7));
                    storage.set_commit_snapshot(u64::MAX);
                    metadata_rows += 1;
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert_eq!(
                metadata_rows, 1,
                "nested callbacks cannot advance the outer read boundary"
            );
            storage.set_commit_snapshot(7);
            let error = storage
                .spill_merged_walk(&roots, &scan.rowids, &mut |_, _| {
                    Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel retained scan"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop(roots);
            drop(scan);
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            let lease = storage.query_roots.retain(&storage, table, 7).unwrap();
            let registry = lease
                .with_snapshot(|snapshot| Ok(Arc::clone(snapshot.registry.as_ref().unwrap())))
                .unwrap();
            storage.drop_table(table);
            storage
                .create_table(make_def("retiring_query", &[("id", ColType::Text, true)]))
                .unwrap();
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [root]);
            let mut count = 0;
            lease
                .with_snapshot(|snapshot| {
                    let relation = snapshot.relation();
                    assert_eq!(relation.schema[0], ColType::Int4);
                    assert_eq!(snapshot.commit_lsn, 7);
                    Storage::walk_spill_bytes(
                        snapshot.reader.as_ref().unwrap(),
                        &relation,
                        snapshot.commit_lsn,
                        &arena,
                        true,
                        None,
                        true,
                        SpillOverlayMode::VisibleScan(&RowScanIds::Statement(&[])),
                        None,
                        &mut |rowid, lsn, representation| {
                            assert_eq!((rowid, lsn), (1, 7));
                            let mut decoded = [Datum::Null; MAX_COLUMNS];
                            let values = match representation {
                                SpilledRowRepresentation::Encoded(bytes) => {
                                    rowenc::decode(
                                        bytes,
                                        &relation.schema[..relation.n_columns],
                                        &mut decoded,
                                    )?;
                                    &decoded[..relation.n_columns]
                                }
                                SpilledRowRepresentation::Values(values) => values,
                            };
                            assert_eq!(values, [Datum::Int4(41)]);
                            count += 1;
                            Ok(core::ops::ControlFlow::Continue(()))
                        },
                    )?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(count, 1);
            drop(lease);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(!registry.take_reclamation_pending());
        });
        arena.reset();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_sst_registry_preflights_capacity_and_reader_overflow() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let roots = [handle(1), handle(5), handle(9), handle(13)];
        let mut copied = Vec::with_capacity(4);
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&roots, true).unwrap();
            registry.pin(&[roots[0], roots[0]], true).unwrap();
            let error = registry.pin(&[handle(17)], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, roots);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(pinned[0].readers.get(), 3);
                pinned[0].readers = NonZeroUsize::new(usize::MAX).unwrap();
            }
            let error = registry.pin(&roots[..2], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(
                    pinned[1].readers.get(),
                    1,
                    "failure cannot partially pin another root"
                );
                pinned[0].readers = NonZeroUsize::new(3).unwrap();
            }
            registry.release(&[roots[0], roots[0]], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied.len(), 4);
            registry.release(&roots, true);
            assert!(registry.take_reclamation_pending());
            assert!(!registry.take_reclamation_pending());
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.pin(&[handle(17)], true).unwrap();
            registry.release(&[handle(17)], true);
        });
    }

    #[test]
    fn retained_sst_registry_keeps_durable_and_temporary_namespaces_separate() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let root = handle(1);
        let mut copied = Vec::with_capacity(1);
        let mut too_small = Vec::new();
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&[root], true).unwrap();
            registry.pin(&[root], false).unwrap();
            let error = registry.copy_roots(true, &mut too_small).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], false);
            registry.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
        });
    }

    #[test]
    fn retained_sst_images_survive_replacement_and_storage_drop() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("retained_sst", &[("id", ColType::Int4, true)]))
            .unwrap();
        let original = handle(1);
        let replacement = handle(5);
        storage.set_spill_list(table, &[original]);
        let mut snapshot = SpillGenerationSnapshot::new(1);
        let mut too_small = SpillGenerationSnapshot::new(0);
        let mut roots = Vec::with_capacity(1);
        let mut other_budget = test_budget(&config);
        let other = Storage::new(&config, &mut other_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = too_small.capture(&storage, table).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(too_small.registry.is_none());
            snapshot.capture(&storage, table).unwrap();
            assert!(snapshot.validate(&storage, table).is_ok());
            assert_eq!(
                snapshot.validate(&other, table).unwrap_err().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            storage.collapse_spill(table, replacement).unwrap();
            assert_eq!(snapshot.relation().handle(0), original);
            assert_eq!(
                storage.table(table).spill_generations().handle(0).unwrap(),
                replacement
            );
            storage.drop_table(table);
            assert_eq!(
                snapshot.validate(&storage, table).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let registry = Arc::clone(snapshot.registry.as_ref().unwrap());
            drop(storage);
            registry.copy_roots(true, &mut roots).unwrap();
            assert_eq!(roots, [original]);
            assert_eq!(snapshot.relation().schema[0], ColType::Int4);
            snapshot.clear();
            registry.copy_roots(true, &mut roots).unwrap();
            assert!(roots.is_empty());
        });
    }
}
