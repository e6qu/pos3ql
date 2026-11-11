//! Guards retain committed definitions and the pending slots that issued a view.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::mem::size_of;
use core::ops::{Deref, DerefMut};
use {
    crate::mem::mutex::Mutex,
    std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use super::table_names::{
    DefinitionWrite, IdentityWrite, NameIdentity, TableNameScope, TableNames,
};

#[cfg(test)]
use super::TableDefBuilder;
use super::{
    CatalogDdlState, DatabaseOid, MAX_COLUMNS, Ownership, PendingTableDef, PendingTableDefSlot,
    TableDef,
};
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(super) struct DefinitionState {
    pub(super) identity: TableIdentity,
    committed: TableDef,
    pub(super) committed_revision: TableDefinitionRevision,
    pub(super) pending: Option<PendingDefinitionHead>,
    /// A CREATE RULE changes the visible history only for its transaction.
    pub(super) pending_has_rules_txid: Option<u32>,
    publication_owner: Option<u32>,
}

impl DefinitionState {
    pub(super) fn committed(&self) -> &TableDef {
        &self.committed
    }

    pub(super) fn edit_committed(&mut self, revision: TableDefinitionRevision) -> &mut TableDef {
        self.committed_revision = revision;
        &mut self.committed
    }

    pub(super) fn install(&mut self, definition: TableDef, identity: TableIdentity) {
        self.committed = definition;
        self.committed_revision = TableDefinitionRevision::INITIAL;
        self.identity = identity;
        self.pending = None;
        self.pending_has_rules_txid = None;
    }

    pub(super) fn mark_rules_present(&mut self) {
        // Staging always preserves sticky rule presence from the committed image.
        self.committed.has_rules = true;
    }

    pub(super) fn claim_publication(&mut self, transaction: u32) -> Result<(), u32> {
        if let Some(owner) = self.publication_owner
            && owner != transaction
        {
            return Err(owner);
        }
        self.publication_owner = Some(transaction);
        Ok(())
    }

    pub(super) fn release_publication(&mut self, transaction: u32) {
        if self.publication_owner == Some(transaction) {
            self.publication_owner = None;
        }
    }

    pub(super) fn publication_owner(&self) -> Option<u32> {
        self.publication_owner
    }

    pub(super) fn locked_by_other(&self, transaction: u32) -> Option<u32> {
        self.identity
            .existence
            .pending_txid()
            .into_iter()
            .chain(self.pending.map(|head| head.transaction))
            .chain(self.pending_has_rules_txid)
            .chain(self.publication_owner)
            .find(|&owner| owner != transaction)
    }
}

/// Identity and existence share publication ownership with the definition.
#[derive(Clone, Copy)]
pub(crate) struct TableIdentity {
    pub(crate) database: DatabaseOid,
    pub(crate) ownership: Ownership,
    /// Monotonic catalog stamp orders dependencies and detects slot reuse.
    pub(crate) created_at: u64,
    pub(super) ddl_version: u64,
    pub(crate) existence: CatalogDdlState,
}

impl TableIdentity {
    const EMPTY: Self = Self {
        database: DatabaseOid::POSTGRES,
        ownership: Ownership::BOOTSTRAP,
        created_at: 0,
        ddl_version: 0,
        existence: CatalogDdlState::Absent,
    };

    pub(crate) fn live(self) -> bool {
        TableNameScope::Committed.visible(self.existence)
    }
}

#[derive(Clone, Copy)]
pub(super) struct PendingDefinitionHead {
    pub(super) tail: u32,
    pub(super) transaction: u32,
    pub(super) schema: super::SqlName,
    pub(super) name: super::SqlName,
}

pub(super) struct TableDefinition {
    state: RwLock<DefinitionState>,
    names: RwLock<TableNames>,
}

impl TableDefinition {
    pub(super) fn new(committed: TableDef) -> Self {
        let state = DefinitionState {
            identity: TableIdentity::EMPTY,
            committed,
            committed_revision: TableDefinitionRevision::INITIAL,
            pending: None,
            pending_has_rules_txid: None,
            publication_owner: None,
        };
        let names = TableNames::from_state(&state);
        Self {
            state: RwLock::new(state),
            names: RwLock::new(names),
        }
    }

    pub(super) fn name_candidate(
        &self,
        scope: TableNameScope,
        database: DatabaseOid,
        schema: &str,
        name: &str,
    ) -> Option<NameIdentity> {
        self.names
            .read()
            .expect("table name lock poisoned")
            .candidate(scope, database, schema, name)
    }

    pub(super) fn try_read_for_batch(
        &self,
    ) -> Result<RwLockReadGuard<'_, DefinitionState>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_read(&self.state, "table definition lock poisoned")
    }

    pub(super) fn name_reservation_candidate(
        &self,
        database: DatabaseOid,
        schema: &str,
        name: &str,
    ) -> Option<NameIdentity> {
        self.names
            .read()
            .expect("table name lock poisoned")
            .reservation_candidate(database, schema, name)
    }

    pub(super) fn try_write_for_batch(
        &self,
    ) -> Result<DefinitionWrite<'_>, super::row_ownership::LockWait<'_>> {
        let state = super::row_ownership::try_write(&self.state, "table definition lock poisoned")?;
        Ok(DefinitionWrite::shared(state, &self.names))
    }

    pub(super) fn row_type_name_conflict(
        &self,
        database: DatabaseOid,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<bool, u32> {
        self.names
            .read()
            .expect("table name lock poisoned")
            .row_type_name_conflict(database, schema, name, transaction)
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, DefinitionState> {
        self.state.read().expect("table definition lock poisoned")
    }

    pub(super) fn try_read(
        &self,
    ) -> std::sync::TryLockResult<RwLockReadGuard<'_, DefinitionState>> {
        self.state.try_read()
    }

    #[cfg(test)]
    pub(super) fn test_write(&self) -> std::sync::TryLockResult<DefinitionWrite<'_>> {
        match self.state.try_write() {
            Ok(state) => Ok(DefinitionWrite::shared(state, &self.names)),
            Err(std::sync::TryLockError::WouldBlock) => Err(std::sync::TryLockError::WouldBlock),
            Err(std::sync::TryLockError::Poisoned(error)) => Err(
                std::sync::TryLockError::Poisoned(std::sync::PoisonError::new(
                    DefinitionWrite::shared(error.into_inner(), &self.names),
                )),
            ),
        }
    }

    pub(super) fn write(&self) -> DefinitionWrite<'_> {
        DefinitionWrite::shared(
            self.state.write().expect("table definition lock poisoned"),
            &self.names,
        )
    }

    fn exclusive(&mut self) -> DefinitionWrite<'_> {
        DefinitionWrite::exclusive(
            self.state
                .get_mut()
                .expect("table definition lock poisoned"),
            &self.names,
        )
    }

    pub(super) fn committed_mut(
        &mut self,
        revision: TableDefinitionRevision,
    ) -> CommittedDefinitionWrite<'_> {
        self.exclusive().into_committed(revision)
    }

    pub(super) fn mark_rules_present(&mut self) {
        self.exclusive().mark_rules_present();
    }

    pub(super) fn identity_mut(&mut self) -> IdentityWrite<'_> {
        self.exclusive().into_identity()
    }

    // Role ownership cannot change the names, database, stamp or existence.
    pub(super) fn ownership_mut(&mut self) -> &mut Ownership {
        &mut self
            .state
            .get_mut()
            .expect("table definition lock poisoned")
            .identity
            .ownership
    }
}

