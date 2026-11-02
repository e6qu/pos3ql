//! Restores object DDL through its ordinary transaction-private undo paths.

use super::*;
use crate::storage::{AccessClass, AccessObject, ExtensionConfigRelationKind, SqlName};

pub(super) fn stage_object_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
) -> Result<bool, SqlError> {
    let txid = transaction.txid;
    match operation {
        WalOp::CreateAccessMethod {
            identity,
            name,
            handler,
        } => {
            let slot = storage.restore_prepared_access_method(
                *identity,
                crate::storage::AccessMethodDefinition {
                    name: SqlName::parse(name)?,
                    handler: *handler,
                },
                txid,
            )?;
            transaction
                .record_catalog_change(storage, DdlUndo::AccessMethodCreated(slot as u32))?;
        }
        WalOp::DropAccessMethod { name } => {
            let slot = storage.access_method_slot(name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared access method does not exist"
                )
            })?;
            storage.drop_access_method(slot, txid);
            transaction
                .record_catalog_change(storage, DdlUndo::AccessMethodDropped(slot as u32))?;
        }
        WalOp::UpsertExtension {
            name,
            schema,
            version,
            relocatable,
            owner,
            created_at,
        } => {
            let namespace = storage.find_schema_visible(schema, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "prepared extension namespace does not exist"
                )
            })?;
            let owner = storage.find_role_visible(owner, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "prepared extension owner does not exist"
                )
            })?;
            let undo = storage.restore_prepared_extension(
                SqlName::parse(name)?,
                namespace,
                *relocatable,
                crate::storage::ExtensionVersion::parse(version)?,
                owner,
                *created_at,
                txid,
            )?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropExtension { name } => {
            // A final tombstone also represents CREATE followed by DROP.
            if let Some(slot) = storage.extension_slot(name, txid) {
                storage.drop_extension_in(slot, txid)?;
                transaction
                    .record_catalog_change(storage, DdlUndo::ExtensionDropped(slot as u32))?;
            }
        }
        WalOp::SetExtensionDependency {
            extension,
            class,
            object_oid,
            schema,
            name,
            kind,
            exists,
        } => {
            let extension = extension_slot(storage, extension, txid)?;
            let object = if *class == AccessClass::Routine {
                storage
                    .routine_slot_by_oid(*object_oid, txid)
                    .map(|slot| AccessObject {
                        class: *class,
                        slot: slot as u16,
                    })
            } else {
                storage.resolve_access_object(*class, schema, name, txid)
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared extension dependency source does not exist"
                )
            })?;
            let (slot, prior) =
                storage.change_extension_dependency(extension, object, *kind, *exists, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ExtensionDependencyChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetExtensionConfig {
            extension,
            ordinal,
            relation_kind,
            schema,
            name,
            condition,
            exists,
        } => {
            let extension = extension_slot(storage, extension, txid)?;
            let class = match relation_kind {
                ExtensionConfigRelationKind::Table => AccessClass::Table,
                ExtensionConfigRelationKind::Sequence => AccessClass::Sequence,
            };
            let object = storage
                .resolve_access_object(class, schema, name, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared extension configuration relation does not exist"
                    )
                })?;
            let relation = crate::storage::ExtensionConfigRelation::from_access_object(object)
                .expect("configuration relation was resolved by its typed class");
            let (slot, prior) = storage.stage_prepared_extension_config(
                extension,
                relation,
                crate::storage::extension_config_condition(condition)?,
                *exists,
                *ordinal,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ExtensionConfigChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::AlterDatabase {
            oid,
            definition,
            owner,
        } => {
            let oid = crate::storage::DatabaseOid::parse(*oid).ok_or_else(|| {
                sql_err!(sqlstate::DATA_EXCEPTION, "prepared database OID is invalid")
            })?;
            let slot = storage.database_slot_by_oid(oid, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "prepared database source does not exist"
                )
            })?;
            storage.role_reference_ready(usize::from(*owner), txid)?;
            let prior_definition = storage.alter_database_definition(slot, *definition, txid)?;
            let prior_owner = storage.set_object_owner(
                AccessObject {
                    class: AccessClass::Database,
                    slot: slot as u16,
                },
                usize::from(*owner),
                txid,
            );
            transaction.record_catalog_change(
                storage,
                DdlUndo::DatabaseAltered {
                    slot: slot as u32,
                    prior_definition,
                    prior_owner,
                },
            )?;
        }
        WalOp::AlterTablespace {
            name,
            new_name,
            options,
            owner,
        } => {
            let slot = storage.tablespace_slot(name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "prepared tablespace source does not exist"
                )
            })?;
            let prior_definition = storage.alter_tablespace_definition(
                slot,
                SqlName::parse(new_name)?,
                *options,
                txid,
            )?;
            let prior_owner = match storage.set_tablespace_owner(slot, usize::from(*owner), txid) {
                Ok(prior) => prior,
                Err(error) => {
                    storage.rollback_tablespace_alter(slot, prior_definition);
                    return Err(error);
                }
            };
            transaction.record_catalog_change(
                storage,
                DdlUndo::TablespaceAltered {
                    slot: slot as u32,
                    prior_definition,
                    prior_owner,
                },
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn extension_slot(storage: &Storage, name: &str, transaction: u32) -> Result<usize, SqlError> {
    storage.extension_slot(name, transaction).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "prepared extension source does not exist"
        )
    })
}
