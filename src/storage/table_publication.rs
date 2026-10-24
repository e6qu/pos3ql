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
                        let pending = storage
                            .pending_table_defs
                            .latest_locked(selected)
                            .expect("validated pending definition exists");
                        (
                            pending.def,
                            pending.column_mapping,
                            TableDefinitionRevision::catalog(pending.revision),
                            pending.rewrites_rows,
                            Some(change.transaction),
                        )
                    }
                    DefinitionSource::Committed {
                        definition,
                        column_mapping,
                        revision,
                        rewrites_rows,
                        ..
                    } => (*definition, *column_mapping, revision, rewrites_rows, None),
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
                #[cfg(test)]
                self.pause_after_scan();
                // Creation binds under the type catalog; every private table edit
                // owns comments. Rescan while both submission gates are held.
                let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
                let comments = try_mutex(&storage.comments, "comment catalog lock poisoned")?;
                Self::acquire_table_dependents(storage, movement, &mut owners)?;
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
            // Every affected definition is published once. No catalog entry
            // changes before all counters and owners have been admitted.
            for owner in owners.slots.iter().flatten() {
                owner.publication.generation.checked_add(1).ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table publication generation exhausted"
                    )
                })?;
            }
            let selected = &owners.slots[table].as_ref().unwrap().definition;
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
                    sequence.schema = movement.schema;
                }
                sequence.owner = rebind_sequence_column(
                    sequence.owner,
                    selected.committed(),
                    &definition,
                    &mapping,
                    false,
                );
                sequence.generator_for = rebind_sequence_column(
                    sequence.generator_for,
                    selected.committed(),
                    &definition,
                    &mapping,
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
                        &mapping,
                        false,
                    );
                    pending.generator_for = rebind_sequence_column(
                        pending.generator_for,
                        selected.committed(),
                        &definition,
                        &mapping,
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
            movement.rewrite(&mut definition);
            for (slot, owner) in owners.slots.iter_mut().enumerate() {
                let Some(owner) = owner else {
                    continue;
                };
                if slot == table {
                    *owner.definition.edit_committed(revision) = definition;
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
                    assert!(storage.pending_table_defs.move_table_references_locked(
                        &mut owner.definition,
                        movement,
                        revision
                    ));
                }
                owner.publication.generation += 1;
                owner.publication.dirty = true;
            }
            storage.rename_stored_query_dependency_locked(
                &mut dependencies,
                DependencyClass::Table,
                table,
                None,
                definition.schema,
                definition.name,
            );
            return Ok(Some(rewrites_rows));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{ForeignKey, comment_stackstr};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize, usize, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let parent = storage
            .create_table(make_def(
                "publication_parent",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let mut child = make_def("publication_child", &[("id", ColType::Int4, false)]);
        child.n_fkeys = 1;
        child.fkeys[0] = ForeignKey {
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
        let (target, mut definition) = storage.select_table_definition(table, 7);
        definition.schema = SqlName::parse("publication_destination").unwrap();
        definition.name = SqlName::parse("publication_renamed").unwrap();
        storage
            .write_table_def(target, definition, &mapping(), false)
            .unwrap()
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
                    .spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_table_def(change)));
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
            let (target, mut definition) = storage.select_table_definition(child, 9);
            definition.columns[0].not_null = super::super::NotNullOrigin::Local;
            let first = storage
                .write_table_def(target, definition, &mapping(), false)
                .unwrap();
            let (target, mut definition) = storage.select_table_definition(child, 9);
            definition.columns[0].not_null = super::super::NotNullOrigin::Nullable;
            let second = storage
                .write_table_def(target, definition, &mapping(), false)
                .unwrap();
            let change = rename(&storage, parent);
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
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
            assert_eq!(storage.commit_table_def(first).unwrap(), Some(false));
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
                .spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_table_def(change)));
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
                .spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_table_def(change)));
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
            .unwrap();
        storage.commit_comment(comment, 7);
        crate::mem::guard::forbid_alloc(|| {
            let change = rename(&storage, parent);
            let generation = storage.tables[child].generation();
            storage.tables[child].publication().generation = u64::MAX;
            assert_eq!(
                storage.commit_table_def(change).err().unwrap().sqlstate,
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
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
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
                    .spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_table_def(change)));
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
                        let (target, mut definition) = storage.select_table_definition(other, 9);
                        definition.n_fkeys = 1;
                        definition.fkeys[0] = ForeignKey {
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
                                .write_table_def(target, definition, &mapping(), false)
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
                    assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
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
            assert_eq!(foreign.commit_table_def(change).unwrap(), None);
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
            assert_eq!(storage.commit_table_def(change).unwrap(), None);
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
            let (target, mut definition) = storage.select_table_definition(parent, 7);
            definition.columns[0].name = SqlName::parse("renamed_id").unwrap();
            let mut columns = [None; MAX_COLUMNS];
            columns[0] = Some(definition.columns[0].name);
            let change = storage
                .write_table_def(target, definition, &columns, false)
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
                storage.commit_table_def(change).err().unwrap().sqlstate,
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
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(false));
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
}