struct DefinitionVersionCell(UnsafeCell<MaybeUninit<PendingTableDefSlot>>);

// SAFETY: each payload is accessed only with its matching occupancy read/write
// guard, or exclusive pool ownership. The payload is initialized before the
// occupied bit is published; read guards prevent retirement and reuse.
unsafe impl Sync for DefinitionVersionCell {}

struct PreparedPendingDefinition {
    slot: u32,
    revision: u64,
    replacement: Option<TableDef>,
}

pub(super) struct PreparedDefinitionReplacements {
    source: super::catalog_sources::CatalogObjectIdentity,
    database: DatabaseOid,
    created_at: u64,
    ddl_version: u64,
    existence: CatalogDdlState,
    committed_revision: TableDefinitionRevision,
    committed: Option<TableDef>,
    pending: crate::mem::lease_pool::SharedSlice<PreparedPendingDefinition>,
}

impl PreparedDefinitionReplacements {
    pub(super) fn changed(&self) -> bool {
        self.committed.is_some() || self.pending.iter().any(|entry| entry.replacement.is_some())
    }
}

/// Dense locks initialize at startup; wide payload pages are touched only by
/// staged definitions. The complete payload capacity is still startup-charged.
pub(super) struct DefinitionVersions {
    slots: FixedVec<RwLock<bool>>,
    images: Box<[DefinitionVersionCell]>,
    allocation: Mutex<usize>,
}

