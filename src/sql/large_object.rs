//! PostgreSQL large objects: typed identities, sparse 2 KiB pages, and
//! transaction-scoped descriptors.

use core::ops::ControlFlow;
use std::io::{Read, Write};

use crate::mem::arena::Arena;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, sqlstate};
use crate::sql::txn::{DdlUndo, LargeObjectDescriptorMode, TxnState};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{
    AccessClass, AccessObject, LARGE_OBJECT_BLOCK_SIZE, LargeObjectOid, MAX_COLUMNS, PrivilegeSet,
    Storage, rowenc,
};

const INV_WRITE: i32 = 0x0002_0000;
const INV_READ: i32 = 0x0004_0000;
const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

pub(crate) fn fast_path_signature(function_oid: i32) -> Option<(&'static [i32], i32)> {
    use crate::sql::types::oid;
    Some(match function_oid {
        715 => (&[oid::OID], oid::OID),
        764 => (&[oid::TEXT], oid::OID),
        765 => (&[oid::OID, oid::TEXT], oid::INT4),
        767 => (&[oid::TEXT, oid::OID], oid::OID),
        952 => (&[oid::OID, oid::INT4], oid::INT4),
        953 => (&[oid::INT4], oid::INT4),
        954 => (&[oid::INT4, oid::INT4], oid::BYTEA),
        955 => (&[oid::INT4, oid::BYTEA], oid::INT4),
        956 => (&[oid::INT4, oid::INT4, oid::INT4], oid::INT4),
        957 => (&[oid::INT4], oid::OID),
        958 => (&[oid::INT4], oid::INT4),
        964 => (&[oid::OID], oid::INT4),
        1004 => (&[oid::INT4, oid::INT4], oid::INT4),
        3170 => (&[oid::INT4, oid::INT8, oid::INT4], oid::INT8),
        3171 => (&[oid::INT4], oid::INT8),
        3172 => (&[oid::INT4, oid::INT8], oid::INT4),
        3457 => (&[oid::OID, oid::BYTEA], oid::OID),
        3458 => (&[oid::OID], oid::BYTEA),
        3459 => (&[oid::OID, oid::INT8, oid::INT4], oid::BYTEA),
        3460 => (&[oid::OID, oid::INT8, oid::BYTEA], oid::VOID),
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str, argument_count: usize) -> Option<(i32, i16)> {
    use crate::sql::types::oid;
    Some(match (name, argument_count) {
        ("lo_create", 1) | ("lo_creat", 1) | ("lo_import", 1 | 2) | ("lo_from_bytea", 2) => {
            (oid::OID, 4)
        }
        ("lo_lseek64", 3) | ("lo_tell64", 1) => (oid::INT8, 8),
        ("loread", 2) | ("lo_get", 1 | 3) => (oid::BYTEA, -1),
        ("lo_put", 3) => (oid::VOID, 4),
        ("lo_export", 2)
        | ("lo_open", 2)
        | ("lo_close", 1)
        | ("lowrite", 2)
        | ("lo_lseek", 3)
        | ("lo_tell", 1)
        | ("lo_unlink", 1)
        | ("lo_truncate", 2)
        | ("lo_truncate64", 2) => (oid::INT4, 4),
        _ => return None,
    })
}

pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&crate::sql::ast::Expr<'a>],
    star: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    let oid = match (name, args.len(), star) {
        ("lo_create", 1, false) => 715,
        ("lo_import", 1, false) => 764,
        ("lo_export", 2, false) => 765,
        ("lo_import", 2, false) => 767,
        ("lo_open", 2, false) => 952,
        ("lo_close", 1, false) => 953,
        ("loread", 2, false) => 954,
        ("lowrite", 2, false) => 955,
        ("lo_lseek", 3, false) => 956,
        ("lo_creat", 1, false) => 957,
        ("lo_tell", 1, false) => 958,
        ("lo_unlink", 1, false) => 964,
        ("lo_truncate", 2, false) => 1004,
        ("lo_lseek64", 3, false) => 3170,
        ("lo_tell64", 1, false) => 3171,
        ("lo_truncate64", 2, false) => 3172,
        ("lo_from_bytea", 2, false) => 3457,
        ("lo_get", 1, false) => 3458,
        ("lo_get", 3, false) => 3459,
        ("lo_put", 3, false) => 3460,
        (
            "lo_create" | "lo_import" | "lo_export" | "lo_open" | "lo_close" | "loread" | "lowrite"
            | "lo_lseek" | "lo_creat" | "lo_tell" | "lo_unlink" | "lo_truncate" | "lo_lseek64"
            | "lo_tell64" | "lo_truncate64" | "lo_from_bytea" | "lo_get" | "lo_put",
            _,
            _,
        ) => {
            return Some(Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                args.len()
            )));
        }
        _ => return None,
    };
    Some((|| {
        let mut values = [Datum::Null; 3];
        for (slot, expression) in args.iter().enumerate() {
            values[slot] = crate::sql::eval::eval_full(expression, arena, params, row, hooks)?;
        }
        if values[..args.len()].iter().any(Datum::is_null) {
            return Ok(Datum::Null);
        }
        let (invocations, statement_arena) = crate::sql::query::active_routine_invocations()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "large-object functions require a resumable query executor"
                )
            })?;
        invocations.resolve_intrinsic(oid, &values[..args.len()], statement_arena, arena)
    })())
}

