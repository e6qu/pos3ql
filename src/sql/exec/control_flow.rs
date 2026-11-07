//! Procedural positions and lexical scopes belong to bounded execution frames.

use super::diagnostics::{CaughtDiagnostic, DiagnosticBuffers};
use super::*;
use crate::mem::arena::ArenaIdentity;

mod program;
use numeric_loops::NumericLoopWorkspace;
use program::ProgramLayout;
pub(super) use program::{CompiledProgram, ProgramSource};

pub(super) enum Step<'syntax, 'values> {
    Complete(Option<TriggerFlow<'values>>),
    Enter(FrameEntry<'syntax, 'values>),
}

impl<'syntax, 'values> Step<'syntax, 'values> {
    pub(super) fn complete() -> Self {
        Self::Complete(None)
    }

    pub(super) fn branch(block: TriggerBlock<'syntax>) -> Self {
        Self::Enter(FrameEntry::new(block, EntryState::Once))
    }

    pub(super) fn while_loop(program: TriggerWhile<'syntax>) -> Self {
        Self::Enter(FrameEntry::new(program.block, EntryState::While))
    }

    pub(super) fn plain_loop(block: TriggerBlock<'syntax>) -> Self {
        Self::Enter(FrameEntry::new(block, EntryState::Loop))
    }