impl DefinitionVersions {
    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) const fn budget_bytes(capacity: usize) -> usize {
        Self::control_bytes().saturating_add(capacity.saturating_mul(
            size_of::<RwLock<bool>>().saturating_add(size_of::<DefinitionVersionCell>()),
        ))
    }

    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        budget.draw(Self::control_bytes(), "table definition version controls")?;
        let mut slots = FixedVec::new(budget, "table definition slot locks", capacity)?;
        for _ in 0..capacity {
            slots
                .push(RwLock::new(false))
                .expect("startup slot capacity reserved");
        }
        budget.draw_array(
            capacity,
            size_of::<DefinitionVersionCell>(),
            "pending_table_defs",
        )?;
        let images = Box::<[DefinitionVersionCell]>::new_uninit_slice(capacity);
        // SAFETY: every cell contains only MaybeUninit behind UnsafeCell. An
        // unoccupied slot is never read, and all payload initialization occurs
        // under its dense write lock before publishing occupancy.
        let images = unsafe { images.assume_init() };
        Ok(Self {
            slots,
            images,
            allocation: Mutex::new(0),
        })
    }

    fn read(&self, slot: usize) -> DefinitionSlotRead<'_> {
        DefinitionSlotRead {
            state: self.slots[slot]
                .read()
                .expect("definition slot lock poisoned"),
            image: &self.images[slot],
        }
    }

    fn write(&self, slot: usize) -> DefinitionSlotWrite<'_> {
        DefinitionSlotWrite {
            state: self.slots[slot]
                .write()
                .expect("definition slot lock poisoned"),
            image: &self.images[slot],
        }
    }

    pub(super) fn references_type(
        &self,
        state: &DefinitionState,
        movement: super::type_reference_moves::TypeReferenceMove,
    ) -> bool {
        if movement.references_definition(&state.committed) {
            return true;
        }
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            if movement.references_definition(&entry.version.def) {
                return true;
            }
            tail = entry.previous;
        }
        false
    }

    pub(super) fn references_table(
        &self,
        state: &DefinitionState,
        movement: super::table_publication::TableReferenceMove,
    ) -> bool {
        if movement.references(state.committed()) {
            return true;
        }
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            if movement.references(&entry.version.def) {
                return true;
            }
            tail = entry.previous;
        }
        false
    }

    /// Allocates every replacement while the original definition owner is held.
    /// The caller must retain this complete plan through its durability boundary.
    pub(super) fn prepare_replacements_locked(
        &self,
        state: &DefinitionState,
        source: super::catalog_sources::CatalogObjectIdentity,
        pool: &std::sync::Arc<crate::mem::lease_pool::LeasePool>,
        mut rewrite: impl FnMut(&TableDef) -> Result<Option<TableDef>, SqlError>,
    ) -> Result<PreparedDefinitionReplacements, SqlError> {
        let committed = rewrite(state.committed())?;
        let count = state
            .pending
            .map_or(0, |head| self.read(head.tail as usize).depth as usize);
        let mut pending = pool
            .lease(count)
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            let replacement = rewrite(&entry.version.def)?;
            pending
                .push_reserved(PreparedPendingDefinition {
                    slot,
                    revision: entry.version.revision,
                    replacement,
                })
                .expect("reserved pending-definition count covers the locked chain");
            tail = entry.previous;
        }
        Ok(PreparedDefinitionReplacements {
            source,
            database: state.identity.database,
            created_at: state.identity.created_at,
            ddl_version: state.identity.ddl_version,
            existence: state.identity.existence,
            committed_revision: state.committed_revision,
            committed,
            pending: pending.freeze(),
        })
    }

    pub(super) fn validate_replacements_locked(
        &self,
        state: &DefinitionState,
        expected_source: super::catalog_sources::CatalogObjectIdentity,
        prepared: &PreparedDefinitionReplacements,
    ) -> Result<(), SqlError> {
        if expected_source != prepared.source
            || state.identity.database != prepared.database
            || state.identity.created_at != prepared.created_at
            || state.identity.ddl_version != prepared.ddl_version
            || (state.identity.existence != prepared.existence
                && !matches!((prepared.existence, state.identity.existence),
                    (CatalogDdlState::PendingCreate { txid }, CatalogDdlState::Present)
                        if state.publication_owner() == Some(txid)))
            || state.committed_revision != prepared.committed_revision
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table definition changed after replacement preparation"
            ));
        }
        let mut tail = state.pending.map(|head| head.tail);
        let mut captured = prepared.pending.iter();
        while let Some(slot) = tail {
            let current = self.read(slot as usize);
            if !captured
                .any(|entry| entry.slot == slot && entry.revision == current.version.revision)
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "pending table definition changed after replacement preparation"
                ));
            }
            tail = current.previous;
        }
        Ok(())
    }

    /// Applies only fully reserved replacements under the validated owner.
    pub(super) fn apply_replacements_locked(
        &self,
        state: &mut DefinitionState,
        expected_source: super::catalog_sources::CatalogObjectIdentity,
        prepared: PreparedDefinitionReplacements,
        revision: TableDefinitionRevision,
    ) -> bool {
        self.validate_replacements_locked(state, expected_source, &prepared)
            .expect("prepared definitions retain their validated publication owner");
        let mut changed = prepared.committed.is_some();
        if let Some(committed) = prepared.committed {
            *state.edit_committed(revision) = committed;
        }
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let current = self.read(slot as usize);
            let selected_revision = current.version.revision;
            let next = current.previous;
            drop(current);
            let captured = prepared
                .pending
                .iter()
                .find(|entry| entry.slot == slot && entry.revision == selected_revision)
                .expect("validated pending definition has a captured source");
            if let Some(replacement) = &captured.replacement {
                let mut owner = self.write(slot as usize);
                owner
                    .as_mut()
                    .expect("validated pending definition exists")
                    .version
                    .def = replacement.clone();
                changed = true;
            }
            tail = next;
        }
        if changed {
            state.committed_revision = revision;
        }
        if let Some(head) = &mut state.pending {
            let latest = self.read(head.tail as usize);
            head.schema = latest.version.def.schema;
            head.name = latest.version.def.name;
        }
        changed
    }

    #[cfg(test)]
    pub(super) fn stage(
        &self,
        definition: &TableDefinition,
        version: PendingTableDef,
        maximum_depth: u32,
    ) -> Result<(), SqlError> {
        self.stage_locked(&mut definition.write(), version, maximum_depth)
    }

    pub(super) fn stage_locked(
        &self,
        state: &mut DefinitionState,
        version: PendingTableDef,
        maximum_depth: u32,
    ) -> Result<(), SqlError> {
        if state.locked_by_other(version.txid).is_some() {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table definition is owned by another transaction"
            ));
        }
        let previous = state.pending.map(|head| head.tail);
        let depth = previous.map_or(Some(1), |slot| {
            self.read(slot as usize).depth.checked_add(1)
        });
        let Some(depth) = depth.filter(|&depth| depth <= maximum_depth) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "relation \"{}\" exceeds max_catalog_versions_per_object ({})",
                state.committed.name.as_str(),
                maximum_depth
            ));
        };
        // Lock order: table, allocation cursor, slot. A reader never retains
        // the cursor, and retirement first excludes readers of its own table.
        let mut cursor = self
            .allocation
            .lock()
            .expect("definition allocation lock poisoned");
        for distance in 0..self.slots.len() {
            let slot = (*cursor + distance) % self.slots.len();
            if *self.slots[slot]
                .read()
                .expect("definition slot lock poisoned")
            {
                continue;
            }
            let tail = u32::try_from(slot).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pending table-definition slot identity exhausted"
                )
            })?;
            let transaction = version.txid;
            let schema = version.def.schema;
            let name = version.def.name;
            self.write(slot).install(PendingTableDefSlot {
                previous,
                depth,
                version,
            });
            state.pending = Some(PendingDefinitionHead {
                tail,
                transaction,
                schema,
                name,
            });
            *cursor = (slot + 1) % self.slots.len();
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "pending table-definition pool is exhausted"
        ))
    }

    pub(super) fn latest_locked(&self, state: &DefinitionState) -> Option<PendingTableDef> {
        state
            .pending
            .map(|head| self.read(head.tail as usize).version.clone())
    }

    pub(super) fn try_for_each_locked(
        &self,
        state: &DefinitionState,
        mut visit: impl FnMut(&TableDef) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        visit(state.committed())?;
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            visit(&entry.version.def)?;
            tail = entry.previous;
        }
        Ok(())
    }

    pub(super) fn latest_index_identities(
        &self,
        state: &DefinitionState,
        output: &mut [Option<super::RelationIdentity>; super::MAX_VALUE_ENFORCERS],
    ) -> usize {
        let Some(head) = state.pending else {
            return 0;
        };
        let entry = self.read(head.tail as usize);
        let mut count = 0usize;
        for identity in entry.version.def.implicit_index_identities().flatten() {
            output[count] = Some(identity);
            count += 1;
        }
        count
    }

    pub(super) fn contains_index_identity(
        &self,
        state: &DefinitionState,
        identity: super::RelationIdentity,
    ) -> bool {
        if state
            .committed()
            .implicit_index_identities()
            .any(|candidate| candidate == Some(identity))
        {
            return true;
        }
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            if entry
                .version
                .def
                .implicit_index_identities()
                .any(|candidate| candidate == Some(identity))
            {
                return true;
            }
            tail = entry.previous;
        }
        false
    }

    pub(super) fn selected_revision(&self, state: &DefinitionState, transaction: u32) -> u64 {
        state
            .pending
            .filter(|head| head.transaction == transaction)
            .map_or(0, |head| self.read(head.tail as usize).version.revision)
    }

    pub(super) fn contains_revision(
        &self,
        state: &DefinitionState,
        transaction: u32,
        revision: u64,
    ) -> bool {
        let mut tail = state
            .pending
            .filter(|head| head.transaction == transaction)
            .map(|head| head.tail);
        while let Some(slot) = tail {
            let image = self.read(slot as usize);
            if image.version.revision == revision {
                return true;
            }
            tail = image.previous;
        }
        false
    }

    pub(super) fn project_columns(
        &self,
        state: &DefinitionState,
        transaction: u32,
        source_revision: u64,
        columns: &mut [u16],
    ) -> bool {
        if self.selected_revision(state, transaction) == source_revision {
            return true;
        }
        let mut tail = state
            .pending
            .filter(|head| head.transaction == transaction)
            .map(|head| head.tail);
        let mut mapping = core::array::from_fn::<_, MAX_COLUMNS, _>(|column| column as u16);
        let mut found = source_revision == 0;
        while let Some(slot) = tail {
            let image = self.read(slot as usize);
            if image.version.revision == source_revision {
                found = true;
                break;
            }
            let mut previous = [u16::MAX; MAX_COLUMNS];
            for (column, target) in image.version.transition.iter().copied().enumerate() {
                if target != u16::MAX {
                    previous[column] = mapping[usize::from(target)];
                }
            }
            mapping = previous;
            tail = image.previous;
        }
        if !found
            || columns.iter().any(|column| {
                mapping
                    .get(usize::from(*column))
                    .is_none_or(|target| *target == u16::MAX)
            })
        {
            return false;
        }
        for column in columns {
            *column = mapping[usize::from(*column)];
        }
        true
    }

    #[cfg(test)]
    pub(super) fn rollback(&self, definition: &TableDefinition, transaction: u32) -> bool {
        self.rollback_locked(&mut definition.write(), transaction)
    }

    pub(super) fn rollback_locked(&self, state: &mut DefinitionState, transaction: u32) -> bool {
        let Some(head) = state.pending.filter(|head| head.transaction == transaction) else {
            return false;
        };
        let _allocation = self
            .allocation
            .lock()
            .expect("definition allocation lock poisoned");
        let previous = self
            .write(head.tail as usize)
            .take()
            .expect("published definition slot exists")
            .previous;
        state.pending = previous.map(|tail| {
            let prior = self.read(tail as usize);
            PendingDefinitionHead {
                tail,
                transaction,
                schema: prior.version.def.schema,
                name: prior.version.def.name,
            }
        });
        true
    }

    pub(super) fn clear(&self, definition: &TableDefinition) {
        self.clear_locked(&mut definition.write());
    }

    pub(super) fn clear_locked(&self, state: &mut DefinitionState) {
        let _allocation = self
            .allocation
            .lock()
            .expect("definition allocation lock poisoned");
        let mut tail = state.pending.take().map(|head| head.tail);
        while let Some(slot) = tail {
            tail = self
                .write(slot as usize)
                .take()
                .expect("published definition slot exists")
                .previous;
        }
    }

    #[cfg(test)]
    pub(super) fn test_write(&self) -> Result<(), ()> {
        let _allocation = self.allocation.try_lock().map_err(|_| ())?;
        for slot in self.slots.iter() {
            drop(slot.try_write().map_err(|_| ())?);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.slots.len()
    }
}

