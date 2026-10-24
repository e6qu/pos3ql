//! Publish a table shape and its dependent names under one set of owners.

use super::catalog_sources::{CatalogObjectIdentity, CatalogObjectSlot};
use super::row_ownership::{LockWait, try_mutex};
use super::table_definitions::{DefinitionState, TableDefinitionRevision, TableIdentity};
use super::type_reference_moves::{DefinitionPublication, TableAttempt, TableOwner};
use super::{
    CatalogDdlState, CommentClass, DatabaseOid, DependencyClass, MAX_COLUMNS, SqlName, Storage,
    TableDef, TableDefinitionChange, rebind_sequence_column,
};
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

    fn matches(self, storage: &Storage, state: &DefinitionState) -> bool {
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
            Self::Committed { committed_revision, .. } => {
                state.committed_revision == committed_revision
            }
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
            && definition.fkeys().iter().any(|key| {
                key.parent_schema == self.prior_schema && key.parent == self.prior_name
            })
    }

    pub(super) fn rewrite(self, definition: &mut TableDef) -> bool {
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

impl DefinitionPublication {
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
                || !storage.pending_table_defs.references_table(&definition, movement)
            {
                continue;
            }
            drop(definition);
            Self::acquire_table(storage, slot, owners)?;
            let definition = &owners.slots[slot].as_ref().unwrap().definition;
            if !movement.applies_to(definition.identity)
                || !storage.pending_table_defs.references_table(definition, movement)
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
        let mut scratch = self.scratch.lock().expect("definition publication scratch poisoned");
        scratch.reset();
        let mut slots = scratch
            .alloc_owned_slice_with(storage.tables.len(), |_| None)
            .expect("startup definition publication reservation covers every table");
        loop {
            let mut owners = TableAttempt { slots: &mut slots };
            let attempt: Result<_, LockWait<'_>> = (|| {
                Self::acquire_table(storage, table, &mut owners)?;
                let selected = &owners.slots[table].as_ref().unwrap().definition;
                if !source.matches(storage, selected) {
                    return Ok(None);
                }
                let (mut definition, mapping, revision, rewrites_rows, transaction) = match source {
                    DefinitionSource::Pending(change) => {
                        let pending = storage.pending_table_defs.latest_locked(selected)
                            .expect("validated pending definition exists");
                        (pending.def, pending.column_mapping,
                         TableDefinitionRevision::catalog(pending.revision),
                         pending.rewrites_rows, Some(change.transaction))
                    }
                    DefinitionSource::Committed { definition, column_mapping, revision,
                        rewrites_rows, .. } => {
                        (*definition, *column_mapping, revision, rewrites_rows, None)
                    }
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
                Self::acquire_table_dependents(storage, movement, &mut owners)?;
                // Creation binds under the type catalog; every private table edit
                // owns comments. Rescan while both submission gates are held.
                let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
                let comments = try_mutex(&storage.comments, "comment catalog lock poisoned")?;
                Self::acquire_table_dependents(storage, movement, &mut owners)?;
                let indexes = try_mutex(&storage.index_catalog, "index catalog lock poisoned")?;
                let sequences = try_mutex(&storage.sequence_catalog, "sequence catalog lock poisoned")?;
                let dependencies = try_mutex(
                    &storage.stored_query_dependencies,
                    "stored-query dependency catalog lock poisoned",
                )?;
                Ok(Some((definition, mapping, revision, rewrites_rows, transaction,
                    movement, types, comments, indexes, sequences, dependencies)))
            })();
            let Some((mut definition, mapping, revision, rewrites_rows, transaction,
                movement, _types, mut comments, mut indexes, mut sequences, mut dependencies)) =
                (match attempt {
                    Ok(value) => value,
                    Err(wait) => {
                        drop(owners);
                        #[cfg(test)]
                        self.waits.fetch_add(1, std::sync::atomic::Ordering::Release);
                        wait.wait();
                        continue;
                    }
                }) else {
                    return Ok(None);
                };
            // Every affected definition is published once. No catalog entry
            // changes before all counters and owners have been admitted.
            for owner in owners.slots.iter().flatten() {
                owner.publication.generation.checked_add(1).ok_or_else(|| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table publication generation exhausted")
                })?;
            }
            let selected = &owners.slots[table].as_ref().unwrap().definition;
            if let Some(transaction) = transaction {
                for index in indexes.iter_mut().filter(|index| {
                    index.database == movement.database
                        && index.ddl_state == (CatalogDdlState::PendingCreate { txid: transaction })
                        && index.table_binding.is_some_and(|binding| binding.source == source.identity())
                }) {
                    let mut promoted = storage.project_index_binding_locked(*index, selected, transaction)
                        .expect("private index retains its table lifetime through commit");
                    promoted.table_binding.as_mut().expect("projected binding exists").revision = 0;
                    *index = promoted;
                }
            }
            for index in indexes.iter_mut().filter(|index| {
                index.database == movement.database
                    && index.schema == movement.prior_schema && index.table == movement.prior_name
            }) {
                if index.ddl_state == CatalogDdlState::Present {
                    for comment in comments.entries.iter_mut().filter(|comment| {
                        comment.used && comment.database == Some(movement.database)
                            && comment.class == CommentClass::Relation && comment.subid == 0
                            && comment.schema == index.schema && comment.name == index.name
                    }) {
                        comment.schema = movement.schema;
                    }
                    for key in 0..index.n_cols {
                        if index.expressions[key].is_none()
                            && let Some(name) = mapping[usize::from(index.columns[key])]
                            && let Some(column) = definition.column_index(name.as_str())
                        {
                            index.columns[key] = column as u16;
                        }
                    }
                    for column in index.include_columns.iter_mut().take(index.n_include_cols) {
                        if let Some(name) = mapping[usize::from(*column)]
                            && let Some(target) = definition.column_index(name.as_str())
                        {
                            *column = target as u16;
                        }
                    }
                }
                index.schema = movement.schema;
                index.table = movement.name;
            }
            for sequence in sequences.definitions.iter_mut().filter(|sequence| sequence.database == movement.database) {
                if sequence.ddl_state == CatalogDdlState::Present
                    && sequence.owner.is_some_and(|owner| {
                        owner.table_schema == movement.prior_schema && owner.table == movement.prior_name
                    })
                    && movement.prior_schema != movement.schema
                {
                    sequence.schema = movement.schema;
                }
                sequence.owner = rebind_sequence_column(sequence.owner, selected.committed(), &definition, &mapping, false);
                sequence.generator_for = rebind_sequence_column(sequence.generator_for, selected.committed(), &definition, &mapping, true);
            }
            for comment in comments.entries.iter_mut().filter(|comment| {
                comment.used && comment.database == Some(movement.database)
                    && matches!(comment.class, CommentClass::Relation | CommentClass::Type)
                    && comment.schema == movement.prior_schema && comment.name == movement.prior_name
            }) {
                comment.schema = movement.schema;
                comment.name = movement.name;
                comment.pending_identity = None;
            }
            if let Some(transaction) = transaction {
                Storage::commit_index_table_names_locked(&mut indexes, &mut comments, source.identity(), transaction);
                for comment in comments.entries.iter_mut().filter(|comment| {
                    comment.used && comment.database == Some(movement.database)
                        && comment.class == CommentClass::Constraint && comment.subid == table as u32
                        && comment.pending_identity.is_some_and(|identity| identity.txid == transaction)
                }) {
                    let identity = comment.pending_identity.take().expect("selected constraint comment");
                    comment.schema = identity.schema;
                    comment.name = identity.name;
                }
            }
            movement.rewrite(&mut definition);
            for (slot, owner) in owners.slots.iter_mut().enumerate() {
                let Some(owner) = owner else { continue; };
                if slot == table {
                    *owner.definition.edit_committed(revision) = definition;
                    if transaction.is_some() {
                        storage.pending_table_defs.clear_locked(&mut owner.definition);
                    }
                    if rewrites_rows {
                        owner.publication.row_layout = super::row_layout::RowLayout::installed_at(owner.publication.generation + 1);
                    }
                } else {
                    assert!(storage.pending_table_defs.move_table_references_locked(
                        &mut owner.definition, movement, revision));
                }
                owner.publication.generation += 1;
                owner.publication.dirty = true;
            }
            storage.rename_stored_query_dependency_locked(&mut dependencies,
                DependencyClass::Table, table, None, definition.schema, definition.name);
            return Ok(Some(rewrites_rows));
        }
    }
}
