//! Retained row bytes stay immutable; appends initialize disjoint tails.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use {
    crate::mem::mutex::Mutex, std::sync::Arc, std::sync::OnceLock, std::sync::RwLock,
    std::sync::RwLockReadGuard, std::sync::RwLockWriteGuard, std::sync::TryLockError,
};

use super::heap_relocation::HeapRemap;
use super::{MaintenanceMode, MaintenanceStep, RowLoc};
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) struct RowHeap {
    inner: Arc<HeapAllocations>,
}

struct HeapAllocations {
    buffers: [Arc<RwLock<RowHeapState>>; 2],
    publication: Arc<RwLock<HeapPublication>>,
}

struct HeapPublication {
    second_active: bool,
    maintenance: HeapMaintenance,
}

enum HeapMaintenance {
    Idle,
    Copying { frontier: usize },
    Remapping(Arc<Mutex<HeapRemap>>),
}

impl HeapPublication {
    fn copy_frontier(&self) -> Option<usize> {
        match self.maintenance {
            HeapMaintenance::Copying { frontier } => Some(frontier),
            HeapMaintenance::Idle | HeapMaintenance::Remapping(_) => None,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(&self.maintenance, HeapMaintenance::Idle)
    }
}

fn write_publication(
    publication: &RwLock<HeapPublication>,
    mode: MaintenanceMode,
) -> Option<RwLockWriteGuard<'_, HeapPublication>> {
    match mode {
        MaintenanceMode::Finish => {
            Some(publication.write().expect("heap publication lock poisoned"))
        }
        MaintenanceMode::Background => match publication.try_write() {
            Ok(owner) => Some(owner),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(_)) => panic!("heap publication lock poisoned"),
        },
    }
}

pub(super) enum HeapCutover<'a> {
    Atomic(&'a mut dyn FnMut(u64) -> Result<(), SqlError>),
    Remapping(Arc<Mutex<HeapRemap>>),
}

pub(super) struct RowHeapState {
    buffer: Box<UnsafeCell<[u8]>>,
    capacity: usize,
    used: AtomicUsize,
    append: Mutex<()>,
    generation: u64,
}

// SAFETY: readers access only ranges below the acquired publication frontier.
// One append owner initializes the disjoint tail before releasing that frontier.
// Retained Arc owners prohibit mutation and reuse of their ranges. Appends
// hold a read guard through initialization; relocation destinations require
// unique allocation ownership.
unsafe impl Sync for RowHeapState {}

#[derive(Clone)]
pub(crate) struct HeapRowRead {
    state: Arc<RwLock<RowHeapState>>,
    location: RowLoc,
}

impl Deref for HeapRowRead {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes(self.location)
    }
}

impl HeapRowRead {
    pub(super) fn same_allocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn location(&self) -> RowLoc {
        self.location
    }

    fn bytes(&self, location: RowLoc) -> &[u8] {
        let state = self.state.read().expect("row heap lock poisoned");
        let start = location.offset as usize;
        let pointer = state.buffer.get().cast::<u8>();
        // SAFETY: get/other validated this range below the acquired frontier.
        // This Arc keeps the allocation alive and excludes relocation and
        // reuse, even after the guard ends or Storage is destroyed. Appends
        // initialize disjoint unpublished tails and cannot change these bytes.
        unsafe { core::slice::from_raw_parts(pointer.add(start), location.len as usize) }
    }

    pub(crate) fn other(&self, location: RowLoc) -> Result<&[u8], SqlError> {
        self.state
            .read()
            .expect("row heap lock poisoned")
            .validate(location)?;
        Ok(self.bytes(location))
    }

    pub(super) fn read_location(&self, location: RowLoc) -> Result<Self, SqlError> {
        self.other(location)?;
        Ok(Self {
            state: Arc::clone(&self.state),
            location,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeapReadSlot {
    Primary,
    Secondary,
}

pub(super) struct HeapReadView {
    heap: Arc<HeapAllocations>,
    primary: HeapRowRead,
    secondary: OnceLock<HeapRowRead>,
}

impl HeapReadView {
    fn bind_secondary(&self, owner: HeapRowRead) -> Result<&HeapRowRead, SqlError> {
        if let Err(owner) = self.secondary.set(owner) {
            let retained = self
                .secondary
                .get()
                .expect("secondary heap owner installed");
            if !retained.same_allocation(&owner)
                || retained.location.generation != owner.location.generation
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "row read view already retains another heap allocation"
                ));
            }
        }
        Ok(self
            .secondary
            .get()
            .expect("secondary heap owner installed"))
    }

    fn owner(&self, location: RowLoc) -> Result<&HeapRowRead, SqlError> {
        if self.primary.location.generation == location.generation {
            return Ok(&self.primary);
        }
        let secondary = match self.secondary.get() {
            Some(owner) => owner,
            None => self.bind_secondary(self.heap.get(location)?)?,
        };
        secondary.other(location)?;
        Ok(secondary)
    }

    pub(super) fn other(&self, location: RowLoc) -> Result<&[u8], SqlError> {
        self.owner(location)?.other(location)
    }

    pub(super) fn read_location(&self, location: RowLoc) -> Result<HeapRowRead, SqlError> {
        self.owner(location)?.read_location(location)
    }

    pub(super) fn slot_for(&self, image: &HeapRowRead) -> Result<HeapReadSlot, SqlError> {
        if self.primary.same_allocation(image) {
            return Ok(HeapReadSlot::Primary);
        }
        if !self
            .heap
            .buffers
            .iter()
            .any(|allocation| Arc::ptr_eq(allocation, &image.state))
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "ordered row heap allocation changed"
            ));
        }
        // An owned image proves that this allocation is initialized and retained.
        self.bind_secondary(image.clone())?;
        Ok(HeapReadSlot::Secondary)
    }

    pub(super) fn generation(&self, slot: HeapReadSlot) -> Result<u64, SqlError> {
        match slot {
            HeapReadSlot::Primary => Ok(self.primary.location.generation),
            HeapReadSlot::Secondary => self
                .secondary
                .get()
                .map(|owner| owner.location.generation)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "captured row has no secondary heap allocation"
                    )
                }),
        }
    }
}

impl HeapAllocations {
    fn get(&self, location: RowLoc) -> Result<HeapRowRead, SqlError> {
        let publication = self
            .publication
            .read()
            .expect("heap publication lock poisoned");
        let active = usize::from(publication.second_active);
        let selected = if matches!(&publication.maintenance, HeapMaintenance::Remapping(_))
            && self.buffers[active]
                .read()
                .expect("row heap lock poisoned")
                .generation
                != location.generation
        {
            1 - active
        } else {
            active
        };
        let allocation = &self.buffers[selected];
        let state = allocation.read().expect("row heap lock poisoned");
        state.validate(location)?;
        Ok(HeapRowRead {
            state: Arc::clone(allocation),
            location,
        })
    }
}