impl Drop for DefinitionVersions {
    fn drop(&mut self) {
        for (occupied, image) in self.slots.iter_mut().zip(self.images.iter_mut()) {
            if *occupied.get_mut().expect("definition slot lock poisoned") {
                // Exclusive pool ownership excludes every slot guard.
                unsafe { image.0.get_mut().assume_init_drop() };
            }
        }
    }
}

struct DefinitionSlotRead<'a> {
    state: RwLockReadGuard<'a, bool>,
    image: &'a DefinitionVersionCell,
}

impl Deref for DefinitionSlotRead<'_> {
    type Target = PendingTableDefSlot;
    fn deref(&self) -> &Self::Target {
        assert!(*self.state, "published definition slot exists");
        // SAFETY: the matching read guard proves occupancy and prevents a
        // writer from changing or retiring this initialized payload.
        unsafe { (&*self.image.0.get()).assume_init_ref() }
    }
}

struct DefinitionSlotWrite<'a> {
    state: RwLockWriteGuard<'a, bool>,
    image: &'a DefinitionVersionCell,
}

impl DefinitionSlotWrite<'_> {
    fn as_mut(&mut self) -> Option<&mut PendingTableDefSlot> {
        if !*self.state {
            return None;
        }
        // SAFETY: occupancy proves initialization; the exclusive guard and
        // this mutable borrow exclude every other access to the payload.
        Some(unsafe { (&mut *self.image.0.get()).assume_init_mut() })
    }

    fn install(&mut self, value: PendingTableDefSlot) {
        assert!(!*self.state, "definition allocation owns an empty slot");
        // SAFETY: the write guard excludes readers and competing writers.
        // Publishing occupancy only after the write makes reads initialized.
        unsafe {
            (*self.image.0.get()).write(value);
        }
        *self.state = true;
    }

    fn take(&mut self) -> Option<PendingTableDefSlot> {
        if !*self.state {
            return None;
        }
        *self.state = false;
        // SAFETY: the occupied slot was initialized and is held exclusively.
        // Clearing occupancy transfers its value without touching empty pages.
        Some(unsafe { (&*self.image.0.get()).assume_init_read() })
    }
}

/// Table ownership prevents retirement until the selected slot is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableDefinitionRevision(DefinitionRevisionSource);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefinitionRevisionSource {
    Initial,
    Catalog(u64),
    Type {
        source: super::type_references::TypeIdentity,
        version: u64,
    },
}

impl TableDefinitionRevision {
    const INITIAL: Self = Self(DefinitionRevisionSource::Initial);

    pub(super) fn catalog(version: u64) -> Self {
        Self(DefinitionRevisionSource::Catalog(version))
    }

    pub(super) fn named_type(source: super::type_references::TypeIdentity, version: u64) -> Self {
        Self(DefinitionRevisionSource::Type { source, version })
    }
}

#[must_use]
pub(crate) struct CommittedDefinitionWrite<'a> {
    state: DefinitionWrite<'a>,
    revision: TableDefinitionRevision,
}

