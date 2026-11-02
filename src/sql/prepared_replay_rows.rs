//! Restores statement metadata whose physical row effects have separate WAL.

use super::*;

pub(super) fn stage_truncate(
    storage: &Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let WalOp::Truncate {
        tables,
        table_count,
        cascade,
        restart_identity,
        command_id,
    } = operation
    else {
        return Ok(false);
    };
    if *table_count == 0 || *table_count > transaction.truncate_table_capacity() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "prepared TRUNCATE exceeds transaction table capacity"
        ));
    }
    let mark = arena.mark();
    let result = (|| {
        let selected = arena
            .alloc_slice_with(*table_count, |_| 0usize)
            .map_err(|_| eval::arena_full())?;
        let mut rest = *tables;
        for output in selected.iter_mut() {
            let schema = take_relation_name(&mut rest)?;
            let table = take_relation_name(&mut rest)?;
            *output = storage
                .find_visible(schema, table, transaction.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "prepared TRUNCATE targets unknown relation \"{}.{}\"",
                        schema,
                        table
                    )
                })?;
        }
        if !rest.is_empty() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared TRUNCATE contains trailing relation bytes"
            ));
        }
        transaction.record_truncate(selected, *cascade, *restart_identity, *command_id)?;
        for &table in selected.iter() {
            storage.record_relation_transaction_truncate(transaction.txid, table)?;
        }
        Ok(())
    })();
    // Only table slots were copied into the transaction's startup-owned list.
    unsafe { arena.rewind_to(mark) };
    result.map(|()| true)
}

fn take_relation_name<'a>(remaining: &mut &'a [u8]) -> Result<&'a str, SqlError> {
    let (&length, tail) = remaining.split_first().ok_or_else(|| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "prepared TRUNCATE has an incomplete relation name"
        )
    })?;
    let (name, tail) = tail.split_at_checked(usize::from(length)).ok_or_else(|| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "prepared TRUNCATE has an incomplete relation name"
        )
    })?;
    let name = core::str::from_utf8(name).map_err(|_| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "prepared TRUNCATE relation name is not UTF-8"
        )
    })?;
    crate::storage::SqlName::parse(name)?;
    *remaining = tail;
    Ok(name)
}
