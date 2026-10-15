//! SQL UPDATE and MERGE move a row through the old and new leaf triggers.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_move<'arena>(
    storage: &mut Storage,
    txn: &mut TxnState,
    selected: PhysicalRow,
    old_definition: &TableDef,
    old_values: &[Datum<'arena>],
    target: usize,
    definition: &TableDef,
    generated: &[Option<&'arena Expr<'arena>>],
    values: &mut [Datum<'arena>],
    arena: &'arena Arena,
    params: &[Datum<'arena>],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    scratch: *mut DmlScratch,
) -> Result<bool, SqlError> {
    let (old_table, rowid, _) = selected.local_parts();
    if !fire_partition_row_triggers(
        storage,
        txn,
        arena,
        seq_session,
        responder,
        scratch,
        old_table,
        old_definition,
        TriggerEvents::DELETE,
        true,
        false,
        ColumnSet::EMPTY,
        Some(old_values),
        None,
    )? {
        return Ok(false);
    }
    if !dml_row_available(storage, txn, selected, "updated")? {
        return Ok(false);
    }
    let prior = storage.write_pending(old_table, rowid, txn.txid, txn.command_id(), None)?;
    if let Err(error) = txn.touch(old_table as u32, rowid, prior) {
        storage.restore_pending(old_table, rowid, txn.txid, prior);
        return Err(error);
    }
    for (column, value) in definition.columns().iter().zip(values.iter_mut()) {
        if column.default.is_generated() {
            *value = Datum::Null;
        }
    }
    if !fire_partition_row_triggers(
        storage,
        txn,
        arena,
        seq_session,
        responder,
        scratch,
        target,
        definition,
        TriggerEvents::INSERT,
        true,
        false,
        ColumnSet::EMPTY,
        None,
        Some(values),
    )? {
        if table_is_referenced(
            storage,
            old_definition.schema.as_str(),
            old_definition.name.as_str(),
            txn.txid,
        ) {
            apply_fk_parent_actions(
                storage,
                txn,
                old_definition.schema.as_str(),
                old_definition.name.as_str(),
                old_values,
                None,
                arena,
                params,
                seq_session,
                MAX_FK_CASCADE_DEPTH,
            )?;
        }
        fire_partition_row_triggers(
            storage,
            txn,
            arena,
            seq_session,
            responder,
            scratch,
            old_table,
            old_definition,
            TriggerEvents::DELETE,
            false,
            false,
            ColumnSet::EMPTY,
            Some(old_values),
            None,
        )?;
        return Ok(false);
    }
    compute_generated(definition, generated, values, storage, txn.txid, arena)?;
    storage.validate_partition_target(target, definition, values, txn.txid)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fire_after_move<'arena>(
    storage: &mut Storage,
    txn: &mut TxnState,
    old_table: usize,
    old_definition: &TableDef,
    old_values: &[Datum<'arena>],
    target: usize,
    definition: &TableDef,
    values: &mut [Datum<'arena>],
    arena: &'arena Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    scratch: *mut DmlScratch,
) -> Result<(), SqlError> {
    fire_partition_row_triggers(
        storage,
        txn,
        arena,
        seq_session,
        responder,
        scratch,
        old_table,
        old_definition,
        TriggerEvents::DELETE,
        false,
        false,
        ColumnSet::EMPTY,
        Some(old_values),
        None,
    )?;
    fire_partition_row_triggers(
        storage,
        txn,
        arena,
        seq_session,
        responder,
        scratch,
        target,
        definition,
        TriggerEvents::INSERT,
        false,
        false,
        ColumnSet::EMPTY,
        None,
        Some(values),
    )?;
    Ok(())
}
