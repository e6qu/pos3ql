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

// This bounds only nested timing frames, not the number of routine calls in a
// statement. Routine execution has its own statement-arena-backed replay log.
const MAX_FUNCTION_TIMING_DEPTH: usize = 1024;

impl FunctionTimingFrame {
    const EMPTY: Self = Self {
        started: 0,
        child_micros: 0,
    };
}

struct FunctionTimingStack {
    frames: [FunctionTimingFrame; MAX_FUNCTION_TIMING_DEPTH],
    depth: usize,
}

std::thread_local! {
    static FUNCTION_TIMING: std::cell::RefCell<FunctionTimingStack> = const {
        std::cell::RefCell::new(FunctionTimingStack {
            frames: [FunctionTimingFrame::EMPTY; MAX_FUNCTION_TIMING_DEPTH],
            depth: 0,
        })
    };
}

/// An active timer belongs to the current SQL thread and ends before it yields.
#[must_use = "keep the timer until the function call finishes"]
pub(crate) struct FunctionTiming {
    depth: Option<usize>,
    _thread: core::marker::PhantomData<*mut ()>,
}

impl FunctionTiming {
    pub(crate) fn finish(mut self) -> Option<(u64, u64)> {
        self.depth.take().map(finish_function_timing)
    }
}

impl Drop for FunctionTiming {
    fn drop(&mut self) {
        if let Some(depth) = self.depth.take() {
            let _ = finish_function_timing(depth);
        }
    }
}

pub(crate) fn begin_function_timing(enabled: bool) -> Result<FunctionTiming, SqlError> {
    if !enabled {
        return Ok(FunctionTiming {
            depth: None,
            _thread: core::marker::PhantomData,
        });
    }
    FUNCTION_TIMING.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.depth == stack.frames.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "function timing stack exhausted ({} active calls)",
                MAX_FUNCTION_TIMING_DEPTH
            ));
        }
        let depth = stack.depth;
        stack.frames[depth] = FunctionTimingFrame {
            started: crate::sql::datetime::now_micros(),
            child_micros: 0,
        };
        stack.depth += 1;
        Ok(FunctionTiming {
            depth: Some(depth),
            _thread: core::marker::PhantomData,
        })
    })
}

fn finish_function_timing(depth: usize) -> (u64, u64) {
    FUNCTION_TIMING.with(|stack| {
        let mut stack = stack.borrow_mut();
        assert_eq!(
            stack.depth,
            depth + 1,
            "function timers finish in reverse call order"
        );
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
        (total, total.saturating_sub(frame.child_micros))
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
        crate::sql::query::with_active_routine_invocations(|context| {
            let (invocations, statement_arena) = context.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "{} requires a resumable query executor",
                    name
                )
            })?;
            invocations.resolve_intrinsic(intrinsic_oid, values, statement_arena, arena)
        })
    })())
}

pub(crate) fn execute(
    intrinsic_oid: i32,
    arguments: &[Datum<'_>],
    engine: &mut super::Engine,
    txn: &TxnState,
    arena: &Arena,
) -> Result<Datum<'static>, SqlError> {
    let key = arena
        .alloc_slice_with(
            arguments.len().checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statistics replay argument count overflow"
                )
            })?,
            |index| {
                if index == 0 {
                    Datum::Int4(intrinsic_oid)
                } else {
                    arguments[index - 1]
                }
            },
        )
        .map_err(|_| super::query::arena_full_pub())?;
    super::sequence::statement_effect(
        super::sequence::StatementEffectKind::StatisticsVoid,
        key,
        super::sequence::StatementEffectRetry::Restart,
        arena,
        || {
            let result =
                execute_unrecorded(intrinsic_oid, arguments, engine, txn).and_then(|value| {
                    arena
                        .alloc_slice_copy(&[value])
                        .map(|values| &*values)
                        .map_err(|_| super::query::arena_full_pub())
                });
            super::sequence::StatementEffectAttempt {
                result,
                rewound_state: false,
            }
        },
    )?;
    Ok(Datum::Text(""))
}

fn execute_unrecorded(
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
                [Datum::Text(value)] => Some(*value),
                [Datum::Null] => None,
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
                    target.expect("only a named target can be unrecognized")
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

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::sql::datetime::test_clock;

    #[test]
    fn function_timers_account_nested_children_without_allocating() {
        let _clock = test_clock::enter(1_000);
        crate::mem::guard::forbid_alloc(|| {
            let parent = begin_function_timing(true).unwrap();
            let child = {
                let _clock = test_clock::enter(1_100);
                begin_function_timing(true).unwrap()
            };
            {
                let _clock = test_clock::enter(1_500);
                assert_eq!(child.finish(), Some((400, 400)));
            }
            {
                let _clock = test_clock::enter(1_800);
                assert_eq!(begin_function_timing(false).unwrap().finish(), None);
                assert_eq!(parent.finish(), Some((800, 400)));
                assert_eq!(begin_function_timing(true).unwrap().finish(), Some((0, 0)));
            }
        });
    }

    #[test]
    fn function_timers_release_unfinished_calls_and_unwinding_children() {
        let _clock = test_clock::enter(1_000);
        let parent = begin_function_timing(true).unwrap();
        let child = {
            let _clock = test_clock::enter(1_100);
            begin_function_timing(true).unwrap()
        };
        {
            let _clock = test_clock::enter(1_500);
            drop(child);
            let unwound = std::panic::catch_unwind(|| {
                let _child = begin_function_timing(true).unwrap();
                panic!("unfinished timed function");
            });
            assert!(unwound.is_err());
        }
        {
            let _clock = test_clock::enter(1_800);
            assert_eq!(parent.finish(), Some((800, 400)));
            assert_eq!(begin_function_timing(true).unwrap().finish(), Some((0, 0)));
        }
    }

    #[test]
    fn function_timers_report_exhaustion_without_disabling_tracking() {
        let _clock = test_clock::enter(1_000);
        let mut timers = Vec::with_capacity(MAX_FUNCTION_TIMING_DEPTH);
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::mem::guard::forbid_alloc(|| {
                for _ in 0..MAX_FUNCTION_TIMING_DEPTH {
                    timers.push(begin_function_timing(true).unwrap());
                }
                let error = match begin_function_timing(true) {
                    Err(error) => error,
                    Ok(_) => panic!("timing exhaustion was accepted"),
                };
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(
                    error.message.as_str(),
                    "function timing stack exhausted (1024 active calls)"
                );
                assert_eq!(begin_function_timing(false).unwrap().finish(), None);
                FUNCTION_TIMING.with(|stack| {
                    assert_eq!(stack.borrow().depth, MAX_FUNCTION_TIMING_DEPTH);
                });
            });
        }));
        while let Some(timer) = timers.pop() {
            drop(timer);
        }
        checked.unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(begin_function_timing(true).unwrap().finish(), Some((0, 0)));
        });
    }
}
