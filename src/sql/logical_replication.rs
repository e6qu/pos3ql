//! SQL control surface for PostgreSQL logical decoding.

use crate::mem::arena::Arena;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, sqlstate};
use crate::sql::txn::TxnState;
use crate::sql::types::{Datum, oid};
use crate::sql_err;

pub(crate) fn result_type(name: &str, argument_count: usize) -> Option<(i32, i16)> {
    match (name, argument_count) {
        ("pg_logical_emit_message", 3 | 4) => Some((oid::PG_LSN, 8)),
        ("pg_create_logical_replication_slot", 2..=5)
        | ("pg_copy_logical_replication_slot", 2..=4)
        | ("pg_replication_slot_advance", 2) => Some((oid::RECORD, -1)),
        ("pg_drop_replication_slot", 1) => Some((oid::VOID, 4)),
        ("pg_stat_reset_replication_slot", 1) | ("pg_stat_reset_subscription_stats", 1) => {
            Some((oid::VOID, 4))
        }
        _ => None,
    }
}

pub(crate) fn is_intrinsic(oid: i32) -> bool {
    matches!(
        oid,
        3577 | 3578 | 3786 | 3780 | 4222 | 4223 | 4224 | 3878 | 6170 | 6232
    )
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
        ("pg_logical_emit_message", 3 | 4, false) => 3577,
        ("pg_create_logical_replication_slot", 2..=5, false) => 3786,
        ("pg_drop_replication_slot", 1, false) => 3780,
        ("pg_copy_logical_replication_slot", 2, false) => 4224,
        ("pg_copy_logical_replication_slot", 3, false) => 4223,
        ("pg_copy_logical_replication_slot", 4, false) => 4222,
        ("pg_replication_slot_advance", 2, false) => 3878,
        ("pg_stat_reset_replication_slot", 1, false) => 6170,
        ("pg_stat_reset_subscription_stats", 1, false) => 6232,
        (
            "pg_logical_emit_message"
            | "pg_create_logical_replication_slot"
            | "pg_drop_replication_slot"
            | "pg_copy_logical_replication_slot"
            | "pg_replication_slot_advance"
            | "pg_stat_reset_replication_slot"
            | "pg_stat_reset_subscription_stats",
            _,
            _,
        ) => {
            return Some(Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            )));
        }
        _ => return None,
    };
    if args.len() > 5 {
        return Some(Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {}(...) with {} arguments does not exist",
            name,
            args.len()
        )));
    }
    Some((|| {
        let mut values = [Datum::Null; 5];
        for (slot, expression) in args.iter().enumerate() {
            values[slot] = crate::sql::eval::eval_full(expression, arena, params, row, hooks)?;
        }
        if !matches!(oid, 6170 | 6232) && values[..args.len()].iter().any(Datum::is_null) {
            return Ok(Datum::Null);
        }
        let oid = if oid == 3577 && matches!(values[2], Datum::Bytea(_)) {
            3578
        } else {
            oid
        };
        let (invocations, statement_arena) = crate::sql::query::active_routine_invocations()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "pg_logical_emit_message requires a resumable query executor"
                )
            })?;
        invocations.resolve_intrinsic(oid, &values[..args.len()], statement_arena, arena)
    })())
}

pub(crate) fn execute<'a>(
    oid: i32,
    arguments: &[Datum<'a>],
    engine: &mut super::Engine,
    txn: &mut TxnState,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if oid != 3577 && oid != 3578 {
        return execute_slot_control(oid, arguments, engine, txn, arena);
    }
    let [
        Datum::Bool(transactional),
        Datum::Text(prefix),
        content,
        rest @ ..,
    ] = arguments
    else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "pg_logical_emit_message arguments have invalid types"
        ));
    };
    if rest.len() > 1
        || rest
            .first()
            .is_some_and(|value| !matches!(value, Datum::Bool(_)))
    {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "pg_logical_emit_message flush argument must be boolean"
        ));
    }
    let content = match content {
        Datum::Text(text) | Datum::Bpchar(text) => text.as_bytes(),
        Datum::Bytea(bytes) => *bytes,
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "pg_logical_emit_message content must be text or bytea"
            ));
        }
    };
    engine
        .emit_logical_message(txn, *transactional, prefix, content)
        .map(Datum::PgLsn)
}

fn text_argument<'a>(value: Datum<'a>, function: &str) -> Result<&'a str, SqlError> {
    match value {
        Datum::Text(value) | Datum::Bpchar(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "{} argument must be name or text",
            function
        )),
    }
}

