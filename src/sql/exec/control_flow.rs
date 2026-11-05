//! Procedural positions and lexical scopes belong to bounded execution frames.

use super::*;
use crate::mem::arena::OwnedArenaSlice;
use numeric_loops::NumericLoopWorkspace;

pub(super) enum Step<'a> {
    Complete(Option<TriggerFlow<'a>>),
    Enter(Frame<'a>),
}

impl<'a> Step<'a> {
    pub(super) fn complete() -> Self {
        Self::Complete(None)
    }

    pub(super) fn branch(block: TriggerBlock<'a>) -> Self {
        Self::Enter(Frame::new(block, Continuation::Once))
    }

    pub(super) fn while_loop(program: TriggerWhile<'a>) -> Self {
        Self::Enter(Frame::new(
            program.block,
            Continuation::While(program.condition),
        ))
    }

    pub(super) fn plain_loop(block: TriggerBlock<'a>) -> Self {
        Self::Enter(Frame::new(block, Continuation::Loop))
    }

    pub(super) fn protected(
        program: TriggerException<'a>,
        boundary: TriggerSubtransaction,
    ) -> Self {
        Self::Enter(Frame::new(
            program.protected,
            Continuation::Protected {
                boundary,
                handlers: program.handlers,
                releasing: false,
            },
        ))
    }
}

pub(super) struct Frame<'a> {
    block: TriggerBlock<'a>,
    next: usize,
    continuation: Continuation<'a>,
}

impl<'a> Frame<'a> {
    fn new(block: TriggerBlock<'a>, continuation: Continuation<'a>) -> Self {
        Self {
            block,
            next: 0,
            continuation,
        }
    }

    fn is_loop(&self) -> bool {
        matches!(
            self.continuation,
            Continuation::While(_)
                | Continuation::Loop
                | Continuation::Numeric(_)
                | Continuation::Rows { .. }
        )
    }
}

struct NumericLoop<'a> {
    workspace: NumericLoopWorkspace<'a>,
    count: usize,
    target: usize,
    prior_target: Datum<'a>,
    value: i32,
    upper: i32,
    step: i32,
    reverse: bool,
}

enum Continuation<'a> {
    Once,
    While(&'a Expr<'a>),
    Loop,
    Numeric(NumericLoop<'a>),
    Rows {
        target: SqlName,
        values: &'a [Datum<'a>],
        index: usize,
    },
    Protected {
        boundary: TriggerSubtransaction,
        handlers: &'a [TriggerExceptionHandler<'a>],
        releasing: bool,
    },
    Handler(TriggerExceptionDiagnostic<'a>),
}

/// The lexical maximum is reserved before local initializers can have effects.
/// A frame owns its next statement and loop/handler state; entering a child does
/// not keep another Rust interpreter invocation on the call stack.
pub(super) struct ControlFrames<'a> {
    slots: OwnedArenaSlice<'a, Option<Frame<'a>>>,
    active: usize,
}

impl<'a> ControlFrames<'a> {
    pub(super) fn new(arena: &'a Arena, block: TriggerBlock<'a>) -> Result<Self, SqlError> {
        fn depth(block: TriggerBlock<'_>) -> usize {
            let mut children = 0;
            numeric_loops::visit_nested_blocks(block, |block, _| {
                children = children.max(depth(block))
            });
            children + 1
        }
        Ok(Self {
            slots: arena
                .alloc_owned_slice_with(depth(block), |_| None)
                .map_err(|_| frame_full())?,
            active: 0,
        })
    }

    fn push(&mut self, frame: Frame<'a>) {
        // Every active frame corresponds to one lexical block edge in depth().
        assert!(
            self.active < self.slots.len(),
            "procedural lexical frame capacity"
        );
        assert!(self.slots[self.active].is_none());
        self.slots[self.active] = Some(frame);
        self.active += 1;
    }

