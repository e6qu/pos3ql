//! Table-name reservation and initialization of an unused table slot.

use super::{
    AccessClass, AccessObject, CatalogDdlState, DependencyClass, Ownership,
    RelationCumulativeStatistics, SqlError, Storage, TableDef, TableSerialState, TableStatistics,
    current_database, table_definitions,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

/// Source validation and installation share owners. A source wait releases the
/// name, lifecycle and definition owners before retrying the name reservation.
pub(super) fn allocate(
    storage: &Storage,
    mut def: TableDef,
    transaction: Option<u32>,
) -> Result<usize, SqlError> {
    loop {
        let names = storage
            .table_creation
            .lock()
            .expect("table creation lock poisoned");
        let duplicate = match transaction {
            Some(txid) => storage.find_visible(def.schema.as_str(), def.name.as_str(), txid),
            None => storage.find_table(def.schema.as_str(), def.name.as_str()),
        };
        if duplicate.is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists",
                def.name.as_str()
            ));
        }
        if let Some(txid) = transaction {
            if let Some(other) =
                storage.ddl_name_locked_by_other(def.schema.as_str(), def.name.as_str(), txid)
            {
                drop(names);
                return Err(storage.catalog_ddl_wait_error(txid, other, def.name.as_str()));
            }
        }
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
        // A reused slot must not keep the dropped table's value indexes.
        storage.release_enforcers(slot);
        storage.clear_pending_table_defs(slot);
        storage.clear_pending_table_statistics(slot);
        storage.clear_object_acl_entries(AccessObject {
            class: AccessClass::Table,
            slot: slot as u16,
        });
        let ownership = storage.initial_ownership(transaction.unwrap_or(0));
        storage.cumulative_statistics().relations[slot] = RelationCumulativeStatistics::EMPTY;
        storage.reset_implicit_index_statistics(slot);
        storage.clear_table_rows(slot);
        let table = &storage.tables[slot];
        let mut state = table.definition.write();
        let source = if def.requires_type_catalog() {
            match super::row_ownership::try_mutex(
                &storage.type_catalog,
                "type catalog lock poisoned",
            ) {
                Ok(source) => Some(source),
                Err(wait) => {
                    drop(state);
                    drop(lifecycle);
                    drop(names);
                    #[cfg(test)]
                    storage
                        .type_publication
                        .waits
                        .fetch_add(1, core::sync::atomic::Ordering::Release);
                    wait.wait();
                    continue;
                }
            }
        } else {
            None
        };
        if let Some(source) = &source {
            source.bind_table_definition(&mut def, transaction.unwrap_or(0))?;
        }
        let created_at = storage.catalog_sequence.next()?;
        state.committed = def;
        state.pending_has_rules_txid = None;
        state.identity = table_definitions::TableIdentity {
            database: current_database(),
            ownership,
            created_at,
            existence: transaction.map_or(CatalogDdlState::Present, |txid| {
                CatalogDdlState::PendingCreate { txid }
            }),
        };
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
            state.committed = TableDef::empty();
            state.identity.ownership = Ownership::BOOTSTRAP;
            state.identity.created_at = 0;
            return Err(error);
        }
        storage.rename_stored_query_dependency(DependencyClass::Table, slot, None, schema, name);
        return Ok(slot);
    }
}