fn slot_record<'a>(
    arena: &'a Arena,
    lsn_name: &'static str,
    name: &'a str,
    lsn: u64,
) -> Result<Datum<'a>, SqlError> {
    let fields = [
        crate::sql::types::RecordField {
            name: "slot_name",
            type_oid: oid::NAME,
            value: Datum::Text(name),
        },
        crate::sql::types::RecordField {
            name: lsn_name,
            type_oid: oid::PG_LSN,
            value: Datum::PgLsn(lsn),
        },
    ];
    arena
        .alloc_slice_copy(&fields)
        .map(|fields| Datum::Record(&*fields))
        .map_err(|_| crate::sql::eval::arena_full())
}

fn execute_slot_control<'a>(
    oid_value: i32,
    arguments: &[Datum<'a>],
    engine: &mut super::Engine,
    txn: &TxnState,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let function = match oid_value {
        3786 => "pg_create_logical_replication_slot",
        3780 => "pg_drop_replication_slot",
        4222..=4224 => "pg_copy_logical_replication_slot",
        3878 => "pg_replication_slot_advance",
        6170 => "pg_stat_reset_replication_slot",
        6232 => "pg_stat_reset_subscription_stats",
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "unknown logical replication intrinsic"
            ));
        }
    };
    if matches!(oid_value, 6170 | 6232) {
        engine.require_superuser(txn.txid, function)?;
        if oid_value == 6170 {
            let name = match arguments[0] {
                Datum::Null => None,
                value => Some(text_argument(value, function)?),
            };
            engine.reset_replication_slot_statistics(name)?;
        } else {
            let oid = match arguments[0] {
                Datum::Null => None,
                Datum::Oid(value) => i32::try_from(value).ok(),
                Datum::Int4(value) if value >= 0 => Some(value),
                _ => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "{} argument must be oid",
                        function
                    ));
                }
            };
            engine.reset_subscription_statistics(oid, txn.txid);
        }
        return Ok(Datum::Null);
    }
    engine.require_replication_privilege(txn.txid)?;
    if oid_value == 3780 {
        let name =
            crate::storage::ReplicationSlotName::parse(text_argument(arguments[0], function)?)?;
        engine.drop_replication_slot(name)?;
        return Ok(Datum::Null);
    }
    if oid_value == 3878 {
        let written_name = text_argument(arguments[0], function)?;
        let name = crate::storage::ReplicationSlotName::parse(written_name)?;
        let target = match arguments[1] {
            Datum::PgLsn(value) => value,
            Datum::Text(value) | Datum::Bpchar(value) => {
                let Datum::PgLsn(value) = crate::sql::eval::cast_to(
                    Datum::Text(value),
                    crate::sql::types::ColType::PgLsn,
                    arena,
                )?
                else {
                    unreachable!()
                };
                value
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "pg_replication_slot_advance target must be pg_lsn"
                ));
            }
        };
        let end_lsn = engine.advance_replication_slot(name.as_str(), target)?;
        return slot_record(arena, "end_lsn", written_name, end_lsn);
    }
    if oid_value == 3786 {
        let written_name = text_argument(arguments[0], function)?;
        let name = crate::storage::ReplicationSlotName::parse(written_name)?;
        let plugin = text_argument(arguments[1], function)?;
        if plugin != "pgoutput" {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "logical decoding output plugin \"{}\" is not supported",
                plugin
            ));
        }
        let option = |index: usize| -> Result<bool, SqlError> {
            match arguments.get(index).copied().unwrap_or(Datum::Bool(false)) {
                Datum::Bool(value) => Ok(value),
                _ => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "{} option must be boolean",
                    function
                )),
            }
        };
        if option(2)? {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "temporary logical replication slots are not supported"
            ));
        }
        if option(3)? {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "two-phase logical decoding is not supported"
            ));
        }
        let lsn = engine.create_replication_slot(
            name,
            crate::storage::ReplicationSlotBehavior {
                two_phase: false,
                failover: option(4)?,
            },
        )?;
        return slot_record(arena, "lsn", written_name, lsn);
    }

    let source = text_argument(arguments[0], function)?;
    let destination = text_argument(arguments[1], function)?;
    if matches!(arguments.get(2), Some(Datum::Bool(true))) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "temporary logical replication slots are not supported"
        ));
    }
    if arguments
        .get(2)
        .is_some_and(|value| !matches!(value, Datum::Bool(_)))
    {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "pg_copy_logical_replication_slot temporary option must be boolean"
        ));
    }
    if let Some(plugin) = arguments.get(3).copied() {
        let plugin = text_argument(plugin, function)?;
        if plugin != "pgoutput" {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "logical decoding output plugin \"{}\" is not supported",
                plugin
            ));
        }
    }
    let lsn = engine.copy_replication_slot(
        crate::storage::ReplicationSlotName::parse(source)?,
        crate::storage::ReplicationSlotName::parse(destination)?,
    )?;
    slot_record(arena, "lsn", destination, lsn)
}
