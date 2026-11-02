//! Publish a table shape and its dependent names under one set of owners.

use super::catalog_sources::{CatalogObjectIdentity, CatalogObjectSlot};
use super::row_ownership::{LockWait, try_mutex};
use super::table_definitions::{
    DefinitionState, PreparedDefinitionReplacements, TableDefinitionRevision, TableIdentity,
};
use super::type_reference_moves::{DefinitionPublication, TableAttempt, TableOwner};
use super::{
    CatalogDdlState, CommentClass, DatabaseOid, DependencyClass, MAX_COLUMNS, SqlName, Storage,
    TableDef, TableDefBuilder, TableDefinitionChange, TypeCommitChange, rebind_sequence_column,
};
use crate::mem::lease_pool::{MutableSlice, SharedSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy)]
pub(super) enum DefinitionSource<'a> {
    Pending(TableDefinitionChange),
    Committed {
        source: CatalogObjectIdentity,
        committed_revision: TableDefinitionRevision,
        definition: &'a TableDef,
        column_mapping: &'a [Option<SqlName>; MAX_COLUMNS],
        revision: TableDefinitionRevision,
        rewrites_rows: bool,
    },
}

impl DefinitionSource<'_> {
    fn identity(self) -> CatalogObjectIdentity {
        match self {
            Self::Pending(change) => change.source,
            Self::Committed { source, .. } => source,
        }
    }

    pub(super) fn matches(self, storage: &Storage, state: &DefinitionState) -> bool {
        let identity = state.identity;
        self.identity().matches(
            storage,
            identity.database,
            identity.created_at,
            identity.existence,
        ) && match self {
            Self::Pending(change) => storage.pending_table_defs.contains_revision(
                state,
                change.transaction,
                change.revision,
            ),
            Self::Committed {
                committed_revision, ..
            } => state.committed_revision == committed_revision,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct TableReferenceMove {
    database: DatabaseOid,
    prior_schema: SqlName,
    prior_name: SqlName,
    schema: SqlName,
    name: SqlName,
}

impl TableReferenceMove {
    fn applies_to(self, identity: TableIdentity) -> bool {
        identity.database == self.database && identity.existence != CatalogDdlState::Absent
    }

    pub(super) fn references(self, definition: &TableDef) -> bool {
        (self.prior_schema != self.schema || self.prior_name != self.name)
            && definition
                .fkeys()
                .iter()
                .any(|key| key.parent_schema == self.prior_schema && key.parent == self.prior_name)
    }

    pub(super) fn rewrite(self, definition: &mut TableDefBuilder) -> bool {
        let mut changed = false;
        for key in definition.fkeys.iter_mut().take(definition.n_fkeys) {
            if key.parent_schema == self.prior_schema && key.parent == self.prior_name {
                key.parent_schema = self.schema;
                key.parent = self.name;
                changed = true;
            }
        }
        changed
    }
}

#[derive(Clone)]
pub(super) struct PreparedPrimaryTable {
    change: TableDefinitionChange,
    revision: TableDefinitionRevision,
    definition: TableDef,
    mapping: SharedSlice<Option<SqlName>>,
    rewrites_rows: bool,
}

pub(super) struct PreparedTablePublication {
    source: CatalogObjectIdentity,
    replacements: Option<PreparedDefinitionReplacements>,
    primary: Option<PreparedPrimaryTable>,
    retiring: bool,
    applied: bool,
}

pub(super) struct PreparedDefinitionTransaction {
    transaction: u32,
    revision: Option<TableDefinitionRevision>,
    table_changes: SharedSlice<TableDefinitionChange>,
    type_changes: SharedSlice<TypeCommitChange>,
    tables: MutableSlice<PreparedTablePublication>,
    metadata_generations: SharedSlice<(usize, u64)>,
    generation_tables: SharedSlice<usize>,
    definitions_finished: bool,
}

impl PreparedDefinitionTransaction {
    pub(super) fn transaction(&self) -> u32 {
        self.transaction
    }
}

#[derive(Clone, Copy)]
struct PreparedTableMove {
    change: TableDefinitionChange,
    movement: TableReferenceMove,
}

fn metadata_error(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error)
}

fn rewrite_definition(
    original: &TableDef,
    table_moves: &[PreparedTableMove],
    type_moves: &[super::type_reference_moves::TypeReferenceMove],
    pool: &std::sync::Arc<crate::mem::lease_pool::LeasePool>,
) -> Result<Option<TableDef>, SqlError> {
    let mut replacement = TableDefBuilder::from_definition(original);
    let mut changed = false;
    // Match every foreign key against its original spelling. A rename to an
    // earlier source's old name must not redirect it a second time.
    for (key, prior) in replacement.fkeys.iter_mut().zip(original.fkeys()) {
        if let Some(selected) = table_moves.iter().find(|selected| {
            prior.parent_schema == selected.movement.prior_schema
                && prior.parent == selected.movement.prior_name
        }) {
            key.parent_schema = selected.movement.schema;
            key.parent = selected.movement.name;
            changed |= key.parent_schema != prior.parent_schema || key.parent != prior.parent;
        }
    }
    for movement in type_moves {
        changed |= movement.rewrite_definition(&mut replacement);
    }
    changed.then(|| replacement.freeze(pool)).transpose()
}

impl DefinitionPublication {
    pub(super) fn metadata_generations(&self, transaction: u32) -> SharedSlice<(usize, u64)> {
        let registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        registry
            .iter()
            .find(|plan| plan.transaction == transaction)
            .map_or(SharedSlice::EMPTY, |plan| plan.metadata_generations.clone())
    }

    pub(super) fn retain_generation_tables(
        &self,
        storage: &Storage,
        transaction: u32,
        tables: SharedSlice<usize>,
    ) -> Result<(), SqlError> {
        let mut registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        if let Some(plan) = registry
            .iter_mut()
            .find(|plan| plan.transaction == transaction)
        {
            plan.generation_tables = tables;
        } else if !tables.is_empty() {
            registry
                .push(PreparedDefinitionTransaction {
                    transaction,
                    revision: None,
                    table_changes: SharedSlice::EMPTY,
                    type_changes: SharedSlice::EMPTY,
                    tables: storage
                        .metadata_pool
                        .lease(0)
                        .expect("empty definition plan needs no capacity"),
                    metadata_generations: SharedSlice::EMPTY,
                    generation_tables: tables,
                    definitions_finished: true,
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "definition publication owners exhausted"
                    )
                })?;
        }
        Ok(())
    }

    pub(super) fn generation_tables(&self, transaction: u32) -> SharedSlice<usize> {
        self.prepared
            .lock()
            .expect("definition publication registry poisoned")
            .iter()
            .find(|plan| plan.transaction == transaction)
            .map_or(SharedSlice::EMPTY, |plan| plan.generation_tables.clone())
    }

    pub(super) fn release_generation_tables(&self, transaction: u32) -> SharedSlice<usize> {
        let mut registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        let Some(index) = registry
            .iter()
            .position(|plan| plan.transaction == transaction)
        else {
            return SharedSlice::EMPTY;
        };
        let tables = core::mem::replace(&mut registry[index].generation_tables, SharedSlice::EMPTY);
        if registry[index].definitions_finished {
            registry.swap_remove(index);
        }
        tables
    }

    fn prepare_transaction(
        &self,
        storage: &Storage,
        transaction: u32,
        table_changes: impl Iterator<Item = TableDefinitionChange> + Clone,
        type_changes: impl Iterator<Item = TypeCommitChange> + Clone,
    ) -> Result<(), SqlError> {
        if table_changes
            .clone()
            .any(|change| change.transaction != transaction)
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table publication change belongs to another transaction"
            ));
        }
        let mut scratch = self
            .scratch
            .lock()
            .expect("definition publication scratch poisoned");
        let mut prepared = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        if let Some(existing) = prepared
            .iter()
            .find(|entry| entry.transaction == transaction)
        {
            if existing
                .table_changes
                .iter()
                .copied()
                .eq(table_changes.clone())
                && existing
                    .type_changes
                    .iter()
                    .copied()
                    .eq(type_changes.clone())
                && existing
                    .tables
                    .iter()
                    .filter_map(|target| target.primary.as_ref())
                    .all(|primary| {
                        let state = storage.tables[primary.change.table()].definition.read();
                        TableDefinitionRevision::catalog(
                            storage
                                .pending_table_defs
                                .selected_revision(&state, transaction),
                        ) == primary.revision
                    })
            {
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "transaction definitions changed after publication preparation"
            ));
        }
        if table_changes.clone().next().is_none() && type_changes.clone().next().is_none() {
            return Ok(());
        }
        if prepared.len() == prepared.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "definition publication owners exhausted"
            ));
        }
        scratch.reset();
        let mut slots = scratch
            .alloc_owned_slice_with(storage.tables.len(), |_| None)
            .expect("startup definition publication reservation covers every table");
        loop {
            let mut owners = TableAttempt {
                slots: &mut slots,
                #[cfg(test)]
                pause_after_release: None,
            };
            #[cfg(test)]
            self.pause_after_scan();
            let attempt: Result<_, LockWait<'_>> = (|| {
                for (slot, _) in storage.tables.iter().enumerate() {
                    if table_changes.clone().any(|change| change.table() == slot) {
                        Self::acquire_table(storage, slot, &mut owners)?;
                    }
                }
                let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
                let comments = try_mutex(&storage.comments, "comment catalog lock poisoned")?;
                Ok((types, comments))
            })();
            let (mut types, _comments) = match attempt {
                Ok(value) => value,
                Err(wait) => {
                    drop(owners);
                    #[cfg(test)]
                    self.waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                    continue;
                }
            };
            let table_count = table_changes
                .clone()
                .enumerate()
                .filter(|(index, change)| {
                    !table_changes
                        .clone()
                        .take(*index)
                        .any(|prior| prior.table() == change.table())
                })
                .count();
            let mut table_moves = storage
                .metadata_pool
                .lease(table_count)
                .map_err(metadata_error)?;
            for change in table_changes.clone() {
                if table_moves
                    .iter()
                    .any(|entry: &PreparedTableMove| entry.change.table() == change.table())
                {
                    continue;
                }
                let state = &owners.slots[change.table()]
                    .as_ref()
                    .expect("source table is owned")
                    .definition;
                if !DefinitionSource::Pending(change).matches(storage, state) {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "table publication source changed before preparation"
                    ));
                }
                let pending = storage
                    .pending_table_defs
                    .latest_locked(state)
                    .expect("validated table change retains a pending source");
                table_moves
                    .push_reserved(PreparedTableMove {
                        change,
                        movement: TableReferenceMove {
                            database: state.identity.database,
                            prior_schema: state.committed().schema,
                            prior_name: state.committed().name,
                            schema: pending.def.schema,
                            name: pending.def.name,
                        },
                    })
                    .expect("table move reservation covers the input changes");
            }
            let type_count = type_changes
                .clone()
                .filter_map(|change| match change {
                    TypeCommitChange::Definition(target) => target.identity_move(&types),
                    TypeCommitChange::Lifecycle(_) => None,
                })
                .count();
            let mut type_moves = storage
                .metadata_pool
                .lease(type_count)
                .map_err(metadata_error)?;
            for change in type_changes.clone() {
                if let TypeCommitChange::Definition(target) = change
                    && let Some(movement) = target.identity_move(&types)
                {
                    type_moves
                        .push_reserved(movement)
                        .expect("type move reservation covers the input changes");
                }
            }
            let attempt: Result<(), LockWait<'_>> = (|| {
                for selected in table_moves.iter() {
                    Self::acquire_table_dependents(storage, selected.movement, &mut owners)?;
                }
                for movement in type_moves.iter() {
                    Self::acquire_tables(storage, *movement, &mut owners)?;
                }
                Ok(())
            })();
            if let Err(wait) = attempt {
                drop(_comments);
                drop(types);
                drop(owners);
                #[cfg(test)]
                self.waits
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
                wait.wait();
                continue;
            }
            // SQL relation ownership spans WAL admission and promotion. It
            // excludes foreign writers while readers of immutable shapes and
            // operations on independent tables continue.
            for (slot, owner) in owners.slots.iter().enumerate() {
                if owner.is_some() {
                    storage.lock_table(
                        transaction,
                        slot,
                        crate::sql::ast::TableLockMode::Share,
                        false,
                    )?;
                }
            }
            let count = owners.slots.iter().flatten().count();
            let mut targets = storage.metadata_pool.lease(count).map_err(metadata_error)?;
            let mut captured_tables = storage
                .metadata_pool
                .lease(table_changes.clone().count())
                .map_err(metadata_error)?;
            for change in table_changes.clone() {
                captured_tables
                    .push_reserved(change)
                    .expect("table input reservation covers the iterator");
            }
            let mut captured_types = storage
                .metadata_pool
                .lease(type_changes.clone().count())
                .map_err(metadata_error)?;
            for change in type_changes.clone() {
                captured_types
                    .push_reserved(change)
                    .expect("type input reservation covers the iterator");
            }
            let revision = table_moves
                .first()
                .map(|selected| TableDefinitionRevision::catalog(selected.change.revision))
                .or_else(|| {
                    type_changes.clone().find_map(|change| match change {
                        TypeCommitChange::Definition(target) => {
                            Some(target.dependent_table_revision())
                        }
                        TypeCommitChange::Lifecycle(_) => None,
                    })
                });
            assert!(
                count == 0 || revision.is_some(),
                "table replacements retain a staged source revision"
            );
            for (slot, owner) in owners.slots.iter().enumerate() {
                let Some(owner) = owner else { continue };
                if let Some(blocker) = owner
                    .definition
                    .publication_owner()
                    .filter(|owner| *owner != transaction)
                {
                    return Err(storage.catalog_ddl_wait_error(
                        transaction,
                        blocker,
                        owner.definition.committed().name.as_str(),
                    ));
                }
                let identity = owner.definition.identity;
                let source = storage.catalog_object_identity(
                    CatalogObjectSlot::Table(slot),
                    identity.database,
                    identity.created_at,
                );
                let replacements = storage.pending_table_defs.prepare_replacements_locked(
                    &owner.definition,
                    source,
                    &storage.metadata_pool,
                    |definition| {
                        rewrite_definition(
                            definition,
                            table_moves.as_slice(),
                            type_moves.as_slice(),
                            &storage.metadata_pool,
                        )
                    },
                )?;
                let primary = if let Some(selected) = table_moves
                    .iter()
                    .find(|entry| entry.change.table() == slot)
                {
                    let pending = storage
                        .pending_table_defs
                        .latest_locked(&owner.definition)
                        .expect("owned primary retains the pending source");
                    let mut definition = rewrite_definition(
                        &pending.def,
                        table_moves.as_slice(),
                        type_moves.as_slice(),
                        &storage.metadata_pool,
                    )?
                    .unwrap_or_else(|| pending.def.clone());
                    definition.has_toast |= owner.definition.committed().has_toast;
                    definition.has_rules |= owner.definition.committed().has_rules;
                    definition.not_null_name_edits = super::ColumnSet::EMPTY;
                    Some(PreparedPrimaryTable {
                        change: selected.change,
                        revision: TableDefinitionRevision::catalog(pending.revision),
                        definition,
                        mapping: pending.column_mapping.clone(),
                        rewrites_rows: pending.rewrites_rows,
                    })
                } else {
                    None
                };
                targets
                    .push_reserved(PreparedTablePublication {
                        source,
                        replacements: Some(replacements),
                        primary,
                        retiring: identity.existence.pending_drop_txid() == Some(transaction),
                        applied: false,
                    })
                    .expect("target reservation covers all owned tables");
            }
            let mut generation_counts = storage
                .metadata_pool
                .lease(targets.len())
                .map_err(metadata_error)?;
            for target in targets.iter() {
                if target.primary.is_some()
                    || (!target.retiring
                        && target
                            .replacements
                            .as_ref()
                            .is_some_and(PreparedDefinitionReplacements::changed))
                {
                    let CatalogObjectSlot::Table(table) = target.source.slot() else {
                        unreachable!()
                    };
                    generation_counts
                        .push_reserved((table, 1))
                        .expect("generation count reservation covers targets");
                }
            }
            types.claim_definition_publication(storage, transaction, type_changes.clone())?;
            for owner in owners.slots.iter_mut().flatten() {
                owner
                    .definition
                    .claim_publication(transaction)
                    .expect("all table claim conflicts were checked under their owners");
            }
            prepared
                .push(PreparedDefinitionTransaction {
                    transaction,
                    revision,
                    table_changes: captured_tables.freeze(),
                    type_changes: captured_types.freeze(),
                    tables: targets,
                    metadata_generations: generation_counts.freeze(),
                    generation_tables: SharedSlice::EMPTY,
                    definitions_finished: false,
                })
                .expect("publication owner slot was reserved before metadata preparation");
            return Ok(());
        }
    }

    fn finish_transaction(&self, storage: &Storage, transaction: u32) -> Result<(), SqlError> {
        let mut scratch = self
            .scratch
            .lock()
            .expect("definition publication scratch poisoned");
        let mut registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        let Some(index) = registry
            .iter()
            .position(|plan| plan.transaction == transaction)
        else {
            return Ok(());
        };
        if registry[index].definitions_finished {
            return Ok(());
        }
        scratch.reset();
        let mut slots = scratch
            .alloc_owned_slice_with(storage.tables.len(), |_| None)
            .expect("startup definition publication reservation covers every table");
        loop {
            let mut owners = TableAttempt {
                slots: &mut slots,
                #[cfg(test)]
                pause_after_release: Some(&self.release_pause),
            };
            let attempt: Result<_, LockWait<'_>> = (|| {
                for target in registry[index].tables.iter() {
                    let CatalogObjectSlot::Table(slot) = target.source.slot() else {
                        unreachable!()
                    };
                    Self::acquire_table(storage, slot, &mut owners)?;
                }
                try_mutex(&storage.type_catalog, "type catalog lock poisoned")
            })();
            let mut types = match attempt {
                Ok(value) => value,
                Err(wait) => {
                    drop(owners);
                    wait.wait();
                    continue;
                }
            };
            for target in registry[index].tables.iter() {
                let CatalogObjectSlot::Table(slot) = target.source.slot() else {
                    unreachable!()
                };
                let state = &owners.slots[slot]
                    .as_ref()
                    .expect("planned table is owned")
                    .definition;
                if state.publication_owner() != Some(transaction)
                    || storage.catalog_object_identity(
                        CatalogObjectSlot::Table(slot),
                        state.identity.database,
                        state.identity.created_at,
                    ) != target.source
                    || (target.primary.is_some() && !target.applied)
                {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "prepared definition publication lost its source owner"
                    ));
                }
                if !target.applied
                    && !(target.retiring && state.identity.existence == CatalogDdlState::Absent)
                {
                    storage.pending_table_defs.validate_replacements_locked(
                        state,
                        target.source,
                        target
                            .replacements
                            .as_ref()
                            .expect("unapplied target retains its replacements"),
                    )?;
                }
            }
            let plan = &mut registry[index];
            for target in plan.tables.iter_mut() {
                let CatalogObjectSlot::Table(slot) = target.source.slot() else {
                    unreachable!()
                };
                let owner = owners.slots[slot].as_mut().expect("planned table is owned");
                if !target.applied
                    && !(target.retiring
                        && owner.definition.identity.existence == CatalogDdlState::Absent)
                    && storage.pending_table_defs.apply_replacements_locked(
                        &mut owner.definition,
                        target.source,
                        target
                            .replacements
                            .take()
                            .expect("unapplied target retains its replacements"),
                        plan.revision
                            .expect("table replacements retain a staged source revision"),
                    )
                {
                    owner.publication.publish_reserved_generation(transaction);
                    owner.publication.dirty = true;
                    owner.definition.publish_names();
                }
                owner.definition.release_publication(transaction);
            }
            types.release_definition_publication(transaction);
            plan.definitions_finished = true;
            return Ok(());
        }
    }

    fn rollback_transaction(&self, storage: &Storage, transaction: u32) {
        storage.release_commit_generations(transaction);
        let _scratch = self
            .scratch
            .lock()
            .expect("definition publication scratch poisoned");
        let mut registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        let Some(index) = registry
            .iter()
            .position(|plan| plan.transaction == transaction)
        else {
            return;
        };
        let plan = registry.swap_remove(index);
        for target in plan.tables.iter() {
            let CatalogObjectSlot::Table(slot) = target.source.slot() else {
                unreachable!()
            };
            storage.tables[slot]
                .definition
                .write()
                .release_publication(transaction);
        }
        storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned")
            .release_definition_publication(transaction);
    }

    fn acquire_table<'storage>(
        storage: &'storage Storage,
        slot: usize,
        owners: &mut TableAttempt<'_, 'storage>,
    ) -> Result<(), LockWait<'storage>> {
        if owners.slots[slot].is_none() {
            let table = &storage.tables[slot];
            let lifecycle = try_mutex(&table.lifecycle, "table lifecycle lock poisoned")?;
            let definition = table.definition.try_write_for_batch()?;
            let publication = try_mutex(&table.publication, "table publication lock poisoned")?;
            owners.slots[slot] = Some(TableOwner {
                definition,
                publication,
                _lifecycle: lifecycle,
            });
        }
        Ok(())
    }

    fn acquire_table_dependents<'storage>(
        storage: &'storage Storage,
        movement: TableReferenceMove,
        owners: &mut TableAttempt<'_, 'storage>,
    ) -> Result<(), LockWait<'storage>> {
        for (slot, table) in storage.tables.iter().enumerate() {
            if owners.slots[slot].is_some() {
                continue;
            }
            let definition = table.definition.try_read_for_batch()?;
            if !movement.applies_to(definition.identity)
                || !storage
                    .pending_table_defs
                    .references_table(&definition, movement)
            {
                continue;
            }
            drop(definition);
            Self::acquire_table(storage, slot, owners)?;
            let definition = &owners.slots[slot].as_ref().unwrap().definition;
            if !movement.applies_to(definition.identity)
                || !storage
                    .pending_table_defs
                    .references_table(definition, movement)
            {
                // Retirement or a private edit may have won the first scan.
                drop(owners.slots[slot].take());
            }
        }
        Ok(())
    }

    pub(super) fn publish_table(
        &self,
        storage: &Storage,
        source: DefinitionSource<'_>,
    ) -> Result<Option<bool>, SqlError> {
        let CatalogObjectSlot::Table(table) = source.identity().slot() else {
            unreachable!("table publication contains a table source");
        };
        if storage.tables.get(table).is_none() {
            return Ok(None);
        }
        let mut scratch = self
            .scratch
            .lock()
            .expect("definition publication scratch poisoned");
        let mut registry = self
            .prepared
            .lock()
            .expect("definition publication registry poisoned");
        let primary = match source {
            DefinitionSource::Pending(change) => Some(
                registry
                    .iter()
                    .find(|plan| plan.transaction == change.transaction)
                    .and_then(|plan| {
                        plan.tables.iter().find_map(|target| {
                            target
                                .primary
                                .as_ref()
                                .filter(|primary| primary.change.source == change.source)
                        })
                    })
                    .cloned()
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::SERIALIZATION_FAILURE,
                            "table publication was not prepared before durability"
                        )
                    })?,
            ),
            DefinitionSource::Committed { .. } => None,
        };
        scratch.reset();
        let mut slots = scratch
            .alloc_owned_slice_with(storage.tables.len(), |_| None)
            .expect("startup definition publication reservation covers every table");
        loop {
            let mut owners = TableAttempt {
                slots: &mut slots,
                #[cfg(test)]
                pause_after_release: matches!(source, DefinitionSource::Committed { .. })
                    .then_some(&self.release_pause),
            };
            let attempt: Result<_, LockWait<'_>> = (|| {
                Self::acquire_table(storage, table, &mut owners)?;
                let selected = &owners.slots[table].as_ref().unwrap().definition;
                if !source.matches(storage, selected) {
                    return Ok(None);
                }
                let (mut definition, mapping, revision, rewrites_rows, transaction) = match source {
                    DefinitionSource::Pending(change) => {
                        let primary = primary.as_ref().expect("pending publication is prepared");
                        (
                            primary.definition.clone(),
                            Ok(primary.mapping.clone()),
                            primary.revision,
                            primary.rewrites_rows,
                            Some(change.transaction),
                        )
                    }
                    DefinitionSource::Committed {
                        definition,
                        column_mapping,
                        revision,
                        rewrites_rows,
                        ..
                    } => (
                        definition.clone(),
                        storage
                            .metadata_pool
                            .copy_slice(&column_mapping[..selected.committed().n_columns])
                            .map_err(metadata_error),
                        revision,
                        rewrites_rows,
                        None,
                    ),
                };
                definition.has_toast |= selected.committed().has_toast;
                definition.has_rules |= selected.committed().has_rules;
                let movement = TableReferenceMove {
                    database: selected.identity.database,
                    prior_schema: selected.committed().schema,
                    prior_name: selected.committed().name,
                    schema: definition.schema,
                    name: definition.name,
                };
                if transaction.is_none() {
                    Self::acquire_table_dependents(storage, movement, &mut owners)?;
                }
                #[cfg(test)]
                self.pause_after_scan();
                // Creation binds under the type catalog; every private table edit
                // owns comments. Rescan while both submission gates are held.
                let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
                let comments = try_mutex(&storage.comments, "comment catalog lock poisoned")?;
                if transaction.is_none() {
                    Self::acquire_table_dependents(storage, movement, &mut owners)?;
                }
                let indexes = try_mutex(&storage.index_catalog, "index catalog lock poisoned")?;
                let sequences =
                    try_mutex(&storage.sequence_catalog, "sequence catalog lock poisoned")?;
                let dependencies = try_mutex(
                    &storage.stored_query_dependencies,
                    "stored-query dependency catalog lock poisoned",
                )?;
                Ok(Some((
                    definition,
                    mapping,
                    revision,
                    rewrites_rows,
                    transaction,
                    movement,
                    types,
                    comments,
                    indexes,
                    sequences,
                    dependencies,
                )))
            })();
            let Some((
                mut definition,
                mapping,
                revision,
                rewrites_rows,
                transaction,
                movement,
                _types,
                mut comments,
                mut indexes,
                mut sequences,
                mut dependencies,
            )) = (match attempt {
                Ok(value) => value,
                Err(wait) => {
                    drop(owners);
                    #[cfg(test)]
                    self.waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                    continue;
                }
            })
            else {
                return Ok(None);
            };
            let mapping = mapping?;
            definition.not_null_name_edits = super::ColumnSet::EMPTY;
            let mut local_replacements = if transaction.is_none() {
                Some(
                    storage
                        .metadata_pool
                        .lease(owners.slots.iter().flatten().count().saturating_sub(1))
                        .map_err(metadata_error)?,
                )
            } else {
                None
            };
            if transaction.is_none() {
                let mut builder = TableDefBuilder::from_definition(&definition);
                if movement.rewrite(&mut builder) {
                    definition = builder.freeze(&storage.metadata_pool)?;
                }
                for (slot, owner) in owners.slots.iter().enumerate() {
                    let Some(owner) = owner else { continue };
                    if slot == table {
                        continue;
                    }
                    let identity = owner.definition.identity;
                    let source = storage.catalog_object_identity(
                        CatalogObjectSlot::Table(slot),
                        identity.database,
                        identity.created_at,
                    );
                    let replacements = storage.pending_table_defs.prepare_replacements_locked(
                        &owner.definition,
                        source,
                        &storage.metadata_pool,
                        |prior| {
                            let mut builder = TableDefBuilder::from_definition(prior);
                            movement
                                .rewrite(&mut builder)
                                .then(|| builder.freeze(&storage.metadata_pool))
                                .transpose()
                        },
                    )?;
                    local_replacements
                        .as_mut()
                        .expect("immediate publication owns local replacements")
                        .push_reserved((slot, source, Some(replacements)))
                        .expect("local replacement reservation covers all dependent owners");
                }
            }
            // Every affected definition is published once. No catalog entry
            // changes before all counters and owners have been admitted.
            if transaction.is_none() {
                for owner in owners.slots.iter().flatten() {
                    owner
                        .publication
                        .preview_unreserved_generation()
                        .map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "table publication generation exhausted"
                            )
                        })?;
                }
            }
            let selected = &owners.slots[table].as_ref().unwrap().definition;
            let mut live_index_identities = [None; super::MAX_VALUE_ENFORCERS];
            let mut live_index_count = 0usize;
            for identity in definition.implicit_index_identities().flatten() {
                live_index_identities[live_index_count] = Some(identity);
                live_index_count += 1;
            }
            if let Some(transaction) = transaction {
                storage.promote_private_index_bindings_locked(
                    &mut indexes,
                    source.identity(),
                    selected,
                    transaction,
                );
            }
            for index in indexes.iter_mut().filter(|index| {
                index.database == movement.database
                    && index.schema == movement.prior_schema
                    && index.table == movement.prior_name
            }) {
                if index.ddl_state == CatalogDdlState::Present {
                    for comment in comments.entries.iter_mut().filter(|comment| {
                        comment.used
                            && comment.database == Some(movement.database)
                            && comment.class == CommentClass::Relation
                            && comment.subid == 0
                            && comment.schema == index.schema
                            && comment.name == index.name
                    }) {
                        comment.schema = movement.schema;
                    }
                    for key in 0..index.n_cols {
                        if index.expressions[key].is_none()
                            && let Some(name) = mapping
                                .get(usize::from(index.columns[key]))
                                .copied()
                                .flatten()
                            && let Some(column) = definition.column_index(name.as_str())
                        {
                            index.columns[key] = column as u16;
                        }
                    }
                    for column in index.include_columns.iter_mut().take(index.n_include_cols) {
                        if let Some(name) = mapping.get(usize::from(*column)).copied().flatten()
                            && let Some(target) = definition.column_index(name.as_str())
                        {
                            *column = target as u16;
                        }
                    }
                }
                index.schema = movement.schema;
                index.table = movement.name;
            }
            for sequence in sequences
                .definitions
                .iter_mut()
                .filter(|sequence| sequence.database == movement.database)
            {
                if sequence.ddl_state != CatalogDdlState::Absent
                    && sequence.owner.is_some_and(|owner| {
                        owner.table_schema == movement.prior_schema
                            && owner.table == movement.prior_name
                    })
                    && movement.prior_schema != movement.schema
                {
                    for comment in comments.entries.iter_mut().filter(|comment| {
                        comment.used
                            && comment.database == Some(movement.database)
                            && comment.class == CommentClass::Relation
                            && comment.subid == 0
                            && comment.schema == sequence.schema
                            && comment.name == sequence.name
                    }) {
                        comment.schema = movement.schema;
                    }
                    sequence.schema = movement.schema;
                }
                sequence.owner = rebind_sequence_column(
                    sequence.owner,
                    selected.committed(),
                    &definition,
                    mapping.as_slice(),
                    false,
                );
                sequence.generator_for = rebind_sequence_column(
                    sequence.generator_for,
                    selected.committed(),
                    &definition,
                    mapping.as_slice(),
                    true,
                );
                if let Some(pending) = &mut sequence.pending_definition {
                    if pending.owner.is_some_and(|owner| {
                        owner.table_schema == movement.prior_schema
                            && owner.table == movement.prior_name
                    }) && movement.prior_schema != movement.schema
                    {
                        pending.schema = movement.schema;
                    }
                    pending.owner = rebind_sequence_column(
                        pending.owner,
                        selected.committed(),
                        &definition,
                        mapping.as_slice(),
                        false,
                    );
                    pending.generator_for = rebind_sequence_column(
                        pending.generator_for,
                        selected.committed(),
                        &definition,
                        mapping.as_slice(),
                        true,
                    );
                }
            }
            for comment in comments.entries.iter_mut().filter(|comment| {
                comment.used
                    && comment.database == Some(movement.database)
                    && matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                    && comment.schema == movement.prior_schema
                    && comment.name == movement.prior_name
            }) {
                comment.schema = movement.schema;
                comment.name = movement.name;
                comment.pending_identity = None;
            }
            if let Some(transaction) = transaction {
                Storage::commit_index_table_names_locked(
                    &mut indexes,
                    &mut comments,
                    source.identity(),
                    transaction,
                );
                for comment in comments.entries.iter_mut().filter(|comment| {
                    comment.used
                        && comment.database == Some(movement.database)
                        && comment.class == CommentClass::Constraint
                        && comment.subid == table as u32
                        && comment
                            .pending_identity
                            .is_some_and(|identity| identity.txid == transaction)
                }) {
                    let identity = comment
                        .pending_identity
                        .take()
                        .expect("selected constraint comment");
                    comment.schema = identity.schema;
                    comment.name = identity.name;
                }
            }
            for (slot, owner) in owners.slots.iter_mut().enumerate() {
                let Some(owner) = owner else {
                    continue;
                };
                if slot == table {
                    *owner.definition.edit_committed(revision) = definition.clone();
                    if transaction.is_some() {
                        storage
                            .pending_table_defs
                            .clear_locked(&mut owner.definition);
                    }
                    if rewrites_rows {
                        owner.publication.row_layout = super::row_layout::RowLayout::installed_at(
                            owner.publication.generation + 1,
                        );
                    }
                } else {
                    let (_, identity, replacement) = local_replacements
                        .as_mut()
                        .expect("immediate publication owns local replacements")
                        .iter_mut()
                        .find(|(target, _, _)| *target == slot)
                        .expect("every immediate dependent retains its prepared replacement");
                    storage.pending_table_defs.apply_replacements_locked(
                        &mut owner.definition,
                        *identity,
                        replacement
                            .take()
                            .expect("dependent replacement applies once"),
                        revision,
                    );
                }
                if let Some(transaction) = transaction {
                    owner.publication.publish_reserved_generation(transaction);
                } else {
                    owner.publication.generation = owner
                        .publication
                        .preview_unreserved_generation()
                        .expect("immediate publication preflight retains all owners");
                }
                owner.publication.dirty = true;
            }
            if let Some(transaction) = transaction {
                let plan = registry
                    .iter_mut()
                    .find(|plan| plan.transaction == transaction)
                    .expect("pending publication retains its prepared transaction");
                let target = plan
                    .tables
                    .iter_mut()
                    .find(|target| target.source == source.identity())
                    .expect("pending publication retains its prepared primary table");
                target.applied = true;
                target.replacements = None;
            }
            storage.rename_stored_query_dependency_locked(
                &mut dependencies,
                DependencyClass::Table,
                table,
                None,
                definition.schema,
                definition.name,
            );
            // A dependent may have an earlier slot and release first. Its new
            // foreign key must already find the parent's name view, then wait
            // for the parent's definition instead of reporting it missing.
            owners.slots[table]
                .as_ref()
                .unwrap()
                .definition
                .publish_names();
            // A statistics reader may own its counters while reading a definition.
            drop(owners);
            drop(_types);
            drop(comments);
            drop(indexes);
            drop(sequences);
            drop(dependencies);
            storage.retain_implicit_index_statistics(
                movement.database,
                table,
                &live_index_identities[..live_index_count],
            );
            return Ok(Some(rewrites_rows));
        }
    }
}

