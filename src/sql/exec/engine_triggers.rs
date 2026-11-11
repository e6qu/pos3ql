//! Row triggers whose bodies need the engine.
//!
//! A write fires its `BEFORE` and `INSTEAD OF` row triggers without the
//! engine, so their bodies cannot run DDL, write views or ruled tables, or
//! call routines that need the engine. When such a body reaches one of
//! those, the outermost firing of the write's row stops the attempt and
//! leaves itself pending in the statement's invocation log. The write rolls
//! back its unfinished row, the engine runs the whole trigger body once, as
//! PostgreSQL would at that row, and the log keeps its result (whether the
//! row is kept and the returned `NEW`). The next attempt reaches the same
//! firing at the same log position and takes that result instead of running
//! the body again, so the body's effects, sequence values and notices happen
//! once.

use super::*;
use crate::sql::query::{self, TriggerCall};
use core::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FiringHost {
    /// No trigger runs without the engine.
    Outside,
    /// The outermost such firing can move to the engine.
    Movable,
    /// The outermost such firing cannot move, so its body's limits stand.
    Fixed,
}

thread_local! {
    static FIRING: Cell<FiringHost> = const { Cell::new(FiringHost::Outside) };
}

pub(super) struct FiringScope(FiringHost);

impl FiringScope {
    fn enter(host: FiringHost) -> Self {
        Self(FIRING.with(|firing| firing.replace(host)))
    }
}

impl Drop for FiringScope {
    fn drop(&mut self) {
        FIRING.with(|firing| firing.set(self.0));
    }
}

/// For a statement a trigger body cannot run without the engine: inside a
/// firing that can move to the engine, stops the attempt so the engine runs
/// the whole body; otherwise `unsupported`.
pub(crate) fn engine_required(unsupported: SqlError) -> SqlError {
    if FIRING.with(Cell::get) == FiringHost::Movable {
        sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "trigger body requires the statement executor"
        )
    } else {
        unsupported
    }
}

/// The identity of a trigger firing that may move to the engine.
#[derive(Clone, Copy)]
pub(super) struct MovableFiring {
    pub(super) slot: u16,
    pub(super) target: crate::storage::TriggerTarget,
    pub(super) event: u8,
}

