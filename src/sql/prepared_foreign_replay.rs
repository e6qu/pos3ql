//! Final foreign catalog images reconstruct reversible prepared overlays.

use super::*;
use crate::storage::foreign::{
    ForeignObjectClass, PreparedForeignChange, PreparedForeignKey, PreparedForeignNamespace,
};

#[inline(never)]
fn stage_foreign_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
    namespace: &[PreparedForeignNamespace],
) -> Result<bool, SqlError> {
    macro_rules! stage {
        ($slot:expr, $creation:expr, $owner:expr, $definition:expr, $prepare:ident,
            $class:ident, $created:ident, $dropped:ident, $altered:ident, $restore:ident) => {{
            let slot = usize::from($slot);
            let transaction_id = transaction.txid;
            let change = storage.$prepare(
                slot,
                $creation,
                $owner,
                $definition,
                transaction_id,
                namespace,
            )?;
            let mark = transaction.ddl().len();
            let result = (|| -> Result<(), SqlError> {
                match change {
                    PreparedForeignChange::Unchanged => {}
                    PreparedForeignChange::Created => {
                        transaction.record_ddl(storage, DdlUndo::$created(slot as u32))?
                    }
                    PreparedForeignChange::Dropped => {
                        transaction.record_ddl(storage, DdlUndo::$dropped(slot as u32))?
                    }
                    PreparedForeignChange::Changed {
                        definition,
                        ownership,
                    } => {
                        if let Some(prior) = definition {
                            transaction.record_ddl(
                                storage,
                                DdlUndo::$altered {
                                    slot: slot as u32,
                                    prior,
                                },
                            )?;
                        }
                        if let Some(prior) = ownership {
                            transaction.record_ddl(
                                storage,
                                DdlUndo::ForeignOwnerChanged {
                                    class: ForeignObjectClass::$class,
                                    slot: slot as u32,
                                    prior,
                                },
                            )?;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                match change {
                    PreparedForeignChange::Unchanged => {}
                    PreparedForeignChange::Created => {
                        storage.foreign_catalog_rollback_create(ForeignObjectClass::$class, slot)
                    }
                    PreparedForeignChange::Dropped => storage.foreign_catalog_rollback_drop(
                        ForeignObjectClass::$class,
                        slot,
                        transaction_id,
                    ),
                    PreparedForeignChange::Changed {
                        definition,
                        ownership,
                    } => {
                        if let Some(prior) = definition {
                            storage.$restore(slot, prior);
                        }
                        if let Some(prior) = ownership {
                            storage.rollback_foreign_catalog_owner(
                                ForeignObjectClass::$class,
                                slot,
                                prior,
                            );
                        }
                    }
                }
                transaction.rewind_ddl(mark);
                return Err(error);
            }
        }};
    }
    match operation {
        WalOp::SetForeignDataWrapper {
            slot,
            created_at,
            owner,
            definition,
        } => stage!(
            *slot,
            *created_at,
            Some(*owner),
            *definition,
            stage_prepared_foreign_wrapper,
            Wrapper,
            ForeignDataWrapperCreated,
            ForeignDataWrapperDropped,
            ForeignDataWrapperAltered,
            rollback_foreign_wrapper_alter
        ),
        WalOp::SetForeignServer {
            slot,
            created_at,
            owner,
            definition,
        } => stage!(
            *slot,
            *created_at,
            Some(*owner),
            *definition,
            stage_prepared_foreign_server,
            Server,
            ForeignServerCreated,
            ForeignServerDropped,
            ForeignServerAltered,
            rollback_foreign_server_alter
        ),
        WalOp::SetUserMapping {
            slot,
            created_at,
            definition,
        } => stage!(
            *slot,
            *created_at,
            None,
            *definition,
            stage_prepared_foreign_mapping,
            Mapping,
            UserMappingCreated,
            UserMappingDropped,
            UserMappingAltered,
            rollback_foreign_mapping_alter
        ),
        WalOp::SetForeignTable {
            slot,
            created_at,
            definition,
        } => stage!(
            *slot,
            *created_at,
            None,
            **definition,
            stage_prepared_foreign_table,
            Table,
            ForeignTableCreated,
            ForeignTableDropped,
            ForeignTableAltered,
            rollback_foreign_table_alter
        ),
        WalOp::RestoreForeignTable(payload) => {
            let (slot, created_at, definition) = crate::wal::decode_foreign_table_payload(payload)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared foreign-table definition is corrupt"
                    )
                })?;
            stage!(
                slot,
                created_at,
                None,
                definition,
                stage_prepared_foreign_table,
                Table,
                ForeignTableCreated,
                ForeignTableDropped,
                ForeignTableAltered,
                rollback_foreign_table_alter
            );
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn is_foreign_record(operation: &WalOp<'_>) -> bool {
    matches!(
        operation,
        WalOp::SetForeignDataWrapper { .. }
            | WalOp::SetForeignServer { .. }
            | WalOp::SetUserMapping { .. }
            | WalOp::SetForeignTable { .. }
            | WalOp::RestoreForeignTable(_)
    )
}

#[inline(never)]
fn namespace_record(operation: &WalOp<'_>) -> Result<Option<PreparedForeignNamespace>, SqlError> {
    let (class, slot, created_at, key) = match operation {
        WalOp::SetForeignDataWrapper {
            slot,
            created_at,
            definition,
            ..
        } => (
            ForeignObjectClass::Wrapper,
            *slot,
            *created_at,
            definition.map(|value| PreparedForeignKey::Wrapper(value.name)),
        ),
        WalOp::SetForeignServer {
            slot,
            created_at,
            definition,
            ..
        } => (
            ForeignObjectClass::Server,
            *slot,
            *created_at,
            definition.map(|value| PreparedForeignKey::Server(value.name)),
        ),
        WalOp::SetUserMapping {
            slot,
            created_at,
            definition,
        } => (
            ForeignObjectClass::Mapping,
            *slot,
            *created_at,
            definition.map(|value| PreparedForeignKey::Mapping {
                server: value.server,
                user: value.user,
            }),
        ),
        WalOp::SetForeignTable {
            slot,
            created_at,
            definition,
        } => (
            ForeignObjectClass::Table,
            *slot,
            *created_at,
            definition.map(|value| PreparedForeignKey::Table(value.table)),
        ),
        WalOp::RestoreForeignTable(payload) => {
            let (slot, created_at, definition) = crate::wal::decode_foreign_table_payload(payload)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "foreign-table namespace image is corrupt"
                    )
                })?;
            (
                ForeignObjectClass::Table,
                slot,
                created_at,
                definition.map(|value| PreparedForeignKey::Table(value.table)),
            )
        }
        _ => return Ok(None),
    };
    crate::storage::CatalogIdentity::require(created_at)?;
    Ok(Some(PreparedForeignNamespace {
        class,
        slot: usize::from(slot),
        created_at,
        key,
    }))
}

fn batch_namespace(
    storage: &Storage,
    batch: &[(u64, &[u8])],
) -> Result<crate::mem::lease_pool::MutableSlice<PreparedForeignNamespace>, SqlError> {
    let count = batch
        .iter()
        .filter(|(_, raw)| {
            crate::wal::decode_record(raw).is_some_and(|operation| is_foreign_record(&operation))
        })
        .count();
    let mut namespace = storage.prepare_foreign_namespace(count)?;
    for (_, raw) in batch {
        let operation = crate::wal::decode_record(raw).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "foreign catalog batch WAL is corrupt"
            )
        })?;
        let Some(image) = namespace_record(&operation)? else {
            continue;
        };
        if namespace.iter().any(|prior: &PreparedForeignNamespace| {
            prior.class == image.class
                && (prior.slot == image.slot
                    || prior.created_at == image.created_at
                    || prior.key.is_some() && prior.key == image.key)
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "foreign catalog batch duplicates a final identity or key"
            ));
        }
        namespace
            .push_reserved(image)
            .expect("foreign image count covers namespace projection");
    }
    Ok(namespace)
}