pub(crate) fn execute<'a>(
    function_oid: i32,
    arguments: &[Datum<'a>],
    storage: &mut Storage,
    txn: &mut TxnState,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mutating_name = match function_oid {
        715 => Some("lo_create"),
        764 | 767 => Some("lo_import"),
        955 => Some("lowrite"),
        957 => Some("lo_creat"),
        964 => Some("lo_unlink"),
        1004 => Some("lo_truncate"),
        3172 => Some("lo_truncate64"),
        3457 => Some("lo_from_bytea"),
        3460 => Some("lo_put"),
        _ => None,
    };
    if txn.read_only
        && let Some(name) = mutating_name
    {
        return Err(sql_err!(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            "cannot execute {}() in a read-only transaction",
            name
        ));
    }
    match function_oid {
        715 => create(storage, txn, oid_arg(arguments[0])?, arena),
        957 => create(storage, txn, None, arena),
        952 => open(
            storage,
            txn,
            oid_required(arguments[0])?,
            int4(arguments[1])?,
        ),
        953 => {
            txn.close_large_object_descriptor(int4(arguments[0])?)?;
            Ok(Datum::Int4(0))
        }
        954 => read_descriptor(
            storage,
            txn,
            int4(arguments[0])?,
            int4(arguments[1])?,
            arena,
        ),
        955 => write_descriptor(storage, txn, int4(arguments[0])?, bytea(arguments[1])?),
        956 => seek(
            storage,
            txn,
            int4(arguments[0])?,
            int4(arguments[1])? as i64,
            int4(arguments[2])?,
            false,
        ),
        3170 => seek(
            storage,
            txn,
            int4(arguments[0])?,
            int8(arguments[1])?,
            int4(arguments[2])?,
            true,
        ),
        958 => tell(txn, int4(arguments[0])?, false),
        3171 => tell(txn, int4(arguments[0])?, true),
        1004 => truncate_descriptor(
            storage,
            txn,
            int4(arguments[0])?,
            int4(arguments[1])? as i64,
        ),
        3172 => truncate_descriptor(storage, txn, int4(arguments[0])?, int8(arguments[1])?),
        964 => unlink(storage, txn, oid_required(arguments[0])?),
        3457 => {
            let requested = oid_arg(arguments[0])?;
            let result = create(storage, txn, requested, arena)?;
            let Datum::Oid(oid) = result else {
                unreachable!()
            };
            write_at(
                storage,
                txn,
                LargeObjectOid::parse(oid).unwrap(),
                0,
                bytea(arguments[1])?,
            )?;
            Ok(result)
        }
        3458 => get(storage, txn, oid_required(arguments[0])?, 0, None, arena),
        3459 => {
            let offset = int8(arguments[1])?;
            let length = int4(arguments[2])?;
            if offset < 0 || length < 0 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid large-object read offset or length"
                ));
            }
            get(
                storage,
                txn,
                oid_required(arguments[0])?,
                offset,
                Some(length as usize),
                arena,
            )
        }
        3460 => {
            let offset = int8(arguments[1])?;
            if offset < 0 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid large-object write offset: {}",
                    offset
                ));
            }
            let oid = oid_required(arguments[0])?;
            require(storage, txn, oid, PrivilegeSet::UPDATE)?;
            write_at(storage, txn, oid, offset, bytea(arguments[2])?)?;
            Ok(Datum::Null)
        }
        764 | 767 => import(function_oid, arguments, storage, txn, arena),
        765 => export(arguments, storage, txn, arena),
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "large-object function does not exist"
        )),
    }
}