impl<'a> CommittedDefinitionWrite<'a> {
    pub(super) fn new(state: DefinitionWrite<'a>, revision: TableDefinitionRevision) -> Self {
        Self { state, revision }
    }
}

impl Deref for CommittedDefinitionWrite<'_> {
    type Target = TableDef;
    fn deref(&self) -> &Self::Target {
        &self.state.committed
    }
}

impl DerefMut for CommittedDefinitionWrite<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state.committed_revision = self.revision;
        &mut self.state.committed
    }
}

pub struct TableDefinitionRead<'a> {
    // Fields drop in order: release the slot before its table ownership.
    pending: Option<DefinitionSlotRead<'a>>,
    pub(super) state: RwLockReadGuard<'a, DefinitionState>,
}

impl Deref for TableDefinitionRead<'_> {
    type Target = TableDef;
    fn deref(&self) -> &TableDef {
        match &self.pending {
            Some(slot) => &slot.version.def,
            None => &self.state.committed,
        }
    }
}

impl<'a> TableDefinitionRead<'a> {
    pub(crate) fn identity(&self) -> TableIdentity {
        self.state.identity
    }

    pub(super) fn revisions(&self) -> (TableDefinitionRevision, u64) {
        (
            self.state.committed_revision,
            self.pending
                .as_ref()
                .map_or(0, |slot| slot.version.revision),
        )
    }

    pub(super) fn current(
        definition: &'a TableDefinition,
        versions: &'a DefinitionVersions,
    ) -> Self {
        let state = definition.read();
        let pending = state.pending.map(|head| versions.read(head.tail as usize));
        Self { state, pending }
    }

    pub(super) fn committed_definition(&self) -> &TableDef {
        &self.state.committed
    }

    pub(super) fn locked_by_other(&self, transaction: u32) -> Option<u32> {
        self.state.locked_by_other(transaction)
    }

    pub(super) fn committed(definition: &'a TableDefinition) -> Self {
        Self {
            state: definition.read(),
            pending: None,
        }
    }

    pub(super) fn visible(
        definition: &'a TableDefinition,
        versions: &'a DefinitionVersions,
        transaction: u32,
    ) -> Self {
        let state = definition.read();
        let pending = state
            .pending
            .filter(|head| head.transaction == transaction)
            .map(|head| versions.read(head.tail as usize));
        Self { state, pending }
    }
}

pub(super) struct PendingDefinitionRead<'a> {
    slot: DefinitionSlotRead<'a>,
    _state: RwLockReadGuard<'a, DefinitionState>,
}

impl Deref for PendingDefinitionRead<'_> {
    type Target = PendingTableDef;
    fn deref(&self) -> &PendingTableDef {
        &self.slot.version
    }
}