    fn pop(&mut self) -> Frame<'a> {
        self.active -= 1;
        self.slots[self.active]
            .take()
            .expect("active procedural frame")
    }

    fn last(&self) -> &Frame<'a> {
        self.slots[self.active - 1]
            .as_ref()
            .expect("active procedural frame")
    }

    fn last_mut(&mut self) -> &mut Frame<'a> {
        self.slots[self.active - 1]
            .as_mut()
            .expect("active procedural frame")
    }

    fn scope<'s>(
        &'s mut self,
        locals: &'s [TriggerLocalDecl<'a>],
        values: &'s mut [Datum<'a>],
    ) -> (&'s [TriggerLocalDecl<'a>], &'s mut [Datum<'a>]) {
        for frame in self.slots[..self.active].iter_mut().rev().flatten() {
            if let Continuation::Numeric(scope) = &mut frame.continuation {
                return (
                    &scope.workspace.locals[..scope.count],
                    &mut scope.workspace.values[..scope.count],
                );
            }
        }
        (locals, values)
    }

    fn diagnostic(&self) -> Option<TriggerExceptionDiagnostic<'a>> {
        self.slots[..self.active]
            .iter()
            .rev()
            .flatten()
            .find_map(|frame| match frame.continuation {
                Continuation::Handler(caught) => Some(caught),
                _ => None,
            })
    }

    fn finish(
        &mut self,
        frame: Frame<'a>,
        locals: &[TriggerLocalDecl<'a>],
        values: &mut [Datum<'a>],
        status: &mut TriggerExecutionStatus<'a>,
        success: bool,
    ) {
        match frame.continuation {
            Continuation::Numeric(scope) => {
                let (_, parent_values) = self.scope(locals, values);
                scope.workspace.values[scope.target] = scope.prior_target;
                parent_values.copy_from_slice(&scope.workspace.values[..parent_values.len()]);
                status.unshadow_output_local(scope.target);
                status.numeric_frames.restore(scope.workspace);
                if success {
                    status.set_found(true);
                }
            }
            Continuation::Rows { .. } if success => status.set_found(true),
            _ => {}
        }
    }
}

fn frame_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL execution frames exceed statement memory"
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "numeric entry binds its evaluated range and lexical scope once"
)]
pub(super) fn start_numeric_loop<'a>(
    program: TriggerFor<'a>,
    lower: i32,
    upper: i32,
    step: i32,
    reverse: bool,
    locals: &[TriggerLocalDecl<'a>],
    values: &[Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'a>, SqlError> {
    if if reverse {
        lower < upper
    } else {
        lower > upper
    } {
        status.set_found(false);
        return Ok(Step::complete());
    }
    let existing = locals.iter().position(|local| local.name == program.target);
    let target = existing.unwrap_or(locals.len());
    let count = locals.len() + usize::from(existing.is_none());
    let workspace = status.numeric_frames.take()?;
    if count > workspace.locals.len() || count > workspace.values.len() {
        status.numeric_frames.restore(workspace);
        return Err(frame_full());
    }
    for (index, local) in workspace.locals[..count].iter_mut().enumerate() {
        *local = if index == target {
            TriggerLocalDecl {
                name: program.target,
                ctype: ColType::Int4,
                ..empty_plpgsql_local()
            }
        } else {
            locals[index]
        };
    }
    workspace.values[..count].fill(Datum::Null);
    workspace.values[..locals.len()].copy_from_slice(&values[..locals.len()]);
    let prior_target = workspace.values[target];
    if let Err(error) = status.shadow_output_local(target) {
        status.numeric_frames.restore(workspace);
        return Err(error);
    }
    workspace.values[target] = Datum::Int4(lower);
    Ok(Step::Enter(Frame::new(
        program.block,
        Continuation::Numeric(NumericLoop {
            workspace,
            count,
            target,
            prior_target,
            value: lower,
            upper,
            step,
            reverse,
        }),
    )))
}

pub(super) fn start_row_loop<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    program: TriggerFor<'a>,
    target: usize,
    rows: TriggerForRows<'a>,
    locals: &[TriggerLocalDecl<'a>],
    values: &mut [Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'a>, SqlError> {
    let Some(value) = rows.values.first().copied() else {
        status.set_found(false);
        return Ok(Step::complete());
    };
    assign_trigger_local(context, locals, values, program.target, value)?;
    status.assign_output_local(target, values[target]);
    Ok(Step::Enter(Frame::new(
        program.block,
        Continuation::Rows {
            target: program.target,
            values: rows.values,
            index: 0,
        },
    )))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the interpreter retains the complete typed firing context"
)]
pub(super) fn execute_trigger_block<'a, Definition0: crate::storage::DefinitionBacking>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    definition: &crate::storage::TableDefinition<Definition0>,
    invocation: TriggerInvocation<'a>,
    old: Option<&[Datum<'a>]>,
    new: &mut Option<&mut [Datum<'a>]>,
    before: bool,
    transition_relations: Option<&'a [(&'a str, &'a crate::sql::ast::MaterializedCte<'a>)]>,
    locals: &[TriggerLocalDecl<'a>],
    values: &mut [Datum<'a>],
    block: TriggerBlock<'a>,
    status: &mut TriggerExecutionStatus<'a>,
    exception: Option<&TriggerExceptionDiagnostic<'_>>,
) -> Result<Option<TriggerFlow<'a>>, SqlError> {
    let mut frames = status
        .control_frames
        .take()
        .expect("one procedural frame owner");
    assert_eq!(frames.active, 0);
    frames.push(Frame::new(block, Continuation::Once));
    let mut outcome = Ok(None);
    let mut pending_error = None;
    while frames.active != 0 {
        let caught = frames.diagnostic();
        let invocation = caught.map_or(invocation, |caught| invocation.with_exception(caught));
        let exception = caught.as_ref().or(exception);
        let action = (|| {
            if let Some(error) = pending_error.take() {
                return Err(error);
            }
            super::super::query::check_timeout()?;
            if let Some(statement) = frames
                .last()
                .block
                .statements
                .get(frames.last().next)
                .copied()
            {
                let (scope, scope_values) = frames.scope(locals, values);
                return execute_trigger_step(
                    context,
                    definition,
                    invocation,
                    old,
                    new,
                    before,
                    transition_relations,
                    scope,
                    scope_values,
                    statement,
                    status,
                    exception,
                )
                .map(Some);
            }
            match frames.last().continuation {
                Continuation::Loop => {
                    frames.last_mut().next = 0;
                }
                Continuation::While(condition) => {
                    let (scope, scope_values) = frames.scope(locals, values);
                    if trigger_condition(
                        context,
                        condition,
                        definition,
                        invocation,
                        old,
                        new.as_deref(),
                        scope,
                        scope_values,
                        status.found,
                    )? {
                        frames.last_mut().next = 0;
                    } else {
                        let frame = frames.pop();
                        frames.finish(frame, locals, values, status, true);
                    }
                }
                Continuation::Numeric(ref range) => {
                    let next = if range.reverse {
                        range.value.checked_sub(range.step)
                    } else {
                        range.value.checked_add(range.step)
                    };
                    if let Some(next) = next.filter(|value| {
                        if range.reverse {
                            *value >= range.upper
                        } else {
                            *value <= range.upper
                        }
                    }) {
                        let frame = frames.last_mut();
                        let Continuation::Numeric(range) = &mut frame.continuation else {
                            unreachable!()
                        };
                        range.value = next;
                        range.workspace.values[range.target] = Datum::Int4(next);
                        frame.next = 0;
                    } else {
                        let frame = frames.pop();
                        frames.finish(frame, locals, values, status, true);
                    }
                }
                Continuation::Rows {
                    target,
                    values: rows,
                    index,
                } => {
                    if let Some(value) = rows.get(index + 1).copied() {
                        let (scope, scope_values) = frames.scope(locals, values);
                        let local = trigger_local_index(scope, target)?;
                        assign_trigger_local(context, scope, scope_values, target, value)?;
                        status.assign_output_local(local, scope_values[local]);
                        let frame = frames.last_mut();
                        frame.continuation = Continuation::Rows {
                            target,
                            values: rows,
                            index: index + 1,
                        };
                        frame.next = 0;
                    } else {
                        let frame = frames.pop();
                        frames.finish(frame, locals, values, status, true);
                    }
                }
                Continuation::Protected { .. } => {
                    let Continuation::Protected {
                        boundary,
                        releasing,
                        ..
                    } = &mut frames.last_mut().continuation
                    else {
                        unreachable!()
                    };
                    *releasing = true;
                    release_trigger_subtransaction(context, boundary)?;
                    let frame = frames.pop();
                    frames.finish(frame, locals, values, status, true);
                }
                Continuation::Once | Continuation::Handler(_) => {
                    let frame = frames.pop();
                    frames.finish(frame, locals, values, status, true);
                }
            }
            Ok(None)
        })();
        match action {
            Ok(Some(Step::Enter(frame))) => {
                frames.last_mut().next += 1;
                frames.push(frame);
            }
            Ok(Some(Step::Complete(None))) => frames.last_mut().next += 1,
            Ok(None) => {}
            Ok(Some(Step::Complete(Some(mut flow)))) => {
                while frames.active != 0 {
                    if frames.last().is_loop() {
                        match flow {
                            TriggerFlow::Continue(0) => {
                                frames.last_mut().next = frames.last().block.statements.len();
                                break;
                            }
                            TriggerFlow::Exit(0) => {
                                let frame = frames.pop();
                                frames.finish(frame, locals, values, status, true);
                                break;
                            }
                            TriggerFlow::Continue(depth) => flow = TriggerFlow::Continue(depth - 1),
                            TriggerFlow::Exit(depth) => flow = TriggerFlow::Exit(depth - 1),
                            TriggerFlow::Return(_) => {}
                        }
                    }
                    if let Continuation::Protected {
                        boundary,
                        releasing,
                        ..
                    } = &mut frames.last_mut().continuation
                    {
                        *releasing = true;
                        if let Err(error) = release_trigger_subtransaction(context, boundary) {
                            pending_error = Some(error);
                            break;
                        }
                    }
                    let frame = frames.pop();
                    frames.finish(frame, locals, values, status, true);
                    if frames.active == 0 {
                        outcome = Ok(Some(flow));
                    }
                }
            }
            Err(mut error) => {
                let mut handler = None;
                while frames.active != 0 {
                    let frame = frames.pop();
                    if let Continuation::Protected {
                        ref boundary,
                        handlers,
                        releasing: false,
                    } = frame.continuation
                    {
                        match rollback_trigger_subtransaction(context, boundary) {
                            Ok(()) => {
                                handler = handlers.iter().find(|handler| {
                                    handler.conditions.iter().copied().any(|condition| {
                                        trigger_exception_matches(condition, &error)
                                    })
                                });
                            }
                            Err(rollback_error) => error = rollback_error,
                        }
                    }
                    frames.finish(frame, locals, values, status, false);
                    if handler.is_some() {
                        break;
                    }
                }
                let Some(handler) = handler else {
                    outcome = Err(error);
                    break;
                };
                match catch_diagnostic(context, invocation, &error) {
                    Ok(caught) => {
                        frames.push(Frame::new(handler.block, Continuation::Handler(caught)))
                    }
                    Err(error) => pending_error = Some(error),
                }
            }
        }
    }
    // Capacity/error exits release every lexical owner before the caller can
    // rewind statement memory. Transaction rollback remains the caller's job.
    while frames.active != 0 {
        let frame = frames.pop();
        frames.finish(frame, locals, values, status, false);
    }
    status.control_frames = Some(frames);
    outcome
}

fn catch_diagnostic<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    invocation: TriggerInvocation<'a>,
    error: &SqlError,
) -> Result<TriggerExceptionDiagnostic<'a>, SqlError> {
    if error.sqlstate == sqlstate::QUERY_CANCELED {
        context
            .txn
            .catch_query_cancellation(super::super::datetime::now_micros());
        super::super::sequence::record_caught_cancellation(context.seq_session());
    }
    let diagnostic = crate::sql::eval::take_diagnostic();
    let copy = |text: &str| {
        context
            .arena
            .alloc_str(text)
            .map_err(|_| super::super::query::arena_full_pub())
    };
    Ok(TriggerExceptionDiagnostic {
        sqlstate: error.sqlstate,
        sqlstate_text: copy(error.sqlstate.as_str())?,
        message: copy(error.message.as_str())?,
        detail: diagnostic
            .as_ref()
            .and_then(|value| value.detail.as_ref())
            .map(|value| copy(value.as_str()))
            .transpose()?,
        hint: diagnostic
            .as_ref()
            .and_then(|value| value.hint.as_ref())
            .map(|value| copy(value.as_str()))
            .transpose()?,
        context: copy(invocation.context().as_str())?,
    })
}

#[cfg(test)]
mod ownership_tests;
