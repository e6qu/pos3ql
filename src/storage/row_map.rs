//! Guarded relation metadata shares stable startup-allocated row cells.

use core::cell::UnsafeCell;
use core::hash::{Hash, Hasher};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::RowState;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::{Fnv1aHasher, MapFull};

const INDEX_STRIPES: usize = 64;
static NEXT_POOL_ID: AtomicUsize = AtomicUsize::new(1);

/// Pool-qualified map identity; neither pool nor map identities are reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowOwnerId {
    pool: usize,
    local: usize,
}

impl RowOwnerId {
    pub(crate) fn index(self) -> usize {
        self.local - 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowCellId {
    index: u32,
    generation: u32,
}

/// A captured pool cell. Access still requires its current table owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowCellPosition {
    owner: RowOwnerId,
    cell: RowCellId,
}

impl RowCellPosition {
    pub(crate) fn owner(self) -> RowOwnerId {
        self.owner
    }

    pub(crate) fn index(self) -> usize {
        self.cell.index as usize
    }
}

/// Bounded owner-list walk. New insertions belong to the next pass.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowOwnerCursor {
    owner: RowOwnerId,
    after: u64,
    through: u64,
    next: Option<RowCellId>,
}

impl RowOwnerCursor {
    pub(crate) fn is_finished(self) -> bool {
        self.next.is_none() || self.after >= self.through
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowOwnerStep {
    Position(RowCellPosition),
    Skipped,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowOverlayUsage {
    pub(crate) live: usize,
    pub(crate) reserved: usize,
    pub(crate) free: usize,
    pub(crate) retired: usize,
}

impl RowOverlayUsage {
    pub(crate) fn pressured(self) -> bool {
        let active = self.live + self.reserved;
        let usable = active + self.free;
        usable != 0 && active >= usable.div_ceil(2)
    }
}

struct IndexEntry {
    owner: RowOwnerId,
    rowid: u64,
    next: Option<u32>,
}

struct RowPayload {
    ordinal: u64,
    rowid: u64,
    state: RowState,
    previous: Option<RowCellId>,
    next: Option<RowCellId>,
}

struct RowCell {
    published_owner: AtomicUsize,
    generation: AtomicU32,
    index: UnsafeCell<IndexEntry>,
    payload: UnsafeCell<MaybeUninit<RowPayload>>,
    free_next: UnsafeCell<Option<u32>>,
}

// SAFETY: Published payloads and membership links belong exclusively to their
// RowEntries guard. Index fields have separate storage and a bucket-stripe
// owner. Unpublished cells and free links belong to the allocator/reservation.
unsafe impl Sync for RowCell {}

struct IndexBucket(UnsafeCell<Option<u32>>);

// SAFETY: Every access holds the bucket's fixed stripe lock.
unsafe impl Sync for IndexBucket {}

struct Allocator {
    free_head: Option<u32>,
    usage: RowOverlayUsage,
}

/// Backing never grows. Hash chains share all cells across stripes, so skew
/// cannot exhaust a private stripe while another stripe has unused capacity.
pub(crate) struct RowOverlayPool {
    what: &'static str,
    identity: usize,
    cells: Box<[RowCell]>,
    buckets: Box<[IndexBucket]>,
    stripes: [RwLock<()>; INDEX_STRIPES],
    allocator: Mutex<Allocator>,
    next_owner: AtomicUsize,
    scan_end: AtomicUsize,
}

impl RowOverlayPool {
    fn bucket_count(capacity: usize) -> Option<usize> {
        capacity
            .checked_mul(2)?
            .checked_next_power_of_two()
            .map(|n| n.max(8))
    }

    pub(crate) fn budget_bytes(capacity: usize) -> usize {
        if capacity > u32::MAX as usize {
            return usize::MAX;
        }
        Self::bucket_count(capacity)
            .and_then(|count| count.checked_mul(size_of::<IndexBucket>()))
            .and_then(|bytes| {
                capacity
                    .checked_mul(size_of::<RowCell>())?
                    .checked_add(bytes)
            })
            .and_then(|bytes| bytes.checked_add(size_of::<Self>() + 2 * size_of::<usize>()))
            .unwrap_or(usize::MAX)
    }

    pub(crate) fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Arc<Self>, BudgetError> {
        let bytes = Self::budget_bytes(capacity);
        let invalid = || BudgetError {
            what,
            requested: usize::MAX,
            remaining: budget.remaining(),
            total: budget.total(),
        };
        if capacity > u32::MAX as usize || bytes == usize::MAX {
            return Err(invalid());
        }
        let bucket_count = Self::bucket_count(capacity).ok_or_else(invalid)?;
        budget.draw(bytes, what)?;
        let cells = (0..capacity)
            .map(|index| RowCell {
                published_owner: AtomicUsize::new(0),
                generation: AtomicU32::new(0),
                index: UnsafeCell::new(IndexEntry {
                    owner: RowOwnerId { pool: 0, local: 0 },
                    rowid: 0,
                    next: None,
                }),
                payload: UnsafeCell::new(MaybeUninit::uninit()),
                free_next: UnsafeCell::new((index + 1 < capacity).then_some((index + 1) as u32)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let buckets = (0..bucket_count)
            .map(|_| IndexBucket(UnsafeCell::new(None)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let identity = NEXT_POOL_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("row overlay pool identities exhausted");
        Ok(Arc::new(Self {
            what,
            identity,
            cells,
            buckets,
            stripes: core::array::from_fn(|_| RwLock::new(())),
            allocator: Mutex::new(Allocator {
                free_head: (capacity != 0).then_some(0),
                usage: RowOverlayUsage {
                    live: 0,
                    reserved: 0,
                    free: capacity,
                    retired: 0,
                },
            }),
            next_owner: AtomicUsize::new(1),
            scan_end: AtomicUsize::new(0),
        }))
    }

    pub(crate) fn capacity(&self) -> usize {
        self.cells.len()
    }

    pub(crate) fn usage(&self) -> RowOverlayUsage {
        self.allocator
            .lock()
            .expect("row overlay allocator poisoned")
            .usage
    }

    pub(crate) fn scan_end(&self) -> usize {
        self.scan_end.load(Ordering::Acquire)
    }

    pub(crate) fn owner_at(&self, index: usize) -> Option<RowOwnerId> {
        let owner = self
            .cells
            .get(index)?
            .published_owner
            .load(Ordering::Acquire);
        (owner != 0).then_some(RowOwnerId {
            pool: self.identity,
            local: owner,
        })
    }

    pub(crate) fn position_at(&self, index: usize) -> Option<RowCellPosition> {
        let cell = self.cells.get(index)?;
        let generation = cell.generation.load(Ordering::Acquire);
        let owner = self.owner_at(index)?;
        if cell.generation.load(Ordering::Acquire) != generation {
            return None;
        }
        Some(RowCellPosition {
            owner,
            cell: RowCellId {
                index: index as u32,
                generation,
            },
        })
    }

    fn register_owner(&self) -> RowOwnerId {
        let local = self
            .next_owner
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("row overlay owner identities exhausted");
        RowOwnerId {
            pool: self.identity,
            local,
        }
    }

    fn bucket(&self, owner: RowOwnerId, rowid: u64) -> usize {
        let mut hash = Fnv1aHasher::default();
        owner.local.hash(&mut hash);
        rowid.hash(&mut hash);
        hash.finish() as usize & (self.buckets.len() - 1)
    }

    fn find(&self, owner: RowOwnerId, rowid: u64) -> Option<RowCellId> {
        let bucket = self.bucket(owner, rowid);
        let _stripe = self.stripes[bucket % INDEX_STRIPES]
            .read()
            .expect("row overlay index poisoned");
        // SAFETY: The stripe guards the bucket and every link reachable from it.
        let mut next = unsafe { *self.buckets[bucket].0.get() };
        while let Some(index) = next {
            let cell = &self.cells[index as usize];
            let entry = unsafe { &*cell.index.get() };
            if entry.owner == owner && entry.rowid == rowid {
                return Some(RowCellId {
                    index,
                    generation: cell.generation.load(Ordering::Relaxed),
                });
            }
            next = entry.next;
        }
        None
    }

    fn reserve(&self) -> Result<RowCellId, MapFull> {
        let mut allocator = self
            .allocator
            .lock()
            .expect("row overlay allocator poisoned");
        let index = allocator.free_head.ok_or(MapFull {
            what: self.what,
            capacity: self.capacity(),
        })?;
        let cell = &self.cells[index as usize];
        // SAFETY: Only the allocator owns free-cell links.
        allocator.free_head = unsafe { *cell.free_next.get() };
        allocator.usage.free -= 1;
        allocator.usage.reserved += 1;
        Ok(RowCellId {
            index,
            generation: cell.generation.load(Ordering::Relaxed),
        })
    }

    fn recycle(&self, id: RowCellId, published: bool) {
        let cell = &self.cells[id.index as usize];
        debug_assert_eq!(cell.published_owner.load(Ordering::Acquire), 0);
        let mut allocator = self
            .allocator
            .lock()
            .expect("row overlay allocator poisoned");
        if published {
            allocator.usage.live -= 1;
        } else {
            allocator.usage.reserved -= 1;
        }
        if let Some(generation) = id.generation.checked_add(1) {
            cell.generation.store(generation, Ordering::Release);
            // SAFETY: The cell has no index or table owner and is not yet free.
            unsafe {
                *cell.free_next.get() = allocator.free_head;
            }
            allocator.free_head = Some(id.index);
            allocator.usage.free += 1;
        } else {
            allocator.usage.retired += 1;
        }
    }

    fn publish_index(&self, owner: RowOwnerId, id: RowCellId, rowid: u64) {
        let bucket = self.bucket(owner, rowid);
        let _stripe = self.stripes[bucket % INDEX_STRIPES]
            .write()
            .expect("row overlay index poisoned");
        let cell = &self.cells[id.index as usize];
        // SAFETY: The reservation owns this unpublished index node. The stripe
        // exclusively owns the bucket receiving it.
        unsafe {
            *cell.index.get() = IndexEntry {
                owner,
                rowid,
                next: *self.buckets[bucket].0.get(),
            };
            *self.buckets[bucket].0.get() = Some(id.index);
        }
        cell.published_owner.store(owner.local, Ordering::Release);
    }

    fn remove_index(&self, owner: RowOwnerId, id: RowCellId, rowid: u64) {
        let bucket = self.bucket(owner, rowid);
        let _stripe = self.stripes[bucket % INDEX_STRIPES]
            .write()
            .expect("row overlay index poisoned");
        // SAFETY: The stripe exclusively owns the bucket and its link fields.
        unsafe {
            let mut link = self.buckets[bucket].0.get();
            while let Some(index) = *link {
                let entry = &mut *self.cells[index as usize].index.get();
                if index == id.index {
                    *link = entry.next;
                    self.cells[index as usize]
                        .published_owner
                        .store(0, Ordering::Release);
                    return;
                }
                link = &mut entry.next;
            }
        }
        panic!("row overlay index lost an owned cell");
    }

    fn owns(&self, owner: RowOwnerId, id: RowCellId) -> bool {
        self.cells.get(id.index as usize).is_some_and(|cell| {
            owner.pool == self.identity
                && cell.published_owner.load(Ordering::Acquire) == owner.local
                && cell.generation.load(Ordering::Acquire) == id.generation
        })
    }

    fn payload_ptr(&self, owner: RowOwnerId, id: RowCellId) -> *mut RowPayload {
        assert!(
            self.owns(owner, id),
            "row overlay cell is stale or belongs to another table"
        );
        self.cells[id.index as usize]
            .payload
            .get()
            .cast::<RowPayload>()
    }
}

/// A table guard owns every published payload in this membership list.
pub(crate) struct RowEntries {
    pool: Arc<RowOverlayPool>,
    owner: RowOwnerId,
    first: Option<RowCellId>,
    last: Option<RowCellId>,
    len: usize,
    last_ordinal: u64,
}

impl RowEntries {
    fn new(pool: Arc<RowOverlayPool>) -> Self {
        let owner = pool.register_owner();
        Self {
            pool,
            owner,
            first: None,
            last: None,
            len: 0,
            last_ordinal: 0,
        }
    }

    pub(super) fn owner_id(&self) -> RowOwnerId {
        self.owner
    }

    pub(super) fn get(&self, rowid: &u64) -> Option<&RowState> {
        let id = self.pool.find(self.owner, *rowid)?;
        // SAFETY: The caller's table read owner prevents removal/reassignment.
        Some(unsafe { &(*self.pool.payload_ptr(self.owner, id)).state })
    }

    pub(super) fn get_mut(&mut self, rowid: &u64) -> Option<&mut RowState> {
        let id = self.pool.find(self.owner, *rowid)?;
        // SAFETY: The caller exclusively owns this table and this payload.
        Some(unsafe { &mut (*self.pool.payload_ptr(self.owner, id)).state })
    }

    pub(super) fn prepare_entry(&mut self, rowid: u64) -> Result<PreparedRowEntry<'_>, MapFull> {
        let existing = self.pool.find(self.owner, rowid);
        let ordinal = if existing.is_none() {
            self.last_ordinal.checked_add(1).ok_or(MapFull {
                what: "row overlay insertion identities",
                capacity: usize::MAX,
            })?
        } else {
            0
        };
        let id = match existing {
            Some(id) => id,
            None => self.pool.reserve()?,
        };
        if existing.is_none() {
            self.last_ordinal = ordinal;
        }
        Ok(PreparedRowEntry {
            rows: self,
            rowid,
            id,
            ordinal,
            vacant: existing.is_none(),
        })
    }

    pub(super) fn owner_cursor(&self) -> RowOwnerCursor {
        RowOwnerCursor {
            owner: self.owner,
            after: 0,
            through: self.last_ordinal,
            next: self.first,
        }
    }

    pub(super) fn step_owner_cursor(&self, cursor: &mut RowOwnerCursor) -> RowOwnerStep {
        if cursor.owner != self.owner || cursor.is_finished() {
            return RowOwnerStep::Complete;
        }
        let id = cursor
            .next
            .expect("unfinished owner cursor has a next cell");
        if !self.pool.owns(self.owner, id) {
            cursor.next = self.first;
            return RowOwnerStep::Skipped;
        }
        // SAFETY: The table owner protects this list and every payload.
        let payload = unsafe { &*self.pool.payload_ptr(self.owner, id) };
        cursor.next = payload.next;
        if payload.ordinal > cursor.through {
            cursor.next = None;
            return RowOwnerStep::Complete;
        }
        if payload.ordinal <= cursor.after {
            return RowOwnerStep::Skipped;
        }
        cursor.after = payload.ordinal;
        RowOwnerStep::Position(RowCellPosition {
            owner: self.owner,
            cell: id,
        })
    }

    pub(super) fn insert(
        &mut self,
        rowid: u64,
        state: RowState,
    ) -> Result<Option<RowState>, MapFull> {
        Ok(self.prepare_entry(rowid)?.publish(state))
    }

    pub(super) fn remove(&mut self, rowid: &u64) -> Option<RowState> {
        let id = self.pool.find(self.owner, *rowid)?;
        // SAFETY: Exclusive table ownership protects this payload and neighbors.
        let payload = unsafe { self.pool.payload_ptr(self.owner, id).read() };
        if let Some(previous) = payload.previous {
            unsafe {
                (*self.pool.payload_ptr(self.owner, previous)).next = payload.next;
            }
        } else {
            self.first = payload.next;
        }
        if let Some(next) = payload.next {
            unsafe {
                (*self.pool.payload_ptr(self.owner, next)).previous = payload.previous;
            }
        } else {
            self.last = payload.previous;
        }
        self.pool.remove_index(self.owner, id, *rowid);
        self.len -= 1;
        self.pool.recycle(id, true);
        Some(payload.state)
    }

    #[cfg(test)]
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&u64, &mut RowState) -> bool) {
        let mut next = self.first;
        while let Some(id) = next {
            let payload = unsafe { &mut *self.pool.payload_ptr(self.owner, id) };
            next = payload.next;
            let rowid = payload.rowid;
            if !keep(&rowid, &mut payload.state) {
                self.remove(&rowid).expect("selected row exists");
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(super) fn capacity(&self) -> usize {
        self.pool.capacity()
    }
    pub(super) fn pool_usage(&self) -> RowOverlayUsage {
        self.pool.usage()
    }

    #[cfg(test)]
    pub(super) fn scan_end(&self) -> usize {
        self.pool.scan_end()
    }

    pub(super) fn entry_at_position(&self, position: RowCellPosition) -> Option<(&u64, &RowState)> {
        if position.owner != self.owner || !self.pool.owns(self.owner, position.cell) {
            return None;
        }
        let payload = unsafe { &*self.pool.payload_ptr(self.owner, position.cell) };
        Some((&payload.rowid, &payload.state))
    }

    pub(super) fn entry_at_position_mut(
        &mut self,
        position: RowCellPosition,
    ) -> Option<(&u64, &mut RowState)> {
        if position.owner != self.owner || !self.pool.owns(self.owner, position.cell) {
            return None;
        }
        let payload = unsafe { &mut *self.pool.payload_ptr(self.owner, position.cell) };
        Some((&payload.rowid, &mut payload.state))
    }

    #[cfg(test)]
    pub(super) fn entry_at_slot(&self, slot: usize) -> Option<(&u64, &RowState)> {
        self.entry_at_position(self.pool.position_at(slot)?)
    }

    pub(super) fn iter(&self) -> RowEntriesIter<'_> {
        RowEntriesIter {
            pool: &self.pool,
            owner: self.owner,
            next: self.first,
        }
    }

    pub(super) fn iter_mut(&mut self) -> RowEntriesIterMut<'_> {
        RowEntriesIterMut {
            pool: &self.pool,
            owner: self.owner,
            next: self.first,
            _exclusive: core::marker::PhantomData,
        }
    }

    pub(super) fn clear(&mut self) {
        while let Some(id) = self.first {
            let rowid = unsafe { (*self.pool.payload_ptr(self.owner, id)).rowid };
            self.remove(&rowid).expect("owned row exists");
        }
    }
}

impl Drop for RowEntries {
    fn drop(&mut self) {
        self.clear();
    }
}

#[must_use]
pub(crate) struct PreparedRowEntry<'a> {
    rows: &'a mut RowEntries,
    rowid: u64,
    id: RowCellId,
    ordinal: u64,
    vacant: bool,
}

impl PreparedRowEntry<'_> {
    #[cfg(test)]
    pub(crate) fn current_state(&self) -> Option<RowState> {
        if self.vacant {
            None
        } else {
            Some(unsafe { (*self.rows.pool.payload_ptr(self.rows.owner, self.id)).state })
        }
    }

    pub(crate) fn replace_existing_state(&mut self, state: RowState) {
        assert!(!self.vacant, "vacant row entry has no existing state");
        unsafe {
            (*self.rows.pool.payload_ptr(self.rows.owner, self.id)).state = state;
        }
    }

    pub(crate) fn publish(mut self, state: RowState) -> Option<RowState> {
        if !self.vacant {
            let payload = unsafe { &mut *self.rows.pool.payload_ptr(self.rows.owner, self.id) };
            return Some(core::mem::replace(&mut payload.state, state));
        }
        let pool = &self.rows.pool;
        // SAFETY: The reservation uniquely owns this unpublished cell.
        unsafe {
            (*pool.cells[self.id.index as usize].payload.get()).write(RowPayload {
                ordinal: self.ordinal,
                rowid: self.rowid,
                state,
                previous: self.rows.last,
                next: None,
            });
        }
        if let Some(last) = self.rows.last {
            unsafe {
                (*pool.payload_ptr(self.rows.owner, last)).next = Some(self.id);
            }
        } else {
            self.rows.first = Some(self.id);
        }
        self.rows.last = Some(self.id);
        pool.publish_index(self.rows.owner, self.id, self.rowid);
        pool.scan_end
            .fetch_max(self.id.index as usize + 1, Ordering::Release);
        self.rows.len += 1;
        {
            let mut allocator = pool
                .allocator
                .lock()
                .expect("row overlay allocator poisoned");
            allocator.usage.reserved -= 1;
            allocator.usage.live += 1;
        }
        self.vacant = false;
        None
    }
}

impl Drop for PreparedRowEntry<'_> {
    fn drop(&mut self) {
        if self.vacant {
            self.rows.pool.recycle(self.id, false);
        }
    }
}

pub(crate) struct RowEntriesIter<'a> {
    pool: &'a RowOverlayPool,
    owner: RowOwnerId,
    next: Option<RowCellId>,
}

impl<'a> Iterator for RowEntriesIter<'a> {
    type Item = (&'a u64, &'a RowState);
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        // SAFETY: The iterator retains its table's shared borrow.
        let payload = unsafe { &*self.pool.payload_ptr(self.owner, id) };
        self.next = payload.next;
        Some((&payload.rowid, &payload.state))
    }
}

pub(crate) struct RowEntriesIterMut<'a> {
    pool: &'a RowOverlayPool,
    owner: RowOwnerId,
    next: Option<RowCellId>,
    _exclusive: core::marker::PhantomData<&'a mut RowEntries>,
}