impl<'a> PendingDefinitionRead<'a> {
    pub(super) fn current(
        definition: &'a TableDefinition,
        versions: &'a DefinitionVersions,
    ) -> Option<Self> {
        let state = definition.read();
        let slot = versions.read(state.pending?.tail as usize);
        Some(Self {
            _state: state,
            slot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ColumnMeta, MAX_COLUMNS, SqlName};
    use super::*;

    fn metadata_pool() -> std::sync::Arc<crate::mem::lease_pool::LeasePool> {
        use crate::mem::lease_pool::{LeasePool, Purpose};
        let bytes = 4 * 1024 * 1024;
        let mut budget = Budget::new(LeasePool::budget_bytes(bytes, 1024));
        LeasePool::new(&mut budget, Purpose::TABLE_METADATA, bytes, 1024).unwrap()
    }

    fn definition(
        epoch: usize,
        pool: &std::sync::Arc<crate::mem::lease_pool::LeasePool>,
    ) -> TableDef {
        let mut definition = TableDefBuilder::empty();
        definition.name = SqlName::parse("guarded").unwrap();
        definition.n_columns = epoch;
        definition.has_toast = epoch % 2 == 1;
        definition.columns[0] = ColumnMeta::EMPTY;
        definition.attributes.initialize(epoch).unwrap();
        definition.freeze(pool).unwrap()
    }

    fn versions(capacity: usize) -> DefinitionVersions {
        let mut budget = Budget::new(DefinitionVersions::budget_bytes(capacity));
        let versions = DefinitionVersions::new(&mut budget, capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert_eq!(versions.capacity(), capacity);
        versions
    }

    fn pending(
        epoch: usize,
        transaction: u32,
        pool: &std::sync::Arc<crate::mem::lease_pool::LeasePool>,
    ) -> PendingTableDef {
        PendingTableDef {
            txid: transaction,
            revision: epoch as u64 + 1,
            def: definition(epoch, pool),
            transition: crate::mem::lease_pool::SharedSlice::EMPTY,
            column_mapping: crate::mem::lease_pool::SharedSlice::EMPTY,
            rewrites_rows: false,
            class_tuple_private: true,
        }
    }

    #[test]
    fn pending_table_definition_storage_rollback_restores_constraint_schema() {
        use super::super::tests::{make_def, test_budget, test_config};
        use super::super::{CommentClass, Storage, comment_stackstr};
        use crate::sql::types::ColType;

        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "storage_comment_target",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let schema = SqlName::parse("public").unwrap();
        let original = SqlName::parse("original_check").unwrap();
        let renamed = SqlName::parse("renamed_check").unwrap();
        storage
            .apply_comment(
                CommentClass::Constraint,
                schema,
                original,
                table as u32,
                Some(comment_stackstr("retained storage comment").unwrap()),
            )
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let (slot, prior) = storage
                .stage_constraint_comment_rename(table, schema, original, renamed, 7)
                .unwrap();
            let mut changed = TableDefBuilder::from_definition(&storage.table_def(table, 7));
            changed.schema = SqlName::parse("moved").unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(SqlName::parse("id").unwrap());
            let change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    changed,
                    &mapping,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            assert!(
                storage
                    .comment_text(
                        CommentClass::Constraint,
                        "moved",
                        "renamed_check",
                        table as u32,
                        7,
                    )
                    .is_some()
            );
            storage.rollback_table_def(change);
            assert!(
                storage
                    .comment_text(
                        CommentClass::Constraint,
                        "public",
                        "renamed_check",
                        table as u32,
                        7,
                    )
                    .is_some(),
                "constraint comment must return to the original schema at storage rollback"
            );
            assert!(
                storage
                    .comment_text(
                        CommentClass::Constraint,
                        "moved",
                        "renamed_check",
                        table as u32,
                        7,
                    )
                    .is_none()
            );
            storage.restore_comment_identity_pending(slot, prior);
            assert!(
                storage
                    .comment_text(
                        CommentClass::Constraint,
                        "public",
                        "original_check",
                        table as u32,
                        7,
                    )
                    .is_some()
            );
        });
    }

    #[test]
    fn table_definition_cells_keep_readers_initialized_through_retirement_and_reuse() {
        let pool = metadata_pool();
        let versions = versions(1);
        let first = TableDefinition::new(definition(1, &pool));
        let other = TableDefinition::new(definition(1, &pool));
        crate::mem::guard::forbid_alloc(|| {
            versions.stage(&first, pending(2, 7, &pool), 8).unwrap();
            let retained = PendingDefinitionRead::current(&first, &versions).unwrap();
            assert_eq!(retained.def.n_columns, 2);
            assert_eq!(
                versions
                    .stage(&other, pending(3, 9, &pool), 8)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(retained.def.n_columns, 2);
            let slot = first.read().pending.unwrap().tail as usize;
            assert!(versions.slots[slot].try_write().is_err());
            drop(retained);
            assert!(versions.rollback(&first, 7));
            assert!(!*versions.slots[slot].read().unwrap());
            versions.stage(&other, pending(3, 9, &pool), 8).unwrap();
            assert_eq!(
                PendingDefinitionRead::current(&other, &versions)
                    .unwrap()
                    .def
                    .n_columns,
                3
            );
            versions.clear(&other);
            assert!(!*versions.slots[slot].read().unwrap());
        });
    }

    #[test]
    fn live_definition_ownership_retains_committed_and_transaction_views() {
        let pool = metadata_pool();
        let versions = versions(2);
        let table = TableDefinition::new(definition(1, &pool));
        crate::mem::guard::forbid_alloc(|| {
            versions.stage(&table, pending(2, 7, &pool), 8).unwrap();
            let committed = TableDefinitionRead::visible(&table, &versions, 8);
            assert_eq!(committed.n_columns, 1);
            assert!(table.test_write().is_err());
            assert!(versions.test_write().is_ok());
            drop(committed);
            let visible = TableDefinitionRead::visible(&table, &versions, 7);
            assert_eq!(visible.n_columns, 2);
            assert!(table.test_write().is_err());
            assert!(versions.test_write().is_err());
            assert!(versions.allocation.try_lock().is_ok());
            drop(visible);
            let selected = PendingDefinitionRead::current(&table, &versions).unwrap();
            assert_eq!(selected.txid, 7);
            assert!(table.test_write().is_err());
            assert!(versions.test_write().is_err());
            assert!(versions.allocation.try_lock().is_ok());
            drop(selected);
            assert!(versions.test_write().is_ok());
            assert!(table.test_write().is_ok());
        });
    }

    #[test]
    fn live_definition_ownership_exhaustion_rollback_and_slot_reuse_are_atomic() {
        let pool = metadata_pool();
        let versions = versions(2);
        let table = TableDefinition::new(definition(1, &pool));
        let other = TableDefinition::new(definition(1, &pool));
        crate::mem::guard::forbid_alloc(|| {
            versions.stage(&table, pending(2, 7, &pool), 2).unwrap();
            versions.stage(&table, pending(3, 7, &pool), 2).unwrap();
            let error = versions.stage(&table, pending(4, 7, &pool), 2).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("max_catalog_versions_per_object")
            );
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 7).n_columns,
                3
            );
            let error = versions.stage(&other, pending(5, 9, &pool), 2).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("pending table-definition pool")
            );
            assert!(other.read().pending.is_none());
            assert!(!versions.rollback(&table, 9));
            assert!(versions.rollback(&table, 7));
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 7).n_columns,
                2
            );
            versions.stage(&other, pending(5, 9, &pool), 2).unwrap();
            assert!(versions.rollback(&table, 7));
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 7).n_columns,
                1
            );
            let error = versions.stage(&other, pending(6, 8, &pool), 2).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(
                TableDefinitionRead::visible(&other, &versions, 9).n_columns,
                5
            );
            versions.stage(&other, pending(6, 9, &pool), 2).unwrap();
            versions.clear(&other);
            assert!(PendingDefinitionRead::current(&other, &versions).is_none());
            versions.stage(&table, pending(7, 11, &pool), 2).unwrap();
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 7).n_columns,
                1
            );
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 11).n_columns,
                7
            );
            versions.clear(&table);
            assert!(versions.test_write().is_ok());
        });
    }

    #[test]
    fn live_definition_ownership_reader_allows_unrelated_publication_and_retirement() {
        let pool = metadata_pool();
        use std::sync::mpsc;

        let versions = versions(4);
        let retained_table = TableDefinition::new(definition(1, &pool));
        let other = TableDefinition::new(definition(1, &pool));
        versions
            .stage(&retained_table, pending(2, 7, &pool), 8)
            .unwrap();
        let retained = PendingDefinitionRead::current(&retained_table, &versions).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let result = crate::mem::guard::forbid_alloc(|| {
                    for epoch in 2..=50 {
                        versions.stage(&other, pending(epoch, 9, &pool), 8)?;
                        assert_eq!(
                            TableDefinitionRead::visible(&other, &versions, 9).n_columns,
                            epoch
                        );
                        assert!(versions.rollback(&other, 9));
                        versions.stage(&other, pending(epoch + 1, 9, &pool), 8)?;
                        versions.clear(&other);
                    }
                    Ok::<(), SqlError>(())
                });
                sender.send(result.is_ok()).unwrap();
            });
            let completed_before_release = receiver.recv_timeout(crate::test_wait::HANG);
            assert_eq!(retained.def.n_columns, 2);
            assert_eq!(retained.txid, 7);
            drop(retained);
            writer.join().unwrap();
            assert_eq!(completed_before_release, Ok(true));
        });
    }

    #[test]
    fn table_catalog_lifecycle_concurrent_readers_retain_one_identity_and_definition() {
        let pool = metadata_pool();
        let versions = versions(2);
        let table = TableDefinition::new(definition(1, &pool));
        {
            let mut state = table.write();
            state.identity.created_at = 1;
            state.identity.ownership.owner = 1;
            state.identity.existence = CatalogDdlState::Present;
        }
        versions.stage(&table, pending(2, 7, &pool), 8).unwrap();
        let start = crate::mem::mutex::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let table = &table;
                let versions = &versions;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let image = TableDefinitionRead::current(table, versions);
                            let identity = image.identity();
                            assert_eq!(
                                identity.created_at as usize,
                                image.committed_definition().n_columns
                            );
                            assert_eq!(
                                usize::from(identity.ownership.owner),
                                image.committed_definition().n_columns % 4
                            );
                            let expected_database = if identity.ownership.owner == 1 {
                                DatabaseOid::POSTGRES
                            } else {
                                DatabaseOid::TEMPLATE1
                            };
                            assert_eq!(identity.database, expected_database);
                            assert_eq!(image.n_columns, image.committed_definition().n_columns + 1);
                            assert_eq!(
                                image.pending.as_ref().unwrap().version.txid,
                                image.state.pending.unwrap().transaction
                            );
                            assert!(table.test_write().is_err());
                            match identity.existence {
                                CatalogDdlState::PendingCreate { txid } => {
                                    assert!(identity.existence.visible_to(txid));
                                    assert!(!identity.existence.visible_to(8));
                                    assert!(!identity.live());
                                }
                                CatalogDdlState::PendingDrop { txid } => {
                                    assert!(!identity.existence.visible_to(txid));
                                    assert!(identity.existence.visible_to(8));
                                    assert!(identity.live());
                                }
                                CatalogDdlState::PendingCreateDrop { .. }
                                | CatalogDdlState::Absent => {
                                    assert!(!identity.existence.visible_to(7));
                                    assert!(!identity.existence.visible_to(8));
                                    assert!(!identity.live());
                                }
                                CatalogDdlState::Present => {
                                    assert!(identity.live());
                                }
                            }
                            drop(image);
                            let own = TableDefinitionRead::visible(table, versions, 7);
                            if own.state.pending.is_some_and(|head| head.transaction == 7) {
                                assert_eq!(own.n_columns, own.committed_definition().n_columns + 1);
                                assert_eq!(own.pending.as_ref().unwrap().version.txid, 7);
                            } else {
                                assert_eq!(own.n_columns, own.committed_definition().n_columns);
                                assert!(own.pending.is_none());
                            }
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 1..=100 {
                    let mut state = table.write();
                    versions.clear_locked(&mut state);
                    let stamp = epoch * 2 + 1;
                    *state.edit_committed(TableDefinitionRevision::catalog(stamp as u64)) =
                        definition(stamp, &pool);
                    state.identity.created_at = stamp as u64;
                    state.identity.ownership.owner = (stamp % 4) as u16;
                    state.identity.database = if stamp % 4 == 1 {
                        DatabaseOid::POSTGRES
                    } else {
                        DatabaseOid::TEMPLATE1
                    };
                    let transaction = if epoch % 2 == 0 { 7 } else { 9 };
                    state.identity.existence = match epoch % 5 {
                        0 => CatalogDdlState::Present,
                        1 => CatalogDdlState::PendingCreate { txid: transaction },
                        2 => CatalogDdlState::PendingDrop { txid: transaction },
                        3 => CatalogDdlState::PendingCreateDrop { txid: transaction },
                        _ => CatalogDdlState::Absent,
                    };
                    versions
                        .stage_locked(&mut state, pending(stamp + 1, transaction, &pool), 8)
                        .unwrap();
                }
            });
        });
        assert_eq!(
            TableDefinitionRead::visible(&table, &versions, 7).n_columns,
            202
        );
        assert_eq!(
            TableDefinitionRead::visible(&table, &versions, 8).n_columns,
            201
        );
        assert!(versions.test_write().is_ok());
    }

    #[test]
    fn live_definition_ownership_independent_writers_share_capacity_without_aliasing() {
        let pool = metadata_pool();
        const WRITERS: usize = 4;
        let versions = versions(WRITERS * 2);
        let tables: [TableDefinition; WRITERS] =
            std::array::from_fn(|_| TableDefinition::new(definition(1, &pool)));
        let start = crate::mem::mutex::Barrier::new(WRITERS);
        std::thread::scope(|scope| {
            for (worker, table) in tables.iter().enumerate() {
                let versions = &versions;
                let start = &start;
                let pool = &pool;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let transaction = worker as u32 + 7;
                        for epoch in 1..=100 {
                            let width = (worker + 1) * 100 + epoch;
                            versions
                                .stage(table, pending(width, transaction, pool), 2)
                                .unwrap();
                            versions
                                .stage(table, pending(width + 1, transaction, pool), 2)
                                .unwrap();
                            assert_eq!(
                                TableDefinitionRead::visible(table, versions, transaction)
                                    .n_columns,
                                width + 1
                            );
                            assert_eq!(
                                TableDefinitionRead::visible(table, versions, 99).n_columns,
                                1
                            );
                            assert!(versions.rollback(table, transaction));
                            assert_eq!(
                                TableDefinitionRead::visible(table, versions, transaction)
                                    .n_columns,
                                width
                            );
                            versions.clear(table);
                            assert!(PendingDefinitionRead::current(table, versions).is_none());
                        }
                    });
                });
            }
        });
        assert!(versions.test_write().is_ok());
        for table in &tables {
            assert!(table.read().pending.is_none());
        }
    }

    #[test]
    fn live_definition_ownership_controls_and_pending_capacity_are_exactly_charged() {
        let pool = metadata_pool();
        let _ = versions(2);
        let mut short = Budget::new(DefinitionVersions::control_bytes() - 1);
        let error = match DefinitionVersions::new(&mut short, 0) {
            Ok(_) => panic!("undersized definition controls accepted"),
            Err(error) => error,
        };
        assert_eq!(error.what, "table definition version controls");
        let mut short = Budget::new(DefinitionVersions::budget_bytes(1) - 1);
        let error = match DefinitionVersions::new(&mut short, 1) {
            Ok(_) => panic!("undersized definition slot accepted"),
            Err(error) => error,
        };
        assert_eq!(error.what, "pending_table_defs");
        let empty = versions(0);
        let table = TableDefinition::new(definition(1, &pool));
        crate::mem::guard::forbid_alloc(|| {
            let error = empty.stage(&table, pending(2, 7, &pool), 8).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("pending table-definition pool")
            );
            assert!(table.read().pending.is_none());
        });
    }

    #[test]
    fn prepared_definition_replacements_preserve_old_pins_and_validate_the_source() {
        use super::super::tests::{make_def, test_budget, test_config};
        use crate::sql::types::ColType;
        let config = test_config();
        let mut first_budget = test_budget(&config);
        let mut first_storage = super::super::Storage::new(&config, &mut first_budget, 0).unwrap();
        let mut second_budget = test_budget(&config);
        let mut second_storage =
            super::super::Storage::new(&config, &mut second_budget, 0).unwrap();
        let first = first_storage
            .create_table(make_def("guarded", &[("id", ColType::Int4, false)]))
            .unwrap();
        let second = second_storage
            .create_table(make_def("guarded", &[("id", ColType::Int4, false)]))
            .unwrap();
        let identity = first_storage.tables[first].identity();
        let source = first_storage.catalog_object_identity(
            super::super::catalog_sources::CatalogObjectSlot::Table(first),
            identity.database,
            identity.created_at,
        );
        let other_slot = first_storage.catalog_object_identity(
            super::super::catalog_sources::CatalogObjectSlot::Table(first + 1),
            identity.database,
            identity.created_at,
        );
        let second_identity = second_storage.tables[second].identity();
        assert_eq!(first, second);
        assert_eq!(identity.database, second_identity.database);
        assert_eq!(identity.created_at, second_identity.created_at);
        let other_origin = second_storage.catalog_object_identity(
            super::super::catalog_sources::CatalogObjectSlot::Table(second),
            second_identity.database,
            second_identity.created_at,
        );
        let pool = &first_storage.metadata_pool;
        let versions = &first_storage.pending_table_defs;
        let table = &first_storage.tables[first].definition;
        versions.stage(table, pending(2, 7, pool), 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prior = {
                let owner = TableDefinitionRead::visible(table, versions, 7);
                (*owner).clone()
            };
            let mut state = table.write();
            let prepared = versions
                .prepare_replacements_locked(&state, source, pool, |original| {
                    let mut candidate = TableDefBuilder::from_definition(original);
                    candidate.name = SqlName::parse("replacement").unwrap();
                    Ok(Some(candidate.freeze(pool)?))
                })
                .unwrap();
            assert!(prepared.changed());
            for wrong_source in [other_slot, other_origin] {
                assert_eq!(
                    versions
                        .validate_replacements_locked(&state, wrong_source, &prepared)
                        .err()
                        .unwrap()
                        .sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
            }
            let original_revision = state.committed_revision;
            state.committed_revision = TableDefinitionRevision::catalog(99);
            assert_eq!(
                versions
                    .validate_replacements_locked(&state, source, &prepared)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            state.committed_revision = original_revision;
            versions
                .validate_replacements_locked(&state, source, &prepared)
                .unwrap();
            assert!(versions.apply_replacements_locked(
                &mut state,
                source,
                prepared,
                TableDefinitionRevision::catalog(100)
            ));
            assert_eq!(state.committed().name.as_str(), "replacement");
            assert_eq!(prior.name.as_str(), "guarded");
            assert_eq!(prior.n_columns, 2);
            assert_eq!(
                versions.latest_locked(&state).unwrap().def.name.as_str(),
                "replacement"
            );

            state.claim_publication(9).unwrap();
            assert_eq!(state.claim_publication(8), Err(9));
            assert_eq!(state.locked_by_other(8), Some(7));
            let retired_slot = state.pending.unwrap().tail;
            let rollback_plan = versions
                .prepare_replacements_locked(&state, source, pool, |original| {
                    if original.n_columns != 2 {
                        return Ok(None);
                    }
                    let mut candidate = TableDefBuilder::from_definition(original);
                    candidate.name = SqlName::parse("must_not_rewrite_reused_slot").unwrap();
                    Ok(Some(candidate.freeze(pool)?))
                })
                .unwrap();
            assert!(versions.rollback_locked(&mut state, 7));
            assert_eq!(state.publication_owner(), Some(9));
            assert_eq!(state.locked_by_other(8), Some(9));
            let unrelated = TableDefinition::new(definition(1, pool));
            for _ in 0..versions.capacity() {
                versions.stage(&unrelated, pending(3, 11, pool), 8).unwrap();
                if unrelated.read().pending.unwrap().tail == retired_slot {
                    break;
                }
                assert!(versions.rollback(&unrelated, 11));
            }
            assert_eq!(unrelated.read().pending.unwrap().tail, retired_slot);
            versions
                .validate_replacements_locked(&state, source, &rollback_plan)
                .unwrap();
            assert!(!versions.apply_replacements_locked(
                &mut state,
                source,
                rollback_plan,
                TableDefinitionRevision::catalog(101)
            ));
            assert_eq!(
                versions
                    .latest_locked(&unrelated.read())
                    .unwrap()
                    .def
                    .name
                    .as_str(),
                "guarded"
            );
            state.release_publication(8);
            assert_eq!(state.publication_owner(), Some(9));
            state.release_publication(9);
            assert_eq!(state.publication_owner(), None);
            versions.clear(&unrelated);
            state.identity.existence = CatalogDdlState::PendingCreate { txid: 9 };
            let creation = versions
                .prepare_replacements_locked(&state, source, pool, |_| Ok(None))
                .unwrap();
            state.identity.existence = CatalogDdlState::Present;
            assert!(
                versions
                    .validate_replacements_locked(&state, source, &creation)
                    .is_err()
            );
            state.claim_publication(8).unwrap();
            assert!(
                versions
                    .validate_replacements_locked(&state, source, &creation)
                    .is_err()
            );
            state.release_publication(8);
            state.claim_publication(9).unwrap();
            versions
                .validate_replacements_locked(&state, source, &creation)
                .unwrap();
            assert!(!versions.apply_replacements_locked(
                &mut state,
                source,
                creation,
                TableDefinitionRevision::catalog(102)
            ));
            state.release_publication(9);
        });
    }
}
