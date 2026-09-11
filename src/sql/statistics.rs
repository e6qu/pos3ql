//! Mutable PostgreSQL cumulative-statistics control functions.

use crate::mem::arena::Arena;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, sqlstate};
use crate::sql::txn::TxnState;
use crate::sql::types::{Datum, oid};
use crate::sql_err;

#[derive(Clone, Copy)]
struct FunctionTimingFrame {
    started: i64,
    child_micros: u64,
}

impl FunctionTimingFrame {
    const EMPTY: Self = Self {
        started: 0,
        child_micros: 0,
    };
}

struct FunctionTimingStack {
    frames: [FunctionTimingFrame; crate::sql::query::MAX_ROUTINE_INVOCATIONS],
    depth: usize,
}

std::thread_local! {
    static FUNCTION_TIMING: std::cell::RefCell<FunctionTimingStack> = const {
        std::cell::RefCell::new(FunctionTimingStack {
            frames: [FunctionTimingFrame::EMPTY; crate::sql::query::MAX_ROUTINE_INVOCATIONS],
            depth: 0,
        })
    };
}

pub(crate) fn begin_function_timing(enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    FUNCTION_TIMING.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.depth == stack.frames.len() {
            return false;
        }
        let depth = stack.depth;
        stack.frames[depth] = FunctionTimingFrame {
            started: crate::sql::datetime::now_micros(),
            child_micros: 0,
        };
        stack.depth += 1;
        true
    })
}

pub(crate) fn finish_function_timing(enabled: bool) -> Option<(u64, u64)> {
    if !enabled {
        return None;
    }
    FUNCTION_TIMING.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.depth == 0 {
            return None;
        }
        stack.depth -= 1;
        let frame = stack.frames[stack.depth];
        let total = crate::sql::datetime::now_micros()
            .saturating_sub(frame.started)
            .max(0) as u64;
        if stack.depth != 0 {
            let parent = stack.depth - 1;
            stack.frames[parent].child_micros =
                stack.frames[parent].child_micros.saturating_add(total);
        }
        Some((total, total.saturating_sub(frame.child_micros)))
    })
}

pub(crate) fn result_type(name: &str, argument_count: usize) -> Option<(i32, i16)> {
    match (name, argument_count) {
        ("pg_stat_force_next_flush" | "pg_stat_clear_snapshot" | "pg_stat_reset", 0)
        | (
            "pg_stat_reset_slru"
            | "pg_stat_reset_shared"
            | "pg_stat_reset_single_table_counters"
            | "pg_stat_reset_single_function_counters"
            | "pg_stat_reset_backend_stats",
            1,
        ) => Some((oid::VOID, 4)),
        _ => None,
    }
}

pub(crate) fn is_intrinsic(oid: i32) -> bool {
    matches!(oid, 2137 | 2230 | 2274 | 2307 | 3775 | 3776 | 3777 | 6387)
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
        ("pg_stat_reset_slru", 1, false) => 2307,
        ("pg_stat_reset_shared", 1, false) => 3775,
        ("pg_stat_reset_single_table_counters", 1, false) => 3776,
        ("pg_stat_reset_single_function_counters", 1, false) => 3777,
        ("pg_stat_reset_backend_stats", 1, false) => 6387,
        (
            "pg_stat_force_next_flush"
            | "pg_stat_clear_snapshot"
            | "pg_stat_reset"
            | "pg_stat_reset_slru"
            | "pg_stat_reset_shared"
            | "pg_stat_reset_single_table_counters"
            | "pg_stat_reset_single_function_counters"
            | "pg_stat_reset_backend_stats",
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
            if value.is_null() && intrinsic_oid != 2307 && intrinsic_oid != 3775 {
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
        2307 => {
            engine.require_superuser(txn.txid, "pg_stat_reset_slru")?;
            let target = match arguments {
                [Datum::Null] => None,
                [Datum::Text(value)] => Some(*value),
                [_] => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "pg_stat_reset_slru argument must be text"
                    ));
                }
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "statistics reset intrinsic has an invalid argument count"
                    ));
                }
            };
            let _ = engine.storage.reset_slru_statistics(target);
            Ok(Datum::Text(""))
        }
        3775 => {
            engine.require_superuser(txn.txid, "pg_stat_reset_shared")?;
            let target = match arguments {
                [Datum::Text(value)] => *value,
                [Datum::Null] => return Ok(Datum::Text("")),
                [_] => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "pg_stat_reset_shared argument must be text"
                    ));
                }
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "statistics reset intrinsic has an invalid argument count"
                    ));
                }
            };
            if !engine.storage.reset_shared_statistics(target) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized reset target: \"{}\"",
                    target
                ));
            }
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
        3777 => {
            engine.require_superuser(txn.txid, "pg_stat_reset_single_function_counters")?;
            let oid = match arguments {
                [Datum::Oid(value)] => i32::try_from(*value).ok(),
                [Datum::Int4(value)] if *value >= 0 => Some(*value),
                [_] => None,
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "statistics reset intrinsic has an invalid argument count"
                    ));
                }
            };
            if let Some(oid) = oid {
                engine.storage.reset_function_statistics(oid);
            }
            Ok(Datum::Text(""))
        }
        6387 => {
            engine.require_superuser(txn.txid, "pg_stat_reset_backend_stats")?;
            match arguments {
                [Datum::Int4(_)] => Ok(Datum::Text("")),
                [_] => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "pg_stat_reset_backend_stats argument must be integer"
                )),
                _ => Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "statistics reset intrinsic has an invalid argument count"
                )),
            }
        }
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "unknown statistics intrinsic"
        )),
    }
}