fn create<'a>(
    storage: &mut Storage,
    txn: &mut TxnState,
    requested: Option<LargeObjectOid>,
    _arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (slot, oid) = storage.create_large_object(requested, txn.txid)?;
    if let Err(error) = txn.record_ddl(DdlUndo::LargeObjectCreated(slot as u32)) {
        storage.rollback_large_object_create(slot);
        return Err(error);
    }
    Ok(Datum::Oid(oid.get()))
}

fn open(
    storage: &Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
    mode: i32,
) -> Result<Datum<'static>, SqlError> {
    let known = INV_READ | INV_WRITE;
    if mode & known == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid flags for opening a large object: {}",
            mode
        ));
    }
    if mode & INV_READ != 0 {
        require(storage, txn, oid, PrivilegeSet::SELECT)?;
    }
    if mode & INV_WRITE != 0 {
        require(storage, txn, oid, PrivilegeSet::UPDATE)?;
    }
    let fd = txn.open_large_object_descriptor(
        oid,
        LargeObjectDescriptorMode {
            // PostgreSQL permits reads through an INV_WRITE descriptor.  The
            // mode controls write permission; INV_READ is the read-only form.
            readable: true,
            writable: mode & INV_WRITE != 0,
        },
    )?;
    Ok(Datum::Int4(fd))
}

fn read_descriptor<'a>(
    storage: &Storage,
    txn: &mut TxnState,
    fd: i32,
    length: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let length = length.max(0);
    let descriptor = *txn.large_object_descriptor(fd)?;
    if !descriptor.mode.readable {
        return Err(sql_err!(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "large object descriptor {} was not opened for reading",
            fd
        ));
    }
    require(storage, txn, descriptor.oid, PrivilegeSet::SELECT)?;
    let result = get(
        storage,
        txn,
        descriptor.oid,
        descriptor.position,
        Some(length as usize),
        arena,
    )?;
    let Datum::Bytea(bytes) = result else {
        unreachable!()
    };
    txn.large_object_descriptor_mut(fd)?.position = descriptor
        .position
        .checked_add(bytes.len() as i64)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "large-object position is out of range"
            )
        })?;
    Ok(result)
}

fn write_descriptor<'a>(
    storage: &mut Storage,
    txn: &mut TxnState,
    fd: i32,
    data: &'a [u8],
) -> Result<Datum<'a>, SqlError> {
    let descriptor = *txn.large_object_descriptor(fd)?;
    if !descriptor.mode.writable {
        return Err(sql_err!(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "large object descriptor {} was not opened for writing",
            fd
        ));
    }
    require(storage, txn, descriptor.oid, PrivilegeSet::UPDATE)?;
    write_at(storage, txn, descriptor.oid, descriptor.position, data)?;
    txn.large_object_descriptor_mut(fd)?.position = descriptor
        .position
        .checked_add(data.len() as i64)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "large-object position is out of range"
            )
        })?;
    Ok(Datum::Int4(i32::try_from(data.len()).map_err(|_| {
        sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "large-object write is too large"
        )
    })?))
}

