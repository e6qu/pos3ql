//! Names are selected without retaining unrelated table definitions.

use core::ops::{Deref, DerefMut};
use std::sync::{RwLock, RwLockWriteGuard};

use super::table_definitions::{
    CommittedDefinitionWrite, DefinitionState, TableDefinitionRevision, TableIdentity,
};
use super::{CatalogDdlState, DatabaseOid, SqlName};

#[derive(Clone, Copy)]
pub(super) enum TableNameScope {
    Committed,
    Transaction(u32),
}

impl TableNameScope {
    pub(super) fn visible(self, existence: CatalogDdlState) -> bool {
        match self {
            Self::Committed => matches!(
                existence,
                CatalogDdlState::Present | CatalogDdlState::PendingDrop { .. }
            ),
            Self::Transaction(transaction) => existence.visible_to(transaction),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct NameIdentity {
    database: DatabaseOid,
    created_at: u64,
}

impl NameIdentity {
    pub(super) fn matches(self, identity: TableIdentity) -> bool {
        self.database == identity.database && self.created_at == identity.created_at
    }
}

#[derive(Clone, Copy)]
pub(super) struct TableNames {
    identity: NameIdentity,
    existence: CatalogDdlState,
    schema: SqlName,
    name: SqlName,
    pending: Option<(u32, SqlName, SqlName)>,
}

impl TableNames {
    pub(super) fn row_type_name_conflict(
        &self,
        database: DatabaseOid,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<bool, u32> {
        if self.identity.database != database || self.existence == CatalogDdlState::Absent {
            return Ok(false);
        }
        let matches = |schema_name: SqlName, relation_name: SqlName| {
            schema_name.as_str() == schema && relation_name.as_str() == name
        };
        let reserved = matches(self.schema, self.name)
            || self
                .pending
                .is_some_and(|(_, schema, name)| matches(schema, name));
        if reserved
            && let Some(owner) = self
                .pending
                .map(|(owner, _, _)| owner)
                .or_else(|| self.existence.pending_txid())
                .filter(|owner| *owner != transaction)
        {
            return Err(owner);
        }
        let (schema, name) = self
            .pending
            .filter(|(owner, _, _)| *owner == transaction)
            .map_or((self.schema, self.name), |(_, schema, name)| (schema, name));
        Ok(self.existence.visible_to(transaction) && matches(schema, name))
    }

    pub(super) fn from_state(state: &DefinitionState) -> Self {
        Self {
            identity: NameIdentity {
                database: state.identity.database,
                created_at: state.identity.created_at,
            },
            existence: state.identity.existence,
            schema: state.committed().schema,
            name: state.committed().name,
            pending: state
                .pending
                .map(|head| (head.transaction, head.schema, head.name)),
        }
    }

    pub(super) fn candidate(
        &self,
        scope: TableNameScope,
        database: DatabaseOid,
        schema: &str,
        name: &str,
    ) -> Option<NameIdentity> {
        if self.identity.database != database || !scope.visible(self.existence) {
            return None;
        }
        let (visible_schema, visible_name) = match scope {
            TableNameScope::Transaction(transaction) => self
                .pending
                .filter(|&(owner, _, _)| owner == transaction)
                .map_or((self.schema, self.name), |(_, schema, name)| (schema, name)),
            TableNameScope::Committed => (self.schema, self.name),
        };
        (visible_schema.as_str() == schema && visible_name.as_str() == name)
            .then_some(self.identity)
    }

    pub(super) fn reservation_candidate(
        &self,
        database: DatabaseOid,
        schema: &str,
        name: &str,
    ) -> Option<NameIdentity> {
        let matches = |candidate_schema: SqlName, candidate_name: SqlName| {
            candidate_schema.as_str() == schema && candidate_name.as_str() == name
        };
        (self.identity.database == database
            && (matches(self.schema, self.name)
                || self
                    .pending
                    .is_some_and(|(_, schema, name)| matches(schema, name))))
        .then_some(self.identity)
    }
}

enum StateWrite<'a> {
    Shared(RwLockWriteGuard<'a, DefinitionState>),
    Exclusive(&'a mut DefinitionState),
}

/// Every state writer publishes names before releasing its definition owner.
#[must_use]
pub(super) struct DefinitionWrite<'a> {
    state: StateWrite<'a>,
    names: &'a RwLock<TableNames>,
}

impl<'a> DefinitionWrite<'a> {
    pub(super) fn publish_names(&self) {
        *self.names.write().expect("table name lock poisoned") = TableNames::from_state(self);
    }

    pub(super) fn shared(
        state: RwLockWriteGuard<'a, DefinitionState>,
        names: &'a RwLock<TableNames>,
    ) -> Self {
        Self {
            state: StateWrite::Shared(state),
            names,
        }
    }

    pub(super) fn exclusive(state: &'a mut DefinitionState, names: &'a RwLock<TableNames>) -> Self {
        Self {
            state: StateWrite::Exclusive(state),
            names,
        }
    }

    pub(super) fn into_committed(
        self,
        revision: TableDefinitionRevision,
    ) -> CommittedDefinitionWrite<'a> {
        CommittedDefinitionWrite::new(self, revision)
    }

    pub(super) fn into_identity(self) -> IdentityWrite<'a> {
        IdentityWrite { state: self }
    }
}

impl Deref for DefinitionWrite<'_> {
    type Target = DefinitionState;
    fn deref(&self) -> &Self::Target {
        match &self.state {
            StateWrite::Shared(state) => state,
            StateWrite::Exclusive(state) => state,
        }
    }
}

impl DerefMut for DefinitionWrite<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.state {
            StateWrite::Shared(state) => state,
            StateWrite::Exclusive(state) => state,
        }
    }
}