    pub(super) fn protected(
        program: TriggerException<'syntax>,
        boundary: TriggerSubtransaction,
    ) -> Self {
        Self::Enter(FrameEntry::new(
            program.protected,
            EntryState::Protected(boundary),
        ))
    }
}

pub(super) struct FrameEntry<'syntax, 'values> {
    block: TriggerBlock<'syntax>,
    state: EntryState<'values>,
}

impl<'syntax, 'values> FrameEntry<'syntax, 'values> {
    fn new(block: TriggerBlock<'syntax>, state: EntryState<'values>) -> Self {
        Self { block, state }
    }
}

// Entry owns a loop's resource together with its position. Only the exhaustive
// split at push can publish them into separate indexed storage.
enum EntryState<'a> {
    Once,
    While,
    Loop,
    Numeric {
        position: NumericLoop,
        workspace: NumericLoopWorkspace<'a>,
    },
    Rows {
        target: SqlName,
        values: super::loop_rows::LoopRows,
        index: usize,
    },
    Protected(TriggerSubtransaction),
    Handler,
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

#[derive(Clone, Copy)]
struct Frame<Resource> {
    block: BlockIndex,
    next: usize,
    source: FrameSource,
    continuation: Continuation,
    resource: Resource,
}

impl<Resource> Frame<Resource> {
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

#[derive(Clone, Copy)]
struct NumericLoop {
    count: usize,
    target: usize,
    value: i32,
    upper: i32,
    step: i32,
    reverse: bool,
}

enum FrameResource<'a> {
    Empty,
    Numeric { workspace: NumericLoopWorkspace<'a> },
    Rows(super::loop_rows::LoopRows),
}

#[derive(Clone, Copy)]
enum Continuation {
    Once,
    While,
    Loop,
    Numeric(NumericLoop),
    Rows {
        target: SqlName,
        index: usize,
    },
    Protected {
        boundary: TriggerSubtransaction,
        releasing: bool,
    },
    Handler,
}

/// The lexical maximum is reserved before local initializers can have effects.
/// A frame owns its next statement and loop/handler state; entering a child does
/// not keep another Rust interpreter invocation on the call stack.
pub(super) struct ControlFrames<'a> {
    source: ProgramSource,
    slots: MutableSlice<Option<Frame<()>>>,
    active: usize,
    diagnostics: DiagnosticBuffers,
    resources: MutableSlice<FrameResource<'a>>,
    workspace: ArenaIdentity,
}

/// Every payload reference has been removed; only owned images and reservations
/// remain. This state cannot execute until all live values have been restored.
pub(super) struct SuspendedControlFrames(ControlFrames<'static>);

impl<'a> ControlFrames<'a> {
    pub(super) fn new(
        arena: &Arena,
        layout: ProgramLayout,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let mut slots = pool.lease(layout.depth).map_err(execution_full)?;
        for _ in 0..layout.depth {
            slots.push_reserved(None).map_err(execution_full)?;
        }
        let mut resources = pool.lease(layout.depth).map_err(execution_full)?;
        for _ in 0..layout.depth {
            resources
                .push_reserved(FrameResource::Empty)
                .map_err(execution_full)?;
        }
        Ok(Self {
            source: layout.source,
            slots,
            active: 0,
            diagnostics: DiagnosticBuffers::new(pool, layout.handlers)?,
            resources,
            workspace: arena.identity(),
        })
    }

    pub(super) fn capture(&mut self) -> Result<(), SqlError> {
        for resource in &mut self.resources[..self.active] {
            if let FrameResource::Numeric { workspace } = resource {
                workspace.values.capture()?;
            }
        }
        Ok(())
    }

    pub(super) fn discard_saved(&mut self) {
        for resource in &mut self.resources[..self.active] {
            if let FrameResource::Numeric { workspace } = resource {
                workspace.values.discard_saved();
            }
        }
    }

    pub(super) fn suspend(self) -> SuspendedControlFrames {
        SuspendedControlFrames(ControlFrames {
            source: self.source,
            slots: self.slots,
            active: self.active,
            diagnostics: self.diagnostics,
            resources: self.resources.map_in_place(|resource| match resource {
                FrameResource::Empty => FrameResource::Empty,
                FrameResource::Rows(rows) => FrameResource::Rows(rows),
                FrameResource::Numeric { workspace } => FrameResource::Numeric {
                    workspace: workspace.suspend(),
                },
            }),
            workspace: self.workspace,
        })
    }

    fn require_source(&self, source: &ProgramSource) -> Result<(), SqlError> {
        if self.source != *source {
            return Err(program::changed());
        }
        Ok(())
    }

    fn require_workspace(&self, arena: &Arena) -> Result<(), SqlError> {
        if self.workspace != arena.identity() {
            return Err(program::changed());
        }
        Ok(())
    }

    fn execution<'scope, 'syntax>(
        &'scope mut self,
        workspace: &Arena,
        program: &'scope CompiledProgram<'syntax>,
    ) -> Result<FrameExecution<'scope, 'syntax, 'a>, SqlError> {
        self.require_source(program.source())?;
        self.require_workspace(workspace)?;
        Ok(FrameExecution {
            state: self,
            program,
        })
    }
}

impl SuspendedControlFrames {
    pub(super) fn source(&self) -> &ProgramSource {
        &self.0.source
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed restoration returns the original owner without runtime allocation"
    )]
    pub(super) fn restore<'out>(
        self,
        arena: &'out Arena,
        program: &CompiledProgram<'_>,
    ) -> Result<ControlFrames<'out>, (Self, SqlError)> {
        if let Err(error) = self.0.require_source(program.source()) {
            return Err((self, error));
        }
        let mark = arena.mark();
        let mut frames = ControlFrames {
            source: self.0.source,
            slots: self.0.slots,
            active: self.0.active,
            diagnostics: self.0.diagnostics,
            resources: self.0.resources.map_in_place(|resource| match resource {
                FrameResource::Empty => FrameResource::Empty,
                FrameResource::Rows(rows) => FrameResource::Rows(rows),
                FrameResource::Numeric { workspace } => FrameResource::Numeric {
                    workspace: workspace.rebind(),
                },
            }),
            workspace: self.0.workspace,
        };
        for resource in &mut frames.resources[..frames.active] {
            if let FrameResource::Numeric { workspace } = resource
                && let Err(error) = workspace.values.restore(arena)
            {
                let saved = frames.suspend();
                // SAFETY: every partially decoded value has moved to the
                // reference-free suspended state before releasing its scratch.
                unsafe { arena.rewind_to(mark) };
                return Err((saved, error));
            }
        }
        // Images remain intact until the enclosing invocation has restored its
        // ordinary values and firing context too. A later failure can resuspend.
        frames.workspace = arena.identity();
        Ok(frames)
    }
}

struct FrameExecution<'scope, 'syntax, 'values> {
    state: &'scope mut ControlFrames<'values>,
    program: &'scope CompiledProgram<'syntax>,
}