fn seek(
    storage: &Storage,
    txn: &mut TxnState,
    fd: i32,
    offset: i64,
    whence: i32,
    wide: bool,
) -> Result<Datum<'static>, SqlError> {
    let descriptor = *txn.large_object_descriptor(fd)?;
    let base = match whence {
        SEEK_SET => 0,
        SEEK_CUR => descriptor.position,
        SEEK_END => object_length(storage, txn.txid, descriptor.oid)?,
        _ => {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid whence setting: {}",
                whence
            ));
        }
    };
    let position = base
        .checked_add(offset)
        .filter(|position| *position >= 0)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid large object seek target: {}",
                offset
            )
        })?;
    if !wide && position > i32::MAX as i64 {
        return Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "large-object position exceeds 32-bit range"
        ));
    }
    txn.large_object_descriptor_mut(fd)?.position = position;
    Ok(if wide {
        Datum::Int8(position)
    } else {
        Datum::Int4(position as i32)
    })
}

fn tell(txn: &TxnState, fd: i32, wide: bool) -> Result<Datum<'static>, SqlError> {
    let position = txn.large_object_descriptor(fd)?.position;
    if !wide && position > i32::MAX as i64 {
        return Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "large-object position exceeds 32-bit range"
        ));
    }
    Ok(if wide {
        Datum::Int8(position)
    } else {
        Datum::Int4(position as i32)
    })
}

fn truncate_descriptor(
    storage: &mut Storage,
    txn: &mut TxnState,
    fd: i32,
    length: i64,
) -> Result<Datum<'static>, SqlError> {
    if length < 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid large object truncation target: {}",
            length
        ));
    }
    let descriptor = *txn.large_object_descriptor(fd)?;
    if !descriptor.mode.writable {
        return Err(sql_err!(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "large object descriptor {} was not opened for writing",
            fd
        ));
    }
    require(storage, txn, descriptor.oid, PrivilegeSet::UPDATE)?;
    truncate(storage, txn, descriptor.oid, length)?;
    Ok(Datum::Int4(0))
}

fn unlink(
    storage: &mut Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
) -> Result<Datum<'static>, SqlError> {
    let slot = storage
        .large_object_slot(oid, txn.txid)
        .ok_or_else(|| missing(oid))?;
    storage.require_owner(
        AccessObject {
            class: AccessClass::LargeObject,
            slot: slot as u16,
        },
        txn.txid,
        "large object",
    )?;
    delete_pages(storage, txn, oid, None)?;
    let dropped = storage
        .drop_large_object(oid, txn.txid)?
        .ok_or_else(|| missing(oid))?;
    debug_assert_eq!(dropped, slot);
    if let Err(error) = txn.record_ddl(DdlUndo::LargeObjectDropped(dropped as u32)) {
        storage.rollback_large_object_drop(dropped, txn.txid);
        return Err(error);
    }
    Ok(Datum::Int4(1))
}

fn get<'a>(
    storage: &Storage,
    txn: &TxnState,
    oid: LargeObjectOid,
    offset: i64,
    requested: Option<usize>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    require(storage, txn, oid, PrivilegeSet::SELECT)?;
    let total = object_length(storage, txn.txid, oid)?;
    let available = total.saturating_sub(offset).max(0) as usize;
    let length = requested.map_or(available, |length| length.min(available));
    let output = arena.alloc_slice_with(length, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "large-object read exceeds the statement arena"
        )
    })?;
    let mut copied = 0usize;
    while copied < length {
        let absolute = offset as usize + copied;
        let page = (absolute / LARGE_OBJECT_BLOCK_SIZE) as u32;
        let within = absolute % LARGE_OBJECT_BLOCK_SIZE;
        let take = (LARGE_OBJECT_BLOCK_SIZE - within).min(length - copied);
        let mut data = [0u8; LARGE_OBJECT_BLOCK_SIZE];
        let (_, stored) = find_page(storage, txn.txid, oid, page, &mut data)?;
        if within < stored {
            let present = take.min(stored - within);
            output[copied..copied + present].copy_from_slice(&data[within..within + present]);
        }
        copied += take;
    }
    Ok(Datum::Bytea(output))
}

