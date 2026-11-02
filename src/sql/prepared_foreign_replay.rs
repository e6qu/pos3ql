//! Final foreign catalog images reconstruct reversible prepared overlays.

use super::*;
use crate::storage::foreign::{ForeignObjectClass, PreparedForeignChange};

#[inline(never)]
pub(super) fn stage_foreign_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
) -> Result<bool, SqlError> {
    macro_rules! stage {
        ($slot:expr, $creation:expr, $owner:expr, $definition:expr, $prepare:ident,
            $class:ident, $created:ident, $dropped:ident, $altered:ident, $restore:ident) => {{
            let slot = usize::from($slot);
            let transaction_id = transaction.txid;
            let change = storage.$prepare(slot, $creation, $owner, $definition, transaction_id)?;
            let mark = transaction.ddl().len();
            let result = (|| -> Result<(), SqlError> {
                match change {
                    PreparedForeignChange::Unchanged => {}
                    PreparedForeignChange::Created => {
                        transaction.record_ddl(DdlUndo::$created(slot as u32))?
                    }
                    PreparedForeignChange::Dropped => {
                        transaction.record_ddl(DdlUndo::$dropped(slot as u32))?
                    }
                    PreparedForeignChange::Changed {
                        definition,
                        ownership,
                    } => {
                        if let Some(prior) = definition {
                            transaction.record_ddl(DdlUndo::$altered {
                                slot: slot as u32,
                                prior,
                            })?;
                        }
                        if let Some(prior) = ownership {
                            transaction.record_ddl(DdlUndo::ForeignOwnerChanged {
                                class: ForeignObjectClass::$class,
                                slot: slot as u32,
                                prior,
                            })?;
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
