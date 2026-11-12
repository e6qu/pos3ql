//! Row locks taken by writes and referential checks, and what a lock holder
//! learns about the version its statement selected.
//!
//! PostgreSQL locks a row before changing it (or, for a foreign key check,
//! before relying on it) and then compares the locked row with the version its
//! snapshot selected. Read committed continues with the latest committed
//! version; repeatable read and serializable fail with `40001` when another
//! transaction changed the row after the snapshot
//! ([`ExecLockRows`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeLockRows.c),
//! [`ExecUpdate`/`ExecDelete`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeModifyTable.c)).

use crate::sql::ast::{LockStrength, LockWait, TransactionIsolation};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::lock::LockDecision;
use crate::sql_err;
use crate::storage::{LockedRowRecheck, RowSnapshot, Storage, VisibleRowHome};

/// The locked row compared with the version the statement selected.
pub(crate) enum LockedVersion {
    /// Still the selected version, or the locking transaction's own change.
    Selected,
    /// Another transaction deleted the row and committed.
    Deleted,
    /// Another transaction replaced the row and committed.
    Updated(VisibleRowHome),
}

/// Locks `rowid` in `table` and compares it with `seen`. A conflicting holder
/// parks the statement (`INTERNAL_LOCK_WAIT`); `None` means `SKIP LOCKED`
/// skipped the row.
pub(crate) fn lock_selected_row(
    storage: &Storage,
    txid: u32,
    table: usize,
    rowid: u64,
    seen: RowSnapshot,
    strength: LockStrength,
    wait: LockWait,
) -> Result<Option<LockedVersion>, SqlError> {
    match storage.acquire_row_lock(table, rowid, txid, strength, wait)? {
        LockDecision::Acquired => {}
        LockDecision::Skipped => return Ok(None),
        LockDecision::Waiting => {
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a row lock"
            ));
        }
    }
    Ok(Some(
        match storage.recheck_locked_row(table, rowid, txid, seen.row_version())? {
            LockedRowRecheck::Unchanged | LockedRowRecheck::OwnChange => LockedVersion::Selected,
            LockedRowRecheck::Deleted => LockedVersion::Deleted,
            LockedRowRecheck::Updated(home) => LockedVersion::Updated(home),
        },
    ))
}

/// Whether a transaction keeps one snapshot, so a concurrent change to a row
/// it locks is a serialization failure rather than a newer version to follow.
pub(crate) fn uses_transaction_snapshot(isolation: TransactionIsolation) -> bool {
    matches!(
        isolation,
        TransactionIsolation::RepeatableRead | TransactionIsolation::Serializable
    )
}

/// PostgreSQL's `40001` for a row another transaction changed after the
/// snapshot. Row locks report every change as an update; `UPDATE` and
/// `DELETE` name a deletion.
pub(crate) fn concurrent_change(deleted: bool) -> SqlError {
    if deleted {
        sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "could not serialize access due to concurrent delete"
        )
    } else {
        sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "could not serialize access due to concurrent update"
        )
    }
}

/// Whether `column` belongs to a unique key, so changing it conflicts with
/// `FOR KEY SHARE` holders such as a referencing row's check.
pub(crate) fn is_key_column<Definition: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<Definition>,
    column: usize,
    txid: u32,
) -> bool {
    let definition = &def.columns()[column];
    definition.primary
        || definition.unique
        || def
            .uniques()
            .iter()
            .any(|key| key.columns().contains(&(column as u16)))
        || storage
            .unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid)
            .any(|index| index.columns[..index.n_cols].contains(&(column as u16)))
}

/// Whether the latest version of a locked row kept every unique key column
/// of the version `seen`. PostgreSQL grants `FOR KEY SHARE` on such a row
/// even under a snapshot that predates the change.
pub(crate) fn key_unchanged(
    storage: &Storage,
    txid: u32,
    table: usize,
    rowid: u64,
    seen: RowSnapshot,
    latest: VisibleRowHome,
) -> Result<bool, SqlError> {
    use crate::sql::types::{ColType, Datum};
    use crate::storage::{MAX_COLUMNS, rowenc};
    let definition = storage.table_def(table, txid).clone();
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    definition.schema(&mut schema);
    let schema = &schema[..definition.n_columns];
    storage.with_row_bytes(table, rowid, latest, |latest| {
        let mut latest_values = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(latest, schema, &mut latest_values)?;
        storage.with_row_bytes(table, rowid, seen, |selected| {
            let mut selected_values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(selected, schema, &mut selected_values)?;
            Ok((0..definition.n_columns).all(|column| {
                latest_values[column] == selected_values[column]
                    || !is_key_column(storage, &definition, column, txid)
            }))
        })
    })
}

/// Applies the isolation rule to a result row that `SELECT ... FOR` locked.
/// `false` drops the row: under read committed, another transaction deleted
/// it. Repeatable read and serializable fail with `40001` when another
/// transaction changed the row after the snapshot, except that `FOR KEY
/// SHARE` survives a change that kept every unique key column.
pub(crate) fn locked_result_row_current(
    storage: &Storage,
    txid: u32,
    table: usize,
    rowid: u64,
    seen: RowSnapshot,
    locked: LockedVersion,
    strength: LockStrength,
) -> Result<bool, SqlError> {
    let repeatable = storage.keeps_transaction_snapshot(txid);
    match locked {
        LockedVersion::Selected => Ok(true),
        LockedVersion::Deleted if repeatable => Err(concurrent_change(false)),
        LockedVersion::Deleted => Ok(false),
        LockedVersion::Updated(latest) if repeatable => {
            if strength == LockStrength::KeyShare
                && key_unchanged(storage, txid, table, rowid, seen, latest)?
            {
                Ok(true)
            } else {
                Err(concurrent_change(false))
            }
        }
        LockedVersion::Updated(_) => Ok(true),
    }
}
