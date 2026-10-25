//! Namespace names retain a private version until their transaction publishes.

use super::type_references::CatalogOrigin;
use super::{CatalogDdlDecision, DatabaseOid, SchemaDef, SqlName, Storage, current_database};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingSchemaName {
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) name: SqlName,
}

#[derive(Clone, Copy)]
pub(crate) struct SchemaNameTarget {
    origin: CatalogOrigin,
    database: DatabaseOid,
    slot: usize,
    created_at: u64,
    source_version: u64,
    version: u64,
    transaction: u32,
    name: SqlName,
}

#[derive(Clone, Copy)]
pub(crate) struct SchemaNameUndo {
    target: SchemaNameTarget,
    prior: Option<PendingSchemaName>,
}

impl SchemaNameTarget {
    pub(crate) fn name(self) -> SqlName { self.name }
}

impl SchemaNameUndo {
    pub(crate) fn slot(self) -> usize { self.target.slot }
}

impl Storage {
    pub(crate) fn prepare_schema_name(&self, slot: usize, transaction: u32) -> Result<SchemaNameTarget, SqlError> {
        if transaction == 0 {
            return Err(sql_err!(sqlstate::NO_ACTIVE_SQL_TRANSACTION, "schema name edit requires an active transaction"));
        }
        let schemas = self.schemas.lock().expect("schema catalog lock poisoned");
        let schema = schemas.get(slot).filter(|schema| schema.database == current_database() && schema.visible_to(transaction))
            .ok_or_else(|| sql_err!(sqlstate::INVALID_SCHEMA_NAME, "schema edit source no longer exists"))?;
        if let Some(owner) = schema.pending_owner_other_than(transaction) {
            let name = schema.name;
            drop(schemas);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        Ok(SchemaNameTarget {
            origin: self.catalog_sources.origin(),
            database: schema.database,
            slot,
            created_at: schema.created_at,
            source_version: schema.version,
            version: schema.pending_name.map_or(schema.version, |pending| pending.version),
            transaction,
            name: schema.name_for(transaction),
        })
    }

    fn schema_name_source_matches(&self, schema: &SchemaDef, target: SchemaNameTarget) -> bool {
        self.catalog_sources.origin() == target.origin && current_database() == target.database
            && schema.database == target.database && schema.created_at == target.created_at
    }

    pub(crate) fn stage_schema_name(&self, target: SchemaNameTarget, name: SqlName) -> Result<SchemaNameUndo, SqlError> {
        let mut schemas = self.schemas.lock().expect("schema catalog lock poisoned");
        let schema = schemas.get(target.slot).filter(|schema| self.schema_name_source_matches(schema, target))
            .ok_or_else(|| sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema edit source has been replaced"))?;
        if let Some(owner) = schema.pending_owner_other_than(target.transaction) {
            let name = schema.name;
            drop(schemas);
            return Err(self.catalog_ddl_wait_error(target.transaction, owner, name.as_str()));
        }
        if !schema.visible_to(target.transaction) || schema.version != target.source_version
            || schema.pending_name.map_or(schema.version, |pending| pending.version) != target.version {
            return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema name changed after edit preparation"));
        }
        if name == schema.name_for(target.transaction) {
            return Err(sql_err!(sqlstate::DUPLICATE_SCHEMA, "schema \"{}\" already exists", name.as_str()));
        }
        for (slot, other) in schemas.iter().enumerate() {
            if slot == target.slot || other.database != target.database { continue; }
            let reserves_name = other.name == name || other.pending_name.is_some_and(|pending| pending.name == name);
            if reserves_name && let Some(owner) = other.pending_owner_other_than(target.transaction) {
                drop(schemas);
                return Err(self.catalog_ddl_wait_error(target.transaction, owner, name.as_str()));
            }
            if other.visible_to(target.transaction) && other.name_for(target.transaction) == name {
                return Err(sql_err!(sqlstate::DUPLICATE_SCHEMA, "schema \"{}\" already exists", name.as_str()));
            }
        }
        let version = self.catalog_sequence.next()?;
        let prior = schemas[target.slot].pending_name;
        schemas[target.slot].pending_name = Some(PendingSchemaName { transaction: target.transaction, version, name });
        Ok(SchemaNameUndo { target: SchemaNameTarget { version, name, ..target }, prior })
    }

    pub(crate) fn finish_schema_name(&self, undo: SchemaNameUndo, decision: CatalogDdlDecision) -> Result<(), SqlError> {
        let mut schemas = self.schemas.lock().expect("schema catalog lock poisoned");
        let schema = schemas.get_mut(undo.target.slot).ok_or_else(|| sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema name source slot is missing"))?;
        if !self.schema_name_source_matches(schema, undo.target) {
            return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema name source has been replaced"));
        }
        if matches!(decision, CatalogDdlDecision::Commit) && schema.pending_name.is_none() { return Ok(()); }
        let pending = schema.pending_name.filter(|pending| pending.transaction == undo.target.transaction)
            .ok_or_else(|| sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema name belongs to another transaction"))?;
        match decision {
            CatalogDdlDecision::Commit => {
                schema.name = pending.name;
                schema.version = pending.version;
                schema.pending_name = None;
            }
            CatalogDdlDecision::Rollback => {
                if pending.version != undo.target.version {
                    return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE, "schema rollback is out of order"));
                }
                schema.pending_name = undo.prior;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    #[test]
    fn schema_name_private_versions_preserve_other_readers_and_nested_rollback() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let old = SqlName::parse("schema_before").unwrap();
        let first = SqlName::parse("schema_first").unwrap();
        let second = SqlName::parse("schema_second").unwrap();
        let slot = storage.create_schema(old).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first_edit = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), first).unwrap();
            assert_eq!(storage.find_schema_visible(old.as_str(), 8), Some(slot));
            assert_eq!(storage.find_schema_visible(first.as_str(), 8), None);
            assert_eq!(storage.find_schema_visible(first.as_str(), 7), Some(slot));
            let second_edit = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), second).unwrap();
            storage.finish_schema_name(second_edit, CatalogDdlDecision::Rollback).unwrap();
            assert_eq!(storage.schema_def_for(slot, 7).name, first);
            storage.finish_schema_name(first_edit, CatalogDdlDecision::Rollback).unwrap();
            assert_eq!(storage.schema_def_for(slot, 7).name, old);
            let final_edit = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), second).unwrap();
            storage.finish_schema_name(final_edit, CatalogDdlDecision::Commit).unwrap();
            assert_eq!(storage.schema_def_for(slot, 8).name, second);
        });
    }

    #[test]
    fn schema_name_sources_reject_replacements_foreign_owners_and_out_of_order_rollback() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let mut other_budget = test_budget(&config);
        let other = Storage::new(&config, &mut other_budget, 0).unwrap();
        let old = SqlName::parse("schema_before").unwrap();
        let first = SqlName::parse("schema_first").unwrap();
        let second = SqlName::parse("schema_second").unwrap();
        let slot = storage.create_schema(old).unwrap();
        assert_eq!(other.create_schema(old).unwrap(), slot);
        crate::mem::guard::forbid_alloc(|| {
            let stale = storage.prepare_schema_name(slot, 7).unwrap();
            assert_eq!(other.stage_schema_name(stale, first).err().unwrap().sqlstate, sqlstate::SERIALIZATION_FAILURE);
            let edit = storage.stage_schema_name(stale, first).unwrap();
            assert_eq!(storage.stage_schema_name(stale, second).err().unwrap().sqlstate, sqlstate::SERIALIZATION_FAILURE);
            let later = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), second).unwrap();
            assert_eq!(storage.finish_schema_name(edit, CatalogDdlDecision::Rollback).unwrap_err().sqlstate, sqlstate::SERIALIZATION_FAILURE);
            storage.finish_schema_name(later, CatalogDdlDecision::Rollback).unwrap();
            storage.finish_schema_name(edit, CatalogDdlDecision::Rollback).unwrap();
            let stale = storage.prepare_schema_name(slot, 7).unwrap();
            storage.drop_schema(slot);
            assert_eq!(storage.create_schema(old).unwrap(), slot);
            assert_eq!(storage.stage_schema_name(stale, first).err().unwrap().sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(storage.schema_def_for(slot, 7).name, old);
            assert_eq!(storage.prepare_schema_name(slot, 0).err().unwrap().sqlstate, sqlstate::NO_ACTIVE_SQL_TRANSACTION);
        });
    }
}
