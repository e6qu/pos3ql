//! Namespace names retain a private version until their transaction publishes.

use super::type_references::CatalogOrigin;
use super::{CatalogDdlDecision, DatabaseOid, SchemaDef, SqlName, Storage, current_database};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

const FIRST_SCHEMA_OID: u64 = 80_000;
pub(crate) const FIRST_TEMP_NAMESPACE_OID: i32 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SchemaIdentity {
    created_at: u64,
}

impl SchemaIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        (created_at < FIRST_TEMP_NAMESPACE_OID as u64 - FIRST_SCHEMA_OID)
            .then_some(Self { created_at })
    }

    pub(crate) fn created_at(self) -> u64 {
        self.created_at
    }

    pub(crate) fn oid(self) -> i32 {
        if self.is_initial_public() { 2200 } else { (FIRST_SCHEMA_OID + self.created_at) as i32 }
    }

    pub(crate) fn is_initial_public(self) -> bool {
        self.created_at == 0
    }
}

#[derive(Clone, Copy)]
pub(super) enum SchemaAllocation {
    New,
    Restore { identity: SchemaIdentity, database_owner: bool },
}

pub(super) fn validate_schema_name(name: SqlName) -> Result<(), SqlError> {
    if name.as_str().is_empty() {
        return Err(sql_err!(sqlstate::INVALID_SCHEMA_NAME, "schema name is empty"));
    }
    if matches!(name.as_str(), "pg_catalog" | "pg_toast" | "information_schema") {
        return Err(sql_err!(sqlstate::DUPLICATE_SCHEMA, "schema \"{}\" already exists", name.as_str()));
    }
    if name.as_str().starts_with("pg_") {
        return Err(sql_err!(sqlstate::RESERVED_NAME, "unacceptable schema name \"{}\"", name.as_str()));
    }
    Ok(())
}

impl super::RoutineBodyKind {
    pub(super) fn bound_sql_kind(self) -> Result<crate::sql::parser::StoredSqlKind, SqlError> {
        match self {
            Self::Return => Ok(crate::sql::parser::StoredSqlKind::Expression),
            Self::Atomic => Ok(crate::sql::parser::StoredSqlKind::Statements),
            Self::String => Err(sql_err!(sqlstate::INVALID_FUNCTION_DEFINITION,
                "a string routine body has no bound namespace positions")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingSchemaName {
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) name: SqlName,
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct SchemaNameUndo {
    target: SchemaNameTarget,
    prior: Option<PendingSchemaName>,
}

impl SchemaNameTarget {
    pub(crate) fn name(self) -> SqlName { self.name }

    pub(crate) fn rewrites_rule(self, name: SqlName, definition: super::RuleDefinition, arena: &crate::mem::arena::Arena) -> Result<bool, SqlError> {
        let mut changed = definition;
        super::rename_schema_path(&mut changed.creation_path, self.name, name)?;
        changed.rename_schema(self.name, name, arena)?;
        Ok(definition.source != changed.source || definition.creation_path != changed.creation_path)
    }

    pub(crate) fn rewrite_table(self, name: SqlName, definition: &mut super::TableDef, arena: &crate::mem::arena::Arena) -> Result<bool, SqlError> {
        let mut changed = super::rename_schema_name(&mut definition.schema, self.name, name);
        for column in definition.columns.iter_mut().take(definition.n_columns) {
            changed |= super::rename_user_type_schema(&mut column.user_type, self.name, name);
        }
        for key in definition.fkeys.iter_mut().take(definition.n_fkeys) {
            changed |= super::rename_schema_name(&mut key.parent_schema, self.name, name);
        }
        changed |= super::rename_table_sql_identity(definition, self.name, name, arena)?;
        Ok(changed)
    }

    pub(crate) fn rewrite_routine(self, name: SqlName, definition: &mut super::RoutineDefinition, arena: &crate::mem::arena::Arena) -> Result<bool, SqlError> {
        let mut changed = false;
        for argument in definition.arguments.iter_mut().take(definition.argument_count) {
            changed |= super::rename_user_type_schema(&mut argument.user_type, self.name, name);
        }
        for parameter in definition.parameters.iter_mut().take(definition.parameter_count) {
            changed |= super::rename_user_type_schema(&mut parameter.user_type, self.name, name);
        }
        for result in definition.result_columns.iter_mut().take(definition.result_column_count) {
            changed |= super::rename_user_type_schema(&mut result.user_type, self.name, name);
        }
        changed |= super::rename_routine_kind_schema(&mut definition.kind, self.name, name);
        if definition.body_kind != super::RoutineBodyKind::String {
            let path = definition.creation_path;
            super::rename_schema_path(&mut definition.creation_path, self.name, name)?;
            changed |= path != definition.creation_path;
            changed |= super::rename_schema_qualified_sql(&mut definition.body, &mut [], self.name, name, definition.body_kind.bound_sql_kind()?, arena)?;
        }
        Ok(changed)
    }
}

impl Storage {
    pub(crate) fn clear_schemas_for_restore(&mut self) {
        self.schemas.lock().expect("schema catalog lock poisoned").fill(SchemaDef::EMPTY);
    }

    pub(crate) fn restore_schema(
        &self, identity: SchemaIdentity, name: SqlName, state: super::CatalogDdlState,
        database_owner: bool,
    ) -> Result<usize, SqlError> {
        self.alloc_schema(name, state, super::Ownership::BOOTSTRAP, SchemaAllocation::Restore { identity, database_owner })
    }

    pub(crate) fn schema_replay_slot(
        &self, name: &str, identity: SchemaIdentity, transaction: u32,
    ) -> Result<usize, SqlError> {
        let slot = self.find_schema_visible(name, transaction)
            .ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "journal schema source does not exist"))?;
        if self.schema_def_for(slot, transaction).identity() != Some(identity) {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION, "journal schema source identity does not match"));
        }
        Ok(slot)
    }

