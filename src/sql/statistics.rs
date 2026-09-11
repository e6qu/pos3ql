//! Mutable PostgreSQL cumulative-statistics control functions.

use crate::mem::arena::Arena;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, sqlstate};
use crate::sql::txn::TxnState;
use crate::sql::types::{Datum, oid};
use crate::sql_err;

pub(crate) fn result_type(name: &str, argument_count: usize) -> Option<(i32, i16)> {
    match (name, argument_count) {
        ("pg_stat_force_next_flush" | "pg_stat_clear_snapshot" | "pg_stat_reset", 0)
        | ("pg_stat_reset_single_table_counters", 1) => Some((oid::VOID, 4)),
        _ => None,
    }
}

pub(crate) fn is_intrinsic(oid: i32) -> bool {
    matches!(oid, 2137 | 2230 | 2274 | 3776)
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
    let intrinsic_oid = match (name, args.len(), star) {
        ("pg_stat_force_next_flush", 0, false) => 2137,
        ("pg_stat_clear_snapshot", 0, false) => 2230,
        ("pg_stat_reset", 0, false) => 2274,
        ("pg_stat_reset_single_table_counters", 1, false) => 3776,
        (
            "pg_stat_force_next_flush"
            | "pg_stat_clear_snapshot"
            | "pg_stat_reset"
            | "pg_stat_reset_single_table_counters",
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
    Some((|| {
        let argument = if let Some(expression) = args.first() {
            let value = crate::sql::eval::eval_full(expression, arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            Some(value)
        } else {
            None
        };
        let values = argument.as_slice();
        let (invocations, statement_arena) = crate::sql::query::active_routine_invocations()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "{} requires a resumable query executor",
                    name
                )
            })?;
        invocations.resolve_intrinsic(intrinsic_oid, values, statement_arena, arena)
    })())
}

pub(crate) fn execute(
    intrinsic_oid: i32,
    arguments: &[Datum<'_>],
    engine: &mut super::Engine,
    txn: &TxnState,
) -> Result<Datum<'static>, SqlError> {
    match intrinsic_oid {
        // pos3ql publishes counters synchronously and never holds a collector
        // snapshot, so both PostgreSQL synchronization operations are already
        // satisfied when the call reaches this boundary.
        2137 | 2230 => Ok(Datum::Text("")),
        2274 => {
            engine.require_superuser(txn.txid, "pg_stat_reset")?;
            engine.storage.reset_current_database_statistics();
            Ok(Datum::Text(""))
        }
        3776 => {
            engine.require_superuser(txn.txid, "pg_stat_reset_single_table_counters")?;
            let relation_oid = match arguments {
                [Datum::Oid(value)] => i32::try_from(*value).ok(),
                [Datum::Int4(value)] if *value >= 0 => Some(*value),
                [
                    Datum::RegObject {
                        type_oid: oid::REGCLASS,
                        referenced_oid,
                        ..
                    },
                ] => Some(*referenced_oid),
                [_] => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "pg_stat_reset_single_table_counters argument must be oid"
                    ));
                }
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "statistics reset intrinsic has an invalid argument count"
                    ));
                }
            };
            if let Some(relation_oid) = relation_oid {
                crate::sql::catalog::reset_relation_statistics_by_oid(
                    &engine.storage,
                    txn.txid,
                    relation_oid,
                );
            }
            Ok(Datum::Text(""))
        }
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "unknown statistics intrinsic"
        )),
    }
}