impl RowHeap {
    #[cfg(test)]
    pub(super) fn retain_allocation(&self) -> HeapRowRead {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        let allocation = &self.inner.buffers[usize::from(publication.second_active)];
        let state = allocation.read().expect("row heap lock poisoned");
        let generation = state.generation;
        HeapRowRead {
            state: Arc::clone(allocation),
            location: RowLoc {
                offset: 0,
                len: 0,
                generation,
            },
        }
    }

    pub(super) fn retain_read_view(&self) -> HeapReadView {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        let active = usize::from(publication.second_active);
        let retain = |allocation: &Arc<RwLock<RowHeapState>>| {
            let state = allocation.read().expect("row heap lock poisoned");
            HeapRowRead {
                state: Arc::clone(allocation),
                location: RowLoc {
                    offset: 0,
                    len: 0,
                    generation: state.generation,
                },
            }
        };
        HeapReadView {
            heap: Arc::clone(&self.inner),
            primary: retain(&self.inner.buffers[active]),
            secondary: if matches!(&publication.maintenance, HeapMaintenance::Remapping(_)) {
                OnceLock::from(retain(&self.inner.buffers[1 - active]))
            } else {
                OnceLock::new()
            },
        }
    }

    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
            + size_of::<HeapAllocations>()
            + 2 * size_of::<usize>()
            + 2 * (size_of::<RwLock<RowHeapState>>() + 2 * size_of::<usize>())
            + size_of::<RwLock<HeapPublication>>()
            + 2 * size_of::<usize>()
    }

    #[cfg(test)]
    pub(super) const fn relocation_budget_bytes(bytes: usize) -> usize {
        bytes
            + size_of::<Arc<RwLock<RowHeapState>>>()
            + size_of::<RwLock<RowHeapState>>()
            + 2 * size_of::<usize>()
    }

    pub(super) fn new(budget: &mut Budget, bytes: usize) -> Result<Self, BudgetError> {
        if bytes > u32::MAX as usize {
            return Err(BudgetError {
                what: "row heap address range",
                requested: bytes,
                remaining: budget.remaining(),
                total: budget.total(),
            });
        }
        budget.draw(Self::control_bytes(), "row heap controls")?;
        budget.draw(bytes, "memtable")?;
        budget.draw(bytes, "row heap relocation buffer")?;
        let allocation = || {
            Arc::new(RwLock::new(RowHeapState {
                // SAFETY: UnsafeCell is layout-transparent and ownership of the
                // allocation transfers exactly once; its length is unchanged.
                buffer: unsafe {
                    Box::from_raw(
                        Box::into_raw(vec![0u8; bytes].into_boxed_slice()) as *mut UnsafeCell<[u8]>
                    )
                },
                capacity: bytes,
                used: AtomicUsize::new(0),
                append: Mutex::new(()),
                generation: 1,
            }))
        };
        Ok(Self {
            inner: Arc::new(HeapAllocations {
                buffers: [allocation(), allocation()],
                publication: Arc::new(RwLock::new(HeapPublication {
                    second_active: false,
                    maintenance: HeapMaintenance::Idle,
                })),
            }),
        })
    }

    /// The caller owns row versions while selecting the capture frontier.
    pub(super) fn begin_copy(&self, mode: MaintenanceMode) -> Result<Option<HeapCopy>, SqlError> {
        let Some(mut publication) = write_publication(&self.inner.publication, mode) else {
            return Ok(None);
        };
        if !publication.is_idle() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation is already active"
            ));
        }
        let source = &self.inner.buffers[usize::from(publication.second_active)];
        let source_state = source.read().expect("row heap lock poisoned");
        let generation = source_state.next_relocation_generation()?;
        let frontier = source_state.used.load(Ordering::Acquire);
        let target_second = !publication.second_active;
        let target = &self.inner.buffers[usize::from(target_second)];
        if Arc::strong_count(target) != 1 || Arc::weak_count(target) != 0 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation buffers are retained by readers"
            ));
        }
        let source = HeapRowRead {
            state: Arc::clone(source),
            location: RowLoc {
                offset: 0,
                len: 0,
                generation: source_state.generation,
            },
        };
        publication.maintenance = HeapMaintenance::Copying { frontier };
        Ok(Some(HeapCopy {
            publication: Arc::clone(&self.inner.publication),
            source,
            target: Some(Arc::clone(target)),
            frontier,
            generation,
            target_second,
            completed: false,
        }))
    }

    /// A retained source stays untouched; only an unretained buffer can change.
    /// Publication follows successful byte and row-location preflight and movement.
    pub(super) fn with_relocation(
        &mut self,
        move_rows: impl FnOnce(&mut HeapRelocation<'_>) -> Result<RelocationCompleted, SqlError>,
    ) -> Result<(), SqlError> {
        let mut publication = self
            .inner
            .publication
            .write()
            .expect("heap publication lock poisoned");
        if !publication.is_idle() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation is already active"
            ));
        }
        let source = &self.inner.buffers[usize::from(publication.second_active)];
        let target = &self.inner.buffers[usize::from(!publication.second_active)];
        if Arc::strong_count(source) == 1 && Arc::weak_count(source) == 0 {
            let mut state = source.write().expect("row heap lock poisoned");
            let mut relocation = HeapRelocation::InPlace(&mut state);
            move_rows(&mut relocation)?;
        } else {
            if Arc::strong_count(target) != 1 || Arc::weak_count(target) != 0 {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row heap relocation buffers are retained by readers"
                ));
            }
            let mut target = target.write().expect("row heap lock poisoned");
            let source = source.read().expect("row heap lock poisoned");
            let mut relocation = HeapRelocation::Copy {
                source,
                target: &mut target,
            };
            move_rows(&mut relocation)?;
            drop(relocation);
            publication.second_active = !publication.second_active;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        self.exclusive()?.append(len)
    }

    pub(crate) fn append_bytes(&self, bytes: &[u8]) -> Result<RowLoc, SqlError> {
        self.append_image(bytes).map(|image| image.location())
    }

    #[cfg(test)]
    pub(crate) fn append_row(
        &self,
        values: &[crate::sql::types::Datum<'_>],
    ) -> Result<RowLoc, SqlError> {
        self.append_row_image(values).map(|image| image.location())
    }

    pub(crate) fn append_image(&self, bytes: &[u8]) -> Result<HeapRowRead, SqlError> {
        self.append_image_with(bytes.len(), |out| {
            out.copy_from_slice(bytes);
            Ok(())
        })
    }

    pub(crate) fn append_row_image(
        &self,
        values: &[crate::sql::types::Datum<'_>],
    ) -> Result<HeapRowRead, SqlError> {
        self.append_image_with(super::rowenc::encoded_len(values), |out| {
            super::rowenc::encode(values, out);
            Ok(())
        })
    }

    #[cfg(test)]
    fn append_with(
        &self,
        len: usize,
        initialize: impl FnOnce(&mut [u8]) -> Result<(), SqlError>,
    ) -> Result<RowLoc, SqlError> {
        self.append_image_with(len, initialize)
            .map(|image| image.location())
    }

    /// Initialization cannot retain the writable range or publish its location.
    /// Release initialization locks before row-version or catalog write ownership.
    fn append_image_with(
        &self,
        len: usize,
        initialize: impl FnOnce(&mut [u8]) -> Result<(), SqlError>,
    ) -> Result<HeapRowRead, SqlError> {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        let allocation = &self.inner.buffers[usize::from(publication.second_active)];
        let state = allocation.read().expect("row heap lock poisoned");
        let _append = state.append.lock().expect("row heap append lock poisoned");
        let start = state.used.load(Ordering::Acquire);
        state.check_capacity(start, len)?;
        let location = RowLoc {
            offset: start as u32,
            len: len as u32,
            generation: state.generation,
        };
        // SAFETY: append ownership excludes another initializer. This tail is
        // beyond the publication frontier and cannot overlap any issued read.
        // The relocation guard prevents movement or reuse until initialization
        // finishes. No reference to this range escapes the initializer.
        let out = unsafe {
            core::slice::from_raw_parts_mut(state.buffer.get().cast::<u8>().add(start), len)
        };
        initialize(out)?;
        state.used.store(start + len, Ordering::Release);
        // Retain the initialized allocation before releasing append ownership.
        Ok(HeapRowRead {
            state: Arc::clone(allocation),
            location,
        })
    }

    pub(crate) fn get(&self, location: RowLoc) -> Result<HeapRowRead, SqlError> {
        self.inner.get(location)
    }

    pub(super) fn published_remap(&self) -> Option<Arc<Mutex<HeapRemap>>> {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        match &publication.maintenance {
            HeapMaintenance::Remapping(map) => Some(Arc::clone(map)),
            HeapMaintenance::Idle | HeapMaintenance::Copying { .. } => None,
        }
    }

    pub(super) fn finish_remap(
        &self,
        map: &Arc<Mutex<HeapRemap>>,
        mode: MaintenanceMode,
    ) -> Result<MaintenanceStep, SqlError> {
        let Some(mut publication) = write_publication(&self.inner.publication, mode) else {
            return Ok(MaintenanceStep::WaitingForOwner);
        };
        if !matches!(&publication.maintenance,
            HeapMaintenance::Remapping(published) if Arc::ptr_eq(published, map))
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap remap no longer owns publication"
            ));
        }
        publication.maintenance = HeapMaintenance::Idle;
        Ok(MaintenanceStep::Complete)
    }

    pub(super) fn current_is_retained(&self) -> bool {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        let allocation = &self.inner.buffers[usize::from(publication.second_active)];
        Arc::strong_count(allocation) != 1 || Arc::weak_count(allocation) != 0
    }

    #[cfg(test)]
    pub(super) fn is_current_allocation(&self, image: &HeapRowRead) -> bool {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        Arc::ptr_eq(
            &self.inner.buffers[usize::from(publication.second_active)],
            &image.state,
        )
    }

    pub(super) fn accepts_publication(&self, image: &HeapRowRead) -> bool {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        Arc::ptr_eq(
            &self.inner.buffers[usize::from(publication.second_active)],
            &image.state,
        ) && publication
            .copy_frontier()
            .is_none_or(|frontier| image.location.offset as usize >= frontier)
    }

    pub(super) fn retain_for_publication(
        &self,
        image: &HeapRowRead,
    ) -> Result<HeapRowRead, SqlError> {
        if self.accepts_publication(image) {
            return Ok(image.clone());
        }
        self.append_image(image)
    }

    pub(crate) fn used(&self) -> usize {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        self.inner.buffers[usize::from(publication.second_active)]
            .read()
            .expect("row heap lock poisoned")
            .used
            .load(Ordering::Acquire)
    }

    pub(crate) fn capacity(&self) -> usize {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        self.inner.buffers[usize::from(publication.second_active)]
            .read()
            .expect("row heap lock poisoned")
            .capacity
    }

    /// A retained view can bind another buffer later; mutable slices require
    /// exclusive ownership of the complete allocation set.
    #[cfg(test)]
    pub(super) fn exclusive(&mut self) -> Result<&mut RowHeapState, SqlError> {
        let active = usize::from(
            self.inner
                .publication
                .read()
                .expect("heap publication lock poisoned")
                .second_active,
        );
        let inner = Arc::get_mut(&mut self.inner).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap reclamation is blocked by retained row bytes"
            )
        })?;
        Arc::get_mut(&mut inner.buffers[active])
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row heap reclamation is blocked by retained row bytes"
                )
            })
            .map(|state| state.get_mut().expect("row heap lock poisoned"))
    }

    /// Appending may acquire the locks while initialized bytes remain retained.
    #[cfg(test)]
    pub(super) fn test_append_locks_available(&self) -> bool {
        self.inner.publication.try_read().is_ok_and(|publication| {
            self.inner.buffers[usize::from(publication.second_active)]
                .try_write()
                .is_ok()
        })
    }

    /// Exclusive buffer access requires every retained byte owner to be released.
    #[cfg(test)]
    pub(super) fn test_exclusive_write(
        &self,
    ) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, RowHeapState>> {
        let publication = match self.inner.publication.try_read() {
            Ok(publication) => publication,
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(std::sync::TryLockError::WouldBlock);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("heap publication lock poisoned");
            }
        };
        let allocation = &self.inner.buffers[usize::from(publication.second_active)];
        // Capturing an owner must acquire this allocation's read guard first.
        // The permanent buffer slots keep the returned guard's address stable.
        let state = allocation.try_write()?;
        if Arc::strong_count(allocation) != 1 {
            return Err(std::sync::TryLockError::WouldBlock);
        }
        Ok(state)
    }

    #[cfg(test)]
    fn current_state(&self) -> &Arc<RwLock<RowHeapState>> {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        &self.inner.buffers[usize::from(publication.second_active)]
    }

    #[cfg(test)]
    fn spare_state(&self) -> &Arc<RwLock<RowHeapState>> {
        let publication = self
            .inner
            .publication
            .read()
            .expect("heap publication lock poisoned");
        &self.inner.buffers[usize::from(!publication.second_active)]
    }
}

