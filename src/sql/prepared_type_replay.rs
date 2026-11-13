//! Reconstruct private type and index changes before resolving a saved transaction.

use super::eval::{SqlError, sqlstate};
use super::txn::{DdlUndo, TxnState};
use crate::mem::arena::Arena;
use crate::sql_err;
use crate::storage::{
    CompositeDef, DomainDef, DomainSpec, MAX_COLUMNS, NamedTypeKind, SqlName, Storage,
    TableDefBuilder,
};
use crate::wal::WalOp;

pub(super) fn stage_type_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
    _record_lsn: u64,
    _apply_floor: u64,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let txid = transaction.txid;
    match operation {
        WalOp::RestoreDomain(payload) | WalOp::RestoreTypedDomain(payload) => {
            let (slot, mut definition, inputs) = crate::wal::decode_domain_payload(payload)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared domain definition is corrupt"
                    )
                })?;
            definition.default_inputs = storage.decode_stored_type_inputs(inputs)?;
            stage_domain(storage, transaction, usize::from(slot), &definition)?;
        }
        WalOp::CreateDomain(slot, definition) => {
            stage_domain(storage, transaction, usize::from(*slot), definition)?
        }
        WalOp::DropDomain { schema, name } => {
            let change = storage
                .stage_type_drop(NamedTypeKind::Domain, schema, name, txid)?
                .ok_or_else(|| missing("domain"))?;
            transaction.record_catalog_change(storage, DdlUndo::DomainDropped(change))?;
        }
        WalOp::AlterDomainIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            let slot = storage
                .domain_slot(schema, name, txid)
                .ok_or_else(|| missing("domain"))?;
            let (_, target) = storage.prepare_domain_edit(slot, txid)?;
            let prior = storage.stage_domain_identity(
                target,
                SqlName::parse(new_schema)?,
                SqlName::parse(new_name)?,
                txid,
            )?;
            transaction.record_catalog_change(storage, DdlUndo::DomainAltered(prior))?;
        }
        WalOp::RestoreEnum { payload } => {
            let definition = crate::wal::decode_enum_payload(payload)?;
            if definition.count > storage.enum_label_capacity() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "an enum type may have at most {} labels",
                    storage.enum_label_capacity()
                ));
            }
            let members = arena
                .alloc_slice_with(definition.count, |_| crate::storage::EnumMember::EMPTY)
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "enum recovery members exceed statement arena"
                    )
                })?;
            definition.read_members(members)?;
            stage_enum(
                storage,
                transaction,
                usize::from(definition.slot),
                definition.created_at,
                definition.schema,
                definition.name,
                members,
            )?;
        }
        WalOp::CreateEnum {
            slot,
            created_at,
            schema,
            name,
            members,
        } => {
            stage_enum(
                storage,
                transaction,
                usize::from(*slot),
                *created_at,
                SqlName::parse(schema)?,
                SqlName::parse(name)?,
                members,
            )?;
        }
        WalOp::DropEnum { schema, name } => {
            let change = storage
                .stage_type_drop(NamedTypeKind::Enum, schema, name, txid)?
                .ok_or_else(|| missing("enum"))?;
            transaction.record_catalog_change(storage, DdlUndo::EnumDropped(change))?;
        }
        WalOp::RenameEnum {
            schema,
            old_name,
            new_name,
        } => {
            stage_enum_identity(
                storage,
                transaction,
                schema,
                old_name,
                schema,
                new_name,
                arena,
            )?;
        }
        WalOp::AlterEnumIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            stage_enum_identity(
                storage,
                transaction,
                schema,
                name,
                new_schema,
                new_name,
                arena,
            )?;
        }
        WalOp::RestoreComposite {
            payload,
            wide_fields,
        } => {
            let (slot, definition) = crate::wal::decode_composite_payload(payload, *wide_fields)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared composite definition is corrupt"
                    )
                })?;
            stage_composite(storage, transaction, usize::from(slot), &definition)?;
        }
        WalOp::CreateComposite { slot, definition } => {
            stage_composite(storage, transaction, usize::from(*slot), definition)?
        }
        WalOp::DropComposite { schema, name } => {
            let change = storage
                .stage_type_drop(NamedTypeKind::Composite, schema, name, txid)?
                .ok_or_else(|| missing("composite"))?;
            transaction.record_catalog_change(storage, DdlUndo::CompositeDropped(change))?;
        }
        WalOp::AlterCompositeIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            let slot = storage
                .composite_slot(schema, name, txid)
                .ok_or_else(|| missing("composite"))?;
            let (mut definition, target) = storage.prepare_composite_edit(slot, txid)?;
            definition.schema = SqlName::parse(new_schema)?;
            definition.name = SqlName::parse(new_name)?;
            let prior = storage.stage_composite_alter(target, definition, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::CompositeAltered(prior))?;
        }
        WalOp::CreateIndex {
            created_at,
            schema,
            name,
            table,
            relation,
            method,
            columns,
            include_columns,
            collations,
            explicit_collations,
            operator_classes,
            resolved_operator_classes,
            operator_class_options,
            descending,
            nulls_first,
            n_cols,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            expressions,
            unique,
            definition,
            expression_dependencies,
        } => {
            let mut stored_expressions = [None; crate::storage::MAX_INDEX_COLS];
            for (index, expression) in expressions.iter().enumerate() {
                stored_expressions[index] = expression
                    .map(crate::storage::index_expression_stackstr)
                    .transpose()?;
            }
            let mut index = crate::storage::IndexDef {
                database: storage.current_database_oid(),
                created_at: *created_at,
                schema: SqlName::parse(schema)?,
                name: SqlName::parse(name)?,
                pending_name: None,
                table: SqlName::parse(table)?,
                relation: *relation,
                table_binding: None,
                pending_table: None,
                method: *method,
                ownership: crate::storage::Ownership::BOOTSTRAP,
                columns: *columns,
                expressions: stored_expressions,
                include_columns: *include_columns,
                collations: *collations,
                explicit_collations: *explicit_collations,
                operator_classes: *operator_classes,
                resolved_operator_classes: *resolved_operator_classes,
                operator_class_options: *operator_class_options,
                descending: *descending,
                nulls_first: *nulls_first,
                n_cols: *n_cols,
                n_include_cols: *n_include_cols,
                nulls_not_distinct: *nulls_not_distinct,
                predicate: predicate
                    .map(crate::storage::index_predicate_stackstr)
                    .transpose()?,
                unique: *unique,
                mutable: *definition,
                pending_definition: None,
                ddl_state: crate::storage::CatalogDdlState::Present,
                pending_parent_drop_txid: None,
                ..crate::storage::IndexDef::EMPTY
            };
            index.restore_expression_dependencies(expression_dependencies.entries())?;
            let slot = storage.create_index(index, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::IndexCreated(slot as u32))?;
        }
        WalOp::AlterIndexDefinition {
            schema,
            name,
            definition,
            expressions,
            predicate,
        } => {
            let slot = storage
                .index_slot(schema, name, txid)
                .ok_or_else(|| missing("index"))?;
            let (expressions, predicate) =
                crate::storage::index_expression_image(*expressions, *predicate)?;
            let prior = storage.alter_index_definition(slot, *definition, txid)?;
            storage.alter_index_expressions(slot, expressions, predicate, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::IndexAltered {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::RenameIndex {
            schema,
            name,
            new_name,
        } => {
            let slot = storage
                .index_slot(schema, name, txid)
                .ok_or_else(|| missing("index"))?;
            let prior = storage.rename_index(slot, SqlName::parse(new_name)?, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::IndexRenamed {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::DropIndex { schema, name } => {
            let slot = storage
                .drop_index(schema, name, txid)?
                .ok_or_else(|| missing("index"))?;
            transaction.record_catalog_change(storage, DdlUndo::IndexDropped(slot as u32))?;
        }
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        } => {
            let table = storage
                .find_visible(schema, name, txid)
                .ok_or_else(|| missing("table"))?;
            let (target, current) = storage.select_table_definition(table, txid);
            let mut definition = TableDefBuilder::from_definition(&current);
            definition.schema = SqlName::parse(new_schema)?;
            for key in definition.fkeys.iter_mut().take(definition.n_fkeys) {
                if key.parent_schema == current.schema && key.parent == current.name {
                    key.parent_schema = definition.schema;
                }
            }
            let mut mapping = [None; MAX_COLUMNS];
            for (to, column) in mapping.iter_mut().zip(current.columns()) {
                *to = Some(column.name);
            }
            transaction.write_table_definition(
                storage,
                target,
                definition.clone(),
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )?;
            super::exec::stage_inbound_foreign_key_moves(
                storage,
                transaction,
                table,
                &current,
                &definition,
            )?;
        }
        WalOp::DropTableFk {
            schema,
            table,
            fk_name,
        } => {
            let table = storage
                .find_visible(schema, table, txid)
                .ok_or_else(|| missing("table"))?;
            let (target, current) = storage.select_table_definition(table, txid);
            let at = current
                .fkeys()
                .iter()
                .position(|key| key.name.as_str() == *fk_name)
                .ok_or_else(|| missing("foreign key"))?;
            let mut definition = TableDefBuilder::from_definition(&current);
            definition.fkeys.copy_within(at + 1..definition.n_fkeys, at);
            definition.n_fkeys -= 1;
            let mut mapping = [None; MAX_COLUMNS];
            for (to, column) in mapping.iter_mut().zip(current.columns()) {
                *to = Some(column.name);
            }
            transaction.write_table_definition(
                storage,
                target,
                definition,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )?;
        }
        WalOp::CreateTable {
            identity,
            definition,
        } => {
            let definition = match definition {
                crate::wal::TableDefinitionSource::Stored(value) => {
                    TableDefBuilder::from_definition(value)
                }
                crate::wal::TableDefinitionSource::Builder(value) => (*value).clone(),
            };
            let table = storage.restore_table(
                *identity,
                definition,
                crate::storage::TableCreation::Pending(txid),
            )?;
            transaction.record_catalog_change(storage, DdlUndo::Created(table as u32))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn missing(kind: &str) -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "prepared journal {} source does not exist",
        kind
    )
}

fn stage_domain(
    storage: &mut Storage,
    transaction: &mut TxnState,
    slot: usize,
    definition: &DomainDef,
) -> Result<(), SqlError> {
    if slot >= storage.domain_count() {
        return Err(missing("domain slot"));
    }
    let txid = transaction.txid;
    let current = storage.domain_for(slot, txid);
    if current.visible_to(txid) {
        if current.database != storage.current_database_oid()
            || current.created_at != definition.created_at
            || current.schema != definition.schema
            || current.name != definition.name
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared domain lifetime does not match its source"
            ));
        }
        let (_, target) = storage.prepare_domain_edit(slot, txid)?;
        let prior = storage.stage_domain_alter(
            target,
            DomainSpec {
                base_domain: definition.base_domain,
                base_user_type: definition.base_user_type,
                base: definition.base,
                base_type_mod: definition.base_type_mod,
                not_null: definition.not_null,
                not_null_name: definition.not_null_name,
                not_null_identity: definition.not_null_identity,
                default_expr: definition.default_expr,
                default_inputs: definition.default_inputs.clone(),
                checks: definition.checks,
                n_checks: definition.n_checks,
            },
            txid,
        )?;
        transaction.record_catalog_change(storage, DdlUndo::DomainAltered(prior))?;
    } else {
        let change = storage.restore_prepared_domain(slot, definition, txid)?;
        transaction.record_catalog_change(storage, DdlUndo::DomainCreated(change))?;
    }
    Ok(())
}

fn stage_composite(
    storage: &mut Storage,
    transaction: &mut TxnState,
    slot: usize,
    definition: &CompositeDef,
) -> Result<(), SqlError> {
    if slot >= storage.composite_count() {
        return Err(missing("composite slot"));
    }
    let txid = transaction.txid;
    let current = storage.composite_for(slot, txid);
    if current.visible_to(txid) {
        if current.database != storage.current_database_oid()
            || current.created_at != definition.created_at
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared composite lifetime does not match its source"
            ));
        }
        let (mut current, target) = storage.prepare_composite_edit(slot, txid)?;
        current.schema = definition.schema;
        current.name = definition.name;
        current.fields = definition.fields;
        current.n_fields = definition.n_fields;
        let prior = storage.stage_composite_alter(target, current, txid)?;
        transaction.record_catalog_change(storage, DdlUndo::CompositeAltered(prior))?;
    } else {
        let change = storage.restore_prepared_composite(slot, definition, txid)?;
        transaction.record_catalog_change(storage, DdlUndo::CompositeCreated(change))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_enum(
    storage: &mut Storage,
    transaction: &mut TxnState,
    slot: usize,
    created_at: u64,
    schema: SqlName,
    name: SqlName,
    members: &[crate::storage::EnumMember],
) -> Result<(), SqlError> {
    if slot >= storage.enum_count() {
        return Err(missing("enum slot"));
    }
    let txid = transaction.txid;
    let current = storage.enum_for(slot, txid);
    if current.visible_to(txid) {
        if current.database != storage.current_database_oid() || current.created_at != created_at {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared enum lifetime does not match its source"
            ));
        }
        drop(current);
        let (_, target) = storage.prepare_enum_edit(slot, txid)?;
        let prior =
            storage.restore_prepared_enum_definition(target, schema, name, members, txid)?;
        transaction.record_catalog_change(storage, DdlUndo::EnumAltered(prior))?;
    } else {
        drop(current);
        let change =
            storage.restore_prepared_enum(slot, created_at, schema, name, members, txid)?;
        transaction.record_catalog_change(storage, DdlUndo::EnumCreated(change))?;
    }
    Ok(())
}

fn stage_enum_identity(
    storage: &mut Storage,
    transaction: &mut TxnState,
    schema: &str,
    name: &str,
    new_schema: &str,
    new_name: &str,
    arena: &Arena,
) -> Result<(), SqlError> {
    let txid = transaction.txid;
    let slot = storage
        .enum_slot(schema, name, txid)
        .ok_or_else(|| missing("enum"))?;
    let (current, target) = storage.prepare_enum_edit(slot, txid)?;
    let mut source = current.members();
    let members = arena
        .alloc_slice_with(source.len(), |_| {
            source.next().expect("captured enum count")
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "enum recovery members exceed statement arena"
            )
        })?;
    let prior = storage.stage_enum_alter(
        target,
        SqlName::parse(new_schema)?,
        SqlName::parse(new_name)?,
        members,
        txid,
    )?;
    transaction.record_catalog_change(storage, DdlUndo::EnumAltered(prior))
}