impl Drop for DefinitionWrite<'_> {
    fn drop(&mut self) {
        self.publish_names();
    }
}

#[must_use]
pub(super) struct IdentityWrite<'a> {
    state: DefinitionWrite<'a>,
}

impl Deref for IdentityWrite<'_> {
    type Target = TableIdentity;
    fn deref(&self) -> &Self::Target {
        &self.state.identity
    }
}

impl DerefMut for IdentityWrite<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state.identity
    }
}

/// The visible tables of one transaction keyed by schema and name: an
/// open-addressed table of slots in caller-provided arena space.
pub(crate) struct VisibleTableNames<'a> {
    slots: &'a mut [u32],
    transaction: u32,
}

impl<'a> VisibleTableNames<'a> {
    pub(crate) const EMPTY: u32 = u32::MAX;

    pub(super) fn new(slots: &'a mut [u32], transaction: u32) -> Self {
        debug_assert!(slots.len().is_power_of_two());
        Self { slots, transaction }
    }

    fn start(&self, schema: &str, name: &str) -> usize {
        use core::hash::Hasher;
        let mut hash = crate::mem::fixed_map::Fnv1aHasher::default();
        hash.write(schema.as_bytes());
        hash.write_u8(0);
        hash.write(name.as_bytes());
        hash.finish() as usize & (self.slots.len() - 1)
    }

    /// Records `slot` unless a lower slot already holds the name.
    pub(super) fn insert_first(
        &mut self,
        storage: &super::Storage,
        slot: usize,
        schema: &str,
        name: &str,
    ) {
        let mask = self.slots.len() - 1;
        let mut at = self.start(schema, name);
        loop {
            match self.slots[at] {
                Self::EMPTY => {
                    self.slots[at] = u32::try_from(slot).expect("table slots fit u32");
                    return;
                }
                existing => {
                    let definition = storage.table_def(existing as usize, self.transaction);
                    if definition.schema.as_str() == schema && definition.name.as_str() == name {
                        return;
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// The slot `Storage::find_visible` returns for this name.
    pub(crate) fn find(&self, storage: &super::Storage, schema: &str, name: &str) -> Option<usize> {
        let mask = self.slots.len() - 1;
        let mut at = self.start(schema, name);
        loop {
            match self.slots[at] {
                Self::EMPTY => return None,
                existing => {
                    let definition = storage.table_def(existing as usize, self.transaction);
                    if definition.schema.as_str() == schema && definition.name.as_str() == name {
                        return Some(existing as usize);
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }
}
