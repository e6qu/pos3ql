//! Guards retain committed definitions and the pending slots that issued a view.

use core::mem::size_of;
use core::ops::Deref;
use std::sync::{Mutex, RwLock, RwLockReadGuard};

use super::table_names::{
    CommittedDefinitionWrite, DefinitionWrite, IdentityWrite, NameIdentity, TableNameScope,
    TableNames,
};

use super::{
    CatalogDdlState, DatabaseOid, Ownership, PendingTableDef, PendingTableDefSlot, TableDef,
};
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(super) struct DefinitionState {
    pub(super) identity: TableIdentity,
    pub(super) committed: TableDef,
    pub(super) pending: Option<PendingDefinitionHead>,
    /// A CREATE RULE changes the visible history only for its transaction.
    pub(super) pending_has_rules_txid: Option<u32>,
}

impl DefinitionState {
    pub(super) fn locked_by_other(&self, transaction: u32) -> Option<u32> {
        self.identity
            .existence
            .pending_txid()
            .into_iter()
            .chain(self.pending.map(|head| head.transaction))
            .chain(self.pending_has_rules_txid)
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
    pub(crate) existence: CatalogDdlState,
}

impl TableIdentity {
    const EMPTY: Self = Self {
        database: DatabaseOid::POSTGRES,
        ownership: Ownership::BOOTSTRAP,
        created_at: 0,
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
            pending: None,
            pending_has_rules_txid: None,
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

    pub(super) fn try_write_for_batch(
        &self,
    ) -> Result<DefinitionWrite<'_>, super::row_ownership::LockWait<'_>> {
        let state = super::row_ownership::try_write(&self.state, "table definition lock poisoned")?;
        Ok(DefinitionWrite::shared(state, &self.names))
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

    pub(super) fn committed_mut(&mut self) -> CommittedDefinitionWrite<'_> {
        self.exclusive().into_committed()
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

/// Every slot is initialized at startup. Readers retain one slot, while the
/// allocation cursor is held only while publishing or retiring bounded links.
pub(super) struct DefinitionVersions {
    slots: FixedVec<RwLock<Option<PendingTableDefSlot>>>,
    allocation: Mutex<usize>,
}

impl DefinitionVersions {
    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) const fn budget_bytes(capacity: usize) -> usize {
        Self::control_bytes() + capacity * size_of::<RwLock<Option<PendingTableDefSlot>>>()
    }

    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        budget.draw(Self::control_bytes(), "table definition version controls")?;
        let mut slots = FixedVec::new(budget, "pending_table_defs", capacity)?;
        for _ in 0..capacity {
            slots
                .push(RwLock::new(None))
                .expect("startup slot capacity reserved");
        }
        Ok(Self {
            slots,
            allocation: Mutex::new(0),
        })
    }

    fn read(&self, slot: usize) -> DefinitionSlotRead<'_> {
        DefinitionSlotRead {
            state: self.slots[slot]
                .read()
                .expect("definition slot lock poisoned"),
        }
    }

    pub(super) fn validate_schema_rename(
        &self,
        definition: &TableDefinition,
        prior: super::SqlName,
        name: super::SqlName,
    ) -> Result<(), SqlError> {
        let state = definition.read();
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let entry = self.read(slot as usize);
            let mut copied = entry.version.def;
            super::rename_table_sql_identity(&mut copied, prior, name)?;
            tail = entry.previous;
        }
        Ok(())
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

    pub(super) fn move_type_references_locked(
        &self,
        state: &mut DefinitionState,
        movement: super::type_reference_moves::TypeReferenceMove,
    ) -> bool {
        let mut changed = movement.rewrite_definition(&mut state.committed);
        let mut tail = state.pending.map(|head| head.tail);
        while let Some(slot) = tail {
            let mut owner = self.slots[slot as usize]
                .write()
                .expect("definition slot lock poisoned");
            let entry = owner.as_mut().expect("published definition slot exists");
            changed |= movement.rewrite_definition(&mut entry.version.def);
            tail = entry.previous;
        }
        changed
    }