#[derive(Default)]
pub(super) struct RelocationValidation {
    prior: Option<RowLoc>,
}

impl RelocationValidation {
    fn validate(&mut self, state: &RowHeapState, location: RowLoc) -> Result<(), SqlError> {
        state.validate(location)?;
        if location.len == 0 {
            return Ok(());
        }
        if let Some(previous) = self.prior {
            if previous == location {
                return Ok(());
            }
            if (location.offset as usize) < previous.offset as usize + previous.len as usize {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "heap compaction contains overlapping row locations"
                ));
            }
        }
        self.prior = Some(location);
        Ok(())
    }
}

/// Only the inactive, unretained buffer is mutable during a private copy.
pub(super) struct HeapCopy {
    publication: Arc<RwLock<HeapPublication>>,
    source: HeapRowRead,
    target: Option<Arc<RwLock<RowHeapState>>>,
    frontier: usize,
    generation: u64,
    target_second: bool,
    completed: bool,
}

impl HeapCopy {
    pub(super) fn belongs_to(&self, heap: &RowHeap) -> bool {
        Arc::ptr_eq(&self.publication, &heap.inner.publication)
    }

    pub(super) fn frontier(&self) -> usize {
        self.frontier
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn source_used(&self) -> usize {
        self.source
            .state
            .read()
            .expect("row heap lock poisoned")
            .used
            .load(Ordering::Acquire)
    }

    pub(super) fn validate_location(
        &self,
        validation: &mut RelocationValidation,
        location: RowLoc,
    ) -> Result<(), SqlError> {
        let state = self.source.state.read().expect("row heap lock poisoned");
        validation.validate(&state, location)
    }

    pub(super) fn copy_bytes(
        &mut self,
        location: RowLoc,
        source_at: usize,
        destination: usize,
        len: usize,
    ) -> Result<(), SqlError> {
        let bytes = self.source.other(location)?;
        let source = bytes
            .get(
                source_at..source_at.checked_add(len).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "heap relocation source range overflow"
                    )
                })?,
            )
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "heap relocation source range exceeds its row"
                )
            })?;
        let mut target = self
            .target
            .as_ref()
            .expect("private heap copy has a target")
            .write()
            .expect("row heap lock poisoned");
        let end = destination.checked_add(len).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap relocation destination range overflow"
            )
        })?;
        let output = target
            .buffer
            .get_mut()
            .get_mut(destination..end)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "heap relocation exceeds its startup buffer"
                )
            })?;
        output.copy_from_slice(source);
        Ok(())
    }

    pub(super) fn source_id(&self) -> usize {
        Arc::as_ptr(&self.publication) as usize
    }

    pub(super) fn published(&self) -> bool {
        self.completed
    }

    /// The caller owns row versions; no row-map walk occurs under publication
    /// ownership for a bounded remap. Appends can extend a private copy's tail.
    pub(super) fn try_cutover(
        &mut self,
        packed_bytes: usize,
        copied_tail: usize,
        cutover: HeapCutover<'_>,
        mode: MaintenanceMode,
    ) -> Result<MaintenanceStep, SqlError> {
        if self.completed {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap relocation no longer owns publication"
            ));
        }
        if let HeapCutover::Remapping(map) = &cutover
            && !map.lock().expect("heap remap lock poisoned").matches_copy(
                self,
                packed_bytes,
                copied_tail,
            )
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap remap was not prepared for this copy"
            ));
        }
        let Some(mut publication) = write_publication(&self.publication, mode) else {
            return Ok(MaintenanceStep::WaitingForOwner);
        };
        if publication.second_active == self.target_second
            || publication.copy_frontier() != Some(self.frontier)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "heap relocation no longer owns publication"
            ));
        }
        let used = self.source_used();
        if used.checked_sub(self.frontier) != Some(copied_tail) {
            return Ok(MaintenanceStep::Advanced);
        }
        let used = packed_bytes.checked_add(copied_tail).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "heap relocation exceeds its startup buffer"
            )
        })?;
        let mut target = self
            .target
            .as_ref()
            .expect("private heap copy has a target")
            .write()
            .expect("row heap lock poisoned");
        if used > target.capacity {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "heap relocation exceeds its startup buffer"
            ));
        }
        let maintenance = match cutover {
            HeapCutover::Atomic(remap) => {
                remap(self.generation)?;
                HeapMaintenance::Idle
            }
            HeapCutover::Remapping(map) => HeapMaintenance::Remapping(map),
        };
        *target.used.get_mut() = used;
        target.generation = self.generation;
        publication.second_active = self.target_second;
        publication.maintenance = maintenance;
        self.completed = true;
        drop(target);
        drop(self.target.take());
        Ok(MaintenanceStep::Complete)
    }
}