impl Storage {
    /// Reserve the transaction's complete dependent shapes before submitting WAL.
    pub(crate) fn prepare_definition_publication(
        &self,
        transaction: u32,
        table_changes: impl Iterator<Item = TableDefinitionChange> + Clone,
        type_changes: impl Iterator<Item = TypeCommitChange> + Clone,
    ) -> Result<(), SqlError> {
        self.definition_publication.prepare_transaction(
            self,
            transaction,
            table_changes,
            type_changes,
        )
    }

    pub(crate) fn finish_definition_publication(&self, transaction: u32) -> Result<(), SqlError> {
        self.definition_publication
            .finish_transaction(self, transaction)
    }

    pub(crate) fn rollback_definition_publication(&self, transaction: u32) {
        self.definition_publication
            .rollback_transaction(self, transaction);
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{
        make_def, publish_table_definition as publish_change, test_budget, test_config,
    };
    use crate::storage::{ForeignKey, comment_stackstr};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize, usize, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let parent = storage
            .create_table(make_def(
                "publication_parent",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let mut child = make_def("publication_child", &[("id", ColType::Int4, false)]);
        child.n_fkeys = 1;
        child.fkeys[0] = ForeignKey {
            identity: None,
            parent_schema: SqlName::parse("public").unwrap(),
            parent: SqlName::parse("publication_parent").unwrap(),
            n_cols: 1,
            n_parent_cols: 1,
            ..ForeignKey::EMPTY
        };
        let child = storage.create_table(child).unwrap();
        let other = storage
            .create_table(make_def(
                "publication_other",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        (storage, parent, child, other)
    }

    fn mapping() -> [Option<SqlName>; MAX_COLUMNS] {
        let mut mapping = [None; MAX_COLUMNS];
        mapping[0] = Some(SqlName::parse("id").unwrap());
        mapping
    }

    fn rename(storage: &Storage, table: usize) -> TableDefinitionChange {
        let (target, definition) = storage.select_table_definition(table, 7);
        let mut definition = TableDefBuilder::from_definition(&definition);
        definition.schema = SqlName::parse("publication_destination").unwrap();
        definition.name = SqlName::parse("publication_renamed").unwrap();
        storage
            .write_table_def(
                target,
                definition,
                &mapping(),
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap()
    }

    fn fill_metadata(storage: &Storage) -> [Option<MutableSlice<u8>>; 128] {
        let mut held = core::array::from_fn(|_| None);
        for entry in &mut held {
            let available = storage.metadata_pool.lease::<u8>(usize::MAX).unwrap_err();
            if available.largest_range_bytes == 0 || available.available_leases == 0 {
                return held;
            }
            *entry = Some(
                storage
                    .metadata_pool
                    .lease(available.largest_range_bytes)
                    .unwrap(),
            );
        }
        panic!("small publication fixture has more than 128 free metadata ranges")
    }

    #[test]
    fn dependent_publication_reserves_before_effects_and_finishes_with_no_metadata_free() {
        let (storage, parent, child, _) = fixture();
        let change = rename(&storage, parent);
        let retained = (*storage.table_def(child, 0)).clone();
        crate::mem::guard::forbid_alloc(|| {
            let held = fill_metadata(&storage);
            let error = storage
                .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            for slot in [parent, child] {
                assert_eq!(
                    storage.tables[slot].definition.read().publication_owner(),
                    None
                );
            }
            assert_eq!(
                storage.table_def(parent, 0).name.as_str(),
                "publication_parent"
            );
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_parent"
            );
            drop(held);
            storage
                .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                .unwrap();
            storage
                .prepare_commit_generations(7, core::iter::empty(), core::iter::empty())
                .unwrap();
            let held = fill_metadata(&storage);
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
            storage.finish_definition_publication(7).unwrap();
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_renamed"
            );
            assert_eq!(retained.fkeys()[0].parent.as_str(), "publication_parent");
            assert_eq!(
                storage.tables[child].definition.read().publication_owner(),
                None
            );
            drop(held);
        });
    }

    #[test]
    fn dependent_publication_composes_two_sources_into_one_target_generation() {
        let (mut storage, parent, child, other) = fixture();
        {
            let revision = storage.reserve_table_definition_revision().unwrap();
            let mut owner = storage.tables[child].definition_mut(revision);
            let mut definition = TableDefBuilder::from_definition(&owner);
            definition.n_fkeys = 2;
            definition.fkeys[1] = definition.fkeys[0];
            definition.fkeys[1].parent = SqlName::parse("publication_other").unwrap();
            *owner = definition.freeze(&storage.metadata_pool).unwrap();
        }
        let first = rename(&storage, parent);
        let (target, definition) = storage.select_table_definition(other, 7);
        let mut definition = TableDefBuilder::from_definition(&definition);
        definition.name = SqlName::parse("other_renamed").unwrap();
        let second = storage
            .write_table_def(
                target,
                definition,
                &mapping(),
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let before = storage.tables[child].publication().generation;
        crate::mem::guard::forbid_alloc(|| {
            storage
                .prepare_definition_publication(7, [first, second].into_iter(), core::iter::empty())
                .unwrap();
            storage
                .prepare_commit_generations(7, core::iter::empty(), core::iter::empty())
                .unwrap();
            let held = fill_metadata(&storage);
            storage.commit_table_def(first).unwrap();
            storage.commit_table_def(second).unwrap();
            storage.finish_definition_publication(7).unwrap();
            let definition = storage.table_def(child, 0);
            assert_eq!(definition.fkeys()[0].parent.as_str(), "publication_renamed");
            assert_eq!(definition.fkeys()[1].parent.as_str(), "other_renamed");
            assert_eq!(storage.tables[child].publication().generation, before + 1);
            drop(held);
        });
    }

    #[test]
    fn canceling_dependent_publication_releases_claims_and_keeps_original_definitions() {
        let (storage, parent, child, _) = fixture();
        let change = rename(&storage, parent);
        crate::mem::guard::forbid_alloc(|| {
            storage
                .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                .unwrap();
            assert_eq!(
                storage.tables[child].definition.read().publication_owner(),
                Some(7)
            );
            storage.rollback_definition_publication(7);
            storage.rollback_table_def(change);
            for slot in [parent, child] {
                assert_eq!(
                    storage.tables[slot].definition.read().publication_owner(),
                    None
                );
            }
            assert_eq!(
                storage.table_def(parent, 0).name.as_str(),
                "publication_parent"
            );
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_parent"
            );
            assert!(
                storage
                    .definition_publication
                    .prepared
                    .lock()
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn dependent_publication_waits_for_writers_before_claims_and_allows_readers() {
        use crate::sql::ast::TableLockMode;
        let (storage, parent, child, other) = fixture();
        let change = rename(&storage, parent);
        crate::mem::guard::forbid_alloc(|| {
            storage
                .lock_table(9, child, TableLockMode::RowExclusive, false)
                .unwrap();
            assert_eq!(
                storage
                    .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert!(storage.definition_publication.scratch.try_lock().is_ok());
            assert!(storage.definition_publication.prepared.try_lock().is_ok());
            assert!(storage.type_catalog.try_lock().is_ok());
            for slot in [parent, child] {
                assert_eq!(
                    storage.tables[slot].definition.read().publication_owner(),
                    None
                );
                assert!(storage.tables[slot].lifecycle.try_lock().is_ok());
                assert!(storage.tables[slot].publication.try_lock().is_ok());
            }
            storage
                .lock_table(10, child, TableLockMode::AccessShare, false)
                .unwrap();
            storage
                .lock_table(11, other, TableLockMode::RowExclusive, false)
                .unwrap();
            storage.release_object_locks(9);
            storage
                .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                .unwrap();
            assert_eq!(
                storage
                    .lock_table(12, child, TableLockMode::RowExclusive, true)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::LOCK_NOT_AVAILABLE
            );
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_parent"
            );
            storage
                .prepare_commit_generations(7, core::iter::empty(), core::iter::empty())
                .unwrap();
            storage.commit_table_def(change).unwrap();
            storage.finish_definition_publication(7).unwrap();
            storage.release_object_locks(7);
            storage.release_row_locks(7);
            storage
                .lock_table(12, child, TableLockMode::RowExclusive, true)
                .unwrap();
            for transaction in [10, 11, 12] {
                storage.release_object_locks(transaction);
            }
        });
    }

    #[test]
    fn admitted_parent_publication_blocks_new_foreign_keys_without_blocking_self_references() {
        let (storage, parent, child, other) = fixture();
        let change = rename(&storage, parent);
        crate::mem::guard::forbid_alloc(|| {
            storage
                .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                .unwrap();
            let mut definition = TableDefBuilder::from_definition(&storage.table_def(child, 0));
            definition.name = SqlName::parse("blocked_fk_child").unwrap();
            assert_eq!(
                storage.create_table_in(definition, 9).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert!(
                storage
                    .find_visible("public", "blocked_fk_child", 9)
                    .is_none()
            );
            let selected = storage.select_table_definition(other, 9).0;
            definition.name = SqlName::parse("publication_other").unwrap();
            assert_eq!(
                storage
                    .write_table_def(
                        selected,
                        definition,
                        &mapping(),
                        false,
                        crate::storage::ColumnSet::EMPTY,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert!(storage.pending_table_def(other).is_none());
            definition.fkeys[0].parent = definition.name;
            let self_reference = storage
                .write_table_def(
                    selected,
                    definition,
                    &mapping(),
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            storage.rollback_table_def(self_reference);
            storage.rollback_definition_publication(7);
            storage.rollback_table_def(change);
            storage.release_object_locks(7);
            definition.name = SqlName::parse("admitted_fk_child").unwrap();
            definition.fkeys[0].parent = SqlName::parse("publication_parent").unwrap();
            let admitted = storage.create_table_in(definition, 9).unwrap();
            storage.rollback_create(admitted);
            storage.release_row_locks(9);
        });
    }

    fn waited(storage: &Storage, before: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while storage.definition_publication.waits.load(Ordering::Acquire) == before {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    #[test]
    fn table_publication_catalog_wait_releases_all_owners_and_allows_other_rows() {
        for blocked in 0..5 {
            let (storage, parent, child, other) = fixture();
            let change = rename(&storage, parent);
            let before = storage.definition_publication.waits.load(Ordering::Acquire);
            let types = (blocked == 0).then(|| storage.type_catalog.lock().unwrap());
            let comments = (blocked == 1).then(|| storage.comments.lock().unwrap());
            let indexes = (blocked == 2).then(|| storage.index_catalog.lock().unwrap());
            let sequences = (blocked == 3).then(|| storage.sequence_catalog.lock().unwrap());
            let dependencies =
                (blocked == 4).then(|| storage.stored_query_dependencies.lock().unwrap());
            std::thread::scope(|scope| {
                let writer = scope
                    .spawn(|| crate::mem::guard::forbid_alloc(|| publish_change(&storage, change)));
                let waiting = waited(&storage, before);
                let released = waiting
                    && [parent, child].into_iter().all(|slot| {
                        storage.tables[slot].lifecycle.try_lock().is_ok()
                            && storage.tables[slot].definition.test_write().is_ok()
                            && storage.tables[slot].publication.try_lock().is_ok()
                    })
                    && (blocked == 0 || storage.type_catalog.try_lock().is_ok())
                    && (blocked == 1 || storage.comments.try_lock().is_ok())
                    && (blocked == 2 || storage.index_catalog.try_lock().is_ok())
                    && (blocked == 3 || storage.sequence_catalog.try_lock().is_ok())
                    && (blocked == 4 || storage.stored_query_dependencies.try_lock().is_ok());
                let progressed = if released {
                    crate::mem::guard::forbid_alloc(|| {
                        let home = storage.heap.append_row(&[Datum::Int4(42)]).unwrap();
                        storage.write_pending(other, 1, 9, 1, Some(home)).unwrap();
                        storage.commit_row(other, 1, 9, 10).unwrap();
                        let _snapshot = storage.nested_statement_snapshot(10);
                        storage.visible_row_count(other, 0).unwrap() == 1
                    })
                } else {
                    false
                };
                drop((types, comments, indexes, sequences, dependencies));
                let result = writer.join().unwrap();
                assert!(
                    waiting && released && progressed,
                    "blocked catalog {blocked}"
                );
                assert_eq!(result.unwrap(), Some(false));
            });
            crate::mem::guard::forbid_alloc(|| {
                let parent = storage.table_def(parent, 0);
                let child = storage.table_def(child, 0);
                assert_eq!(parent.name.as_str(), "publication_renamed");
                assert_eq!(child.fkeys()[0].parent, parent.name);
                assert_eq!(child.fkeys()[0].parent_schema, parent.schema);
            });
        }
    }

    #[test]
    fn table_publication_updates_every_private_foreign_key_version_and_rollback() {
        let (storage, parent, child, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let (target, definition) = storage.select_table_definition(child, 9);
            let mut definition = TableDefBuilder::from_definition(&definition);
            definition.columns[0].not_null = super::super::NotNullOrigin::Local;
            let first = storage
                .write_table_def(
                    target,
                    definition,
                    &mapping(),
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let (target, definition) = storage.select_table_definition(child, 9);
            let mut definition = TableDefBuilder::from_definition(&definition);
            definition.columns[0].not_null = super::super::NotNullOrigin::Nullable;
            let second = storage
                .write_table_def(
                    target,
                    definition,
                    &mapping(),
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let change = rename(&storage, parent);
            assert_eq!(publish_change(&storage, change).unwrap(), Some(false));
            for transaction in [0, 9] {
                let child = storage.table_def(child, transaction);
                assert_eq!(child.fkeys()[0].parent.as_str(), "publication_renamed");
                assert_eq!(
                    child.fkeys()[0].parent_schema.as_str(),
                    "publication_destination"
                );
            }
            storage.rollback_table_def(second);
            assert_eq!(
                storage.table_def(child, 9).columns[0].not_null,
                super::super::NotNullOrigin::Local
            );
            assert_eq!(
                storage.table_def(child, 9).fkeys()[0].parent.as_str(),
                "publication_renamed"
            );
            assert_eq!(
                storage
                    .prepare_table_definition_enforcers(first.row_target())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE,
                "already prepared row input must retain its original committed revision"
            );
            assert_eq!(publish_change(&storage, first).unwrap(), Some(false));
            assert!(storage.pending_table_def(child).is_none());
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_renamed"
            );
        });
    }

    #[test]
    fn table_publication_reader_wait_releases_source_and_retains_selected_definition() {
        let (storage, parent, child, _) = fixture();
        let change = rename(&storage, parent);
        let before = storage.definition_publication.waits.load(Ordering::Acquire);
        let retained = storage.table_def(child, 0);
        std::thread::scope(|scope| {
            let writer = scope
                .spawn(|| crate::mem::guard::forbid_alloc(|| publish_change(&storage, change)));
            let waiting = waited(&storage, before);
            let released = waiting
                && storage.tables[parent].definition.test_write().is_ok()
                && storage.tables[parent].publication.try_lock().is_ok()
                && storage.type_catalog.try_lock().is_ok()
                && storage.comments.try_lock().is_ok();
            let old = retained.fkeys()[0].parent.as_str() == "publication_parent";
            drop(retained);
            let result = writer.join().unwrap();
            assert!(waiting && released && old);
            assert_eq!(result.unwrap(), Some(false));
        });
        assert_eq!(
            storage.table_def(child, 0).fkeys()[0].parent.as_str(),
            "publication_renamed"
        );
    }

    #[test]
    fn table_publication_wait_revalidates_rolled_back_source_without_catalog_changes() {
        let (storage, parent, child, _) = fixture();
        let change = rename(&storage, parent);
        let before = storage.definition_publication.waits.load(Ordering::Acquire);
        let held = storage.sequence_catalog.lock().unwrap();
        std::thread::scope(|scope| {
            let writer = scope
                .spawn(|| crate::mem::guard::forbid_alloc(|| publish_change(&storage, change)));
            let waiting = waited(&storage, before);
            if waiting {
                crate::mem::guard::forbid_alloc(|| storage.rollback_table_def(change));
            }
            drop(held);
            let result = writer.join().unwrap();
            assert!(waiting);
            assert_eq!(result.unwrap(), None);
        });
        assert_eq!(
            storage.table_def(parent, 0).name.as_str(),
            "publication_parent"
        );
        assert_eq!(
            storage.table_def(child, 0).fkeys()[0].parent.as_str(),
            "publication_parent"
        );
    }

    #[test]
    fn table_publication_exhaustion_preserves_comments_and_pending_versions_for_retry() {
        let (storage, parent, child, _) = fixture();
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("publication_parent").unwrap();
        let (comment, _) = storage
            .set_comment(
                CommentClass::Relation,
                schema,
                name,
                0,
                Some(comment_stackstr("retained table comment").unwrap()),
                7,
            )
            .unwrap()
            .unwrap();
        storage.commit_comment(comment, 7);
        crate::mem::guard::forbid_alloc(|| {
            let change = rename(&storage, parent);
            let generation = storage.tables[child].generation();
            storage.tables[child].publication().generation = u64::MAX;
            assert_eq!(
                publish_change(&storage, change).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.table_def(parent, 0).name, name);
            assert_eq!(
                storage.table_def(parent, 7).name.as_str(),
                "publication_renamed"
            );
            assert!(
                storage
                    .comment_text(CommentClass::Relation, "public", name.as_str(), 0, 0)
                    .is_some()
            );
            assert!(storage.tables[parent].lifecycle.try_lock().is_ok());
            assert!(storage.tables[child].definition.test_write().is_ok());
            assert!(storage.definition_publication.scratch.try_lock().is_ok());
            storage.tables[child].publication().generation = generation;
            assert_eq!(publish_change(&storage, change).unwrap(), Some(false));
            assert!(
                storage
                    .comment_text(
                        CommentClass::Relation,
                        "publication_destination",
                        "publication_renamed",
                        0,
                        0
                    )
                    .is_some()
            );
            assert!(storage.pending_table_def(parent).is_none());
        });
    }

    #[test]
    fn table_publication_rescans_late_creation_and_private_foreign_key_edits() {
        struct ResumeScan<'a>(&'a std::sync::atomic::AtomicU8);
        impl Drop for ResumeScan<'_> {
            fn drop(&mut self) {
                self.0.store(0, Ordering::Release);
            }
        }
        for creation in [false, true] {
            let (storage, parent, _, other) = fixture();
            let change = rename(&storage, parent);
            storage
                .definition_publication
                .scan_pause
                .store(1, Ordering::Release);
            std::thread::scope(|scope| {
                let writer = scope
                    .spawn(|| crate::mem::guard::forbid_alloc(|| publish_change(&storage, change)));
                let resume = ResumeScan(&storage.definition_publication.scan_pause);
                let deadline = Instant::now() + Duration::from_secs(5);
                while storage
                    .definition_publication
                    .scan_pause
                    .load(Ordering::Acquire)
                    != 2
                    && Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                let paused = storage
                    .definition_publication
                    .scan_pause
                    .load(Ordering::Acquire)
                    == 2;
                let submitted = if paused {
                    crate::mem::guard::forbid_alloc(|| {
                        let (target, definition) = storage.select_table_definition(other, 9);
                        let mut definition = TableDefBuilder::from_definition(&definition);
                        definition.n_fkeys = 1;
                        definition.fkeys[0] = ForeignKey {
                            identity: None,
                            parent_schema: SqlName::parse("public").unwrap(),
                            parent: SqlName::parse("publication_parent").unwrap(),
                            n_cols: 1,
                            n_parent_cols: 1,
                            ..ForeignKey::EMPTY
                        };
                        if creation {
                            definition.name = SqlName::parse("publication_late_child").unwrap();
                            (storage.create_table_in(definition, 9).unwrap(), None)
                        } else {
                            let change = storage
                                .write_table_def(
                                    target,
                                    definition,
                                    &mapping(),
                                    false,
                                    crate::storage::ColumnSet::EMPTY,
                                )
                                .unwrap();
                            (other, Some(change))
                        }
                    })
                } else {
                    (other, None)
                };
                drop(resume);
                let result = writer.join().unwrap();
                assert!(paused);
                assert_eq!(result.unwrap(), Some(false));
                let selected = storage.table_def(submitted.0, 9);
                assert_eq!(selected.fkeys()[0].parent.as_str(), "publication_renamed");
                assert_eq!(
                    selected.fkeys()[0].parent_schema.as_str(),
                    "publication_destination"
                );
                drop(selected);
                if let Some(change) = submitted.1 {
                    assert_eq!(publish_change(&storage, change).unwrap(), Some(false));
                } else {
                    storage.commit_create(submitted.0);
                }
                assert_eq!(
                    storage.table_def(submitted.0, 0).fkeys()[0].parent.as_str(),
                    "publication_renamed"
                );
            });
        }
    }

    #[test]
    fn table_publication_rejects_foreign_storage_and_reused_source_lifetimes() {
        let (storage, parent, child, _) = fixture();
        let (foreign, foreign_parent, _, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let change = rename(&storage, parent);
            assert_eq!(publish_change(&foreign, change).unwrap(), None);
            assert_eq!(
                foreign.table_def(foreign_parent, 0).name.as_str(),
                "publication_parent"
            );
            storage.rollback_table_def(change);
            storage.commit_drop(parent);
            let replacement = storage
                .create_table_in(
                    make_def("publication_replacement", &[("id", ColType::Int4, false)]),
                    8,
                )
                .unwrap();
            assert_eq!(replacement, parent);
            storage.commit_create(replacement);
            assert_eq!(publish_change(&storage, change).unwrap(), None);
            assert_eq!(
                storage.table_def(replacement, 0).name.as_str(),
                "publication_replacement"
            );
            assert_eq!(
                storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                "publication_parent"
            );
            assert!(storage.definition_publication.scratch.try_lock().is_ok());
        });
    }

    #[test]
    fn table_publication_exhaustion_does_not_promote_private_index_bindings() {
        let (storage, parent, _, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let (target, definition) = storage.select_table_definition(parent, 7);
            let mut definition = TableDefBuilder::from_definition(&definition);
            definition.columns[0].name = SqlName::parse("renamed_id").unwrap();
            let mut columns = [None; MAX_COLUMNS];
            columns[0] = Some(definition.columns[0].name);
            let change = storage
                .write_table_def(
                    target,
                    definition,
                    &columns,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let index = storage
                .create_index(
                    super::super::IndexDef {
                        schema: definition.schema,
                        name: SqlName::parse("publication_private_key").unwrap(),
                        table: definition.name,
                        n_cols: 1,
                        collations: [crate::sql::ast::Collation::None;
                            super::super::MAX_INDEX_COLS],
                        ..super::super::IndexDef::EMPTY
                    },
                    7,
                )
                .unwrap();
            let generation = storage.tables[parent].generation();
            storage.tables[parent].publication().generation = u64::MAX;
            assert_eq!(
                publish_change(&storage, change).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let binding = storage.index_catalog()[index].table_binding.unwrap();
            assert!(binding.matches_selected(
                &storage,
                &storage.tables[parent].definition.read(),
                7
            ));
            assert_eq!(
                storage.table_def(parent, 0).columns()[0].name.as_str(),
                "id"
            );
            storage.tables[parent].publication().generation = generation;
            assert_eq!(publish_change(&storage, change).unwrap(), Some(false));
            let binding = storage.index_catalog()[index].table_binding.unwrap();
            assert!(binding.matches_selected(
                &storage,
                &storage.tables[parent].definition.read(),
                0
            ));
            assert_eq!(
                storage.table_def(parent, 0).columns()[0].name.as_str(),
                "renamed_id"
            );
        });
    }

    #[test]
    fn table_publication_released_child_finds_parent_before_its_definition_releases() {
        struct ResumeRelease<'a>(&'a std::sync::atomic::AtomicU8);
        impl Drop for ResumeRelease<'_> {
            fn drop(&mut self) {
                self.0.store(0, Ordering::Release);
            }
        }
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let child = storage
            .create_table(make_def(
                "publication_release_child",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let mut parent_definition =
            make_def("publication_release_parent", &[("id", ColType::Int4, true)]);
        parent_definition.columns[0].primary = true;
        parent_definition.columns[0].unique = true;
        let parent = storage.create_table(parent_definition).unwrap();
        assert!(child < parent);
        crate::mem::guard::forbid_alloc(|| {
            let (target, definition) = storage.select_table_definition(child, 8);
            let mut definition = TableDefBuilder::from_definition(&definition);
            definition.n_fkeys = 1;
            definition.fkeys[0] = ForeignKey {
                identity: None,
                name: SqlName::parse("publication_parent_link").unwrap(),
                parent_schema: parent_definition.schema,
                parent: parent_definition.name,
                n_cols: 1,
                n_parent_cols: 1,
                ..ForeignKey::EMPTY
            };
            let change = storage
                .write_table_def(
                    target,
                    definition,
                    &mapping(),
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            assert_eq!(publish_change(&storage, change).unwrap(), Some(false));
        });
        let change = rename(&storage, parent);
        let database = storage.current_database_oid();
        // A resumed scan must not arm the independent owner-release pause.
        storage
            .definition_publication
            .scan_pause
            .store(3, Ordering::Release);
        storage
            .definition_publication
            .release_pause
            .store(1, Ordering::Release);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::sql::set_execution_database_oid(database);
                crate::mem::guard::forbid_alloc(|| publish_change(&storage, change))
            });
            let resume = ResumeRelease(&storage.definition_publication.release_pause);
            let deadline = Instant::now() + Duration::from_secs(5);
            while storage
                .definition_publication
                .release_pause
                .load(Ordering::Acquire)
                != 2
                && Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            let paused = storage
                .definition_publication
                .release_pause
                .load(Ordering::Acquire)
                == 2;
            let (child_moved, parent_name_visible, parent_retained) = if paused {
                crate::mem::guard::forbid_alloc(|| {
                    let definition = storage.table_def(child, 0);
                    let key = definition.fkeys()[0];
                    (
                        key.parent.as_str() == "publication_renamed",
                        storage.tables[parent]
                            .definition
                            .name_candidate(
                                super::super::table_names::TableNameScope::Committed,
                                database,
                                key.parent_schema.as_str(),
                                key.parent.as_str(),
                            )
                            .is_some(),
                        storage.tables[parent].definition.test_write().is_err(),
                    )
                })
            } else {
                (false, false, false)
            };
            let reader = scope.spawn(|| {
                crate::sql::set_execution_database_oid(database);
                crate::mem::guard::forbid_alloc(|| {
                    storage.find_table("publication_destination", "publication_renamed")
                })
            });
            drop(resume);
            assert_eq!(writer.join().unwrap().unwrap(), Some(false));
            assert_eq!(reader.join().unwrap(), Some(parent));
            assert!(paused && child_moved && parent_name_visible && parent_retained);
        });
    }
}