    pub(crate) fn commit_comment_identity(&self, slot: usize, transaction: u32) {
        let mut comments = self.comment_catalog();
        let comment = &mut comments.entries[slot];
        if comment.pending_drop.is_some_and(|pending| pending.txid == transaction) { return; }
        if let Some(identity) = comment.pending_identity.filter(|identity| identity.txid == transaction) {
            comment.schema = identity.schema;
            comment.name = identity.name;
            comment.pending_identity = None;
        }
    }

    pub(crate) fn stage_index_schema_references(
        &self, slot: usize, old: SqlName, new: SqlName, transaction: u32,
        arena: &crate::mem::arena::Arena,
    ) -> Result<Option<Option<super::PendingIndexDefinition>>, SqlError> {
        let mut indexes = self.index_catalog();
        let index = indexes[slot];
        if index.database != current_database() || !index.visible_to(transaction) { return Ok(None); }
        let mut definition = index.definition_for(transaction);
        let mut changed = false;
        for expression in definition.expressions.iter_mut().flatten() {
            changed |= super::rename_schema_qualified_sql(expression, &mut [], old, new, crate::sql::parser::StoredSqlKind::Expression, arena)?;
        }
        if let Some(predicate) = &mut definition.predicate {
            changed |= super::rename_schema_qualified_sql(predicate, &mut [], old, new, crate::sql::parser::StoredSqlKind::Expression, arena)?;
        }
        if !changed { return Ok(None); }
        if let Some(owner) = index.pending_owner_other_than(transaction) {
            drop(indexes);
            return Err(self.catalog_ddl_wait_error(transaction, owner, index.name.as_str()));
        }
        indexes[slot].pending_definition = Some(super::PendingIndexDefinition {
            txid: transaction, definition: definition.mutable_for(transaction),
            expressions: definition.expressions, predicate: definition.predicate,
        });
        Ok(Some(index.pending_definition))
    }