pub(super) fn stage_batch(
    storage: &mut Storage,
    transaction: &mut TxnState,
    batch: &[(u64, &[u8])],
    _arena: &Arena,
) -> Result<(), SqlError> {
    let namespace = batch_namespace(storage, batch)?;
    let mark = transaction.ddl().len();
    let result = (|| {
        for class in [
            ForeignObjectClass::Wrapper,
            ForeignObjectClass::Server,
            ForeignObjectClass::Mapping,
            ForeignObjectClass::Table,
        ] {
            for (_, raw) in batch {
                let operation = crate::wal::decode_record(raw).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "foreign catalog batch WAL is corrupt"
                    )
                })?;
                if namespace_record(&operation)?.is_some_and(|image| image.class == class) {
                    stage_foreign_record(storage, transaction, &operation, namespace.as_slice())?;
                }
            }
        }
        Ok(())
    })();
    if result.is_err() {
        for position in (mark..transaction.ddl().len()).rev() {
            super::rollback_catalog_change(
                storage,
                transaction.ddl()[position].clone(),
                transaction.txid,
            );
        }
        transaction.rewind_ddl(mark);
    }
    result
}

#[inline(never)]
fn restore_committed_record(
    storage: &Storage,
    operation: &WalOp<'_>,
    namespace: &[PreparedForeignNamespace],
) -> Result<(), SqlError> {
    macro_rules! restore {
        ($slot:expr, $created:expr, $owner:expr, $definition:expr, $prepare:ident, $class:ident) => {{
            let slot = usize::from($slot);
            let change = storage.$prepare(slot, $created, $owner, $definition, 0, namespace)?;
            match change {
                PreparedForeignChange::Unchanged => {}
                PreparedForeignChange::Created => {
                    storage.foreign_catalog_commit_create(ForeignObjectClass::$class, slot)
                }
                PreparedForeignChange::Dropped => {
                    storage.foreign_catalog_commit_drop(ForeignObjectClass::$class, slot)
                }
                PreparedForeignChange::Changed {
                    definition,
                    ownership,
                } => {
                    if definition.is_some() {
                        storage.foreign_catalog_commit_alter(ForeignObjectClass::$class, slot, 0);
                    }
                    if ownership.is_some() {
                        storage.commit_foreign_catalog_owner(ForeignObjectClass::$class, slot, 0);
                    }
                }
            }
        }};
    }
    match operation {
        WalOp::SetForeignDataWrapper {
            slot,
            created_at,
            owner,
            definition,
        } => restore!(
            *slot,
            *created_at,
            Some(*owner),
            *definition,
            stage_prepared_foreign_wrapper,
            Wrapper
        ),
        WalOp::SetForeignServer {
            slot,
            created_at,
            owner,
            definition,
        } => restore!(
            *slot,
            *created_at,
            Some(*owner),
            *definition,
            stage_prepared_foreign_server,
            Server
        ),
        WalOp::SetUserMapping {
            slot,
            created_at,
            definition,
        } => restore!(
            *slot,
            *created_at,
            None,
            *definition,
            stage_prepared_foreign_mapping,
            Mapping
        ),
        WalOp::SetForeignTable {
            slot,
            created_at,
            definition,
        } => restore!(
            *slot,
            *created_at,
            None,
            **definition,
            stage_prepared_foreign_table,
            Table
        ),
        WalOp::RestoreForeignTable(payload) => {
            let (slot, created_at, definition) = crate::wal::decode_foreign_table_payload(payload)
                .ok_or_else(|| {
                    sql_err!(sqlstate::DATA_EXCEPTION, "foreign-table image is corrupt")
                })?;
            restore!(
                slot,
                created_at,
                None,
                definition,
                stage_prepared_foreign_table,
                Table
            );
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "foreign batch selected a different WAL class"
            ));
        }
    }
    Ok(())
}

/// Startup is exclusive: a rejected committed batch aborts recovery before any
/// connection is served. This path shares final-key admission with PREPARE.
pub(super) fn restore_committed_batch(
    storage: &mut Storage,
    batch: &[(u64, &[u8])],
    _arena: &Arena,
) -> Result<(), SqlError> {
    let namespace = batch_namespace(storage, batch)?;
    for class in [
        ForeignObjectClass::Wrapper,
        ForeignObjectClass::Server,
        ForeignObjectClass::Mapping,
        ForeignObjectClass::Table,
    ] {
        for (_, raw) in batch {
            let operation = crate::wal::decode_record(raw).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "foreign catalog batch WAL is corrupt"
                )
            })?;
            if namespace_record(&operation)?.is_some_and(|image| image.class == class) {
                restore_committed_record(storage, &operation, namespace.as_slice())?;
            }
        }
    }
    Ok(())
}