impl Drop for HeapCopy {
    fn drop(&mut self) {
        if !self.completed {
            let mut publication = self
                .publication
                .write()
                .expect("heap publication lock poisoned");
            assert_eq!(publication.copy_frontier(), Some(self.frontier));
            assert_ne!(publication.second_active, self.target_second);
            publication.maintenance = HeapMaintenance::Idle;
        }
    }
}

/// Only finish can produce the token needed to publish a relocated heap.
pub(super) struct RelocationCompleted {
    _private: (),
}

pub(super) enum HeapRelocation<'a> {
    InPlace(&'a mut RowHeapState),
    Copy {
        source: RwLockReadGuard<'a, RowHeapState>,
        target: &'a mut RowHeapState,
    },
}

impl HeapRelocation<'_> {
    fn source(&self) -> &RowHeapState {
        match self {
            Self::InPlace(state) => state,
            Self::Copy { source, .. } => source,
        }
    }

    pub(super) fn validate_relocation(
        &self,
        locations: impl Iterator<Item = RowLoc>,
    ) -> Result<(), SqlError> {
        self.source().validate_relocation(locations)
    }

    pub(super) fn next_relocation_generation(&self) -> Result<u64, SqlError> {
        self.source().next_relocation_generation()
    }

    pub(super) fn relocate(&mut self, source: RowLoc, destination: usize) {
        match self {
            Self::InPlace(state) => state.relocate(source, destination),
            Self::Copy {
                source: state,
                target,
            } => {
                assert!(
                    state.validate(source).is_ok(),
                    "unvalidated heap relocation source"
                );
                let start = source.offset as usize;
                // SAFETY: the range is validated against the initialized frontier.
                // The source read guard and retained owners prevent modification;
                // the target is a different allocation with unique ownership.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        state.buffer.get().cast::<u8>().add(start),
                        source.len as usize,
                    )
                };
                target.buffer.get_mut()[destination..destination + source.len as usize]
                    .copy_from_slice(bytes);
            }
        }
    }

    pub(super) fn finish(&mut self, used: usize, generation: u64) -> RelocationCompleted {
        debug_assert_eq!(self.source().generation.checked_add(1), Some(generation));
        match self {
            Self::InPlace(state) => state.finish_relocation(used, generation),
            Self::Copy { target, .. } => {
                debug_assert!(used <= target.capacity);
                *target.used.get_mut() = used;
                target.generation = generation;
            }
        }
        RelocationCompleted { _private: () }
    }
}