fn write_at(
    storage: &mut Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
    offset: i64,
    data: &[u8],
) -> Result<(), SqlError> {
    let end = offset
        .checked_add(data.len() as i64)
        .filter(|end| *end >= 0)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "large-object write position is out of range"
            )
        })?;
    if end > i64::from(i32::MAX) * LARGE_OBJECT_BLOCK_SIZE as i64 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "large object is too large"
        ));
    }
    let mut consumed = 0usize;
    while consumed < data.len() {
        let absolute = offset as usize + consumed;
        let page = (absolute / LARGE_OBJECT_BLOCK_SIZE) as u32;
        let within = absolute % LARGE_OBJECT_BLOCK_SIZE;
        let take = (LARGE_OBJECT_BLOCK_SIZE - within).min(data.len() - consumed);
        let mut page_data = [0u8; LARGE_OBJECT_BLOCK_SIZE];
        let (rowid, prior_len) = find_page(storage, txn.txid, oid, page, &mut page_data)?;
        page_data[within..within + take].copy_from_slice(&data[consumed..consumed + take]);
        let stored = prior_len.max(within + take);
        write_page(storage, txn, oid, page, rowid, &page_data[..stored])?;
        consumed += take;
    }
    Ok(())
}

fn truncate(
    storage: &mut Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
    length: i64,
) -> Result<(), SqlError> {
    if length == 0 {
        return delete_pages(storage, txn, oid, None);
    }
    let last_byte = usize::try_from(length - 1).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "large object is too large"
        )
    })?;
    let page = (last_byte / LARGE_OBJECT_BLOCK_SIZE) as u32;
    let stored = last_byte % LARGE_OBJECT_BLOCK_SIZE + 1;
    delete_pages(storage, txn, oid, Some((page, stored)))?;
    let mut data = [0u8; LARGE_OBJECT_BLOCK_SIZE];
    let (rowid, existing) = find_page(storage, txn.txid, oid, page, &mut data)?;
    if existing != stored {
        write_page(storage, txn, oid, page, rowid, &data[..stored])?;
    }
    Ok(())
}

