//! A namespace move stages each affected definition in its owning transaction.

use super::*;
use crate::sql::txn::DdlUndo;
use crate::storage::{rename_schema_name, rename_schema_qualified_sql, rename_user_type_schema};

pub(super) fn rename(
    storage: &mut Storage,
    txn: &mut TxnState,
    slot: usize,
    name: SqlName,
    arena: &Arena,
) -> Result<(), SqlError> {
    let source = storage.prepare_schema_name(slot, txn.txid)?;
    let old = source.name();
    let undo = storage.stage_schema_name(source, name)?;
    txn.record_catalog_change(storage, DdlUndo::SchemaRenamed(undo))?;

    // Publish type identities privately before rebinding their dependents.
    for slot in 0..storage.domain_count() {
        let definition = storage.domain_for(slot, txn.txid);
        if definition.database != storage.current_database_oid()
            || !definition.visible_to(txn.txid) || definition.schema != old { continue; }
        let (_, target) = storage.prepare_domain_edit(slot, txn.txid)?;
        let change = storage.stage_domain_identity(target, name, definition.name, txn.txid)?;
        txn.record_catalog_change(storage, DdlUndo::DomainAltered(change))?;
    }
    for slot in 0..storage.enum_count() {
        let definition = storage.enum_for(slot, txn.txid);
        if definition.database != storage.current_database_oid()
            || !definition.visible_to(txn.txid) || definition.schema != old { continue; }
        let (definition, target) = storage.prepare_enum_edit(slot, txn.txid)?;
        let members = arena.alloc_slice_with(definition.members().len(), |_| crate::storage::EnumMember::EMPTY)
            .map_err(|_| arena_full())?;
        for (target, member) in members.iter_mut().zip(definition.members()) { *target = member; }
        let change = storage.stage_enum_alter(target, name, definition.name, members, txn.txid)?;
        txn.record_catalog_change(storage, DdlUndo::EnumAltered(change))?;
    }
    for slot in 0..storage.composite_count() {
        let mut definition = storage.composite_for(slot, txn.txid);
        if definition.database != storage.current_database_oid()
            || !definition.visible_to(txn.txid) { continue; }
        let mut changed = rename_schema_name(&mut definition.schema, old, name);
        for field in definition.fields.iter_mut().take(definition.n_fields) {
            changed |= rename_user_type_schema(&mut field.user_type, old, name);
        }
        if changed {
            let (_, target) = storage.prepare_composite_edit(slot, txn.txid)?;
            let change = storage.stage_composite_alter(target, definition, txn.txid)?;
            txn.record_catalog_change(storage, DdlUndo::CompositeAltered(change))?;
        }
    }
    for slot in 0..storage.domain_count() {
        let definition = storage.domain_for(slot, txn.txid);
        if definition.database != storage.current_database_oid() || !definition.visible_to(txn.txid) { continue; }
        let mut spec = crate::storage::DomainSpec {
            base_domain: definition.base_domain,
            base_user_type: definition.base_user_type,
            base: definition.base,
            base_type_mod: definition.base_type_mod,
            not_null: definition.not_null,
            not_null_name: definition.not_null_name,
            default_expr: definition.default_expr,
            checks: definition.checks,
            n_checks: definition.n_checks,
        };
        let mut changed = rename_user_type_schema(&mut spec.base_domain, old, name);
        changed |= rename_user_type_schema(&mut spec.base_user_type, old, name);
        if let Some(default) = &mut spec.default_expr {
            changed |= rename_schema_qualified_sql(default, &mut [], old, name)?;
        }
        for check in spec.checks.iter_mut().take(spec.n_checks) {
            changed |= rename_schema_qualified_sql(&mut check.expression, &mut [], old, name)?;
        }
        if changed {
            let (_, target) = storage.prepare_domain_edit(slot, txn.txid)?;
            let change = storage.stage_domain_alter(target, spec, txn.txid)?;
            txn.record_catalog_change(storage, DdlUndo::DomainAltered(change))?;
        }
    }

    for slot in 0..storage.table_count() {
        if storage.table(slot).database() != storage.current_database_oid()
            || !storage.table(slot).visible_to(txn.txid) { continue; }
        let (target, mut definition) = storage.select_table_definition(slot, txn.txid);
        if source.rewrite_table(name, &mut definition)? {
            let mut mapping = [None; MAX_COLUMNS];
            for (entry, column) in mapping.iter_mut().zip(definition.columns()) {
                *entry = Some(column.name);
            }
            txn.write_table_definition(storage, target, definition, &mapping, false)?;
        }
    }
    for slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(slot, txn.txid);
        if sequence.database != storage.current_database_oid() || !sequence.visible_to(txn.txid) { continue; }
        let mut schema = sequence.schema;
        let mut owner = sequence.owner;
        let mut generator_for = sequence.generator_for;
        let mut changed = rename_schema_name(&mut schema, old, name);
        if let Some(owner) = &mut owner {
            changed |= rename_schema_name(&mut owner.table_schema, old, name);
        }
        if let Some(generator) = &mut generator_for {
            changed |= rename_schema_name(&mut generator.table_schema, old, name);
        }
        if !changed { continue; }
        let target = storage.prepare_sequence_edit(slot, txn.txid)?;
        let change = storage.stage_sequence_alter(target, crate::storage::SequenceAlteration {
            schema, name: sequence.name,
            spec: SeqSpec {
                data_type: sequence.data_type, increment: sequence.increment,
                min_value: sequence.min_value, max_value: sequence.max_value,
                start_value: sequence.start_value, cache: sequence.cache, cycle: sequence.cycle,
            },
            owner, generator_for, persistence: sequence.persistence, restart: None,
        }, txn.txid)?;
        txn.record_sequence_change(storage, change)?;
    }
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if view.database != storage.current_database_oid() || !view.visible_to(txn.txid)
            || view.schema_for(txn.txid) != old { continue; }
        let change = storage.stage_view_schema(storage.prepare_view_edit(slot, txn.txid)?, name, txn.txid)?;
        txn.record_catalog_change(storage, DdlUndo::ViewChanged(crate::storage::ViewPublication::Altered(change)))?;
    }
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txn.txid);
        if routine.database != storage.current_database_oid() || !routine.visible_to(txn.txid) { continue; }
        if routine.schema_for(txn.txid) == old {
            let change = storage.alter_routine_identity(storage.prepare_routine_edit(slot, txn.txid)?, name, routine.name_for(txn.txid), txn.txid)?;
            txn.record_routine_change(storage, change)?;
        }
        let mut definition = crate::storage::RoutineDefinition {
            arguments: routine.arguments, argument_count: routine.argument_count,
            parameters: routine.parameters, parameter_count: routine.parameter_count,
            kind: routine.kind, result_columns: routine.result_columns,
            result_column_count: routine.result_column_count, language: routine.language,
            attributes: routine.attributes, configs: routine.configs, config_count: routine.config_count,
            body_kind: routine.body_kind, body: routine.body, creation_path: routine.creation_path,
        };
        let dependencies = storage.snapshot_routine_dependencies_for(slot, txn.txid, arena)?;
        let changed = source.rewrite_routine(name, &mut definition)?;
        if changed || dependencies.entries().iter().any(|dependency| dependency.schema == old || dependency.referenced_schema == old) {
            let change = storage.replace_routine(storage.prepare_routine_edit(slot, txn.txid)?, definition,
                crate::storage::RoutineDependencies::RenameSchema { old, new: name }, txn.txid)?;
            txn.record_routine_change(storage, change)?;
        }
    }
    for slot in 0..storage.rule_count() {
        let Some(rule) = storage.rule_slot_visible_to(slot, txn.txid) else { continue; };
        let dependencies = storage.snapshot_rule_dependencies(slot, txn.txid, arena)?;
        if !source.rewrites_rule(name, rule.definition_for(txn.txid))?
            && !dependencies.entries().iter().any(|dependency| dependency.schema == old || dependency.referenced_schema == old) { continue; }
        let (change, _) = storage.alter_rule(storage.prepare_rule_edit(slot, txn.txid)?,
            crate::storage::RuleMetadataChange::Schema { old, new: name }, txn.txid)?;
        txn.record_catalog_change(storage, DdlUndo::RuleAltered(change))?;
    }
    for slot in 0..storage.comment_capacity() {
        if let Some(prior) = storage.stage_schema_comment_name(slot, old, name, txn.txid)? {
            txn.record_catalog_change(storage, DdlUndo::ConstraintCommentRenamed { slot: slot as u32, prior })?;
        }
    }
    Ok(())
}