impl RowHeapState {
    fn check_capacity(&self, start: usize, len: usize) -> Result<(), SqlError> {
        if len > self.capacity - start {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "memtable is full ({} bytes); with object storage on, rows spill at the next checkpoint — retry, raise memtable_bytes, or enable object storage",
                self.capacity
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        let start = *self.used.get_mut();
        self.check_capacity(start, len)?;
        let location = RowLoc {
            offset: start as u32,
            len: len as u32,
            generation: self.generation,
        };
        *self.used.get_mut() = start + len;
        Ok((location, &mut self.buffer.get_mut()[start..start + len]))
    }

    pub(super) fn validate(&self, location: RowLoc) -> Result<(), SqlError> {
        if location.generation != self.generation {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "row heap location was invalidated by relocation"
            ));
        }
        let end = (location.offset as usize).checked_add(location.len as usize);
        if end.is_none_or(|end| end > self.used.load(Ordering::Acquire)) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "row location exceeds initialized heap bytes"
            ));
        }
        Ok(())
    }

    /// Preflight the complete relocation set before moving any bytes or handles.
    pub(super) fn validate_relocation(
        &self,
        locations: impl Iterator<Item = RowLoc>,
    ) -> Result<(), SqlError> {
        let mut validation = RelocationValidation::default();
        for location in locations {
            validation.validate(self, location)?;
        }
        Ok(())
    }

    pub(super) fn relocate(&mut self, source: RowLoc, destination: usize) {
        let start = source.offset as usize;
        debug_assert!(destination <= start);
        if start != destination {
            self.buffer
                .get_mut()
                .copy_within(start..start + source.len as usize, destination);
        }
    }

    pub(super) fn next_relocation_generation(&self) -> Result<u64, SqlError> {
        self.generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation generation is exhausted"
            )
        })
    }

    pub(super) fn finish_relocation(&mut self, used: usize, generation: u64) {
        debug_assert!(used <= *self.used.get_mut());
        debug_assert_eq!(self.generation.checked_add(1), Some(generation));
        *self.used.get_mut() = used;
        self.generation = generation;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heap(bytes: usize) -> RowHeap {
        let mut budget = Budget::new(2 * bytes + RowHeap::control_bytes());
        let heap = RowHeap::new(&mut budget, bytes).unwrap();
        assert_eq!(budget.remaining(), 0);
        heap
    }

    #[test]
    fn sliced_heap_cleanup_heap_views_keep_both_generations_without_aliasing() {
        let heap = heap(32);
        crate::mem::guard::forbid_alloc(|| {
            heap.append_bytes(b"dead").unwrap();
            let old = heap.append_image(b"keep").unwrap();
            let before = heap.retain_read_view();
            let mut copy = heap.begin_copy(MaintenanceMode::Finish).unwrap().unwrap();
            copy.copy_bytes(old.location(), 0, 0, 4).unwrap();
            assert_eq!(
                copy.try_cutover(
                    4,
                    0,
                    HeapCutover::Atomic(&mut |_| Ok(())),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Complete
            );
            drop(copy);
            let current = heap.append_image(b"next").unwrap();
            assert_eq!(old.location().offset, current.location().offset);
            let after = heap.retain_read_view();
            assert_eq!(before.slot_for(&old).unwrap(), HeapReadSlot::Primary);
            assert_eq!(before.slot_for(&current).unwrap(), HeapReadSlot::Secondary);
            assert_eq!(after.slot_for(&current).unwrap(), HeapReadSlot::Primary);
            assert_eq!(after.slot_for(&old).unwrap(), HeapReadSlot::Secondary);
            assert_eq!(
                heap.begin_copy(MaintenanceMode::Background)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(heap);
            assert_eq!(before.other(old.location()).unwrap(), b"keep");
            assert_eq!(before.other(current.location()).unwrap(), b"next");
            assert_eq!(after.other(old.location()).unwrap(), b"keep");
            assert_eq!(after.other(current.location()).unwrap(), b"next");
        });
    }

    #[test]
    fn sliced_heap_cleanup_heap_view_lazily_binds_a_new_allocation() {
        let heap = heap(32);
        crate::mem::guard::forbid_alloc(|| {
            let original = heap.append_bytes(b"keep").unwrap();
            let view = heap.retain_read_view();
            let mut copy = heap.begin_copy(MaintenanceMode::Finish).unwrap().unwrap();
            copy.copy_bytes(original, 0, 0, 4).unwrap();
            let location = RowLoc {
                generation: copy.generation(),
                ..original
            };
            assert_eq!(
                copy.try_cutover(
                    4,
                    0,
                    HeapCutover::Atomic(&mut |_| Ok(())),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Complete
            );
            assert!(view.secondary.get().is_none());
            assert_eq!(view.other(location).unwrap(), b"keep");
            assert!(view.secondary.get().is_some());
            assert_eq!(view.other(original).unwrap(), b"keep");
        });
    }

    #[test]
    fn sliced_heap_cleanup_heap_copy_owns_controls_after_heap_destruction() {
        let heap = heap(32);
        let retained = heap.append_image(b"keep").unwrap();
        let mut copy = crate::mem::guard::forbid_alloc(|| heap.begin_copy(MaintenanceMode::Finish))
            .unwrap()
            .expect("synchronous heap copy");
        drop(heap);
        crate::mem::guard::forbid_alloc(|| {
            copy.copy_bytes(retained.location(), 0, 0, 2).unwrap();
            copy.copy_bytes(retained.location(), 2, 2, 2).unwrap();
            assert_eq!(
                copy.try_cutover(
                    4,
                    0,
                    HeapCutover::Atomic(&mut |_| Ok(())),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Complete
            );
            drop(copy);
            assert_eq!(&*retained, b"keep");
        });
    }

    #[test]
    fn sliced_heap_cleanup_heap_cancel_allows_exclusive_reuse_and_retry() {
        let mut heap = heap(32);
        crate::mem::guard::forbid_alloc(|| {
            let location = heap.append_bytes(b"keep").unwrap();
            let mut copy = heap
                .begin_copy(MaintenanceMode::Finish)
                .unwrap()
                .expect("synchronous heap copy");
            copy.copy_bytes(location, 0, 0, 1).unwrap();
            let error = heap.with_relocation(|_| unreachable!()).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            drop(copy);
            heap.with_relocation(|relocation| {
                let generation = relocation.next_relocation_generation()?;
                relocation.relocate(location, 0);
                Ok(relocation.finish(4, generation))
            })
            .unwrap();
            let current = RowLoc {
                generation: location.generation + 1,
                ..location
            };
            assert_eq!(&*heap.get(current).unwrap(), b"keep");
            let mut copy = heap
                .begin_copy(MaintenanceMode::Finish)
                .unwrap()
                .expect("synchronous heap copy");
            copy.copy_bytes(current, 0, 0, 4).unwrap();
            assert_eq!(
                copy.try_cutover(
                    4,
                    0,
                    HeapCutover::Atomic(&mut |_| Ok(())),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Complete
            );
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        generation: current.generation + 1,
                        ..current
                    })
                    .unwrap(),
                b"keep"
            );
        });
    }

    #[test]
    fn shared_heap_relocation_heap_copy_retains_inputs_and_publishes_late_bytes() {
        let heap = heap(32);
        crate::mem::guard::forbid_alloc(|| {
            heap.append_bytes(b"dead").unwrap();
            let old = heap.append_image(b"keep").unwrap();
            let mut copy = heap
                .begin_copy(MaintenanceMode::Finish)
                .unwrap()
                .expect("synchronous heap copy");
            copy.validate_location(&mut RelocationValidation::default(), old.location())
                .unwrap();
            copy.copy_bytes(old.location(), 0, 0, 4).unwrap();
            let late = heap.append_image(b"next").unwrap();
            let remapped = core::cell::Cell::new(false);
            assert_eq!(
                copy.try_cutover(
                    4,
                    0,
                    HeapCutover::Atomic(&mut |_| {
                        remapped.set(true);
                        Ok(())
                    }),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Advanced
            );
            assert!(!remapped.get());
            assert_eq!(&*heap.get(old.location()).unwrap(), b"keep");
            assert!(!heap.accepts_publication(&old));
            assert!(heap.accepts_publication(&late));
            let rehomed = heap.retain_for_publication(&old).unwrap();
            assert!(rehomed.location().offset as usize >= copy.frontier());
            let tail = copy.source_used() - copy.frontier();
            let location = RowLoc {
                offset: copy.frontier() as u32,
                len: tail as u32,
                generation: old.location().generation,
            };
            copy.copy_bytes(location, 0, 4, tail).unwrap();
            let generation = copy.generation();
            assert_eq!(
                copy.try_cutover(
                    4,
                    tail,
                    HeapCutover::Atomic(&mut |_| Ok(())),
                    MaintenanceMode::Finish
                )
                .unwrap(),
                MaintenanceStep::Complete
            );
            assert_eq!(heap.used(), 12);
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        offset: 4,
                        len: 4,
                        generation
                    })
                    .unwrap(),
                b"next"
            );
            assert_eq!(&*old, b"keep");
            assert_eq!(&*late, b"next");
            assert_eq!(&*rehomed, b"keep");
        });
    }

    #[test]
    fn shared_heap_relocation_heap_cancel_and_generation_exhaustion_release_controls() {
        let mut heap = heap(32);
        crate::mem::guard::forbid_alloc(|| {
            let image = heap.append_image(b"keep").unwrap();
            let mut copy = heap
                .begin_copy(MaintenanceMode::Finish)
                .unwrap()
                .expect("synchronous heap copy");
            copy.copy_bytes(image.location(), 0, 0, 1).unwrap();
            drop(copy);
            let used = heap.used();
            let retained = heap.retain_for_publication(&image).unwrap();
            assert_eq!(retained.location(), image.location());
            assert_eq!(heap.used(), used);
            drop(retained);
            drop(image);
            heap.exclusive().unwrap().generation = u64::MAX;
            let error = heap.begin_copy(MaintenanceMode::Finish).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                heap.inner
                    .publication
                    .read()
                    .unwrap()
                    .copy_frontier()
                    .is_none()
            );
            assert!(heap.test_exclusive_write().is_ok());
        });
    }

    #[test]
    fn owned_write_inputs_failed_append_preserves_frontier_and_releases_owners() {
        let mut heap = heap(16);
        crate::mem::guard::forbid_alloc(|| {
            let image = heap.append_image(b"keep").unwrap();
            let error = heap
                .append_image_with(4, |out| {
                    out.copy_from_slice(b"lost");
                    Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "encoding failed"
                    ))
                })
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(heap.used(), 4);
            let error = heap
                .append_image(b"too many bytes for this heap")
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(heap.used(), 4);
            assert_eq!(&*image, b"keep");
            drop(image);
            // No owner from either failure may prevent exclusive reclamation.
            assert!(heap.exclusive().is_ok());
        });
    }

    #[test]
    fn shared_heap_append_preserves_frontier_on_failure_and_exhaustion() {
        let heap = heap(8);
        crate::mem::guard::forbid_alloc(|| {
            let old = heap.append_bytes(b"keep").unwrap();
            let pinned = heap.get(old).unwrap();
            let error = heap
                .append_with(4, |out| {
                    out[..2].copy_from_slice(b"xx");
                    Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "encoding failed"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(heap.used(), 4);
            assert!(pinned.other(RowLoc::test(4, 4)).is_err());
            assert_eq!(&*pinned, b"keep");
            for len in [5, usize::MAX] {
                assert_eq!(
                    heap.append_with(len, |_| panic!("capacity must preflight"))
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_eq!(heap.used(), 4);
            }
            let new = heap.append_bytes(b"next").unwrap();
            assert_eq!(new, RowLoc::test(4, 4));
            assert_eq!(pinned.other(new).unwrap(), b"next");
            assert_eq!(
                heap.append_bytes(b"!").unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
        });
    }

    #[test]
    fn shared_heap_append_completes_while_an_older_reader_is_pinned() {
        use std::sync::atomic::AtomicBool;
        let heap = heap(8);
        let old = heap.append_bytes(b"keep").unwrap();
        let pinned = heap.get(old).unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let location = heap.append_bytes(b"next").unwrap();
                    done.store(true, Ordering::Release);
                    location
                })
            });
            let completed_while_pinned = crate::mem::guard::forbid_alloc(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    assert_eq!(&*pinned, b"keep");
                    std::thread::yield_now();
                }
                done.load(Ordering::Acquire)
            });
            drop(pinned);
            let new = writer.join().unwrap();
            assert!(
                completed_while_pinned,
                "append must progress before the old reader releases"
            );
            assert_eq!(&*heap.get(new).unwrap(), b"next");
        });
    }

    #[test]
    fn shared_heap_append_hides_incomplete_bytes_from_existing_readers() {
        use std::sync::atomic::AtomicBool;
        let heap = heap(8);
        let old = heap.append_bytes(b"keep").unwrap();
        let pinned = heap.get(old).unwrap();
        let entered = AtomicBool::new(false);
        let release = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    heap.append_with(4, |out| {
                        out[..2].copy_from_slice(b"ne");
                        entered.store(true, Ordering::Release);
                        while !release.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                        out[2..].copy_from_slice(b"xt");
                        Ok(())
                    })
                    .unwrap()
                })
            });
            let initialized_while_pinned = crate::mem::guard::forbid_alloc(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !entered.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let entered = entered.load(Ordering::Acquire);
                assert_eq!(&*pinned, b"keep");
                assert!(pinned.other(RowLoc::test(4, 4)).is_err());
                release.store(true, Ordering::Release);
                entered
            });
            drop(pinned);
            let new = writer.join().unwrap();
            assert!(initialized_while_pinned);
            assert_eq!(&*heap.get(new).unwrap(), b"next");
        });
    }

    #[test]
    fn shared_heap_append_parallel_writers_publish_disjoint_complete_ranges() {
        let heap = heap(128 * 8);
        let start = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            let writers: [_; 4] = core::array::from_fn(|worker| {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let mut locations = [RowLoc::EMPTY; 32];
                        for (index, location) in locations.iter_mut().enumerate() {
                            let value = ((worker as u64) << 32) | index as u64;
                            *location = heap.append_bytes(&value.to_le_bytes()).unwrap();
                            assert_eq!(&*heap.get(*location).unwrap(), &value.to_le_bytes());
                        }
                        locations
                    })
                })
            });
            start.wait();
            let mut occupied = [false; 128];
            for (worker, writer) in writers.into_iter().enumerate() {
                for (index, location) in writer.join().unwrap().into_iter().enumerate() {
                    let slot = location.offset as usize / 8;
                    assert!(!occupied[slot]);
                    occupied[slot] = true;
                    let value = ((worker as u64) << 32) | index as u64;
                    assert_eq!(&*heap.get(location).unwrap(), &value.to_le_bytes());
                }
            }
            assert!(occupied.into_iter().all(|used| used));
            assert_eq!(heap.used(), heap.capacity());
        });
    }

    #[test]
    fn row_heap_ownership_pins_bytes_and_releases_after_errors() {
        let mut heap = heap(8);
        let (location, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"abcd");
        crate::mem::guard::forbid_alloc(|| {
            let read = heap.get(location).unwrap();
            assert!(heap.test_exclusive_write().is_err());
            assert_eq!(&*read, b"abcd");
            assert_eq!(read.other(location).unwrap(), b"abcd");
            assert!(read.other(RowLoc::test(3, 2)).is_err());
            drop(read);
            assert!(heap.test_exclusive_write().is_ok());
            assert!(heap.get(RowLoc::test(u32::MAX, u32::MAX)).is_err());
            assert!(heap.get(RowLoc::test(4, 1)).is_err());
            assert!(heap.test_exclusive_write().is_ok());
            assert_eq!(heap.used(), 4);
            assert_eq!(heap.capacity(), 8);
        });
    }

    #[test]
    fn row_heap_ownership_exhaustion_preserves_position_and_initialized_bytes() {
        let mut heap = heap(4);
        crate::mem::guard::forbid_alloc(|| {
            let (location, bytes) = heap.append(4).unwrap();
            bytes.copy_from_slice(b"full");
            for len in [1, usize::MAX] {
                let error = heap.append(len).unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(heap.used(), 4);
                assert_eq!(&*heap.get(location).unwrap(), b"full");
            }
            let (empty, bytes) = heap.append(0).unwrap();
            assert!(bytes.is_empty());
            assert_eq!(empty, RowLoc::test(4, 0));
            assert!(heap.get(empty).unwrap().is_empty());
        });
    }

    #[test]
    fn row_heap_ownership_relocation_rejects_invalid_sets_before_mutation() {
        let mut heap = heap(12);
        let (_, bytes) = heap.append(12).unwrap();
        bytes.copy_from_slice(b"abcdefghijkl");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive().unwrap();
            let overlapping = [RowLoc::test(0, 5), RowLoc::test(4, 2)];
            assert_eq!(
                state
                    .validate_relocation(overlapping.into_iter())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let outside = [RowLoc::test(8, 5)];
            assert!(state.validate_relocation(outside.into_iter()).is_err());
            assert_eq!(&*heap.get(RowLoc::test(0, 12)).unwrap(), b"abcdefghijkl");
            assert_eq!(heap.used(), 12);
            let state = heap.exclusive().unwrap();
            let valid = [
                RowLoc::test(4, 4),
                RowLoc::test(4, 4),
                RowLoc::test(4, 0),
                RowLoc::test(8, 4),
            ];
            state.validate_relocation(valid.into_iter()).unwrap();
            state.relocate(valid[0], 0);
            state.relocate(valid[3], 4);
            let generation = state.next_relocation_generation().unwrap();
            state.finish_relocation(8, generation);
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        offset: 0,
                        len: 8,
                        generation
                    })
                    .unwrap(),
                b"efghijkl"
            );
            assert!(heap.get(RowLoc::test(8, 1)).is_err());
        });
    }

    #[test]
    fn row_heap_generation_rejects_reused_ranges_and_preserves_current_appends() {
        let mut heap = heap(12);
        let (_, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"dead");
        let (old, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"keep");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive().unwrap();
            state.validate_relocation(core::iter::once(old)).unwrap();
            let generation = state.next_relocation_generation().unwrap();
            state.relocate(old, 0);
            state.finish_relocation(4, generation);
            let current = RowLoc {
                offset: 0,
                len: 4,
                generation,
            };
            let (new, bytes) = heap.append(4).unwrap();
            bytes.copy_from_slice(b"next");
            assert_eq!(old.offset, new.offset);
            let error = match heap.get(old) {
                Ok(_) => panic!("a reused range must reject its stale locator"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(
                error.message.as_str(),
                "row heap location was invalidated by relocation"
            );
            let read = heap.get(current).unwrap();
            assert_eq!(&*read, b"keep");
            assert_eq!(read.other(new).unwrap(), b"next");
            assert_eq!(
                read.other(old).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                read.other(RowLoc::EMPTY).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            drop(read);
            assert!(heap.test_exclusive_write().is_ok());
        });
    }

    #[test]
    fn row_heap_generation_exhaustion_preserves_bytes_and_current_locations() {
        let mut heap = heap(4);
        heap.exclusive().unwrap().generation = u64::MAX;
        let (location, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"last");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive().unwrap();
            state
                .validate_relocation(core::iter::once(location))
                .unwrap();
            let error = state.next_relocation_generation().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "row heap relocation generation is exhausted"
            );
            assert_eq!(&*heap.get(location).unwrap(), b"last");
            assert_eq!(heap.used(), 4);
        });
    }

    #[test]
    fn row_heap_generation_readers_reject_detached_locations_during_relocation() {
        let mut heap = heap(8);
        let (_, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"dead");
        let (old, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"keep");
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            match heap.get(old) {
                                Ok(read) => assert_eq!(&*read, b"keep"),
                                Err(error) => {
                                    assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE)
                                }
                            }
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                let mut state = loop {
                    if let Ok(state) = heap.test_exclusive_write() {
                        break state;
                    }
                    std::thread::yield_now();
                };
                state.validate_relocation(core::iter::once(old)).unwrap();
                let generation = state.next_relocation_generation().unwrap();
                state.relocate(old, 0);
                state.finish_relocation(4, generation);
                let (_, bytes) = state.append(4).unwrap();
                bytes.copy_from_slice(b"next");
            });
        });
        assert!(heap.get(old).is_err());
        assert!(heap.test_exclusive_write().is_ok());
    }

    #[test]
    fn row_heap_ownership_readers_observe_complete_byte_updates() {
        let mut heap = heap(8);
        let (location, bytes) = heap.append(8).unwrap();
        bytes.copy_from_slice(&0u64.to_le_bytes());
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let row = heap.get(location).unwrap();
                            let epoch = u64::from_le_bytes((&*row).try_into().unwrap());
                            assert!(epoch <= 500);
                            assert_eq!(row.other(location).unwrap(), &epoch.to_le_bytes());
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 1u64..=500 {
                    loop {
                        if let Ok(mut writer) = heap.test_exclusive_write() {
                            writer.buffer.get_mut()[..8].copy_from_slice(&epoch.to_le_bytes());
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            });
        });
        assert_eq!(&*heap.get(location).unwrap(), &500u64.to_le_bytes());
    }

    #[test]
    fn shared_heap_append_owned_bytes_survive_heap_teardown_and_thread_transfer() {
        let heap = heap(8);
        let first = heap.append_bytes(b"keep").unwrap();
        let read = crate::mem::guard::forbid_alloc(|| {
            let read = heap.get(first).unwrap();
            let second = heap.append_bytes(b"next").unwrap();
            assert_eq!(read.other(second).unwrap(), b"next");
            assert!(heap.get(RowLoc::test(8, 1)).is_err());
            read
        });
        let released = Arc::downgrade(&read.state);
        crate::mem::guard::forbid_alloc(|| drop(heap));
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        assert_eq!(&*read, b"keep");
                        assert_eq!(read.other(first).unwrap(), b"keep");
                        drop(read);
                    });
                })
                .join()
                .unwrap();
        });
        assert!(released.upgrade().is_none());
    }

    #[test]
    fn row_heap_ownership_reclamation_rejects_retained_bytes_before_mutation() {
        let mut heap = heap(8);
        let location = heap.append_bytes(b"keep").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let read = heap.get(location).unwrap();
            let error = heap.exclusive().err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "row heap reclamation is blocked by retained row bytes"
            );
            assert!(heap.append(4).is_err());
            assert_eq!(heap.used(), 4);
            assert_eq!(&*read, b"keep");
            let appended = heap.append_bytes(b"next").unwrap();
            assert_eq!(read.other(appended).unwrap(), b"next");
            drop(read);
            let state = heap.exclusive().unwrap();
            let generation = state.next_relocation_generation().unwrap();
            state
                .validate_relocation(core::iter::once(appended))
                .unwrap();
            state.relocate(appended, 0);
            state.finish_relocation(4, generation);
            assert_eq!(
                heap.get(location).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        offset: 0,
                        len: 4,
                        generation
                    })
                    .unwrap(),
                b"next"
            );
        });
    }

    #[test]
    fn shared_heap_append_retained_bytes_survive_copy_relocation_and_busy_retry() {
        let mut heap = heap(16);
        heap.append_bytes(b"dead").unwrap();
        let first = heap.append_bytes(b"keep").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old = heap.get(first).unwrap();
            heap.with_relocation(|move_rows| {
                move_rows.validate_relocation(core::iter::once(first))?;
                let generation = move_rows.next_relocation_generation()?;
                move_rows.relocate(first, 0);
                assert_eq!(&*old, b"keep");
                Ok(move_rows.finish(4, generation))
            })
            .unwrap();
            assert_eq!(&*old, b"keep");
            assert_eq!(old.other(first).unwrap(), b"keep");
            assert_eq!(heap.used(), 4);
            assert_eq!(
                heap.get(first).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let second = RowLoc {
                offset: 0,
                len: 4,
                generation: 2,
            };
            let current = heap.get(second).unwrap();
            let source = Arc::as_ptr(heap.current_state());
            let error = heap
                .with_relocation(|_| panic!("busy buffers entered relocation"))
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "row heap relocation buffers are retained by readers"
            );
            assert_eq!(Arc::as_ptr(heap.current_state()), source);
            let appended = heap.append_bytes(b"next").unwrap();
            assert_eq!(current.other(appended).unwrap(), b"next");
            assert_eq!(
                old.other(appended).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            drop(old);
            heap.with_relocation(|move_rows| {
                move_rows.validate_relocation(core::iter::once(appended))?;
                let generation = move_rows.next_relocation_generation()?;
                move_rows.relocate(appended, 0);
                Ok(move_rows.finish(4, generation))
            })
            .unwrap();
            assert_eq!(&*current, b"keep");
            assert_eq!(current.other(appended).unwrap(), b"next");
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        offset: 0,
                        len: 4,
                        generation: 3
                    })
                    .unwrap(),
                b"next"
            );
            drop(current);
        });
    }

    #[test]
    fn shared_heap_append_copy_relocation_progresses_while_old_bytes_are_read() {
        let mut heap = heap(16);
        heap.append_bytes(b"dead").unwrap();
        let location = heap.append_bytes(b"keep").unwrap();
        let old = heap.get(location).unwrap();
        let start = std::sync::Barrier::new(2);
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let start_ref = &start;
            let done_ref = &done;
            let reader = scope.spawn(move || {
                start_ref.wait();
                crate::mem::guard::forbid_alloc(|| {
                    while !done_ref.load(Ordering::Acquire) {
                        assert_eq!(&*old, b"keep");
                        std::thread::yield_now();
                    }
                    assert_eq!(&*old, b"keep");
                });
            });
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                heap.with_relocation(|move_rows| {
                    move_rows.validate_relocation(core::iter::once(location))?;
                    let generation = move_rows.next_relocation_generation()?;
                    move_rows.relocate(location, 0);
                    Ok(move_rows.finish(4, generation))
                })
                .unwrap();
                let appended = heap.append_bytes(b"next").unwrap();
                assert_eq!(&*heap.get(appended).unwrap(), b"next");
                done.store(true, Ordering::Release);
            });
            reader.join().unwrap();
        });
    }

    #[test]
    fn shared_heap_append_copy_relocation_preserves_rejected_sources_and_generation() {
        let mut heap = heap(8);
        let location = heap.append_bytes(b"original").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let retained = heap.get(location).unwrap();
            let owner = Arc::as_ptr(heap.current_state());
            let overlap = [RowLoc::test(0, 6), RowLoc::test(4, 4)];
            let error = heap
                .with_relocation(|move_rows| {
                    move_rows.validate_relocation(overlap.into_iter())?;
                    panic!("invalid ranges passed preflight")
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert_eq!(Arc::as_ptr(heap.current_state()), owner);
            assert_eq!(
                heap.spare_state()
                    .read()
                    .unwrap()
                    .used
                    .load(Ordering::Acquire),
                0
            );
            assert_eq!(heap.used(), 8);
            assert_eq!(&*retained, b"original");
            drop(retained);
        });
        heap.exclusive().unwrap().generation = u64::MAX;
        let exhausted = RowLoc {
            generation: u64::MAX,
            ..location
        };
        crate::mem::guard::forbid_alloc(|| {
            let retained = heap.get(exhausted).unwrap();
            let owner = Arc::as_ptr(heap.current_state());
            let error = heap
                .with_relocation(|move_rows| {
                    move_rows.validate_relocation(core::iter::once(exhausted))?;
                    move_rows.next_relocation_generation()?;
                    panic!("generation exhaustion passed preflight")
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(Arc::as_ptr(heap.current_state()), owner);
            assert_eq!(
                heap.spare_state()
                    .read()
                    .unwrap()
                    .used
                    .load(Ordering::Acquire),
                0
            );
            assert_eq!(&*retained, b"original");
        });
    }

    #[test]
    fn row_heap_ownership_controls_and_address_limits_are_charged_at_startup() {
        let _ = heap(8);
        let mut short = Budget::new(16 + RowHeap::control_bytes() - 1);
        let error = RowHeap::new(&mut short, 8).err().unwrap();
        assert_eq!(error.what, "row heap relocation buffer");
        if usize::BITS > 32 {
            let mut budget = Budget::new(usize::MAX);
            let error = match RowHeap::new(&mut budget, u32::MAX as usize + 1) {
                Ok(_) => panic!("unaddressable heap accepted"),
                Err(error) => error,
            };
            assert_eq!(error.what, "row heap address range");
            assert_eq!(budget.used(), 0);
        }
        assert!(
            crate::config::Config::parse("transaction_mode = mvcc\nmemtable_bytes = 4GiB")
                .unwrap_err()
                .message
                .contains("row-location")
        );
    }
}
