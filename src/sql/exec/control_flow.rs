//! Procedural positions and lexical scopes belong to bounded execution frames.

use super::diagnostics::{CaughtDiagnostic, DiagnosticBuffers};
use super::*;
use crate::mem::arena::OwnedArenaSlice;
use numeric_loops::{BlockEdge, NumericLoopWorkspace};

pub(super) enum Step<'a> {
    Complete(Option<TriggerFlow<'a>>),
    Enter(FrameEntry<'a>),
}

impl<'a> Step<'a> {
    pub(super) fn complete() -> Self {
        Self::Complete(None)
    }

    pub(super) fn branch(block: TriggerBlock<'a>) -> Self {
        Self::Enter(FrameEntry::new(block, Continuation::Once))
    }

    pub(super) fn while_loop(program: TriggerWhile<'a>) -> Self {
        Self::Enter(FrameEntry::new(program.block, Continuation::While))
    }

    pub(super) fn plain_loop(block: TriggerBlock<'a>) -> Self {
        Self::Enter(FrameEntry::new(block, Continuation::Loop))
    }

    pub(super) fn protected(
        program: TriggerException<'a>,
        boundary: TriggerSubtransaction,
    ) -> Self {
        Self::Enter(FrameEntry::new(
            program.protected,
            Continuation::Protected {
                boundary,
                releasing: false,
            },
        ))
    }
}

pub(super) struct FrameEntry<'a> {
    block: TriggerBlock<'a>,
    continuation: Continuation<'a>,
}