impl<'a> Iterator for RowEntriesIterMut<'a> {
    type Item = (&'a u64, &'a mut RowState);
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        // SAFETY: The exclusive table borrow prevents membership changes. Each
        // cell is yielded once; distinct returned mutable states cannot alias.
        let payload = unsafe { &mut *self.pool.payload_ptr(self.owner, id) };
        self.next = payload.next;
        Some((&payload.rowid, &mut payload.state))
    }
}

pub struct RowMap {
    pool: Arc<RowOverlayPool>,
    state: RwLock<RowEntries>,
}

impl RowMap {
    pub(super) fn budget_bytes(capacity: usize) -> usize {
        RowOverlayPool::budget_bytes(capacity)
    }
    pub(super) fn new(
        budget: &mut Budget,
        name: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self::from_pool(RowOverlayPool::new(
            budget, name, capacity,
        )?))
    }
    pub(crate) fn from_pool(pool: Arc<RowOverlayPool>) -> Self {
        Self {
            state: RwLock::new(RowEntries::new(Arc::clone(&pool))),
            pool,
        }
    }
    pub(super) fn pool(&self) -> &Arc<RowOverlayPool> {
        &self.pool
    }
    pub(crate) fn owner_id(&self) -> RowOwnerId {
        self.read().owner_id()
    }
    pub(super) fn try_read_for_batch(
        &self,
    ) -> Result<RwLockReadGuard<'_, RowEntries>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_read(&self.state, "table row state lock poisoned")
    }
    pub(super) fn try_write_for_batch(
        &self,
    ) -> Result<RwLockWriteGuard<'_, RowEntries>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_write(&self.state, "table row state lock poisoned")
    }
    pub(super) fn read(&self) -> RwLockReadGuard<'_, RowEntries> {
        self.state.read().expect("table row state lock poisoned")
    }
    pub(super) fn write(&self) -> RwLockWriteGuard<'_, RowEntries> {
        self.state.write().expect("table row state lock poisoned")
    }
    #[cfg(test)]
    pub(super) fn test_write(&self) -> std::sync::TryLockResult<RwLockWriteGuard<'_, RowEntries>> {
        self.state.try_write()
    }
    fn exclusive(&mut self) -> &mut RowEntries {
        self.state.get_mut().expect("table row state lock poisoned")
    }
    pub fn get(&self, rowid: &u64) -> Option<RowState> {
        self.read().get(rowid).copied()
    }
    pub fn get_mut(&mut self, rowid: &u64) -> Option<&mut RowState> {
        self.exclusive().get_mut(rowid)
    }
    pub fn insert(&mut self, rowid: u64, state: RowState) -> Result<Option<RowState>, MapFull> {
        self.exclusive().insert(rowid, state)
    }
    pub fn remove(&mut self, rowid: &u64) -> Option<RowState> {
        self.exclusive().remove(rowid)
    }
    pub fn clear(&mut self) {
        self.exclusive().clear();
    }
    pub fn len(&self) -> usize {
        self.read().len()
    }
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.read().capacity()
    }
    pub fn iter(&self) -> RowMapIter<'_> {
        let state = self.read();
        let next = state.first;
        RowMapIter { state, next }
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u64, &mut RowState)> {
        self.exclusive().iter_mut()
    }
}