    /// Exclusive rewriting keeps the cached latest name aligned with its slot.
    pub(super) fn rename_schema(
        &mut self,
        definition: &mut TableDefinition,
        prior: super::SqlName,
        name: super::SqlName,
    ) -> Result<(), SqlError> {
        let mut state = definition.exclusive();
        let result: Result<(), SqlError> = (|| {
            let mut tail = state.pending.map(|head| head.tail);
            while let Some(slot) = tail {
                let entry = self.slots[slot as usize]
                    .get_mut()
                    .expect("definition slot lock poisoned")
                    .as_mut()
                    .expect("published definition slot exists");
                let pending = &mut entry.version;
                super::rename_schema_name(&mut pending.def.schema, prior, name);
                for column in pending.def.columns.iter_mut().take(pending.def.n_columns) {
                    super::rename_user_type_schema(&mut column.user_type, prior, name);
                }
                for key in pending.def.fkeys.iter_mut().take(pending.def.n_fkeys) {
                    super::rename_schema_name(&mut key.parent_schema, prior, name);
                }
                super::rename_table_sql_identity(&mut pending.def, prior, name)?;
                tail = entry.previous;
            }
            Ok(())
        })();
        if let Some(head) = &mut state.pending {
            let latest = self.read(head.tail as usize);
            head.schema = latest.version.def.schema;
            head.name = latest.version.def.name;
        }
        result
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
            if self.slots[slot]
                .read()
                .expect("definition slot lock poisoned")
                .is_some()
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
            *self.slots[slot]
                .write()
                .expect("definition slot lock poisoned") = Some(PendingTableDefSlot {
                previous,
                depth,
                version,
            });
            state.pending = Some(PendingDefinitionHead {
                tail,
                transaction,
                schema: version.def.schema,
                name: version.def.name,
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
            .map(|head| self.read(head.tail as usize).version)
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
        let previous = self.slots[head.tail as usize]
            .write()
            .expect("definition slot lock poisoned")
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

    fn clear_locked(&self, state: &mut DefinitionState) {
        let _allocation = self
            .allocation
            .lock()
            .expect("definition allocation lock poisoned");
        let mut tail = state.pending.take().map(|head| head.tail);
        while let Some(slot) = tail {
            tail = self.slots[slot as usize]
                .write()
                .expect("definition slot lock poisoned")
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

struct DefinitionSlotRead<'a> {
    state: RwLockReadGuard<'a, Option<PendingTableDefSlot>>,
}

impl Deref for DefinitionSlotRead<'_> {
    type Target = PendingTableDefSlot;
    fn deref(&self) -> &Self::Target {
        self.state
            .as_ref()
            .expect("published definition slot exists")
    }
}

/// Table ownership prevents retirement until the selected slot is released.
pub struct TableDefinitionRead<'a> {
    // Fields drop in order: release the slot before its table ownership.
    pending: Option<DefinitionSlotRead<'a>>,
    state: RwLockReadGuard<'a, DefinitionState>,
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

    fn definition(epoch: usize) -> TableDef {
        let mut definition = TableDef::empty();
        definition.name = SqlName::parse("guarded").unwrap();
        definition.n_columns = epoch;
        definition.has_toast = epoch % 2 == 1;
        definition.columns[0] = ColumnMeta::EMPTY;
        definition
    }

    fn versions(capacity: usize) -> DefinitionVersions {
        let mut budget = Budget::new(DefinitionVersions::budget_bytes(capacity));
        let versions = DefinitionVersions::new(&mut budget, capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert_eq!(versions.capacity(), capacity);
        versions
    }

    fn pending(epoch: usize, transaction: u32) -> PendingTableDef {
        PendingTableDef {
            txid: transaction,
            def: definition(epoch),
            column_mapping: [None; MAX_COLUMNS],
            rewrites_rows: false,
        }
    }

    #[test]
    fn pending_table_definition_storage_rollback_restores_constraint_schema() {
        use super::super::tests::{make_def, test_budget, test_config};
        use super::super::{CommentClass, Storage, comment_stackstr};
        use crate::sql::types::ColType;

        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
            let mut changed = *storage.table_def(table, 7);
            changed.schema = SqlName::parse("moved").unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(SqlName::parse("id").unwrap());
            storage
                .write_table_def(table, 7, changed, &mapping, false)
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
            storage.rollback_table_def(table, 7);
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
    fn live_definition_ownership_retains_committed_and_transaction_views() {
        let versions = versions(2);
        let table = TableDefinition::new(definition(1));
        crate::mem::guard::forbid_alloc(|| {
            versions.stage(&table, pending(2, 7), 8).unwrap();
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
        let versions = versions(2);
        let table = TableDefinition::new(definition(1));
        let other = TableDefinition::new(definition(1));
        crate::mem::guard::forbid_alloc(|| {
            versions.stage(&table, pending(2, 7), 2).unwrap();
            versions.stage(&table, pending(3, 7), 2).unwrap();
            let error = versions.stage(&table, pending(4, 7), 2).unwrap_err();
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
            let error = versions.stage(&other, pending(5, 9), 2).unwrap_err();
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
            versions.stage(&other, pending(5, 9), 2).unwrap();
            assert!(versions.rollback(&table, 7));
            assert_eq!(
                TableDefinitionRead::visible(&table, &versions, 7).n_columns,
                1
            );
            let error = versions.stage(&other, pending(6, 8), 2).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(
                TableDefinitionRead::visible(&other, &versions, 9).n_columns,
                5
            );
            versions.stage(&other, pending(6, 9), 2).unwrap();
            versions.clear(&other);
            assert!(PendingDefinitionRead::current(&other, &versions).is_none());
            versions.stage(&table, pending(7, 11), 2).unwrap();
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
        use std::sync::mpsc;
        use std::time::Duration;
        let versions = versions(4);
        let retained_table = TableDefinition::new(definition(1));
        let other = TableDefinition::new(definition(1));
        versions.stage(&retained_table, pending(2, 7), 8).unwrap();
        let retained = PendingDefinitionRead::current(&retained_table, &versions).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let result = crate::mem::guard::forbid_alloc(|| {
                    for epoch in 2..=50 {
                        versions.stage(&other, pending(epoch, 9), 8)?;
                        assert_eq!(
                            TableDefinitionRead::visible(&other, &versions, 9).n_columns,
                            epoch
                        );
                        assert!(versions.rollback(&other, 9));
                        versions.stage(&other, pending(epoch + 1, 9), 8)?;
                        versions.clear(&other);
                    }
                    Ok::<(), SqlError>(())
                });
                sender.send(result.is_ok()).unwrap();
            });
            let completed_before_release = receiver.recv_timeout(Duration::from_secs(3));
            assert_eq!(retained.def.n_columns, 2);
            assert_eq!(retained.txid, 7);
            drop(retained);
            writer.join().unwrap();
            assert_eq!(completed_before_release, Ok(true));
        });
    }

    #[test]
    fn table_catalog_lifecycle_concurrent_readers_retain_one_identity_and_definition() {
        let versions = versions(2);
        let table = TableDefinition::new(definition(1));
        {
            let mut state = table.write();
            state.identity.created_at = 1;
            state.identity.ownership.owner = 1;
            state.identity.existence = CatalogDdlState::Present;
        }
        versions.stage(&table, pending(2, 7), 8).unwrap();
        let start = std::sync::Barrier::new(4);
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
                    state.committed = definition(stamp);
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
                        .stage_locked(&mut state, pending(stamp + 1, transaction), 8)
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
        const WRITERS: usize = 4;
        let versions = versions(WRITERS * 2);
        let tables: [TableDefinition; WRITERS] =
            std::array::from_fn(|_| TableDefinition::new(definition(1)));
        let start = std::sync::Barrier::new(WRITERS);
        std::thread::scope(|scope| {
            for (worker, table) in tables.iter().enumerate() {
                let versions = &versions;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let transaction = worker as u32 + 7;
                        for epoch in 1..=100 {
                            let width = (worker + 1) * 100 + epoch;
                            versions
                                .stage(table, pending(width, transaction), 2)
                                .unwrap();
                            versions
                                .stage(table, pending(width + 1, transaction), 2)
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
        let table = TableDefinition::new(definition(1));
        crate::mem::guard::forbid_alloc(|| {
            let error = empty.stage(&table, pending(2, 7), 8).unwrap_err();
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
}