impl<'a> FrameEntry<'a> {
    fn new(block: TriggerBlock<'a>, continuation: Continuation<'a>) -> Self {
        Self {
            block,
            continuation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockIndex(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatementIndex {
    block: BlockIndex,
    statement: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameSource {
    Root,
    Handler,
    Statement(StatementIndex),
}

struct Frame<'a> {
    block: BlockIndex,
    next: usize,
    source: FrameSource,
    continuation: Continuation<'a>,
}

impl Frame<'_> {
    fn is_loop(&self) -> bool {
        matches!(
            self.continuation,
            Continuation::While
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
    While,
    Loop,
    Numeric(NumericLoop<'a>),
    Rows {
        target: SqlName,
        values: super::loop_rows::LoopRows,
        index: usize,
    },
    Protected {
        boundary: TriggerSubtransaction,
        releasing: bool,
    },
    Handler(CaughtDiagnostic),
}

/// The lexical maximum is reserved before local initializers can have effects.
/// A frame owns its next statement and loop/handler state; entering a child does
/// not keep another Rust interpreter invocation on the call stack.
pub(super) struct ControlFrames<'a> {
    blocks: &'a [TriggerBlock<'a>],
    slots: OwnedArenaSlice<'a, Option<Frame<'a>>>,
    active: usize,
    diagnostics: DiagnosticBuffers,
}

impl<'a> ControlFrames<'a> {
    pub(super) fn new(arena: &'a Arena, block: TriggerBlock<'a>) -> Result<Self, SqlError> {
        fn layout(block: TriggerBlock<'_>) -> (usize, usize, usize) {
            let mut count = 1;
            let mut depth = 1;
            let mut handlers = 0;
            numeric_loops::visit_nested_blocks(block, |child, edge| {
                let (child_count, child_depth, child_handlers) = layout(child);
                count += child_count;
                depth = depth.max(child_depth + 1);
                handlers =
                    handlers.max(child_handlers + usize::from(matches!(edge, BlockEdge::Handler)));
            });
            (count, depth, handlers)
        }
        fn compile<'a>(block: TriggerBlock<'a>, blocks: &mut [TriggerBlock<'a>], next: &mut usize) {
            blocks[*next] = block;
            *next += 1;
            numeric_loops::visit_nested_blocks(block, |child, _| compile(child, blocks, next));
        }
        let (count, depth, handlers) = layout(block);
        let blocks = arena
            .alloc_slice_with(count, |_| TriggerBlock { statements: &[] })
            .map_err(|_| frame_full())?;
        let mut next = 0;
        compile(block, blocks, &mut next);
        assert_eq!(next, count);
        Ok(Self {
            blocks,
            slots: arena
                .alloc_owned_slice_with(depth, |_| None)
                .map_err(|_| frame_full())?,
            active: 0,
            diagnostics: DiagnosticBuffers::new(arena, handlers)?,
        })
    }

    fn block(&self, index: BlockIndex) -> TriggerBlock<'a> {
        self.blocks[index.0]
    }

    fn index(&self, block: TriggerBlock<'a>) -> BlockIndex {
        BlockIndex(
            self.blocks
                .iter()
                .position(|candidate| core::ptr::eq(candidate.statements, block.statements))
                .expect("entered block belongs to the compiled procedural program"),
        )
    }

    fn push(&mut self, entry: FrameEntry<'a>, source: FrameSource) {
        // Each active frame corresponds to one lexical edge in layout().
        assert!(
            self.active < self.slots.len(),
            "procedural lexical frame capacity"
        );
        assert!(self.slots[self.active].is_none());
        self.slots[self.active] = Some(Frame {
            block: self.index(entry.block),
            next: 0,
            source,
            continuation: entry.continuation,
        });
        self.active += 1;
    }

    fn start(&mut self, block: TriggerBlock<'a>) {
        assert_eq!(self.active, 0);
        self.push(
            FrameEntry::new(block, Continuation::Once),
            FrameSource::Root,
        );
    }

    fn enter(&mut self, entry: FrameEntry<'a>) {
        let parent = self.last_mut();
        let source = FrameSource::Statement(StatementIndex {
            block: parent.block,
            statement: parent.next,
        });
        parent.next += 1;
        self.push(entry, source);
    }

    fn source_statement(&self, source: FrameSource) -> TriggerStatement<'a> {
        let FrameSource::Statement(source) = source else {
            unreachable!("frame has no enclosing statement")
        };
        self.block(source.block).statements[source.statement]
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
        locals: &'s [TriggerLocalDecl],
        values: &'s mut [Datum<'a>],
    ) -> (&'s [TriggerLocalDecl], &'s mut [Datum<'a>]) {
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

    fn diagnostic(&self) -> Option<(CaughtDiagnostic, PlpgsqlScope)> {
        self.slots[..self.active]
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(depth, frame)| frame.as_ref().map(|frame| (depth, frame)))
            .find_map(|(depth, frame)| match frame.continuation {
                Continuation::Handler(caught) => Some((caught, PlpgsqlScope::frame(depth + 1))),
                _ => None,
            })
    }

    fn finish(
        &mut self,
        arena: &Arena,
        frame: Frame<'a>,
        locals: &[TriggerLocalDecl],
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
            Continuation::Handler(diagnostic) => self.diagnostics.release(arena, diagnostic),
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
    counter_scope: PlpgsqlScope,
    locals: &[TriggerLocalDecl],
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
    let existing = locals
        .iter()
        .rposition(|local| local.name == program.target);
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
                scope: counter_scope,
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
    Ok(Step::Enter(FrameEntry::new(
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

#[expect(
    clippy::too_many_arguments,
    reason = "row loop entry preserves source cardinality and its complete local scope"
)]
pub(super) fn start_row_loop<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    program: TriggerFor<'a>,
    target: usize,
    rows: super::loop_rows::LoopRows,
    query_source: bool,
    locals: &[TriggerLocalDecl],
    values: &mut [Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'a>, SqlError> {
    if rows.is_empty() {
        let empty = query_source
            .then(|| rows.empty_query_row(context.arena))
            .transpose()?;
        drop(rows);
        if let Some(empty) = empty {
            assign_trigger_local(context, locals, values, program.target, empty)?;
            status.assign_output_local(target, values[target]);
        }
        status.set_found(false);
        return Ok(Step::complete());
    }
    let value = rows.value(0, context.arena)?;
    assign_trigger_local(context, locals, values, program.target, value)?;
    status.assign_output_local(target, values[target]);
    Ok(Step::Enter(FrameEntry::new(
        program.block,
        Continuation::Rows {
            target: program.target,
            values: rows,
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
    locals: &[TriggerLocalDecl],
    values: &mut [Datum<'a>],
    block: TriggerBlock<'a>,
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Option<TriggerFlow<'a>>, SqlError> {
    let mut frames = status
        .control_frames
        .take()
        .expect("one procedural frame owner");
    assert_eq!(frames.active, 0);
    frames.start(block);
    let mut outcome = Ok(None);
    let mut pending_error = None;
    while frames.active != 0 {
        let caught = frames.diagnostic();
        let exception = caught.as_ref().map(|(diagnostic, _)| diagnostic);
        let action = (|| {
            if let Some(error) = pending_error.take() {
                return Err(error);
            }
            super::super::query::check_timeout()?;
            if let Some(statement) = frames
                .block(frames.last().block)
                .statements
                .get(frames.last().next)
                .copied()
            {
                let invocation =
                    bind_exception_variables(invocation, caught, statement, context.arena)?;
                let child_scope = PlpgsqlScope::frame(frames.active + 1);
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
                    child_scope,
                    status,
                    exception,
                )
                .map(Some);
            }
            match frames.last().continuation {
                Continuation::Loop => {
                    frames.last_mut().next = 0;
                }
                Continuation::While => {
                    let TriggerStatement::While(program) =
                        frames.source_statement(frames.last().source)
                    else {
                        unreachable!("while frame source")
                    };
                    let condition = program.condition;
                    let invocation = bind_exception_variables(
                        invocation,
                        caught,
                        TriggerStatement::While(program),
                        context.arena,
                    )?;
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
                        frames.finish(context.arena, frame, locals, values, status, true);
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
                        frames.finish(context.arena, frame, locals, values, status, true);
                    }
                }
                Continuation::Rows {
                    target,
                    values: ref rows,
                    index,
                } => {
                    if index + 1 < rows.len() {
                        let value = rows.value(index + 1, context.arena)?;
                        let (scope, scope_values) = frames.scope(locals, values);
                        let local = trigger_local_index(scope, target)?;
                        assign_trigger_local(context, scope, scope_values, target, value)?;
                        status.assign_output_local(local, scope_values[local]);
                        let frame = frames.last_mut();
                        let Continuation::Rows { index, .. } = &mut frame.continuation else {
                            unreachable!("row-loop frame")
                        };
                        *index += 1;
                        frame.next = 0;
                    } else {
                        let frame = frames.pop();
                        frames.finish(context.arena, frame, locals, values, status, true);
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
                    frames.finish(context.arena, frame, locals, values, status, true);
                }
                Continuation::Once | Continuation::Handler(_) => {
                    let frame = frames.pop();
                    frames.finish(context.arena, frame, locals, values, status, true);
                }
            }
            Ok(None)
        })();
        match action {
            Ok(Some(Step::Enter(frame))) => {
                frames.enter(frame);
            }
            Ok(Some(Step::Complete(None))) => frames.last_mut().next += 1,
            Ok(None) => {}
            Ok(Some(Step::Complete(Some(mut flow)))) => {
                while frames.active != 0 {
                    if frames.last().is_loop() {
                        match flow {
                            TriggerFlow::Continue(0) => {
                                frames.last_mut().next =
                                    frames.block(frames.last().block).statements.len();
                                break;
                            }
                            TriggerFlow::Exit(0) => {
                                let frame = frames.pop();
                                frames.finish(context.arena, frame, locals, values, status, true);
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
                    frames.finish(context.arena, frame, locals, values, status, true);
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
                        releasing: false,
                    } = frame.continuation
                    {
                        match rollback_trigger_subtransaction(context, boundary) {
                            Ok(()) => {
                                let TriggerStatement::Exception(program) =
                                    frames.source_statement(frame.source)
                                else {
                                    unreachable!("exception frame source")
                                };
                                handler = program.handlers.iter().find(|handler| {
                                    handler.conditions.iter().copied().any(|condition| {
                                        trigger_exception_matches(condition, &error)
                                    })
                                });
                            }
                            Err(rollback_error) => error = rollback_error,
                        }
                    }
                    frames.finish(context.arena, frame, locals, values, status, false);
                    if handler.is_some() {
                        break;
                    }
                }
                let Some(handler) = handler else {
                    outcome = Err(error);
                    break;
                };
                match catch_diagnostic(context, invocation, &error, &mut frames.diagnostics) {
                    Ok(caught) => frames.push(
                        FrameEntry::new(handler.block, Continuation::Handler(caught)),
                        FrameSource::Handler,
                    ),
                    Err(error) => pending_error = Some(error),
                }
            }
        }
    }
    // Capacity/error exits release every lexical owner before the caller can
    // rewind statement memory. Transaction rollback remains the caller's job.
    while frames.active != 0 {
        let frame = frames.pop();
        frames.finish(context.arena, frame, locals, values, status, false);
    }
    status.control_frames = Some(frames);
    outcome
}

// Bind snapshots before a child query borrows the remaining arena. Only input
// sites that can resolve the handler variables need independent copies.
fn bind_exception_variables<'a>(
    invocation: TriggerInvocation<'a>,
    diagnostic: Option<(CaughtDiagnostic, PlpgsqlScope)>,
    statement: TriggerStatement<'a>,
    arena: &'a Arena,
) -> Result<TriggerInvocation<'a>, SqlError> {
    use super::super::plpgsql_inputs::Kind;
    let Some((diagnostic, scope)) = diagnostic else {
        return Ok(invocation);
    };
    let site = |kind, key| {
        invocation
            .input_sites
            .iter()
            .any(|site| (site.kind, site.key) == (kind, key) && site.exception_variables)
    };
    let expression = |value: &Expr<'a>| site(Kind::Expression, core::ptr::from_ref(value) as usize);
    let optional = |value: Option<&Expr<'a>>| value.is_some_and(expression);
    let dynamic = |query: PlpgsqlDynamicQuery<'a>| {
        expression(query.command) || query.arguments.iter().any(|value| expression(value))
    };
    let needed = match statement {
        TriggerStatement::Assign(value) => value.exception_variables,
        TriggerStatement::LocalAssign(value) => value.exception_variables,
        TriggerStatement::Perform(query)
        | TriggerStatement::ReturnQuery(query)
        | TriggerStatement::SelectInto(TriggerSelectInto { query, .. }) => {
            site(Kind::Query, core::ptr::from_ref(query) as usize)
        }
        TriggerStatement::Utility { source, .. }
        | TriggerStatement::Dml { source, .. }
        | TriggerStatement::CursorOpen(PlpgsqlCursorOpen {
            query: PlpgsqlCursorQuery::Static(source),
            ..
        }) => site(Kind::Statement, source.as_ptr() as usize),
        TriggerStatement::DynamicQuery(query)
        | TriggerStatement::ReturnDynamicQuery(query)
        | TriggerStatement::DynamicSelectInto(PlpgsqlDynamicSelectInto { query, .. })
        | TriggerStatement::CursorOpen(PlpgsqlCursorOpen {
            query: PlpgsqlCursorQuery::Dynamic(query),
            ..
        }) => dynamic(query),
        TriggerStatement::Assert(value) => expression(value.condition) || optional(value.message),
        TriggerStatement::Raise(TriggerRaise::Message(value)) => {
            value.arguments.iter().any(|value| expression(value))
                || optional(value.message)
                || optional(value.detail)
                || optional(value.hint)
                || match value.sqlstate {
                    TriggerRaiseSqlState::Parsed(_) => false,
                    TriggerRaiseSqlState::Expression(value) => expression(value),
                }
        }
        TriggerStatement::If(value) => value
            .branches
            .iter()
            .any(|branch| optional(branch.condition)),
        TriggerStatement::Case(value) => {
            optional(value.operand) || value.branches.iter().any(|branch| expression(branch.when))
        }
        TriggerStatement::For(value) => match value.source {
            TriggerForSource::Numeric {
                lower, upper, step, ..
            } => expression(lower) || expression(upper) || expression(step),
            TriggerForSource::Query(query) => {
                site(Kind::Query, core::ptr::from_ref(query) as usize)
            }
            TriggerForSource::DynamicQuery(query) => dynamic(query),
            TriggerForSource::Array {
                expression: value, ..
            } => expression(value),
        },
        TriggerStatement::While(value) => expression(value.condition),
        TriggerStatement::LoopControl(
            TriggerLoopControl::Exit { condition, .. }
            | TriggerLoopControl::Continue { condition, .. },
        ) => optional(condition),
        TriggerStatement::ReturnNext(value) => optional(value),
        TriggerStatement::Return(TriggerReturn::Value(value)) => expression(value),
        TriggerStatement::Null
        | TriggerStatement::TransactionControl(_)
        | TriggerStatement::CursorMotion(_)
        | TriggerStatement::CursorClose(_)
        | TriggerStatement::Raise(TriggerRaise::Rethrow)
        | TriggerStatement::GetDiagnostics(_)
        | TriggerStatement::GetStackedDiagnostics(_)
        | TriggerStatement::Exception(_)
        | TriggerStatement::Loop(_)
        | TriggerStatement::Return(
            TriggerReturn::New | TriggerReturn::Old | TriggerReturn::Null | TriggerReturn::Void,
        ) => false,
    };
    if needed {
        Ok(invocation.with_exception(diagnostic.variables(arena, arena)?, scope))
    } else {
        Ok(invocation)
    }
}

fn catch_diagnostic(
    context: &mut TriggerExecContext<'_, '_, '_>,
    invocation: TriggerInvocation<'_>,
    error: &SqlError,
    buffers: &mut DiagnosticBuffers,
) -> Result<CaughtDiagnostic, SqlError> {
    if error.sqlstate == sqlstate::QUERY_CANCELED {
        context
            .txn
            .catch_query_cancellation(super::super::datetime::now_micros());
        super::super::sequence::record_caught_cancellation(context.seq_session());
    }
    let diagnostic = crate::sql::eval::take_diagnostic();
    buffers.capture(
        context.arena,
        error,
        diagnostic.as_ref(),
        invocation.context().as_str(),
    )
}

#[cfg(test)]
mod ownership_tests;
