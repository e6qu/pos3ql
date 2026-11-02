//! Rebuilds subscription DDL as private overlays for a prepared transaction.

use super::*;

pub(super) fn stage_subscription_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
) -> Result<bool, SqlError> {
    let txid = transaction.txid;
    match operation {
        WalOp::CreateSubscription {
            created_at,
            name,
            owner,
            connection,
            publications,
            publication_count,
            enabled,
            slot: publisher_slot,
            behavior,
            bootstrap,
        } => {
            if behavior.two_phase {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared subscription requests unsupported two-phase apply"
                ));
            }
            let name = crate::storage::SqlName::parse(name)?;
            let connection = crate::storage::SubscriptionConnInfo::parse(connection)?;
            if *enabled {
                validate_recovered_enabled_subscription(connection, name)?;
            }
            let names = publications.get(..*publication_count).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared subscription publication count exceeds its image"
                )
            })?;
            let slot = storage.create_subscription(
                crate::storage::SubscriptionSpec {
                    name,
                    connection,
                    publications: names,
                    enabled: *enabled,
                    slot: *publisher_slot,
                    behavior: *behavior,
                    bootstrap: *bootstrap,
                },
                txid,
            )?;
            let result = storage
                .restore_subscription_stream_identity(slot, *created_at, 1)
                .and_then(|()| {
                    storage.restore_subscription_owner(slot, *owner, txid)?;
                    transaction.record_ddl(DdlUndo::SubscriptionCreated(slot as u32))
                });
            if result.is_err() {
                storage.rollback_subscription_create(slot);
            }
            result?;
        }
        WalOp::DropSubscription { name } => {
            let slot = storage.drop_subscription(name, txid)?.ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "prepared transaction drops unknown subscription \"{}\"",
                    name
                )
            })?;
            if let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionDropped(slot as u32)) {
                storage.rollback_subscription_drop(slot, txid);
                return Err(error);
            }
        }
        WalOp::SetSubscriptionEnabled { name, enabled } => {
            let (slot, _) = selected(storage, name, txid)?;
            if *enabled {
                validate_recovered_enabled_subscription(
                    storage.subscription_definition_to(slot, txid).connection,
                    crate::storage::SqlName::parse(name)?,
                )?;
            }
            if let crate::storage::SubscriptionEnabledChange::Changed { prior } =
                storage.set_subscription_enabled(slot, *enabled, txid)?
                && let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionEnabled {
                    slot: slot as u32,
                    prior,
                })
            {
                storage.restore_subscription_enabled(slot, prior);
                return Err(error);
            }
        }
        WalOp::SetSubscriptionBootstrap { name, bootstrap } => {
            let (slot, _) = selected(storage, name, txid)?;
            if let crate::storage::SubscriptionBootstrapChange::Changed { prior } =
                storage.set_subscription_bootstrap(slot, *bootstrap, txid)?
                && let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionBootstrapChanged {
                    slot: slot as u32,
                    prior,
                })
            {
                storage.restore_subscription_bootstrap(slot, prior);
                return Err(error);
            }
        }
        WalOp::AlterSubscription {
            name,
            connection,
            publications,
            publication_count,
            slot: publisher_slot,
            behavior,
        } => {
            let (slot, subscription) = selected(storage, name, txid)?;
            let connection = crate::storage::SubscriptionConnInfo::parse(connection)?;
            if subscription.enabled_to(txid) {
                validate_recovered_enabled_subscription(
                    connection,
                    crate::storage::SqlName::parse(name)?,
                )?;
            }
            let names = publications.get(..*publication_count).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared subscription publication count exceeds its image"
                )
            })?;
            let definition = crate::storage::SubscriptionDefinition::from_parts(
                connection,
                names,
                *publisher_slot,
                *behavior,
            )?;
            let change = storage.set_subscription_definition(slot, definition, txid)?;
            if change.changed
                && let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionDefinitionChanged {
                    slot: slot as u32,
                    prior: change.prior,
                })
            {
                storage.restore_subscription_definition(slot, change.prior);
                return Err(error);
            }
        }
        WalOp::SetSubscriptionOwner { name, owner } => {
            let (slot, _) = selected(storage, name, txid)?;
            let prior = storage.set_subscription_owner(slot, usize::from(*owner), txid)?;
            if let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionOwnerChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.restore_subscription_owner_pending(slot, prior);
                return Err(error);
            }
        }
        WalOp::RenameSubscription { name, new_name } => {
            let (slot, _) = selected(storage, name, txid)?;
            let prior = storage.rename_subscription(
                slot,
                crate::storage::SqlName::parse(new_name)?,
                txid,
            )?;
            if let Err(error) = transaction.record_ddl(DdlUndo::SubscriptionRenamed {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_subscription_rename(slot, prior);
                return Err(error);
            }
        }
        WalOp::CreateReplicationSlot { .. }
        | WalOp::AlterReplicationSlot { .. }
        | WalOp::DropReplicationSlot { .. }
        | WalOp::AdvanceReplicationSlot { .. }
        | WalOp::CompleteSubscriptionCleanup { .. }
        | WalOp::FailSubscription { .. }
        | WalOp::ResetSubscriptionRelations { .. }
        | WalOp::AddSubscriptionRelation { .. }
        | WalOp::AdvanceSubscription { .. } => {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared transaction contains a replication worker or independent slot operation"
            ));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn selected(
    storage: &Storage,
    name: &str,
    transaction: u32,
) -> Result<(usize, crate::storage::SubscriptionDef), SqlError> {
    storage.subscription(name, transaction).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "prepared transaction targets unknown subscription \"{}\"",
            name
        )
    })
}