/// Holds the table owner and returns detached row-state images.
pub struct RowMapIter<'a> {
    state: RwLockReadGuard<'a, RowEntries>,
    next: Option<RowCellId>,
}
impl Iterator for RowMapIter<'_> {
    type Item = (u64, RowState);
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        let payload = unsafe { &*self.state.pool.payload_ptr(self.state.owner, id) };
        self.next = payload.next;
        Some((payload.rowid, payload.state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowLoc;

    fn image(lsn: u64) -> RowState {
        RowState::committed_only_at(RowLoc::test(0, 4), lsn)
    }

    fn map(capacity: usize) -> RowMap {
        let mut budget = Budget::new(RowMap::budget_bytes(capacity));
        let map = RowMap::new(&mut budget, "test table rows", capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        map
    }

    #[test]
    fn table_state_lookup_images_survive_removal_and_slot_reuse() {
        let mut rows = map(2);
        crate::mem::guard::forbid_alloc(|| {
            rows.insert(1, image(7)).unwrap();
            let retained = rows.get(&1).unwrap();
            rows.get_mut(&1).unwrap().committed_lsn = 8;
            assert_eq!(retained.committed_lsn, 7);
            assert_eq!(rows.remove(&1).unwrap().committed_lsn, 8);
            rows.insert(2, image(9)).unwrap();
            assert!(rows.get(&1).is_none());
            assert_eq!(retained.committed_lsn, 7);
            assert_eq!(rows.get(&2).unwrap().committed_lsn, 9);
            rows.clear();
            assert!(rows.is_empty());
            assert_eq!(retained.committed_lsn, 7);
        });
    }

    #[test]
    fn table_state_iteration_retains_one_guard_and_returns_detached_images() {
        let mut rows = map(2);
        rows.insert(1, image(7)).unwrap();
        rows.insert(2, image(8)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut view = rows.iter();
            let (rowid, retained) = view.next().unwrap();
            assert!(rows.state.try_write().is_err());
            assert_eq!(view.count(), 1);
            // Consuming the iterator releases its guard; its detached item
            // cannot retain a lock or observe later mutation.
            let mut writer = rows.state.try_write().unwrap();
            writer.get_mut(&rowid).unwrap().committed_lsn = 99;
            assert_ne!(retained.committed_lsn, 99);
        });
    }

    #[test]
    fn table_state_readers_observe_complete_row_map_updates_without_allocation() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RowMap>();
        let mut rows = map(4);
        for rowid in 1..=4 {
            rows.insert(rowid, image(0)).unwrap();
        }
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let rows = &rows;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let point = rows.get(&1).unwrap();
                            assert_eq!(point.committed_lsn, point.checkpoint_change_lsn);
                            let mut epoch = None;
                            let mut count = 0;
                            for (_, state) in rows.iter() {
                                assert_eq!(state.committed_lsn, state.checkpoint_change_lsn);
                                assert_eq!(
                                    *epoch.get_or_insert(state.committed_lsn),
                                    state.committed_lsn
                                );
                                count += 1;
                            }
                            assert_eq!(count, 4);
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for generation in 1..=500 {
                    let mut writer = rows.state.write().unwrap();
                    for (_, state) in writer.iter_mut() {
                        state.committed_lsn = generation;
                        state.checkpoint_change_lsn = generation;
                    }
                }
            });
        });
        assert_eq!(rows.get(&1).unwrap().committed_lsn, 500);
    }

    #[test]
    fn table_state_capacity_failure_preserves_rows_and_reuses_released_space() {
        let mut rows = map(4);
        crate::mem::guard::forbid_alloc(|| {
            for rowid in 1..=4 {
                rows.insert(rowid, image(rowid)).unwrap();
            }
            let error = rows.insert(5, image(5)).unwrap_err();
            assert_eq!(error.what, "test table rows");
            assert_eq!(error.capacity, 4);
            assert_eq!(rows.len(), rows.capacity());
            rows.remove(&2).unwrap();
            rows.insert(5, image(5)).unwrap();
            let mut found = 0u8;
            for (rowid, state) in rows.iter() {
                assert_eq!(state.committed_lsn, rowid);
                assert_eq!(found & (1 << rowid), 0);
                found |= 1 << rowid;
            }
            assert_eq!(found, (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5));
        });
    }

    fn colliding_keys<const N: usize>(capacity: usize) -> [u64; N] {
        use core::hash::{Hash, Hasher};
        let mask = (capacity * 2).next_power_of_two().max(8) - 1;
        let mut keys = [0; N];
        let mut count = 0;
        for key in 0..131_072u64 {
            let mut hash = crate::mem::fixed_map::Fnv1aHasher::default();
            1usize.hash(&mut hash);
            key.hash(&mut hash);
            if hash.finish() as usize & mask == mask {
                keys[count] = key;
                count += 1;
                if count == N {
                    return keys;
                }
            }
        }
        panic!("collision fixture did not fill its fixed key array");
    }

    #[test]
    fn stable_row_slots_collision_removal_preserves_unvisited_positions() {
        let mut rows = map(8);
        let keys = colliding_keys::<9>(8);
        crate::mem::guard::forbid_alloc(|| {
            for (slot, rowid) in keys[..8].iter().enumerate() {
                rows.insert(*rowid, image(slot as u64)).unwrap();
            }
            let mut state = rows.write();
            for (slot, rowid) in keys[..8].iter().enumerate() {
                assert_eq!(state.entry_at_slot(slot).unwrap().0, rowid);
            }
            // Lookup deletion crosses the wrapped cluster; row cells stay put.
            state.remove(&keys[0]).unwrap();
            state.remove(&keys[3]).unwrap();
            state.insert(keys[8], image(99)).unwrap();
            assert_eq!(state.entry_at_slot(3).unwrap().0, &keys[8]);
            for slot in [1, 2, 4, 5, 6, 7] {
                assert_eq!(state.entry_at_slot(slot).unwrap().0, &keys[slot]);
                assert_eq!(state.get(&keys[slot]).unwrap().committed_lsn, slot as u64);
            }
            let mut visited = 0;
            for slot in 1..state.scan_end() {
                if state.entry_at_slot(slot).is_some() {
                    visited += 1;
                }
            }
            assert_eq!(visited, 7);
            assert!(state.get(&keys[0]).is_none());
            assert!(state.get(&keys[3]).is_none());
        });
    }

    #[test]
    fn stable_row_slots_exhaustion_update_and_reuse_leave_other_rows_valid() {
        let mut rows = map(2);
        crate::mem::guard::forbid_alloc(|| {
            rows.insert(1, image(7)).unwrap();
            rows.insert(2, image(8)).unwrap();
            let retained = rows.get(&1).unwrap();
            let error = rows.insert(3, image(9)).unwrap_err();
            assert_eq!(error.what, "test table rows");
            assert_eq!(error.capacity, 2);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.insert(1, image(10)).unwrap().unwrap(), retained);
            assert_eq!(rows.remove(&1).unwrap().committed_lsn, 10);
            rows.insert(3, image(11)).unwrap();
            let state = rows.read();
            assert_eq!(state.entry_at_slot(0).unwrap().0, &3);
            assert_eq!(state.entry_at_slot(1).unwrap().0, &2);
            assert_eq!(state.get(&2).unwrap().committed_lsn, 8);
            assert!(state.get(&1).is_none());
            assert_eq!(retained.committed_lsn, 7);
        });
    }

    #[test]
    fn stable_row_slots_retention_visits_wrapped_collision_entries_once() {
        let mut rows = map(8);
        let keys = colliding_keys::<8>(8);
        crate::mem::guard::forbid_alloc(|| {
            for (position, key) in keys.iter().enumerate() {
                rows.insert(*key, image(position as u64)).unwrap();
            }
            let mut visited = [0u8; 8];
            rows.write().retain(|key, state| {
                let position = keys.iter().position(|candidate| candidate == key).unwrap();
                visited[position] += 1;
                state.committed_lsn = 99;
                position % 2 == 1
            });
            assert_eq!(visited, [1; 8]);
            assert_eq!(rows.len(), 4);
            for (position, key) in keys.iter().enumerate() {
                if position % 2 == 1 {
                    assert_eq!(rows.get(key).unwrap().committed_lsn, 99);
                    assert_eq!(rows.read().entry_at_slot(position).unwrap().0, key);
                } else {
                    assert!(rows.get(key).is_none());
                }
            }
        });
    }

    #[test]
    fn stable_row_slots_startup_reserves_shared_controls_without_hidden_capacity() {
        for capacity in [0, 1, 8, 17] {
            let bytes = RowMap::budget_bytes(capacity);
            let mut budget = Budget::new(bytes);
            let rows = RowMap::new(&mut budget, "stable rows", capacity).unwrap();
            assert_eq!(budget.remaining(), 0);
            assert_eq!(rows.capacity(), capacity);
            let mut short = Budget::new(bytes - 1);
            let error = RowMap::new(&mut short, "stable rows", capacity)
                .err()
                .unwrap();
            assert_eq!(error.what, "stable rows");
            let mut budget = Budget::new(bytes);
            let pool = RowOverlayPool::new(&mut budget, "shared rows", capacity).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let left = RowMap::from_pool(Arc::clone(&pool));
                let right = RowMap::from_pool(Arc::clone(&pool));
                assert_ne!(left.owner_id(), right.owner_id());
                assert_eq!(left.capacity(), capacity);
                assert_eq!(right.capacity(), capacity);
            });
            assert_eq!(budget.remaining(), 0);
        }
    }

    #[test]
    fn stable_row_slots_sparse_scan_limit_survives_clear_and_reuse() {
        let mut rows = map(128);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(rows.read().scan_end(), 0);
            rows.insert(1, image(7)).unwrap();
            let paused = rows.read().scan_end();
            assert_eq!(paused, 1);
            rows.remove(&1).unwrap();
            rows.clear();
            assert_eq!(rows.read().scan_end(), paused);
            rows.insert(2, image(8)).unwrap();
            assert_eq!(rows.read().scan_end(), paused);
            rows.insert(3, image(9)).unwrap();
            assert_eq!(rows.read().scan_end(), 2);
            assert_eq!(rows.capacity(), 128);
            assert_eq!(rows.iter().count(), 2);
            assert_eq!(rows.get(&2).unwrap().committed_lsn, 8);
            assert_eq!(rows.get(&3).unwrap().committed_lsn, 9);
        });
    }

    fn shared(capacity: usize) -> (Arc<RowOverlayPool>, RowMap, RowMap) {
        let mut budget = Budget::new(RowOverlayPool::budget_bytes(capacity));
        let pool = RowOverlayPool::new(&mut budget, "row_overlay_slots", capacity).unwrap();
        let left = RowMap::from_pool(Arc::clone(&pool));
        let right = RowMap::from_pool(Arc::clone(&pool));
        (pool, left, right)
    }

    #[test]
    fn shared_row_reservations_cancel_publish_and_preserve_existing_entries() {
        let (pool, mut left, mut right) = shared(2);
        crate::mem::guard::forbid_alloc(|| {
            {
                let mut rows = left.write();
                let entry = rows.prepare_entry(11).unwrap();
                assert!(entry.current_state().is_none());
                assert!(pool.usage().pressured());
                assert_eq!(
                    pool.usage(),
                    RowOverlayUsage {
                        live: 0,
                        reserved: 1,
                        free: 1,
                        retired: 0
                    }
                );
                right.insert(12, image(2)).unwrap();
                assert_eq!(
                    right.insert(13, image(3)).unwrap_err().what,
                    "row_overlay_slots"
                );
                drop(entry);
                assert_eq!(
                    pool.usage(),
                    RowOverlayUsage {
                        live: 1,
                        reserved: 0,
                        free: 1,
                        retired: 0
                    }
                );
                let entry = rows.prepare_entry(11).unwrap();
                assert!(entry.publish(image(1)).is_none());
            }
            {
                let mut rows = left.write();
                let mut entry = rows.prepare_entry(11).unwrap();
                assert_eq!(entry.current_state().unwrap().committed_lsn, 1);
                entry.replace_existing_state(image(5));
                drop(entry);
            }
            assert_eq!(left.get(&11).unwrap().committed_lsn, 5);
            assert_eq!(left.insert(11, image(7)).unwrap().unwrap().committed_lsn, 5);
            assert_eq!(
                pool.usage(),
                RowOverlayUsage {
                    live: 2,
                    reserved: 0,
                    free: 0,
                    retired: 0
                }
            );
            right.clear();
            left.insert(13, image(8)).unwrap();
            assert_eq!(left.len(), 2);
            assert!(right.is_empty());
            left.clear();
            assert_eq!(pool.usage().free, 2);
            assert!(!pool.usage().pressured());
        });
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reservation = left.exclusive().prepare_entry(99).unwrap();
            panic!("cancel unpublished row on unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(
            pool.usage(),
            RowOverlayUsage {
                live: 0,
                reserved: 0,
                free: 2,
                retired: 0
            }
        );
    }

    #[test]
    fn shared_row_cells_reject_foreign_and_reused_positions() {
        let (pool, mut left, mut right) = shared(2);
        let (_, mut independent, _) = shared(2);
        crate::mem::guard::forbid_alloc(|| {
            left.insert(1, image(1)).unwrap();
            independent.insert(1, image(3)).unwrap();
            let original = pool.position_at(0).unwrap();
            assert_eq!(original.owner(), left.owner_id());
            assert!(right.read().entry_at_position(original).is_none());
            assert!(independent.read().entry_at_position(original).is_none());
            let retained = left.get(&1).unwrap();
            left.remove(&1).unwrap();
            right.insert(1, image(2)).unwrap();
            assert!(left.read().entry_at_position(original).is_none());
            assert!(right.read().entry_at_position(original).is_none());
            let replacement = pool.position_at(0).unwrap();
            assert_eq!(
                right
                    .read()
                    .entry_at_position(replacement)
                    .unwrap()
                    .1
                    .committed_lsn,
                2
            );
            assert_eq!(retained.committed_lsn, 1);
            left.insert(2, image(4)).unwrap();
            let mut left_rows = left.write();
            let mut iter = left_rows.iter_mut();
            let (_, left_value) = iter.next().unwrap();
            let right_rows = right.read();
            let right_value = right_rows.get(&1).unwrap();
            left_value.committed_lsn = 9;
            assert_eq!(right_value.committed_lsn, 2);
            assert_eq!(left_value.committed_lsn, 9);
        });
    }

    #[test]
    fn shared_row_collision_chains_share_all_capacity_and_keep_references_stable() {
        let (pool, mut left, mut right) = shared(8);
        let keys = colliding_keys::<9>(8);
        crate::mem::guard::forbid_alloc(|| {
            for (index, key) in keys[..7].iter().enumerate() {
                left.insert(*key, image(index as u64)).unwrap();
            }
            right.insert(keys[0], image(99)).unwrap();
            let left_rows = left.read();
            let reference = left_rows.get(&keys[0]).unwrap();
            // The other owner may unlink and recycle a physical cell while
            // a borrowed state remains live in this table.
            right.remove(&keys[0]).unwrap();
            right.insert(keys[1], image(100)).unwrap();
            assert_eq!(reference.committed_lsn, 0);
            assert_eq!(left_rows.iter().count(), 7);
            drop(left_rows);
            assert!(left.insert(keys[7], image(7)).is_err());
            left.remove(&keys[3]).unwrap();
            left.insert(keys[8], image(8)).unwrap();
            {
                let mut rows = left.write();
                let mut values = rows.iter_mut();
                let (_, first) = values.next().unwrap();
                let (_, second) = values.next().unwrap();
                first.committed_lsn = 200;
                second.committed_lsn = 201;
                assert_eq!(first.committed_lsn, 200);
                assert_eq!(second.committed_lsn, 201);
                assert_eq!(pool.usage().live, 8);
            }
            drop(right);
            assert_eq!(pool.usage().live, 7);
            left.insert(keys[7], image(7)).unwrap();
            assert_eq!(pool.usage().live, 8);
        });
    }

    #[test]
    fn shared_row_generation_exhaustion_retires_cells_without_reviving_handles() {
        let (pool, mut left, mut right) = shared(1);
        crate::mem::guard::forbid_alloc(|| {
            left.insert(1, image(1)).unwrap();
            let original = pool.position_at(0).unwrap();
            left.remove(&1).unwrap();
            pool.cells[0]
                .generation
                .store(u32::MAX - 1, Ordering::Release);
            right.insert(2, image(2)).unwrap();
            let penultimate = pool.position_at(0).unwrap();
            right.remove(&2).unwrap();
            left.insert(3, image(3)).unwrap();
            let last = pool.position_at(0).unwrap();
            assert_eq!(last.cell.generation, u32::MAX);
            assert!(left.read().entry_at_position(original).is_none());
            assert!(left.read().entry_at_position(penultimate).is_none());
            left.remove(&3).unwrap();
            assert!(left.read().entry_at_position(last).is_none());
            assert_eq!(
                pool.usage(),
                RowOverlayUsage {
                    live: 0,
                    reserved: 0,
                    free: 0,
                    retired: 1
                }
            );
            let error = right.insert(4, image(4)).unwrap_err();
            assert_eq!(error.what, "row_overlay_slots");
            assert_eq!(error.capacity, 1);
            assert!(left.is_empty());
            assert!(right.is_empty());
        });
    }

    #[test]
    fn shared_row_owner_cursor_charges_lost_hints_and_rejects_late_insertions() {
        let (_, mut left, mut right) = shared(8);
        crate::mem::guard::forbid_alloc(|| {
            for rowid in 1..=6 {
                left.insert(rowid, image(rowid)).unwrap();
            }
            let mut cursor = left.read().owner_cursor();
            for rowid in 1..=4 {
                let rows = left.read();
                let RowOwnerStep::Position(position) = rows.step_owner_cursor(&mut cursor) else {
                    panic!("old row missing");
                };
                assert_eq!(*rows.entry_at_position(position).unwrap().0, rowid);
            }
            left.remove(&5).unwrap();
            right.insert(5, image(50)).unwrap();
            left.insert(7, image(7)).unwrap();
            let mut skipped = 0;
            let mut surviving = 0;
            for _ in 0..8 {
                let rows = left.read();
                match rows.step_owner_cursor(&mut cursor) {
                    RowOwnerStep::Skipped => skipped += 1,
                    RowOwnerStep::Position(position) => {
                        assert_eq!(*rows.entry_at_position(position).unwrap().0, 6);
                        surviving += 1;
                    }
                    RowOwnerStep::Complete => break,
                }
            }
            assert_eq!(
                skipped, 5,
                "restart and each old prefix link consume one step"
            );
            assert_eq!(surviving, 1);
            assert!(cursor.is_finished());
            assert_eq!(
                right.read().step_owner_cursor(&mut cursor),
                RowOwnerStep::Complete
            );
            let mut before_clear = left.read().owner_cursor();
            left.clear();
            left.insert(9, image(9)).unwrap();
            assert_eq!(
                left.read().step_owner_cursor(&mut before_clear),
                RowOwnerStep::Skipped
            );
            assert_eq!(
                left.read().step_owner_cursor(&mut before_clear),
                RowOwnerStep::Complete
            );
            left.exclusive().last_ordinal = u64::MAX - 1;
            {
                let entry = left.exclusive().prepare_entry(10).unwrap();
                drop(entry);
            }
            let before = left.pool().usage();
            let error = left.exclusive().prepare_entry(11).err().unwrap();
            assert_eq!(error.what, "row overlay insertion identities");
            assert_eq!(left.pool().usage(), before);
            left.insert(9, image(90)).unwrap();
            assert_eq!(left.get(&9).unwrap().committed_lsn, 90);
        });
    }

    #[test]
    fn shared_row_parallel_owners_keep_borrowed_state_without_index_ownership() {
        let (pool, mut left, mut right) = shared(4);
        left.insert(1, image(1)).unwrap();
        let bucket = pool.bucket(left.owner_id(), 1);
        let key = (1..1000)
            .find(|key| pool.bucket(right.owner_id(), *key) == bucket)
            .unwrap();
        right.insert(key, image(2)).unwrap();
        let rows = left.read();
        let borrowed = rows.get(&1).unwrap();
        assert!(pool.stripes[bucket % INDEX_STRIPES].try_write().is_ok());
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    for _ in 0..4 {
                        assert_eq!(right.remove(&key).unwrap().committed_lsn, 2);
                        right.insert(key, image(2)).unwrap();
                    }
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(borrowed.committed_lsn, 1);
                assert_eq!(rows.iter().count(), 1);
            });
            writer.join().unwrap();
        });
        assert_eq!(borrowed.committed_lsn, 1);
    }

    #[test]
    #[cfg(not(miri))]
    fn shared_row_tables_read_and_write_without_other_table_or_stripe_ownership() {
        let (pool, left, mut right) = shared(4);
        let left_bucket = pool.bucket(left.owner_id(), 1);
        let right_key = (1..1000)
            .find(|key| {
                pool.bucket(right.owner_id(), *key) % INDEX_STRIPES != left_bucket % INDEX_STRIPES
            })
            .unwrap();
        right.insert(right_key, image(7)).unwrap();
        let left_owner = left.write();
        let stripe_owner = pool.stripes[left_bucket % INDEX_STRIPES].write().unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let done = &done;
            scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    for round in 0..256 {
                        assert_eq!(right.get(&right_key).unwrap().committed_lsn, 7 + round);
                        right.insert(right_key, image(8 + round)).unwrap();
                    }
                });
                done.store(true, Ordering::Release);
            });
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done.load(Ordering::Acquire) && std::time::Instant::now() < until {
                std::thread::yield_now();
            }
            let independent = done.load(Ordering::Acquire);
            drop(stripe_owner);
            drop(left_owner);
            assert!(
                independent,
                "another table or stripe serialized independent reads"
            );
        });
    }
}