    pub(crate) fn stage_schema_comment_name(
        &self, slot: usize, old: SqlName, new: SqlName, transaction: u32,
    ) -> Result<Option<Option<super::PendingCommentIdentity>>, SqlError> {
        let mut comments = self.comment_catalog();
        let comment = &comments.entries[slot];
        if !comment.used || comment.database != Some(current_database()) { return Ok(None); }
        let identity = comment.pending_identity.filter(|identity| identity.txid == transaction);
        let mut schema = identity.map_or(comment.schema, |identity| identity.schema);
        let mut name = identity.map_or(comment.name, |identity| identity.name);
        let mut changed = super::rename_schema_name(&mut schema, old, new);
        if comment.class == super::CommentClass::Schema {
            changed |= super::rename_schema_name(&mut name, old, new);
        }
        if !changed { return Ok(None); }
        if comment.pending_identity.is_some_and(|identity| identity.txid != transaction)
            || comment.pending.is_some_and(|pending| pending.txid != transaction) {
            return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE,
                "schema comment identity belongs to another transaction"));
        }
        let prior = comment.pending_identity;
        comments.entries[slot].pending_identity = Some(super::PendingCommentIdentity {
            txid: transaction, schema, name,
        });
        Ok(Some(prior))
    }

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
        validate_schema_name(name)?;
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
    fn schema_name_identities_reject_replacement_replay_and_duplicate_restore() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let name = SqlName::parse("reused_schema").unwrap();
        let other = SqlName::parse("duplicate_identity").unwrap();
        let slot = storage.create_schema(name).unwrap();
        let original = storage.schema_def(slot).identity().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(storage.restore_schema(original, other, super::super::CatalogDdlState::Present, false)
                .unwrap_err().sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(storage.find_schema(name.as_str()), Some(slot));
            assert_eq!(storage.find_schema(other.as_str()), None);
            storage.drop_schema(slot);
            assert_eq!(storage.create_schema(name).unwrap(), slot);
            assert_ne!(storage.schema_def(slot).identity(), Some(original));
            assert_eq!(storage.schema_replay_slot(name.as_str(), original, 0).unwrap_err().sqlstate,
                sqlstate::DATA_EXCEPTION);
            assert_eq!(storage.find_schema(name.as_str()), Some(slot));
        });
    }

    #[test]
    fn schema_name_identifier_exhaustion_preserves_existing_names_and_allows_renames() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let name = SqlName::parse("existing_schema").unwrap();
        let new = SqlName::parse("renamed_schema").unwrap();
        let rejected = SqlName::parse("exhausted_schema").unwrap();
        let slot = storage.create_schema(name).unwrap();
        let identity = storage.schema_def(slot).identity().unwrap();
        storage.replace_catalog_sequence_for_test(1_919_999);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(SchemaIdentity::parse(0).unwrap().oid(), 2200);
            assert_eq!(SchemaIdentity::parse(1_919_999).unwrap().oid(), 1_999_999);
            assert!(SchemaIdentity::parse(1_920_000).is_none());
            assert!(SchemaIdentity::parse(u64::MAX).is_none());
            assert_eq!(storage.create_schema(rejected).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.find_schema(rejected.as_str()), None);
            assert_eq!(storage.schema_def(slot).identity(), Some(identity));
            let undo = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), new).unwrap();
            storage.finish_schema_name(undo, CatalogDdlDecision::Commit).unwrap();
            assert_eq!(storage.schema_def(slot).name, new);
            assert_eq!(storage.schema_def(slot).identity(), Some(identity));
        });
    }

    #[test]
    fn schema_name_paths_preserve_quoted_commas_quotes_and_unrelated_spacing() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let old = SqlName::parse("schema,\"quoted\"").unwrap();
        let new = SqlName::parse("Next,\"quoted\"").unwrap();
        let slot = storage.create_schema(old).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut path = crate::util::StackStr::from_str(" public, \"schema,\"\"quoted\"\"\", \"$user\" ");
            super::super::rename_schema_path(&mut path, old, new).unwrap();
            assert_eq!(path.as_str(), " public, \"Next,\"\"quoted\"\"\", \"$user\" ");
            let undo = storage.stage_schema_name(storage.prepare_schema_name(slot, 7).unwrap(), new).unwrap();
            let resolved = storage.compute_path(path.as_str(), "postgres", 7);
            assert!(resolved.entries().contains(&super::super::PathEntry::Schema(slot as u16)));
            storage.finish_schema_name(undo, CatalogDdlDecision::Rollback).unwrap();
            let unchanged = path;
            super::super::rename_schema_path(&mut path, old, new).unwrap();
            assert_eq!(path, unchanged);
        });
    }

    #[test]
    fn schema_name_path_exhaustion_preserves_the_complete_source() {
        let old = SqlName::parse("old").unwrap();
        let new = SqlName::parse(&"\"".repeat(63)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut path = crate::util::StackStr::from_str("public,old");
            let original = path;
            assert_eq!(super::super::rename_schema_path(&mut path, old, new).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(path, original);
        });
    }

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
