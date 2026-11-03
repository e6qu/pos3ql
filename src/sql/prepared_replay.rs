//! Restore prepared changes privately before admitting their durable resolution.

use super::eval::{SqlError, sqlstate};
use super::txn::{DdlUndo, TxnState};
use crate::mem::Arena;
use crate::sql_err;
use crate::storage::{AccessClass, AccessObject, SqlName, Storage};
use crate::wal::WalOp;

pub(super) fn stage_catalog_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: WalOp<'_>,
    record_lsn: u64,
    apply_floor: u64,
    arena: &Arena,
) -> Result<(), SqlError> {
    if super::prepared_type_replay::stage_type_record(
        storage,
        transaction,
        &operation,
        record_lsn,
        apply_floor,
        arena,
    )? {
        return Ok(());
    }
    if super::prepared_query_replay::stage_query_record(storage, transaction, &operation, arena)? {
        return Ok(());
    }
    if super::prepared_object_replay::stage_object_record(storage, transaction, &operation)? {
        return Ok(());
    }
    if super::prepared_subscription_replay::stage_subscription_record(
        storage,
        transaction,
        &operation,
    )? || super::prepared_replay_rows::stage_truncate(storage, transaction, &operation, arena)?
    {
        return Ok(());
    }
    let txid = transaction.txid;
    match operation {
        WalOp::DatabaseScope { oid } => {
            if oid != storage.current_database_oid().get() {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared journal changes database scope"
                ));
            }
        }
        WalOp::ReserveCatalogSequence(identity) => {
            storage.restore_catalog_identity_sequence(identity.get())
        }
        // Logical messages are carried by the retained journal for decoding;
        // they have no catalog or row installation to reconstruct.
        WalOp::LogicalMessage { .. } => {}
        WalOp::CreateRoutine {
            identity,
            definition,
            dependencies,
        } => {
            let change = storage.replay_routine_definition(
                identity,
                *definition,
                dependencies.materialize(storage, txid, |section| match section {
                    crate::storage::StoredInputSection::RoutineBody => {
                        Some(definition.body.as_str())
                    }
                    crate::storage::StoredInputSection::ParameterDefault(index) => definition
                        .parameters()
                        .get(usize::from(index))
                        .and_then(|parameter| parameter.mode.default_sql()),
                    _ => None,
                })?,
                txid,
            )?;
            transaction.record_routine_change(storage, change)?;
        }
        WalOp::BrinMaintenance {
            index_created_at,
            pages_per_range,
            summarized_until_page,
            unsummarized_ranges,
            range_count,
        } => {
            // Summarization is an absolute, nontransactional maintenance image.
            let slot = (0..storage.index_count())
                .find(|slot| {
                    storage
                        .index_visible_to(*slot, txid)
                        .is_some_and(|index| index.created_at == index_created_at)
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared BRIN source is unavailable"
                    )
                })?;
            storage.restore_brin_maintenance(
                slot,
                pages_per_range,
                summarized_until_page,
                &unsummarized_ranges[..usize::from(range_count)],
            )?;
        }
        WalOp::CreatePublication {
            created_at,
            name,
            owner,
            all_tables,
            tables,
            table_column_masks,
            table_include_descendants,
            table_filter_sql,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
        } => {
            storage.role_reference_ready(usize::from(owner), txid)?;
            let masks = table_column_masks.materialize(table_count).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared publication masks are corrupt"
                )
            })?;
            let slot = storage.create_publication(
                crate::storage::PublicationSpec {
                    creation: crate::storage::CatalogCreation::Restore(
                        crate::storage::CatalogIdentity::require(created_at)?,
                    ),
                    name: SqlName::parse(name)?,
                    all_tables,
                    tables: &tables[..table_count],
                    table_column_masks: &masks[..table_count],
                    table_include_descendants: &table_include_descendants[..table_count],
                    table_filter_sql: &table_filter_sql[..table_count],
                    schemas: &schemas[..schema_count],
                    publish_insert,
                    publish_update,
                    publish_delete,
                    publish_truncate,
                    publish_via_partition_root,
                    publish_generated_columns,
                },
                txid,
            )?;
            storage.restore_publication_owner(slot, owner);
            transaction.record_catalog_change(storage, DdlUndo::PublicationCreated(slot as u32))?;
        }
        WalOp::DropPublication { name } => {
            let slot = storage.drop_publication(name, txid)?.ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared publication is unavailable"
                )
            })?;
            transaction.record_catalog_change(storage, DdlUndo::PublicationDropped(slot as u32))?;
        }
        WalOp::AlterPublication {
            name,
            all_tables,
            tables,
            table_column_masks,
            table_include_descendants,
            table_filter_sql,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
        } => {
            let masks = table_column_masks.materialize(table_count).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared publication masks are corrupt"
                )
            })?;
            let definition = crate::storage::PublicationDefinition {
                all_tables,
                tables,
                table_column_masks: masks,
                table_include_descendants,
                table_filters: crate::storage::PublicationFilters::from_sql(
                    &table_filter_sql[..table_count],
                )?,
                table_count,
                schemas,
                schema_count,
                publish_insert,
                publish_update,
                publish_delete,
                publish_truncate,
                publish_via_partition_root,
                publish_generated_columns,
            };
            let (slot, prior) = storage.alter_publication(name, definition, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::PublicationAltered {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetPublicationOwner { name, owner } => {
            let (slot, _) = storage.publication_definition(name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared publication is unavailable"
                )
            })?;
            let prior = storage.set_publication_owner(slot, usize::from(owner), txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::PublicationOwnerChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::RenamePublication { name, new_name } => {
            let (slot, _) = storage.publication_definition(name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared publication is unavailable"
                )
            })?;
            let prior = storage.rename_publication(slot, SqlName::parse(new_name)?, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::PublicationRenamed {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetCast {
            identity,
            definition,
        } => {
            let undo = storage.restore_prepared_cast(identity, definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropCast { source, target } => {
            let source = storage.bind_routine_result(source, txid)?;
            let target = storage.bind_routine_result(target, txid)?;
            let slot = storage.drop_cast(source, target, txid)?.ok_or_else(|| {
                sql_err!(sqlstate::DATA_EXCEPTION, "prepared cast is unavailable")
            })?;
            transaction.record_catalog_change(storage, DdlUndo::CastDropped(slot as u32))?;
        }
        WalOp::SetOperator {
            identity,
            definition,
        } => {
            let undo = storage.restore_prepared_operator(identity, definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropOperator {
            schema,
            name,
            mut signature,
        } => {
            signature.left = signature
                .left
                .map(|value| storage.bind_routine_result(value, txid))
                .transpose()?;
            signature.right = signature
                .right
                .map(|value| storage.bind_routine_result(value, txid))
                .transpose()?;
            let slot = storage
                .operator_slot_exact(schema, name, signature, txid)
                .ok_or_else(|| {
                    sql_err!(sqlstate::DATA_EXCEPTION, "prepared operator is unavailable")
                })?;
            storage.drop_operator(slot, txid);
            transaction.record_catalog_change(storage, DdlUndo::OperatorDropped(slot as u32))?;
        }
        WalOp::SetOperatorFamily {
            identity,
            definition,
        } => {
            let undo = storage.restore_prepared_operator_family(identity, *definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::RestoreOperatorFamily(payload) => {
            let (identity, definition) = crate::wal::decode_operator_family_payload(payload)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared operator family is corrupt"
                    )
                })?;
            let undo = storage.restore_prepared_operator_family(identity, definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropOperatorFamily { schema, name } => {
            let slot = storage
                .operator_family_slot_exact(schema, name, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared operator family is unavailable"
                    )
                })?;
            storage.drop_operator_family(slot, txid);
            transaction
                .record_catalog_change(storage, DdlUndo::OperatorFamilyDropped(slot as u32))?;
        }
        WalOp::SetOperatorClass {
            identity,
            definition,
        } => {
            let undo = storage.restore_prepared_operator_class(identity, *definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::RestoreOperatorClass(payload) => {
            let (identity, definition) = crate::wal::decode_operator_class_payload(payload)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared operator class is corrupt"
                    )
                })?;
            let undo = storage.restore_prepared_operator_class(identity, definition, txid)?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropOperatorClass { schema, name } => {
            let slot = storage
                .operator_class_slot_exact(schema, name, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared operator class is unavailable"
                    )
                })?;
            storage.drop_operator_class(slot, txid);
            transaction
                .record_catalog_change(storage, DdlUndo::OperatorClassDropped(slot as u32))?;
        }
        WalOp::SetCollation {
            slot,
            created_at,
            definition,
        } => {
            let undo = storage.restore_prepared_collation(
                usize::from(slot),
                created_at,
                definition,
                txid,
            )?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropCollation { schema, name } => {
            let slot = storage.collation_slot(schema, name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared collation is unavailable"
                )
            })?;
            storage.drop_collation(slot, txid);
            transaction.record_catalog_change(storage, DdlUndo::CollationDropped(slot as u32))?;
        }
        WalOp::SetConversion {
            slot,
            created_at,
            definition,
        } => {
            let undo = storage.restore_prepared_conversion(
                usize::from(slot),
                created_at,
                definition,
                txid,
            )?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropConversion { schema, name } => {
            let slot = storage.conversion_slot(schema, name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared conversion is unavailable"
                )
            })?;
            storage.drop_conversion(slot, txid);
            transaction.record_catalog_change(storage, DdlUndo::ConversionDropped(slot as u32))?;
        }
        WalOp::SetTextSearch {
            slot,
            created_at,
            definition,
        } => {
            let undo = storage.restore_prepared_text_search(
                usize::from(slot),
                created_at,
                definition,
                txid,
            )?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropTextSearch { kind, schema, name } => {
            let slot = storage
                .text_search_slot(kind, schema, name, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared text search object is unavailable"
                    )
                })?;
            storage.drop_text_search_object(slot, txid);
            transaction.record_catalog_change(storage, DdlUndo::TextSearchDropped(slot as u32))?;
        }
        WalOp::SetEventTrigger {
            slot,
            created_at,
            definition,
        } => {
            let undo = storage.restore_prepared_event_trigger(
                usize::from(slot),
                created_at,
                definition,
                txid,
            )?;
            transaction.record_catalog_change(storage, undo)?;
        }
        WalOp::DropEventTrigger { name } => {
            let slot = storage.event_trigger_slot(name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared event trigger is unavailable"
                )
            })?;
            storage.drop_event_trigger(slot, txid)?;
            transaction
                .record_catalog_change(storage, DdlUndo::EventTriggerDropped(slot as u32))?;
        }
        WalOp::Comment {
            class,
            schema,
            name,
            subid,
            text,
        } => {
            let class = crate::storage::CommentClass::from_u8(class).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared comment has invalid class"
                )
            })?;
            if let Some((slot, prior)) = storage.set_comment(
                class,
                SqlName::parse(schema)?,
                SqlName::parse(name)?,
                subid,
                text.map(crate::storage::comment_stackstr).transpose()?,
                txid,
            )? {
                transaction.record_catalog_change(
                    storage,
                    DdlUndo::CommentSet {
                        slot: slot as u32,
                        prior,
                    },
                )?;
            }
        }
        WalOp::UpsertRole {
            slot,
            oid,
            name,
            attributes,
        } => {
            let slot = usize::from(slot);
            let prior = storage.restore_prepared_role(
                slot,
                oid,
                SqlName::parse(name)?,
                attributes,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::DropRole { name } => {
            let slot = storage.find_role_visible(name, txid).ok_or_else(|| {
                sql_err!(sqlstate::DATA_EXCEPTION, "prepared role is unavailable")
            })?;
            let prior = storage.drop_role_in(slot, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::RenameRole { name, new_name } => {
            let slot = storage.find_role_visible(name, txid).ok_or_else(|| {
                sql_err!(sqlstate::DATA_EXCEPTION, "prepared role is unavailable")
            })?;
            let prior = storage.rename_role(slot, SqlName::parse(new_name)?, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::UpsertRoleMembership {
            identity,
            role,
            member,
            grantor,
            options,
        } => {
            let (slot, prior) = storage
                .restore_prepared_role_membership(identity, role, member, grantor, options, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleMembershipChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::DropRoleMembership { identity } => {
            let slot = identity.slot();
            if slot >= storage.role_membership_count() {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared membership is unavailable"
                ));
            }
            let member = storage.role_membership(slot);
            if !member.visible_to(txid)
                || member.creation_to(txid).created_at() != identity.created_at()
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared membership selects another lifetime"
                ));
            }
            let (selected, prior) = storage.change_role_membership(
                usize::from(member.role),
                usize::from(member.member),
                usize::from(member.grantor),
                member.options_to(txid),
                false,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleMembershipChanged {
                    slot: selected as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetRoleSetting {
            role,
            database,
            name,
            value,
        } => {
            let role = role
                .map(|name| {
                    storage
                        .find_role_visible(name, txid)
                        .map(|slot| slot as u16)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "prepared role setting source is unavailable"
                            )
                        })
                })
                .transpose()?;
            let database = database
                .map(|oid| {
                    let oid = crate::storage::DatabaseOid::parse(oid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared role setting database is invalid"
                        )
                    })?;
                    storage.database_slot_by_oid(oid, txid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared role setting database is unavailable"
                        )
                    })?;
                    Ok(oid)
                })
                .transpose()?;
            let scope = match (role, database) {
                (Some(role), None) => crate::storage::RoleSettingScope::RoleAllDatabases(role),
                (Some(role), Some(database)) => {
                    crate::storage::RoleSettingScope::RoleInDatabase { role, database }
                }
                (None, Some(database)) => {
                    crate::storage::RoleSettingScope::AllRolesInDatabase(database)
                }
                (None, None) => {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared role setting scope is invalid"
                    ));
                }
            };
            let value = value
                .map(|value| {
                    let value = crate::util::StackStr::from_str(value);
                    if value.is_truncated() {
                        Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared role setting exceeds its bound"
                        ))
                    } else {
                        Ok(value)
                    }
                })
                .transpose()?;
            let (slot, prior) =
                storage.change_role_setting(scope, SqlName::parse(name)?, value, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::RoleSettingChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetObjectOwner {
            class,
            object_oid,
            schema,
            name,
            owner,
        } => {
            let object = access_object(storage, class, object_oid, schema, name, txid)?;
            let owner = storage.find_role_visible(owner, txid).ok_or_else(|| {
                sql_err!(sqlstate::DATA_EXCEPTION, "prepared owner is unavailable")
            })?;
            let prior = match object.class {
                AccessClass::Schema => {
                    storage.set_schema_owner(usize::from(object.slot), owner, txid)?
                }
                AccessClass::Tablespace => {
                    storage.set_tablespace_owner(usize::from(object.slot), owner, txid)?
                }
                AccessClass::Extension => {
                    storage.set_extension_owner(usize::from(object.slot), owner, txid)?
                }
                _ => storage.set_object_owner(object, owner, txid),
            };
            transaction
                .record_catalog_change(storage, DdlUndo::ObjectOwnerChanged { object, prior })?;
        }
        WalOp::SetObjectAcl {
            class,
            object_oid,
            schema,
            name,
            grantee,
            grantor,
            privileges,
            grant_options,
            order,
        } => {
            let object = access_object(storage, class, object_oid, schema, name, txid)?;
            let grantee = acl_role(storage, grantee, txid)?;
            let grantor = acl_role(storage, grantor, txid)?;
            let (slot, prior) = storage.change_acl(
                object,
                grantee,
                grantor,
                privileges,
                grant_options,
                crate::storage::ObjectAclWrite::Exact {
                    transaction: txid,
                    order,
                },
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ObjectAclChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetColumnAcl {
            class,
            schema,
            name,
            column,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            let object = access_object(storage, class, 0, schema, name, txid)?;
            let target = crate::storage::ColumnPrivilegeTarget::new(object, column)?;
            let grantee = acl_role(storage, grantee, txid)?;
            let grantor = acl_role(storage, grantor, txid)?;
            let (slot, prior) = storage.change_column_acl(
                target,
                grantee,
                grantor,
                privileges,
                grant_options,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ColumnAclChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetDefaultAcl {
            slot,
            expected,
            image,
        } => {
            let slot = usize::from(slot);
            let prior = storage.stage_default_acl_image(slot, expected, image, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::DefaultAclChanged {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::CreateLargeObject {
            oid,
            created_at,
            allocated,
        } => {
            let oid = crate::storage::LargeObjectOid::parse(oid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared large object identifier is invalid"
                )
            })?;
            let slot = storage.restore_prepared_large_object(oid, created_at, allocated, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::LargeObjectCreated(slot as u32))?;
        }
        WalOp::DropLargeObject { oid } => {
            let oid = crate::storage::LargeObjectOid::parse(oid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared large object identifier is invalid"
                )
            })?;
            let slot = storage.drop_large_object(oid, txid)?.ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared large object is unavailable"
                )
            })?;
            transaction.record_catalog_change(storage, DdlUndo::LargeObjectDropped(slot as u32))?;
        }
        // These records belong to independent maintenance transactions or SQL
        // commands that cannot enter a prepared transaction.
        WalOp::CreateReplicationSlot { .. }
        | WalOp::AlterReplicationSlot { .. }
        | WalOp::DropReplicationSlot { .. }
        | WalOp::AdvanceReplicationSlot { .. }
        | WalOp::CompleteSubscriptionCleanup { .. }
        | WalOp::FailSubscription { .. }
        | WalOp::ResetSubscriptionRelations { .. }
        | WalOp::AddSubscriptionRelation { .. }
        | WalOp::AdvanceSubscription { .. }
        | WalOp::SetSystemSetting { .. }
        | WalOp::CreateDatabase { .. }
        | WalOp::DropDatabase { .. }
        | WalOp::CreateTablespace { .. }
        | WalOp::DropTablespace { .. }
        | WalOp::ResetUnloggedRelations => {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared journal contains a nontransactional operation"
            ));
        }
        _ => {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared journal contains an unsupported catalog operation"
            ));
        }
    }
    Ok(())
}

fn access_object(
    storage: &Storage,
    class: u8,
    oid: i32,
    schema: &str,
    name: &str,
    transaction: u32,
) -> Result<AccessObject, SqlError> {
    let class = AccessClass::from_u8(class)
        .ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "prepared object class is invalid"))?;
    let object = if class == AccessClass::Routine {
        storage
            .routine_slot_by_oid(oid, transaction)
            .map(Storage::routine_access_object)
    } else {
        storage.resolve_access_object(class, schema, name, transaction)
    };
    object.ok_or_else(|| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "prepared catalog source is unavailable"
        )
    })
}

fn acl_role(storage: &Storage, name: &str, transaction: u32) -> Result<u16, SqlError> {
    if name == "PUBLIC" {
        return Ok(crate::storage::PUBLIC_ROLE);
    }
    storage
        .find_role_visible(name, transaction)
        .map(|slot| slot as u16)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared privilege role is unavailable"
            )
        })
}