/// Fires a trigger body through `run`. `movable` names a firing whose body
/// may move to the engine: a `BEFORE` or `INSTEAD OF` trigger, which reads no
/// transition tables. A statement trigger has neither `old` nor `new`.
pub(super) fn fire<'s, 'a, 'b>(
    context: &mut TriggerExecContext<'s, 'a, 'b>,
    movable: Option<MovableFiring>,
    old: Option<&[Datum<'a>]>,
    new: &mut Option<&mut [Datum<'a>]>,
    run: impl FnOnce(
        &mut TriggerExecContext<'s, 'a, 'b>,
        &mut Option<&mut [Datum<'a>]>,
    ) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    if !matches!(context.host, PlpgsqlExecHost::Atomic { .. })
        || FIRING.with(Cell::get) != FiringHost::Outside
    {
        return run(context, new);
    }
    let logged = query::active_invocation_position().is_some();
    let Some(movable) = movable.filter(|_| logged) else {
        let _scope = FiringScope::enter(FiringHost::Fixed);
        return run(context, new);
    };
    let _scope = FiringScope::enter(FiringHost::Movable);
    let arena = context.arena;
    let arguments = firing_arguments(old, new.as_deref(), arena)?;
    let call = TriggerCall {
        slot: movable.slot,
        target: movable.target,
        event: movable.event,
        old: old.is_some(),
        new: new.is_some(),
        rows: fingerprint(encode_projected_pub(arguments, arena)?),
    };
    let replayed = query::with_active_routine_invocations(|log| match log {
        Some((calls, statement)) => calls.replay_trigger(call, statement, arena),
        None => Ok(None),
    })?;
    if let Some(row) = replayed {
        // The body ran on the engine: its effects cannot be repeated by a
        // restart, so the write reads synchronously from here on.
        query::protect_statement_effects();
        return apply_result(row, new, arena);
    }
    let start = query::active_invocation_position().ok_or_else(log_changed)?;
    let replay = crate::sql::sequence::begin_replay_cursor()?;
    let effects = context.txn.transaction_effect_mark();
    match run(context, new) {
        Err(error) if error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION => {
            Err(query::with_active_routine_invocations(|log| match log {
                Some((calls, statement)) => calls.defer_trigger(
                    query::DeferredFiring {
                        start,
                        replay,
                        effects,
                        call,
                    },
                    arguments,
                    statement,
                ),
                None => log_changed(),
            }))
        }
        outcome => outcome,
    }
}

/// The firing's `OLD` then `NEW` values, copied before the body changes `NEW`.
fn firing_arguments<'a>(
    old: Option<&[Datum<'a>]>,
    new: Option<&[Datum<'a>]>,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    let old = old.unwrap_or(&[]);
    let new = new.unwrap_or(&[]);
    let values = arena
        .alloc_slice_with(old.len() + new.len(), |_| Datum::Null)
        .map_err(|_| query::arena_full_pub())?;
    values[..old.len()].copy_from_slice(old);
    values[old.len()..].copy_from_slice(new);
    Ok(values)
}

/// FNV-1a over a firing's encoded rows.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn apply_result<'a>(
    row: &'a [u8],
    new: &mut Option<&mut [Datum<'a>]>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let values = decoded_row(row, arena)?;
    let width = values.len();
    let expected = 1 + new.as_deref().map_or(0, <[Datum<'a>]>::len);
    let Datum::Bool(kept) = values[0] else {
        return Err(changed_result());
    };
    if width != expected {
        return Err(changed_result());
    }
    if let Some(new) = new {
        new.copy_from_slice(&values[1..width]);
    }
    Ok(kept)
}

fn decoded_row<'a>(row: &'a [u8], arena: &'a Arena) -> Result<&'a mut [Datum<'a>], SqlError> {
    let values = arena
        .alloc_slice_with(projected_row_width(row), |_| Datum::Null)
        .map_err(|_| query::arena_full_pub())?;
    decode_projected_row_into(row, arena, values)?;
    Ok(values)
}

fn changed_result() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "a replayed row trigger result does not match its row"
    )
}

fn log_changed() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "routine invocation log changed during a trigger firing"
    )
}

/// Runs a pending row trigger on the engine and encodes its result row in
/// `scratch`: whether the row is kept, then the returned `NEW` values.
#[expect(
    clippy::too_many_arguments,
    reason = "a trigger body runs against the statement's transaction and execution services"
)]
pub(crate) fn run_pending_trigger<'w>(
    engine: &mut crate::sql::Engine,
    call: TriggerCall,
    arguments: &[u8],
    txn: &mut TxnState,
    sqlprep: &mut crate::sql::prep::SqlPreparedPool,
    cursors: &mut crate::sql::cursor::CursorPool,
    guc: &crate::sql::guc::GucState,
    responder: &mut Responder<'_>,
    scratch: &'w Arena,
) -> Result<&'w [u8], SqlError> {
    let _scope = FiringScope::enter(FiringHost::Outside);
    let trigger = engine.storage.trigger(usize::from(call.slot));
    let arguments = scratch
        .alloc_slice_copy(arguments)
        .map_err(|_| query::arena_full_pub())?;
    match call.target {
        crate::storage::TriggerTarget::Table(table) => {
            let definition = (*engine.storage.table_def(usize::from(table), txn.txid)).clone();
            run_pending_firing(
                engine,
                call,
                &trigger,
                &definition,
                arguments,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                scratch,
            )
        }
        crate::storage::TriggerTarget::View(view) => {
            let definition =
                view_trigger_definition(&engine.storage, usize::from(view), txn.txid, scratch)?;
            run_pending_firing(
                engine,
                call,
                &trigger,
                &definition,
                arguments,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                scratch,
            )
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "a trigger body runs against the statement's transaction and execution services"
)]
fn run_pending_firing<'w, Definition0: crate::storage::DefinitionBacking>(
    engine: &mut crate::sql::Engine,
    call: TriggerCall,
    trigger: &crate::storage::TriggerDef,
    definition: &crate::storage::TableDefinition<Definition0>,
    arguments: &'w [u8],
    txn: &mut TxnState,
    sqlprep: &mut crate::sql::prep::SqlPreparedPool,
    cursors: &mut crate::sql::cursor::CursorPool,
    guc: &crate::sql::guc::GucState,
    responder: &mut Responder<'_>,
    scratch: &'w Arena,
) -> Result<&'w [u8], SqlError> {
    let columns = definition.n_columns;
    let values = decoded_row(arguments, scratch)?;
    let width = values.len();
    let rows = usize::from(call.old) + usize::from(call.new);
    if width != rows * columns {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a pending row trigger's rows do not match its relation"
        ));
    }
    let (old, new) = values.split_at_mut(if call.old { columns } else { 0 });
    let old = call.old.then_some(&*old);
    let mut new = call.new.then_some(new);
    // The body's own commands advance the counter; the write that fired it
    // continues as its own command, as it would after an inline firing.
    let command = txn.command_id();
    // Writes in the body hand their AFTER triggers to it, as in an AFTER
    // trigger the engine runs.
    let prior = txn.write_completion_boundary;
    txn.write_completion_boundary = Some(crate::sql::txn::WriteCompletionBoundary {
        mark: txn.statement_mark(engine.wal.stage_mark(txn.txid), engine.storage.lock_mark()),
        command,
        depth: txn.trigger_depth(),
    });
    let mut context = TriggerExecContext {
        host: PlpgsqlExecHost::Routine {
            engine,
            guc,
            sqlprep,
            cursors,
            transaction_context: PlpgsqlTransactionContext::Atomic,
        },
        txn,
        arena: scratch,
        params: crate::sql::eval::NO_PARAMS,
        responder,
    };
    let kept = if call.old || call.new {
        execute_row_trigger_body(
            &mut context,
            call.slot,
            trigger,
            call.target,
            definition,
            call.event,
            true,
            old,
            &mut new,
            None,
        )
    } else {
        execute_statement_trigger(
            &mut context,
            Some(call.slot),
            trigger,
            call.target,
            crate::storage::RelationDefinition::from_definition(definition),
            call.event,
            true,
            None,
        )
        .map(|()| true)
    };
    let txn = context.txn;
    txn.write_completion_boundary = prior;
    txn.resume_command(command);
    let kept = kept?;
    let new = new.as_deref().unwrap_or(&[]);
    let result = scratch
        .alloc_slice_with(new.len() + 1, |_| Datum::Bool(kept))
        .map_err(|_| query::arena_full_pub())?;
    result[1..].copy_from_slice(new);
    encode_projected_pub(result, scratch)
}
