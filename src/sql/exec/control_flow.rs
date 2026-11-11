//! Procedural positions and lexical scopes belong to bounded execution frames.

use super::diagnostics::{CaughtDiagnostic, DiagnosticBuffers, PendingError};
use super::*;
use crate::mem::arena::ArenaIdentity;

mod program;
use numeric_loops::NumericLoopWorkspace;
use program::ProgramLayout;
pub(super) use program::{CompiledProgram, ProgramSource};

/// What a PL/pgSQL invocation is doing, for its error context line.
#[derive(Clone, Copy)]
pub(super) enum ProceduralActivity<'s> {
    /// Function entry or exit: `PL/pgSQL function f()`.
    Routine,
    /// `... line N at NAME`, where `source` slices the program text.
    Statement { source: &'s str, name: &'static str },
    /// `... [line N] TEXT`, for a phase of the call outside any statement.
    Phase {
        source: Option<&'s str>,
        text: &'static str,
    },
}

/// A PL/pgSQL invocation's error context frame (`plpgsql_exec_error_callback`).
pub(super) struct ProceduralFrame<'p, 's> {
    signature: &'p str,
    program: Option<&'p CompiledProgram<'s>>,
    activity: core::cell::Cell<ProceduralActivity<'s>>,
}

impl<'p, 's> ProceduralFrame<'p, 's> {
    pub(super) fn new(
        signature: &'p str,
        program: Option<&'p CompiledProgram<'s>>,
        activity: ProceduralActivity<'s>,
    ) -> Self {
        Self {
            signature,
            program,
            activity: core::cell::Cell::new(activity),
        }
    }

    pub(super) fn set(&self, activity: ProceduralActivity<'s>) {
        self.activity.set(activity);
    }

    fn line(&self, source: &str) -> Option<usize> {
        self.program.and_then(|program| program.line_of(source))
    }
}

impl crate::sql::error_context::Frame for ProceduralFrame<'_, '_> {
    fn write_line(&self, out: &mut dyn core::fmt::Write, _positioned: bool) -> core::fmt::Result {
        write!(out, "PL/pgSQL function {}", self.signature)?;
        match self.activity.get() {
            ProceduralActivity::Routine => Ok(()),
            ProceduralActivity::Statement { source, name } => match self.line(source) {
                Some(line) => write!(out, " line {line} at {name}"),
                None => Ok(()),
            },
            ProceduralActivity::Phase { source, text } => {
                match source.and_then(|source| self.line(source)) {
                    Some(line) => write!(out, " line {line} {text}"),
                    None => write!(out, " {text}"),
                }
            }
        }
    }

    fn assignment(&self) -> Option<&str> {
        match self.activity.get() {
            ProceduralActivity::Statement {
                source,
                name: "assignment",
            } => Some(source),
            _ => None,
        }
    }
}

/// The function a PL/pgSQL invocation names in its context lines, as
/// PostgreSQL's `format_procedure` writes it: `f(integer)`, schema-qualified
/// when not visible, or `inline_code_block` for `DO`.
pub(super) fn procedural_signature<'a>(
    context: &TriggerExecContext<'_, 'a, '_>,
    invocation: TriggerInvocation<'_>,
) -> Result<&'a str, SqlError> {
    if matches!(invocation.program_kind, PlpgsqlProgramKind::Anonymous) {
        return Ok("inline_code_block");
    }
    if let Some(signature) = crate::sql::catalog::routine_name_by_oid(
        context.storage(),
        context.txn.txid,
        invocation.routine_oid,
        true,
        context.arena,
    )? {
        return Ok(signature);
    }
    // A trigger function whose catalog row this transaction cannot see is
    // named as it was invoked; trigger functions take no arguments.
    context
        .arena
        .alloc_str_display(format_args!(
            "{}.{}()",
            invocation.routine_schema, invocation.routine_name
        ))
        .map_err(|_| super::super::query::arena_full_pub())
}

/// Records the context of an error raised finishing a function call: the
/// missing RETURN (`PL/pgSQL function f()`) or the conversion of its result.
pub(super) fn at_routine_exit<T>(
    context: &TriggerExecContext<'_, '_, '_>,
    invocation: TriggerInvocation<'_>,
    run: impl FnOnce() -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    run().map_err(|error| routine_exit_error(context, invocation, error))
}