fn delete_pages(
    storage: &mut Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
    keep_before: Option<(u32, usize)>,
) -> Result<(), SqlError> {
    loop {
        let table = storage.large_object_page_table();
        let mut found = [0u64; 256];
        let mut found_count = 0usize;
        storage.for_each_row_state(table, &mut |rowid, state| {
            let Some(home) = storage.visible_row_home(table, rowid, state, txn.txid)? else {
                return Ok(ControlFlow::Continue(()));
            };
            let matches = storage.with_row_bytes(table, rowid, home, |bytes| {
                let (database, candidate, page, _) = decode_page(bytes)?;
                Ok(database == storage.current_database_oid()
                    && candidate == oid
                    && keep_before
                        .is_none_or(|(last, within)| page > last || (page == last && within == 0)))
            })?;
            if matches {
                found[found_count] = rowid;
                found_count += 1;
                if found_count == found.len() {
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
        if found_count == 0 {
            break;
        }
        for &rowid in &found[..found_count] {
            let prior = storage.write_pending(table, rowid, txn.txid, txn.command_id(), None)?;
            if let Err(error) = txn.touch(table as u32, rowid, prior) {
                storage.restore_pending(table, rowid, txn.txid, prior);
                return Err(error);
            }
        }
    }
    Ok(())
}

fn find_page(
    storage: &Storage,
    txid: u32,
    oid: LargeObjectOid,
    page: u32,
    output: &mut [u8; LARGE_OBJECT_BLOCK_SIZE],
) -> Result<(Option<u64>, usize), SqlError> {
    let table = storage.large_object_page_table();
    let mut found = None;
    storage.for_each_row_state(table, &mut |rowid, state| {
        let Some(home) = storage.visible_row_home(table, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        storage.with_row_bytes(table, rowid, home, |bytes| {
            let (database, candidate, candidate_page, data) = decode_page(bytes)?;
            if database == storage.current_database_oid()
                && candidate == oid
                && candidate_page == page
            {
                if found.is_some() {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "duplicate large-object page"
                    ));
                }
                if data.len() > output.len() {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "large-object page exceeds {} bytes",
                        LARGE_OBJECT_BLOCK_SIZE
                    ));
                }
                output[..data.len()].copy_from_slice(data);
                found = Some((rowid, data.len()));
            }
            Ok(())
        })?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(found.map_or((None, 0), |(rowid, length)| (Some(rowid), length)))
}

fn write_page(
    storage: &mut Storage,
    txn: &mut TxnState,
    oid: LargeObjectOid,
    page: u32,
    rowid: Option<u64>,
    data: &[u8],
) -> Result<(), SqlError> {
    let values = [
        Datum::Oid(storage.current_database_oid().get() as u32),
        Datum::Oid(oid.get()),
        Datum::Int4(page as i32),
        Datum::Bytea(data),
    ];
    let len = rowenc::encoded_len(&values);
    let (loc, bytes) = storage.heap.append(len)?;
    rowenc::encode(&values, bytes);
    let rowid = rowid.unwrap_or_else(|| storage.next_rowid());
    let table = storage.large_object_page_table();
    let prior = storage.write_pending(table, rowid, txn.txid, txn.command_id(), Some(loc))?;
    if let Err(error) = txn.touch(table as u32, rowid, prior) {
        storage.restore_pending(table, rowid, txn.txid, prior);
        return Err(error);
    }
    Ok(())
}

fn object_length(storage: &Storage, txid: u32, oid: LargeObjectOid) -> Result<i64, SqlError> {
    if storage.large_object_slot(oid, txid).is_none() {
        return Err(missing(oid));
    }
    let table = storage.large_object_page_table();
    let mut length = 0i64;
    storage.for_each_row_state(table, &mut |rowid, state| {
        let Some(home) = storage.visible_row_home(table, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        storage.with_row_bytes(table, rowid, home, |bytes| {
            let (database, candidate, page, data) = decode_page(bytes)?;
            if database == storage.current_database_oid() && candidate == oid {
                length =
                    length.max(page as i64 * LARGE_OBJECT_BLOCK_SIZE as i64 + data.len() as i64);
            }
            Ok(())
        })?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(length)
}

fn decode_page(
    bytes: &[u8],
) -> Result<(crate::storage::DatabaseOid, LargeObjectOid, u32, &[u8]), SqlError> {
    let mut values = [Datum::Null; MAX_COLUMNS];
    rowenc::decode(
        bytes,
        &[ColType::Oid, ColType::Oid, ColType::Int4, ColType::Bytea],
        &mut values,
    )?;
    let Datum::Oid(database) = values[0] else {
        return Err(corrupt_page());
    };
    let database = crate::storage::DatabaseOid::parse(database as i32).ok_or_else(corrupt_page)?;
    let Datum::Oid(oid) = values[1] else {
        return Err(corrupt_page());
    };
    let oid = LargeObjectOid::parse(oid).ok_or_else(corrupt_page)?;
    let Datum::Int4(page) = values[2] else {
        return Err(corrupt_page());
    };
    let page = u32::try_from(page).map_err(|_| corrupt_page())?;
    let Datum::Bytea(data) = values[3] else {
        return Err(corrupt_page());
    };
    Ok((database, oid, page, data))
}

pub(crate) fn for_each_page(
    storage: &Storage,
    txid: u32,
    callback: &mut impl FnMut(LargeObjectOid, u32, &[u8]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let table = storage.large_object_page_table();
    storage.for_each_row_state(table, &mut |rowid, state| {
        let Some(home) = storage.visible_row_home(table, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        storage.with_row_bytes(table, rowid, home, |bytes| {
            let (database, oid, page, data) = decode_page(bytes)?;
            if database == storage.current_database_oid() {
                callback(oid, page, data)?;
            }
            Ok(())
        })?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

fn corrupt_page() -> SqlError {
    sql_err!(sqlstate::DATA_EXCEPTION, "corrupt large-object page")
}

fn require(
    storage: &Storage,
    txn: &TxnState,
    oid: LargeObjectOid,
    privilege: PrivilegeSet,
) -> Result<(), SqlError> {
    let slot = storage
        .large_object_slot(oid, txn.txid)
        .ok_or_else(|| missing(oid))?;
    let role = storage.current_role_slot(txn.txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    let object = AccessObject {
        class: AccessClass::LargeObject,
        slot: slot as u16,
    };
    if storage.has_object_privilege(object, role, privilege, txn.txid) {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for large object {}",
            oid.get()
        ))
    }
}

fn import<'a>(
    function_oid: i32,
    arguments: &[Datum<'a>],
    storage: &mut Storage,
    txn: &mut TxnState,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    require_superuser(storage, txn, "lo_import")?;
    let Datum::Text(path) = arguments[0] else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "lo_import path must be text"
        ));
    };
    let requested = if function_oid == 767 {
        oid_arg(arguments[1])?
    } else {
        None
    };
    let result = create(storage, txn, requested, arena)?;
    let Datum::Oid(raw_oid) = result else {
        unreachable!()
    };
    let oid = LargeObjectOid::parse(raw_oid).unwrap();
    let mut file = std::fs::File::open(path).map_err(|error| {
        sql_err!(
            sqlstate::IO_ERROR,
            "could not open server file \"{}\": {}",
            path,
            error
        )
    })?;
    let mut buffer = [0u8; 8 * 1024];
    let mut offset = 0i64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            sql_err!(
                sqlstate::IO_ERROR,
                "could not read server file \"{}\": {}",
                path,
                error
            )
        })?;
        if read == 0 {
            break;
        }
        write_at(storage, txn, oid, offset, &buffer[..read])?;
        offset += read as i64;
    }
    Ok(result)
}

fn export<'a>(
    arguments: &[Datum<'a>],
    storage: &Storage,
    txn: &TxnState,
    _arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    require_superuser(storage, txn, "lo_export")?;
    let oid = oid_required(arguments[0])?;
    let Datum::Text(path) = arguments[1] else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "lo_export path must be text"
        ));
    };
    require(storage, txn, oid, PrivilegeSet::SELECT)?;
    let length = object_length(storage, txn.txid, oid)?;
    let mut file = std::fs::File::create(path).map_err(|error| {
        sql_err!(
            sqlstate::IO_ERROR,
            "could not create server file \"{}\": {}",
            path,
            error
        )
    })?;
    let mut offset = 0i64;
    while offset < length {
        let page = (offset as usize / LARGE_OBJECT_BLOCK_SIZE) as u32;
        let mut data = [0u8; LARGE_OBJECT_BLOCK_SIZE];
        let (_, stored) = find_page(storage, txn.txid, oid, page, &mut data)?;
        let take = usize::try_from((length - offset).min(LARGE_OBJECT_BLOCK_SIZE as i64))
            .expect("bounded by a page");
        if stored < take {
            data[stored..take].fill(0);
        }
        file.write_all(&data[..take]).map_err(|error| {
            sql_err!(
                sqlstate::IO_ERROR,
                "could not write server file \"{}\": {}",
                path,
                error
            )
        })?;
        offset += take as i64;
    }
    Ok(Datum::Int4(1))
}

fn require_superuser(storage: &Storage, txn: &TxnState, function: &str) -> Result<(), SqlError> {
    let role = storage.current_role_slot(txn.txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    if storage.role(role).attributes_to(txn.txid).superuser {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for function {}",
            function
        ))
    }
}

