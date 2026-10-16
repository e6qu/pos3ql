//! Table-name reservation and initialization of an unused table slot.

use super::{
    AccessClass, AccessObject, CatalogDdlState, DependencyClass, Ownership, SqlError, Storage,
    TableDef, TableSerialState, current_database, table_definitions,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

pub(super) struct TableCreation<'storage> {
    storage: &'storage Storage,
    _names: std::sync::MutexGuard<'storage, ()>,
}

impl<'storage> TableCreation<'storage> {
    pub(super) fn new(storage: &'storage Storage) -> Self {
        Self {
            storage,
            _names: storage
                .table_creation
                .lock()
                .expect("table creation lock poisoned"),
        }
    }

    /// The name guard covers initialization of an empty slot, without object I/O.
    /// Selected lifecycle ownership excludes retirement through failure cleanup.
    pub(super) fn allocate(
        &self,
        def: TableDef,
        transaction: Option<u32>,
    ) -> Result<usize, SqlError> {
        let storage = self.storage;
        let Some((slot, _lifecycle)) = storage
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
        let created_at = storage.catalog_sequence.next();
        let stamp = created_at;
        storage.cumulative_statistics().relations[slot] = RelationCumulativeStatistics::EMPTY;
        storage.reset_implicit_index_statistics(slot);
        storage.clear_table_rows(slot);
        let table = &storage.tables[slot];
        {
            let mut state = table.definition.write();
            state.committed = def;
            state.pending_has_rules_txid = None;
            state.identity = table_definitions::TableIdentity {
                database: current_database(),
                ownership,
                created_at: stamp,
                existence: transaction.map_or(CatalogDdlState::Present, |txid| {
                    CatalogDdlState::PendingCreate { txid }
                }),
            };
        }
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
        Ok(slot)
    }
}