/// Adds the function-exit context line of [`at_routine_exit`] to `error`.
pub(super) fn routine_exit_error(
    context: &TriggerExecContext<'_, '_, '_>,
    invocation: TriggerInvocation<'_>,
    error: SqlError,
) -> SqlError {
    if error.sqlstate.internal_control().is_some() {
        return error;
    }
    let signature = match procedural_signature(context, invocation) {
        Ok(signature) => signature,
        Err(signature_error) => return signature_error,
    };
    let activity = if error.sqlstate == sqlstate::FUNCTION_EXECUTED_NO_RETURN_STATEMENT {
        ProceduralActivity::Routine
    } else {
        ProceduralActivity::Phase {
            source: None,
            text: "while casting return value to function's return type",
        }
    };
    let frame = ProceduralFrame::new(signature, None, activity);
    crate::sql::error_context::with_frame(&frame, || crate::sql::error_context::capture(error))
}

/// PostgreSQL's name for a statement in a context line (`plpgsql_stmt_typename`).
pub(super) fn statement_name(statement: TriggerStatement<'_>) -> &'static str {
    match statement {
        TriggerStatement::Null => "NULL",
        TriggerStatement::TransactionControl(PlpgsqlTransactionCommand::Commit(_)) => "COMMIT",
        TriggerStatement::TransactionControl(PlpgsqlTransactionCommand::Rollback(_)) => "ROLLBACK",
        TriggerStatement::Assign(_) | TriggerStatement::LocalAssign(_) => "assignment",
        TriggerStatement::SelectInto(_)
        | TriggerStatement::Utility { .. }
        | TriggerStatement::Dml { .. } => "SQL statement",
        TriggerStatement::Call { .. } => "CALL",
        TriggerStatement::DynamicQuery(_) | TriggerStatement::DynamicSelectInto(_) => "EXECUTE",
        TriggerStatement::CursorOpen(_) => "OPEN",
        TriggerStatement::CursorMotion(motion) if motion.move_only => "MOVE",
        TriggerStatement::CursorMotion(_) => "FETCH",
        TriggerStatement::CursorClose(_) => "CLOSE",
        TriggerStatement::Perform(_) => "PERFORM",
        TriggerStatement::Assert(_) => "ASSERT",
        TriggerStatement::Raise(_) => "RAISE",
        TriggerStatement::GetDiagnostics(_) => "GET DIAGNOSTICS",
        TriggerStatement::GetStackedDiagnostics(_) => "GET STACKED DIAGNOSTICS",
        TriggerStatement::Exception(_) => "statement block",
        // A nested BEGIN ... END block is an always-selected branch.
        TriggerStatement::If(program)
            if matches!(
                program.branches,
                [TriggerBranch {
                    condition: None,
                    ..
                }]
            ) =>
        {
            "statement block"
        }
        TriggerStatement::If(_) => "IF",
        TriggerStatement::Case(_) => "CASE",
        TriggerStatement::For(program) => match program.source {
            TriggerForSource::Numeric { .. } => "FOR with integer loop variable",
            TriggerForSource::Query(_) => "FOR over SELECT rows",
            TriggerForSource::Cursor { .. } => "FOR over cursor",
            TriggerForSource::DynamicQuery(_) => "FOR over EXECUTE statement",
            TriggerForSource::Array { .. } => "FOREACH over array",
        },
        TriggerStatement::While(_) => "WHILE",
        TriggerStatement::Loop(_) => "LOOP",
        TriggerStatement::LoopControl(TriggerLoopControl::Exit { .. }) => "EXIT",
        TriggerStatement::LoopControl(TriggerLoopControl::Continue { .. }) => "CONTINUE",
        TriggerStatement::ReturnNext(_) => "RETURN NEXT",
        TriggerStatement::ReturnQuery(_) | TriggerStatement::ReturnDynamicQuery(_) => {
            "RETURN QUERY"
        }
        TriggerStatement::Return(_) => "RETURN",
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "entry transfers loop and subtransaction owners without heap allocation"
)]
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
    Cursor {
        position: CursorLoop,
        workspace: NumericLoopWorkspace<'a>,
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
                | Continuation::Cursor(_)
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

/// A cursor `FOR` loop's place: its locals extend the enclosing scope's
/// `count` slots with its record `target` and the hidden `portal` slot
/// holding the name of the portal it opened.
#[derive(Clone, Copy)]
struct CursorLoop {
    count: usize,
    target: usize,
    portal: usize,
    /// The cursor variable was NULL before `OPEN` named the portal: the loop
    /// resets it when it ends.
    reset: bool,
    opened: bool,
    /// Whether a row was fetched: `FOUND` once the loop ends.
    iterated: bool,
}

enum FrameResource<'a> {
    Empty,
    /// The locals of a loop that declares its own variable.
    Numeric {
        workspace: NumericLoopWorkspace<'a>,
    },
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
    Cursor(CursorLoop),
    Protected {
        boundary: TriggerSubtransaction,
        releasing: bool,
    },
    Handler,
}

#[expect(
    clippy::large_enum_variant,
    reason = "an unfinished SQL error stays inline without runtime heap allocation"
)]
enum ExecutionProgress<'a> {
    Ready,
    Statements,
    Flow(TriggerFlow<'a>),
    Error(PendingError),
    Complete,
}