fn oid_arg(value: Datum<'_>) -> Result<Option<LargeObjectOid>, SqlError> {
    let raw = match value {
        Datum::Oid(value) => value,
        Datum::Int4(value) if value >= 0 => value as u32,
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "large-object identity must be oid"
            ));
        }
    };
    Ok(LargeObjectOid::parse(raw))
}

fn oid_required(value: Datum<'_>) -> Result<LargeObjectOid, SqlError> {
    oid_arg(value)?
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "large object 0 does not exist"))
}

fn int4(value: Datum<'_>) -> Result<i32, SqlError> {
    match value {
        Datum::Int2(value) => Ok(value as i32),
        Datum::Int4(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "large-object argument must be integer"
        )),
    }
}

fn int8(value: Datum<'_>) -> Result<i64, SqlError> {
    match value {
        Datum::Int2(value) => Ok(value as i64),
        Datum::Int4(value) => Ok(value as i64),
        Datum::Int8(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "large-object argument must be bigint"
        )),
    }
}

fn bytea(value: Datum<'_>) -> Result<&[u8], SqlError> {
    match value {
        Datum::Bytea(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "large-object data must be bytea"
        )),
    }
}

fn missing(oid: LargeObjectOid) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_OBJECT,
        "large object {} does not exist",
        oid.get()
    )
}