impl<'syntax, 'a> FrameExecution<'_, 'syntax, 'a> {
    fn push(&mut self, entry: FrameEntry<'_, 'a>, source: FrameSource) {
        // Each active frame corresponds to one lexical edge in layout().
        assert!(
            self.state.active < self.state.slots.len(),
            "procedural lexical frame capacity"
        );
        assert!(self.state.slots[self.state.active].is_none());
        assert!(matches!(
            self.state.resources[self.state.active],
            FrameResource::Empty
        ));
        let (continuation, resource) = match entry.state {
            EntryState::Once => (Continuation::Once, FrameResource::Empty),
            EntryState::While => (Continuation::While, FrameResource::Empty),
            EntryState::Loop => (Continuation::Loop, FrameResource::Empty),
            EntryState::Numeric {
                position,
                workspace,
            } => (
                Continuation::Numeric(position),
                FrameResource::Numeric { workspace },
            ),
            EntryState::Rows {
                target,
                values,
                index,
            } => (
                Continuation::Rows { target, index },
                FrameResource::Rows(values),
            ),
            EntryState::Protected(boundary) => (
                Continuation::Protected {
                    boundary,
                    releasing: false,
                },
                FrameResource::Empty,
            ),
            EntryState::Handler => (Continuation::Handler, FrameResource::Empty),
        };
        self.state.resources[self.state.active] = resource;
        self.state.slots[self.state.active] = Some(Frame {
            block: self.program.index(entry.block),
            next: 0,
            source,
            continuation,
            resource: (),
        });
        self.state.active += 1;
    }

    fn start(&mut self, block: TriggerBlock<'_>) {
        assert_eq!(self.state.active, 0);
        self.push(FrameEntry::new(block, EntryState::Once), FrameSource::Root);
    }

    fn enter(&mut self, entry: FrameEntry<'_, 'a>) {
        let parent = self.last_mut();
        let source = FrameSource::Statement(StatementIndex {
            block: parent.block,
            statement: parent.next,
        });
        parent.next += 1;
        self.push(entry, source);
    }

    fn source_statement(&self, source: FrameSource) -> TriggerStatement<'syntax> {
        let FrameSource::Statement(source) = source else {
            unreachable!("frame has no enclosing statement")
        };
        self.program.block(source.block).statements[source.statement]
    }

    fn pop(&mut self) -> Frame<FrameResource<'a>> {
        self.state.active -= 1;
        let position = self.state.slots[self.state.active]
            .take()
            .expect("active procedural frame");
        Frame {
            block: position.block,
            next: position.next,
            source: position.source,
            continuation: position.continuation,
            resource: core::mem::replace(
                &mut self.state.resources[self.state.active],
                FrameResource::Empty,
            ),
        }
    }

    fn last(&self) -> &Frame<()> {
        self.state.slots[self.state.active - 1]
            .as_ref()
            .expect("active procedural frame")
    }

    fn last_mut(&mut self) -> &mut Frame<()> {
        self.state.slots[self.state.active - 1]
            .as_mut()
            .expect("active procedural frame")
    }

    fn scope<'s>(
        &'s mut self,
        locals: &'s [TriggerLocalDecl],
        values: &'s mut [Datum<'a>],
    ) -> (
        &'s [TriggerLocalDecl],
        &'s mut [Datum<'a>],
        Option<CaughtDiagnostic<'s>>,
    ) {
        let diagnostic = self.state.diagnostics.current();
        for (frame, resource) in self.state.slots[..self.state.active]
            .iter()
            .zip(&mut self.state.resources[..self.state.active])
            .rev()
        {
            if let Some(Frame {
                continuation: Continuation::Numeric(scope),
                ..
            }) = frame
            {
                let FrameResource::Numeric { workspace, .. } = resource else {
                    unreachable!("numeric frame resource")
                };
                return (
                    &workspace.locals[..scope.count],
                    &mut workspace.values[..scope.count],
                    diagnostic,
                );
            }
        }
        (locals, values, diagnostic)
    }

    fn diagnostic(&self) -> Option<(CaughtDiagnostic<'_>, PlpgsqlScope)> {
        let scope = self.state.slots[..self.state.active]
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(depth, frame)| frame.as_ref().map(|frame| (depth, frame)))
            .find_map(|(depth, frame)| match frame.continuation {
                Continuation::Handler => Some(PlpgsqlScope::frame(depth + 1)),
                _ => None,
            });
        match (self.state.diagnostics.current(), scope) {
            (None, None) => None,
            (Some(diagnostic), Some(scope)) => Some((diagnostic, scope)),
            _ => unreachable!("handler positions and diagnostic owners share scope order"),
        }
    }

    fn finish(
        &mut self,
        frame: Frame<FrameResource<'a>>,
        locals: &[TriggerLocalDecl],
        values: &mut [Datum<'a>],
        status: &mut TriggerExecutionStatus<'a>,
        success: bool,
    ) {
        match frame.continuation {
            Continuation::Numeric(scope) => {
                let FrameResource::Numeric { mut workspace } = frame.resource else {
                    unreachable!("numeric frame resource")
                };
                let (_, parent_values, _) = self.scope(locals, values);
                // A numeric child changes its own slots. The parent still owns
                // the shadowed value when assignments are copied back.
                if let Some(prior) = parent_values.get(scope.target).copied() {
                    workspace.values[scope.target] = prior;
                }
                parent_values.copy_from_slice(&workspace.values[..parent_values.len()]);
                status.unshadow_output_local(scope.target);
                status.numeric_frames.restore(workspace);
                if success {
                    status.set_found(true);
                }
            }
            Continuation::Rows { .. } if success => status.set_found(true),
            Continuation::Handler => self.state.diagnostics.release(),
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

pub(super) fn execution_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL execution control: {}",
        error
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "numeric entry binds its evaluated range and lexical scope once"
)]
pub(super) fn start_numeric_loop<'syntax, 'a>(
    program: TriggerFor<'syntax>,
    lower: i32,
    upper: i32,
    step: i32,
    reverse: bool,
    counter_scope: PlpgsqlScope,
    locals: &[TriggerLocalDecl],
    values: &[Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'syntax, 'a>, SqlError> {
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
    let mut workspace = status.numeric_frames.take()?;
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
    if let Err(error) = status.shadow_output_local(target, prior_target) {
        status.numeric_frames.restore(workspace);
        return Err(error);
    }
    workspace.values[target] = Datum::Int4(lower);
    Ok(Step::Enter(FrameEntry::new(
        program.block,
        EntryState::Numeric {
            position: NumericLoop {
                count,
                target,
                value: lower,
                upper,
                step,
                reverse,
            },
            workspace,
        },
    )))
}

pub(super) fn start_row_loop<'syntax, 'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    program: TriggerFor<'syntax>,
    rows: super::loop_rows::LoopRows,
    query_source: bool,
    locals: &[TriggerLocalDecl],
    values: &mut [Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'syntax, 'a>, SqlError> {
    if rows.is_empty() {
        let empty = query_source
            .then(|| rows.empty_query_row(context.arena))
            .transpose()?;
        drop(rows);
        if let Some(empty) = empty {
            assign_trigger_local(context, locals, values, program.target, empty)?;
        }
        status.set_found(false);
        return Ok(Step::complete());
    }
    let value = rows.value(0, context.arena)?;
    assign_trigger_local(context, locals, values, program.target, value)?;
    Ok(Step::Enter(FrameEntry::new(
        program.block,
        EntryState::Rows {
            target: program.target,
            index: 0,
            values: rows,
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
    program: &CompiledProgram<'a>,
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Option<TriggerFlow<'a>>, SqlError> {
    let mut retained = status
        .control_frames
        .take()
        .expect("one procedural frame owner");
    let mut frames = retained.execution(context.arena, program)?;
    assert_eq!(frames.state.active, 0);
    frames.start(program.root());
    let mut outcome = Ok(None);
    let mut pending_error = None;
    while frames.state.active != 0 {
        let action = (|| {
            if let Some(error) = pending_error.take() {
                return Err(error);
            }
            super::super::query::check_timeout()?;
            if let Some(statement) = frames
                .program
                .block(frames.last().block)
                .statements
                .get(frames.last().next)
                .copied()
            {
                let invocation = bind_exception_variables(
                    invocation,
                    frames.diagnostic(),
                    statement,
                    context.arena,
                )?;
                let child_scope = PlpgsqlScope::frame(frames.state.active + 1);
                let (scope, scope_values, exception) = frames.scope(locals, values);
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
                        frames.diagnostic(),
                        TriggerStatement::While(program),
                        context.arena,
                    )?;
                    let (scope, scope_values, _) = frames.scope(locals, values);
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
                        let target = range.target;
                        frame.next = 0;
                        let FrameResource::Numeric { workspace, .. } =
                            &mut frames.state.resources[frames.state.active - 1]
                        else {
                            unreachable!("numeric frame resource")
                        };
                        workspace.values[target] = Datum::Int4(next);
                    } else {
                        let frame = frames.pop();
                        frames.finish(frame, locals, values, status, true);
                    }
                }
                Continuation::Rows { target, index } => {
                    let FrameResource::Rows(rows) =
                        &frames.state.resources[frames.state.active - 1]
                    else {
                        unreachable!("row frame resource")
                    };
                    if index + 1 < rows.len() {
                        let value = rows.value(index + 1, context.arena)?;
                        let (scope, scope_values, _) = frames.scope(locals, values);
                        assign_trigger_local(context, scope, scope_values, target, value)?;
                        let frame = frames.last_mut();
                        let Continuation::Rows { index, .. } = &mut frame.continuation else {
                            unreachable!("row-loop frame")
                        };
                        *index += 1;
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
                Continuation::Once | Continuation::Handler => {
                    let frame = frames.pop();
                    frames.finish(frame, locals, values, status, true);
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
                while frames.state.active != 0 {
                    if frames.last().is_loop() {
                        match flow {
                            TriggerFlow::Continue(0) => {
                                frames.last_mut().next =
                                    frames.program.block(frames.last().block).statements.len();
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
                    if frames.state.active == 0 {
                        outcome = Ok(Some(flow));
                    }
                }
            }
            Err(mut error) => {
                let mut handler = None;
                while frames.state.active != 0 {
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
                    frames.finish(frame, locals, values, status, false);
                    if handler.is_some() {
                        break;
                    }
                }
                let Some(handler) = handler else {
                    outcome = Err(error);
                    break;
                };
                match catch_diagnostic(context, invocation, &error, &mut frames.state.diagnostics) {
                    Ok(()) => frames.push(
                        FrameEntry::new(handler.block, EntryState::Handler),
                        FrameSource::Handler,
                    ),
                    Err(error) => pending_error = Some(error),
                }
            }
        }
    }
    // Capacity/error exits release every lexical owner before the caller can
    // rewind statement memory. Transaction rollback remains the caller's job.
    while frames.state.active != 0 {
        let frame = frames.pop();
        frames.finish(frame, locals, values, status, false);
    }
    status.control_frames = Some(retained);
    outcome
}

// Bind snapshots before a child query borrows the remaining arena. Only input
// sites that can resolve the handler variables need independent copies.
fn bind_exception_variables<'a>(
    invocation: TriggerInvocation<'a>,
    diagnostic: Option<(CaughtDiagnostic<'_>, PlpgsqlScope)>,
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
        Ok(invocation.with_exception(diagnostic.variables(arena)?, scope))
    } else {
        Ok(invocation)
    }
}

fn catch_diagnostic(
    context: &mut TriggerExecContext<'_, '_, '_>,
    invocation: TriggerInvocation<'_>,
    error: &SqlError,
    buffers: &mut DiagnosticBuffers,
) -> Result<(), SqlError> {
    if error.sqlstate == sqlstate::QUERY_CANCELED {
        context
            .txn
            .catch_query_cancellation(super::super::datetime::now_micros());
        super::super::sequence::record_caught_cancellation(context.seq_session());
    }
    let diagnostic = crate::sql::eval::take_diagnostic();
    buffers.capture(error, diagnostic.as_ref(), invocation.context().as_str())
}

#[cfg(test)]
mod ownership_tests;
