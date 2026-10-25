//! Table-name reservation and initialization of an unused table slot.

use super::{
    AccessClass, AccessObject, CatalogDdlState, DependencyClass, Ownership,
    RelationCumulativeStatistics, SqlError, Storage, TableDef, TableSerialState, TableStatistics,
    current_database, table_definitions,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

#[derive(Clone, Copy)]
pub(crate) enum TableCreation {
    Committed,
    Pending(u32),
}

/// Source validation and installation share owners. A source wait releases the
/// name, lifecycle and definition owners before retrying the name reservation.
pub(super) fn allocate(
    storage: &Storage,
    mut def: TableDef,
    allocation: super::CatalogAllocation,
    creation: TableCreation,
) -> Result<usize, SqlError> {
    def.attributes.initialize(def.n_columns)?;
    if !def.attributes.validate(def.n_columns) {
        return Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "invalid column attribute layout"
        ));
    }
    super::constraint_names::table(&mut def)?;
    let transaction = match creation {
        TableCreation::Committed => 0,
        TableCreation::Pending(transaction) => transaction,
    };
    let ownership = storage.initial_ownership(transaction)?;
    loop {
        let names = storage
            .table_creation
            .lock()
            .expect("table creation lock poisoned");
        let duplicate = match creation {
            TableCreation::Pending(txid) => {
                storage.find_visible(def.schema.as_str(), def.name.as_str(), txid)
            }
            TableCreation::Committed => storage.find_table(def.schema.as_str(), def.name.as_str()),
        };
        if duplicate.is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        if let TableCreation::Pending(txid) = creation
            && let Some(other) =
                storage.ddl_name_locked_by_other(def.schema.as_str(), def.name.as_str(), txid)
        {
            drop(names);
            return Err(storage.catalog_ddl_wait_error(txid, other, def.name.as_str()));
        }
        let (slot, lifecycle) = match allocation {
            super::CatalogAllocation::New => {
                let Some((slot, lifecycle)) = storage
                    .tables
                    .iter()
                    .take(storage.table_count())
                    .enumerate()
                    .find_map(|(slot, table)| {
                        let lifecycle = match table.lifecycle.try_lock() {
                            Ok(guard) => guard,
                            Err(std::sync::TryLockError::WouldBlock) => return None,
                            Err(std::sync::TryLockError::Poisoned(_)) => {
                                panic!("table lifecycle lock poisoned")
                            }
                        };
                        table.is_free().then_some((slot, lifecycle))
                    })
                else {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many tables (limit {})",
                        storage.table_count()
                    ));
                };
                (slot, lifecycle)
            }
            super::CatalogAllocation::Restore(position) => {
                let table = storage
                    .tables
                    .get(position.slot())
                    .filter(|_| position.slot() < storage.table_count())
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "saved table position exceeds configured capacity"
                        )
                    })?;
                let lifecycle = match super::row_ownership::try_mutex(
                    &table.lifecycle,
                    "table lifecycle lock poisoned",
                ) {
                    Ok(lifecycle) => lifecycle,
                    Err(wait) => {
                        drop(names);
                        wait.wait();
                        continue;
                    }
                };
                if !table.is_free()
                    || storage
                        .tables
                        .iter()
                        .take(storage.table_count())
                        .any(|candidate| {
                            let identity = candidate.identity();
                            identity.database == current_database()
                                && identity.existence != CatalogDdlState::Absent
                                && identity.created_at == position.created_at()
                        })
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "saved table position is unavailable or its creation identity is already used"
                    ));
                }
                (position.slot(), lifecycle)
            }
        };
        // A reused slot must not keep the dropped table's value indexes.
        storage.release_enforcers(slot);
        storage.clear_pending_table_defs(slot);
        storage.clear_pending_table_statistics(slot);
        storage.clear_object_acl_entries(AccessObject {
            class: AccessClass::Table,
            slot: slot as u16,
        });
        storage.cumulative_statistics().relations[slot] = RelationCumulativeStatistics::EMPTY;
        storage.reset_implicit_index_statistics(slot);
        storage.clear_table_rows(slot);
        let table = &storage.tables[slot];
        let mut state = table.definition.write();
        let source = match super::row_ownership::try_mutex(
            &storage.type_catalog,
            "type catalog lock poisoned",
        ) {
            Ok(source) => source,
            Err(wait) => {
                drop(state);
                drop(lifecycle);
                drop(names);
                #[cfg(test)]
                storage
                    .definition_publication
                    .waits
                    .fetch_add(1, core::sync::atomic::Ordering::Release);
                wait.wait();
                continue;
            }
        };
        storage.require_row_type_name(
            &source,
            def.schema,
            def.name,
            transaction,
            Some(super::RuleTarget::Table(slot as u16)),
            None,
        )?;
        source.bind_table_definition(&mut def, transaction)?;
        let created_at = match allocation {
            super::CatalogAllocation::New => storage.catalog_sequence.next()?,
            super::CatalogAllocation::Restore(position) => {
                storage.catalog_sequence.observe(position.created_at());
                position.created_at()
            }
        };
        state.install(
            def,
            table_definitions::TableIdentity {
                database: current_database(),
                ownership,
                created_at,
                ddl_version: created_at,
                existence: match creation {
                    TableCreation::Committed => CatalogDdlState::Present,
                    TableCreation::Pending(txid) => CatalogDdlState::PendingCreate { txid },
                },
            },
        );
        table.publication().row_layout = super::row_layout::RowLayout::initial();
        drop(state);
        drop(source);
        table.mark_dirty();
        table
            .statistics
            .set_image(TableStatistics::EMPTY, Some(false), true);
        // A reused slot must not inherit the dropped table's sequences or
        // spilled rows.
        *table
            .serial
            .lock()
            .expect("table serial state lock poisoned") = TableSerialState::EMPTY;
        table.spill_generations.write().clear();
        let schema = {
            let definition = table.definition();
            definition.schema
        };
        let name = {
            let definition = table.definition();
            definition.name
        };
        if let Err(error) = storage.refresh_enforcers_visible(slot, None) {
            storage.rollback_create_locked(slot);
            let mut state = table.definition.write();
            let mut identity = state.identity;
            identity.ownership = Ownership::BOOTSTRAP;
            identity.created_at = 0;
            state.install(TableDef::empty(), identity);
            return Err(error);
        }
        storage.rename_stored_query_dependency(DependencyClass::Table, slot, None, schema, name);
        return Ok(slot);
    }
}