impl ExecutionProgress<'_> {
    fn return_value(&self) -> Option<Datum<'_>> {
        match self {
            Self::Flow(TriggerFlow::Return(TriggerReturnValue::Value(value))) => Some(*value),
            _ => None,
        }
    }

    fn rebind<'out>(self) -> ExecutionProgress<'out> {
        match self {
            Self::Ready => ExecutionProgress::Ready,
            Self::Statements => ExecutionProgress::Statements,
            Self::Flow(flow) => ExecutionProgress::Flow(match flow {
                TriggerFlow::Exit(depth) => TriggerFlow::Exit(depth),
                TriggerFlow::Continue(depth) => TriggerFlow::Continue(depth),
                TriggerFlow::Return(value) => TriggerFlow::Return(match value {
                    TriggerReturnValue::New => TriggerReturnValue::New,
                    TriggerReturnValue::Old => TriggerReturnValue::Old,
                    TriggerReturnValue::Null => TriggerReturnValue::Null,
                    TriggerReturnValue::Void => TriggerReturnValue::Void,
                    TriggerReturnValue::Value(_) => TriggerReturnValue::Value(Datum::Null),
                }),
            }),
            Self::Error(error) => ExecutionProgress::Error(error),
            Self::Complete => ExecutionProgress::Complete,
        }
    }
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
    progress: ExecutionProgress<'a>,
    saved_return: Option<local_values::SavedValues>,
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
            progress: ExecutionProgress::Ready,
            saved_return: None,
        })
    }

    pub(super) fn capture(&mut self, rows: &std::sync::Arc<LeasePool>) -> Result<(), SqlError> {
        for resource in &mut self.resources[..self.active] {
            if let FrameResource::Numeric { workspace } = resource {
                workspace.values.capture()?;
            }
        }
        if let Some(value) = self.progress.return_value() {
            let mut saved = local_values::SavedValues::new(rows)?;
            saved.replace(&[value])?;
            self.saved_return = Some(saved);
        }
        Ok(())
    }

    pub(super) fn discard_saved(&mut self) {
        self.saved_return = None;
        for resource in &mut self.resources[..self.active] {
            if let FrameResource::Numeric { workspace } = resource {
                workspace.values.discard_saved();
            }
        }
    }

    pub(super) fn suspend(self) -> SuspendedControlFrames {
        assert_eq!(
            self.progress.return_value().is_some(),
            self.saved_return.is_some(),
            "an unfinished return needs its owned value image"
        );
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
            progress: self.progress.rebind(),
            saved_return: self.saved_return,
        })
    }

    fn require_source(&self, source: &ProgramSource) -> Result<(), SqlError> {
        if self.source != *source {
            return Err(program::changed());
        }
        Ok(())
    }

    pub(super) fn require_workspace(&self, arena: &Arena) -> Result<(), SqlError> {
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
            progress: self.0.progress.rebind(),
            saved_return: self.0.saved_return,
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
        if let Some(saved) = &frames.saved_return {
            let value = match saved.read(0, arena) {
                Ok(value) => value,
                Err(error) => {
                    let saved = frames.suspend();
                    // SAFETY: suspension cleared every partially restored payload.
                    unsafe { arena.rewind_to(mark) };
                    return Err((saved, error));
                }
            };
            let ExecutionProgress::Flow(TriggerFlow::Return(TriggerReturnValue::Value(slot))) =
                &mut frames.progress
            else {
                unreachable!("saved return value belongs to its unfinished return")
            };
            *slot = value;
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
            EntryState::Cursor {
                position,
                workspace,
            } => (
                Continuation::Cursor(position),
                FrameResource::Numeric { workspace },
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
        // A cursor loop opens its portal and fetches before its body runs.
        let next = if matches!(continuation, Continuation::Cursor(_)) {
            entry.block.statements.len()
        } else {
            0
        };
        self.state.slots[self.state.active] = Some(Frame {
            block: self.program.index(entry.block),
            next,
            source,
            continuation,
            resource: (),
        });
        self.state.active += 1;
    }

    fn start(&mut self, block: TriggerBlock<'_>) {
        assert_eq!(self.state.active, 0);
        assert!(matches!(self.state.progress, ExecutionProgress::Ready));
        self.push(FrameEntry::new(block, EntryState::Once), FrameSource::Root);
        self.state.progress = ExecutionProgress::Statements;
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

    /// The context activity of the statement `source` names.
    fn source_activity(&self, source: FrameSource) -> ProceduralActivity<'syntax> {
        let FrameSource::Statement(index) = source else {
            return ProceduralActivity::Routine;
        };
        let block = self.program.block(index.block);
        ProceduralActivity::Statement {
            source: block.sources[index.statement],
            name: statement_name(block.statements[index.statement]),
        }
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

    fn begin_error(
        &mut self,
        error: SqlError,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        let error = crate::sql::error_context::capture(error);
        let cleanup = self.state.slots[..self.state.active]
            .iter()
            .flatten()
            .any(|frame| {
                matches!(
                    frame.continuation,
                    Continuation::Protected {
                        releasing: false,
                        ..
                    }
                )
            });
        if !cleanup {
            // Releasing lexical resources runs no SQL. The original diagnostic
            // can go straight to the caller without reserving a second image.
            return Err(error);
        }
        self.state.progress = ExecutionProgress::Error(PendingError::capture(error, pool));
        Ok(())
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
            let count = match frame {
                Some(Frame {
                    continuation: Continuation::Numeric(scope),
                    ..
                }) => scope.count,
                Some(Frame {
                    continuation: Continuation::Cursor(scope),
                    ..
                }) => scope.count,
                _ => continue,
            };
            let FrameResource::Numeric { workspace, .. } = resource else {
                unreachable!("loop variable frame resource")
            };
            return (
                &workspace.locals[..count],
                &mut workspace.values[..count],
                diagnostic,
            );
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

    /// [`Self::finish`] for a frame leaving the running program: a cursor
    /// loop closes the portal it opened on every exit. After an error, a
    /// savepoint or transaction rollback may already have closed it.
    fn finish_in(
        &mut self,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        frame: Frame<FrameResource<'a>>,
        locals: &[TriggerLocalDecl],
        values: &mut [Datum<'a>],
        status: &mut TriggerExecutionStatus<'a>,
        success: bool,
    ) {
        if let (
            Continuation::Cursor(scope),
            FrameResource::Numeric { workspace },
            PlpgsqlExecHost::Routine { cursors, .. },
        ) = (frame.continuation, &frame.resource, &mut context.host)
            && let Datum::Text(portal) = workspace.values[scope.portal]
        {
            cursors.close_pinned(portal);
        }
        self.finish(frame, locals, values, status, success);
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
            Continuation::Cursor(scope) => {
                let FrameResource::Numeric { mut workspace } = frame.resource else {
                    unreachable!("cursor loop frame resource")
                };
                let TriggerStatement::For(TriggerFor {
                    source: TriggerForSource::Cursor { open, .. },
                    ..
                }) = self.source_statement(frame.source)
                else {
                    unreachable!("cursor loop frame source")
                };
                let (_, parent_values, _) = self.scope(locals, values);
                if scope.reset
                    && let Some(variable) = workspace.locals[..parent_values.len()]
                        .iter()
                        .rposition(|local| local.name == open.variable)
                {
                    workspace.values[variable] = Datum::Null;
                }
                if let Some(prior) = parent_values.get(scope.target).copied() {
                    workspace.values[scope.target] = prior;
                }
                parent_values.copy_from_slice(&workspace.values[..parent_values.len()]);
                status.unshadow_output_local(scope.target);
                status.numeric_frames.restore(workspace);
                if success {
                    status.set_found(scope.iterated);
                }
            }
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

/// Enters a cursor `FOR` loop: its record and portal slots extend the
/// enclosing scope, and its frame opens the cursor before the body runs.
pub(super) fn start_cursor_loop<'syntax, 'a>(
    program: TriggerFor<'syntax>,
    loop_scope: PlpgsqlScope,
    locals: &[TriggerLocalDecl],
    values: &[Datum<'a>],
    status: &mut TriggerExecutionStatus<'a>,
) -> Result<Step<'syntax, 'a>, SqlError> {
    let existing = locals
        .iter()
        .rposition(|local| local.name == program.target);
    let target = existing.unwrap_or(locals.len());
    let portal = locals.len() + usize::from(existing.is_none());
    let count = portal + 1;
    let mut workspace = status.numeric_frames.take()?;
    if count > workspace.locals.len() || count > workspace.values.len() {
        status.numeric_frames.restore(workspace);
        return Err(frame_full());
    }
    for (index, local) in workspace.locals[..count].iter_mut().enumerate() {
        *local = if index == target {
            TriggerLocalDecl {
                scope: loop_scope,
                name: program.target,
                ctype: ColType::Record,
                record_shape: Some(PlpgsqlRecordShape::Dynamic),
                ..empty_plpgsql_local()
            }
        } else if index == portal {
            TriggerLocalDecl {
                scope: loop_scope,
                name: cursor_loop_portal_local(),
                ctype: ColType::Refcursor,
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
    workspace.values[target] = Datum::Null;
    Ok(Step::Enter(FrameEntry::new(
        program.block,
        EntryState::Cursor {
            position: CursorLoop {
                count,
                target,
                portal,
                reset: false,
                opened: false,
                iterated: false,
            },
            workspace,
        },
    )))
}

fn initialize_phase<'s, 'a, 'b, T>(
    status: &mut TriggerExecutionStatus<'a>,
    context: &mut TriggerExecContext<'s, 'a, 'b>,
    evaluate: impl FnOnce(
        &mut TriggerExecContext<'s, 'a, 'b>,
        Option<&mut Option<super::super::query::RoutineInvocationState>>,
    ) -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    let retained = context.txn.write_completion_boundary.is_some()
        && matches!(&context.host, PlpgsqlExecHost::Routine { .. });
    if !retained {
        return evaluate(context, None);
    }
    let position = match status.pending_statement.take() {
        Some(position) => {
            if let Err(((), error)) = position.with_resume(
                (),
                super::super::lock::AdvisoryPositionTail::Appended,
                |(), admission| admission.commit(),
            ) {
                status.pending_statement = Some(position);
                return Err(error);
            }
            position
        }
        None => super::super::sequence::SavedStatementPosition::capture()?,
    };
    let result = evaluate(context, Some(&mut status.pending_query));
    if result.as_ref().is_err_and(is_execution_wait) {
        status.pending_statement = Some(position);
    }
    result
}

impl<'declarations, 'a> execution_frame::ExecutionFrame<'declarations, 'a> {
    pub(super) fn initialize(
        &mut self,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        invocation: TriggerInvocation<'a>,
        transition: TriggerTransition<'_, '_, 'a>,
        program: &CompiledProgram<'a>,
        seeded: &[Datum<'a>],
    ) -> Result<(), SqlError> {
        if self.initialization == execution_frame::Initialization::Complete {
            return Ok(());
        }
        let signature = procedural_signature(context, invocation)?;
        let frame = ProceduralFrame::new(signature, Some(program), ProceduralActivity::Routine);
        crate::sql::error_context::within(&frame, || {
            self.initialize_locals(&frame, context, invocation, transition, program, seeded)
        })
    }

    fn initialize_locals(
        &mut self,
        context_frame: &ProceduralFrame<'_, 'a>,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        invocation: TriggerInvocation<'a>,
        transition: TriggerTransition<'_, '_, 'a>,
        program: &CompiledProgram<'a>,
        seeded: &[Datum<'a>],
    ) -> Result<(), SqlError> {
        use execution_frame::Initialization;
        let frames = self
            .status
            .control_frames
            .as_ref()
            .expect("one procedural frame owner");
        frames.require_source(program.source())?;
        frames.require_workspace(context.arena)?;
        let declaration_start = self
            .locals
            .len()
            .checked_sub(program.declarations.len())
            .expect("local initialization layout");
        assert!(seeded.len() <= declaration_start, "local parameter layout");
        loop {
            let index = match self.initialization {
                Initialization::Complete => return Ok(()),
                Initialization::Value(index) | Initialization::Coerce(index) => index,
            };
            if index == self.locals.len() {
                self.initialization = Initialization::Complete;
                return Ok(());
            }
            let local = self.locals[index];
            context_frame.set(match index.checked_sub(declaration_start) {
                Some(declaration) => ProceduralActivity::Phase {
                    source: Some(program.declarations[declaration].source),
                    text: "during statement block local variable initialization",
                },
                None => ProceduralActivity::Phase {
                    source: None,
                    text: "while storing call arguments into local variables",
                },
            });
            if matches!(self.initialization, Initialization::Value(_)) {
                let value = if let Some(value) = seeded.get(index).copied() {
                    Some(value)
                } else {
                    self.values[index] = Datum::Null;
                    if index < declaration_start {
                        None
                    } else if let Some(initial) =
                        program.declarations[index - declaration_start].initial
                    {
                        let scope = TriggerLocalScope {
                            locals: &self.locals[..=index],
                            values: &self.values[..=index],
                            found: Some(false),
                            invocation,
                            transition: &transition,
                        };
                        Some(initialize_phase(
                            &mut self.status,
                            context,
                            |context, continuation| {
                                eval_procedural_expression(context, initial, &scope, continuation)
                            },
                        )?)
                    } else {
                        // A declared domain can reject its implicit NULL just
                        // as it rejects an explicitly supplied initial value.
                        Some(Datum::Null)
                    }
                };
                let Some(value) = value else {
                    self.initialization = Initialization::Value(index + 1);
                    continue;
                };
                // Preserve an evaluated expression before a conversion can
                // yield. Resuming conversion must not repeat that expression.
                self.values[index] = value;
                self.initialization = Initialization::Coerce(index);
            }
            let value = self.values[index];
            self.values[index] =
                initialize_phase(&mut self.status, context, |context, continuation| {
                    let txid = context.txn.txid;
                    let arena = context.arena;
                    execute_procedural_query(context, false, continuation, |storage, sequences| {
                        coerce_plpgsql_local(local, value, storage, txid, arena, sequences)
                    })
                })?;
            self.initialization = Initialization::Value(index + 1);
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the interpreter retains the complete typed firing context"
    )]
    pub(super) fn execute_block(
        &mut self,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        definition: crate::storage::RelationDefinition<'_>,
        invocation: TriggerInvocation<'a>,
        old: Option<&[Datum<'a>]>,
        new: &mut Option<&mut [Datum<'a>]>,
        before: bool,
        transition_relations: Option<&'a [(&'a str, &'a crate::sql::ast::MaterializedCte<'a>)]>,
        program: &CompiledProgram<'a>,
    ) -> Result<Option<TriggerFlow<'a>>, SqlError> {
        let signature = procedural_signature(context, invocation)?;
        let frame = ProceduralFrame::new(signature, Some(program), ProceduralActivity::Routine);
        crate::sql::error_context::with_frame(&frame, || {
            self.run_statements(
                &frame,
                context,
                definition,
                invocation,
                old,
                new,
                before,
                transition_relations,
                program,
            )
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the interpreter retains the complete typed firing context"
    )]
    fn run_statements(
        &mut self,
        context_frame: &ProceduralFrame<'_, 'a>,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        definition: crate::storage::RelationDefinition<'_>,
        invocation: TriggerInvocation<'a>,
        old: Option<&[Datum<'a>]>,
        new: &mut Option<&mut [Datum<'a>]>,
        before: bool,
        transition_relations: Option<&'a [(&'a str, &'a crate::sql::ast::MaterializedCte<'a>)]>,
        program: &CompiledProgram<'a>,
    ) -> Result<Option<TriggerFlow<'a>>, SqlError> {
        if self.initialization != execution_frame::Initialization::Complete {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "procedural execution started before local initialization completed"
            ));
        }
        let locals = &self.locals;
        let values = &mut self.values;
        let status = &mut self.status;
        let mut retained = status
            .control_frames
            .take()
            .expect("one procedural frame owner");
        let mut frames = match retained.execution(context.arena, program) {
            Ok(frames) => frames,
            Err(error) => {
                status.control_frames = Some(retained);
                return Err(error);
            }
        };
        if matches!(frames.state.progress, ExecutionProgress::Ready) {
            frames.start(program.root());
        } else if matches!(frames.state.progress, ExecutionProgress::Complete) {
            status.control_frames = Some(retained);
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a completed procedural invocation cannot run again"
            ));
        }
        let mut outcome = Ok(None);
        let mut paused = false;
        'execute: while frames.state.active != 0 {
            match core::mem::replace(&mut frames.state.progress, ExecutionProgress::Statements) {
                ExecutionProgress::Flow(mut flow) => {
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
                                    frames.finish_in(context, frame, locals, values, status, true);
                                    break;
                                }
                                TriggerFlow::Continue(depth) => {
                                    flow = TriggerFlow::Continue(depth - 1)
                                }
                                TriggerFlow::Exit(depth) => flow = TriggerFlow::Exit(depth - 1),
                                TriggerFlow::Return(_) => {}
                            }
                        }
                        context_frame.set(frames.source_activity(frames.last().source));
                        if let Continuation::Protected {
                            boundary,
                            releasing,
                        } = &mut frames.last_mut().continuation
                        {
                            *releasing = true;
                            if let Err(error) = release_trigger_subtransaction(context, boundary) {
                                if is_execution_wait(&error) {
                                    frames.state.progress = ExecutionProgress::Flow(flow);
                                    outcome = Err(error);
                                    paused = true;
                                    break 'execute;
                                }
                                if let Err(error) =
                                    frames.begin_error(error, &context.storage().execution_control)
                                {
                                    outcome = Err(error);
                                    break 'execute;
                                }
                                break;
                            }
                        }
                        let frame = frames.pop();
                        frames.finish_in(context, frame, locals, values, status, true);
                        if frames.state.active == 0 {
                            outcome = Ok(Some(flow));
                        }
                    }
                    continue;
                }
                ExecutionProgress::Error(error) => {
                    let mut handler = None;
                    while frames.state.active != 0 {
                        if let Continuation::Protected {
                            boundary,
                            releasing: false,
                        } = frames.last().continuation
                        {
                            match rollback_trigger_subtransaction(context, &boundary) {
                                Ok(()) => {
                                    let TriggerStatement::Exception(program) =
                                        frames.source_statement(frames.last().source)
                                    else {
                                        unreachable!("exception frame source")
                                    };
                                    handler = program.handlers.iter().find(|handler| {
                                        handler.conditions.iter().copied().any(|condition| {
                                            trigger_exception_matches(condition, &error.error)
                                        })
                                    });
                                }
                                Err(wait) if is_execution_wait(&wait) => {
                                    // The protected frame and original diagnostic
                                    // remain owned until rollback actually finishes.
                                    frames.state.progress = ExecutionProgress::Error(error);
                                    outcome = Err(wait);
                                    paused = true;
                                    break 'execute;
                                }
                                Err(rollback_error) => {
                                    let frame = frames.pop();
                                    frames.finish_in(context, frame, locals, values, status, false);
                                    drop(error);
                                    if let Err(error) = frames.begin_error(
                                        rollback_error,
                                        &context.storage().execution_control,
                                    ) {
                                        outcome = Err(error);
                                        break 'execute;
                                    }
                                    continue 'execute;
                                }
                            }
                        }
                        let frame = frames.pop();
                        frames.finish_in(context, frame, locals, values, status, false);
                        if handler.is_some() {
                            break;
                        }
                    }
                    let Some(handler) = handler else {
                        outcome = Err(error.publish());
                        break;
                    };
                    match catch_diagnostic(context, error, &mut frames.state.diagnostics) {
                        Ok(()) => frames.push(
                            FrameEntry::new(handler.block, EntryState::Handler),
                            FrameSource::Handler,
                        ),
                        Err(error) => {
                            if let Err(error) =
                                frames.begin_error(error, &context.storage().execution_control)
                            {
                                outcome = Err(error);
                                break;
                            }
                        }
                    }
                    continue;
                }
                ExecutionProgress::Statements => {}
                ExecutionProgress::Ready | ExecutionProgress::Complete => {
                    unreachable!("active procedural execution position")
                }
            }
            let action = (|| {
                super::super::query::check_timeout()?;
                if let Some(statement) = frames
                    .program
                    .block(frames.last().block)
                    .statements
                    .get(frames.last().next)
                    .copied()
                {
                    context_frame.set(ProceduralActivity::Statement {
                        source: frames.program.block(frames.last().block).sources
                            [frames.last().next],
                        name: statement_name(statement),
                    });
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
                context_frame.set(frames.source_activity(frames.last().source));
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
                        let found = status.found;
                        // The repeated check is its own statement boundary.
                        if initialize_phase(status, context, |context, continuation| {
                            trigger_condition(
                                context,
                                condition,
                                definition,
                                invocation,
                                old,
                                new.as_deref(),
                                scope,
                                scope_values,
                                found,
                                continuation,
                            )
                        })? {
                            frames.last_mut().next = 0;
                        } else {
                            let frame = frames.pop();
                            frames.finish_in(context, frame, locals, values, status, true);
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
                            frames.finish_in(context, frame, locals, values, status, true);
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
                            frames.finish_in(context, frame, locals, values, status, true);
                        }
                    }
                    Continuation::Cursor(state) => {
                        let TriggerStatement::For(TriggerFor {
                            source: TriggerForSource::Cursor { open, fetch },
                            ..
                        }) = frames.source_statement(frames.last().source)
                        else {
                            unreachable!("cursor loop frame source")
                        };
                        let child_scope = PlpgsqlScope::frame(frames.state.active + 1);
                        if !state.opened {
                            let statement = TriggerStatement::CursorOpen(open);
                            let invocation = bind_exception_variables(
                                invocation,
                                frames.diagnostic(),
                                statement,
                                context.arena,
                            )?;
                            let (scope, scope_values, exception) = frames.scope(locals, values);
                            let variable = trigger_local_index(scope, open.variable)?;
                            let reset = scope_values[variable].is_null();
                            execute_trigger_step(
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
                            )?;
                            let Datum::Text(portal) = scope_values[variable] else {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "cursor FOR loop opened no portal"
                                ));
                            };
                            if let PlpgsqlExecHost::Routine { cursors, .. } = &mut context.host {
                                cursors.pin(portal)?;
                            }
                            scope_values[state.portal] = Datum::Text(portal);
                            let Continuation::Cursor(state) = &mut frames.last_mut().continuation
                            else {
                                unreachable!("cursor loop frame")
                            };
                            state.opened = true;
                            state.reset = reset;
                        }
                        // The loop leaves FOUND alone until it ends.
                        let found = status.found;
                        let (scope, scope_values, exception) = frames.scope(locals, values);
                        execute_trigger_step(
                            context,
                            definition,
                            invocation,
                            old,
                            new,
                            before,
                            transition_relations,
                            scope,
                            scope_values,
                            TriggerStatement::CursorMotion(fetch),
                            child_scope,
                            status,
                            exception,
                        )?;
                        let fetched = status.found == Some(true);
                        status.found = found;
                        if fetched {
                            let frame = frames.last_mut();
                            let Continuation::Cursor(state) = &mut frame.continuation else {
                                unreachable!("cursor loop frame")
                            };
                            state.iterated = true;
                            frame.next = 0;
                        } else {
                            let frame = frames.pop();
                            frames.finish_in(context, frame, locals, values, status, true);
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
                        frames.finish_in(context, frame, locals, values, status, true);
                    }
                    Continuation::Once | Continuation::Handler => {
                        let frame = frames.pop();
                        frames.finish_in(context, frame, locals, values, status, true);
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
                Ok(Some(Step::Complete(Some(flow)))) => {
                    frames.state.progress = ExecutionProgress::Flow(flow);
                }
                Err(error) if is_execution_wait(&error) => {
                    outcome = Err(error);
                    paused = true;
                    break;
                }
                Err(error) => {
                    if let Err(error) =
                        frames.begin_error(error, &context.storage().execution_control)
                    {
                        outcome = Err(error);
                        break;
                    }
                }
            }
        }
        // Terminal exits release lexical owners before statement memory reuse.
        // A private wait keeps those scopes for the caller's capture boundary.
        // Transaction rollback remains the caller's job.
        if !paused {
            while frames.state.active != 0 {
                let frame = frames.pop();
                frames.finish_in(context, frame, locals, values, status, false);
            }
            frames.state.progress = ExecutionProgress::Complete;
        }
        status.control_frames = Some(retained);
        outcome
    }
}

pub(super) fn is_execution_wait(error: &SqlError) -> bool {
    matches!(
        error.sqlstate.internal_control(),
        Some(
            crate::sql::eval::InternalControl::LockWait
                | crate::sql::eval::InternalControl::IoWait
                | crate::sql::eval::InternalControl::RoutineInvocation
        )
    )
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
        | TriggerStatement::Call { source, .. }
        | TriggerStatement::Dml { source, .. } => site(Kind::Statement, source.as_ptr() as usize),
        TriggerStatement::CursorOpen(PlpgsqlCursorOpen {
            query: PlpgsqlCursorQuery::Static(source),
            arguments,
            ..
        }) => {
            site(Kind::Statement, source.as_ptr() as usize)
                || arguments.iter().any(|argument| expression(argument.value))
        }
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
            // The loop's OPEN binds them when it runs.
            TriggerForSource::Cursor { .. } => false,
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
    error: PendingError,
    buffers: &mut DiagnosticBuffers,
) -> Result<(), SqlError> {
    if error.error.sqlstate == sqlstate::QUERY_CANCELED {
        context
            .txn
            .catch_query_cancellation(super::super::datetime::now_micros());
        super::super::sequence::record_caught_cancellation(context.seq_session());
    }
    let _ = crate::sql::eval::take_diagnostic();
    error.catch(buffers)
}

#[cfg(test)]
mod ownership_tests;

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod resumption_tests;
