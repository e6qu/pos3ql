//! Lexical positions, declared types and live values share one execution owner.

use super::*;
use crate::mem::lease_pool::SharedSlice;

pub(super) struct ExecutionFrame<'declarations, 'values> {
    pub(super) locals: LocalDeclarations<'declarations>,
    pub(super) values: local_values::LocalValues<'values>,
    pub(super) status: TriggerExecutionStatus<'values>,
    pub(super) initialization: Initialization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Initialization {
    Value(usize),
    Coerce(usize),
    Complete,
}

pub(super) enum LocalDeclarations<'a> {
    Borrowed {
        values: &'a [TriggerLocalDecl],
        controls: std::sync::Arc<LeasePool>,
    },
    Owned(SharedSlice<TriggerLocalDecl>),
}

impl LocalDeclarations<'_> {
    fn capture(&self) -> Result<SharedSlice<TriggerLocalDecl>, SqlError> {
        match self {
            Self::Borrowed { values, controls } => controls
                .copy_slice(values)
                .map_err(control_flow::execution_full),
            Self::Owned(values) => Ok(values.clone()),
        }
    }
}

impl core::ops::Deref for LocalDeclarations<'_> {
    type Target = [TriggerLocalDecl];
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed { values, .. } => values,
            Self::Owned(values) => values,
        }
    }
}

pub(super) struct SuspendedExecutionFrame {
    locals: SharedSlice<TriggerLocalDecl>,
    values: local_values::LocalValues<'static>,
    numeric_frames: NumericLoopFrames<'static>,
    control_frames: control_flow::SuspendedControlFrames,
    found: Option<bool>,
    row_count: i64,
    set_result: Option<PlpgsqlSetResult>,
    outputs: MutableSlice<OutputBinding>,
    pending_write: Option<write_completion::PendingNestedWrite>,
    pending_statement: Option<super::super::sequence::SavedStatementPosition>,
    initialization: Initialization,
}

impl<'declarations, 'values> ExecutionFrame<'declarations, 'values> {
    pub(super) fn new(
        arena: &Arena,
        program: &CompiledProgram<'_>,
        declarations: &'declarations [TriggerLocalDecl],
        output_count: usize,
        controls: &std::sync::Arc<LeasePool>,
        rows: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            values: local_values::LocalValues::new(controls, rows, declarations.len())?,
            status: TriggerExecutionStatus::new(
                arena,
                program,
                declarations,
                output_count,
                controls,
                rows,
            )?,
            locals: LocalDeclarations::Borrowed {
                values: declarations,
                controls: std::sync::Arc::clone(controls),
            },
            initialization: Initialization::Value(0),
        })
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed capture returns the live execution owner without allocating"
    )]
    pub(super) fn suspend(mut self) -> Result<SuspendedExecutionFrame, (Self, SqlError)> {
        // Normal calls keep using their compiled declarations. Only an actual
        // pause needs an independent metadata image; resumed calls share it.
        let locals = match self.locals.capture() {
            Ok(locals) => locals,
            Err(error) => return Err((self, error)),
        };
        let captured = self.values.capture().and_then(|()| {
            self.status
                .control_frames
                .as_mut()
                .expect("one procedural frame owner")
                .capture(self.values.row_pool())
        });
        if let Err(error) = captured {
            self.values.discard_saved();
            self.status
                .control_frames
                .as_mut()
                .expect("one procedural frame owner")
                .discard_saved();
            return Err((self, error));
        }
        let TriggerExecutionStatus {
            numeric_frames,
            control_frames,
            found,
            row_count,
            set_result,
            outputs,
            pending_write,
            pending_statement,
        } = self.status;
        Ok(SuspendedExecutionFrame {
            locals,
            values: self.values.suspend(),
            numeric_frames: numeric_frames.suspend(),
            control_frames: control_frames
                .expect("one procedural frame owner")
                .suspend(),
            found,
            row_count,
            set_result,
            outputs,
            pending_write,
            pending_statement,
            initialization: self.initialization,
        })
    }
}

impl SuspendedExecutionFrame {
    pub(super) fn source(&self) -> &ProgramSource {
        self.control_frames.source()
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed restoration retains every image and reservation without allocating"
    )]
    pub(super) fn restore<'out>(
        self,
        arena: &'out Arena,
        program: &CompiledProgram<'_>,
    ) -> Result<ExecutionFrame<'out, 'out>, (Self, SqlError)> {
        let Self {
            locals,
            values,
            numeric_frames,
            control_frames,
            found,
            row_count,
            set_result,
            outputs,
            pending_write,
            pending_statement,
            initialization,
        } = self;
        let mark = arena.mark();
        let mut control_frames = match control_frames.restore(arena, program) {
            Ok(frames) => frames,
            Err((control_frames, error)) => {
                return Err((
                    Self {
                        locals,
                        values,
                        numeric_frames,
                        control_frames,
                        found,
                        row_count,
                        set_result,
                        outputs,
                        pending_write,
                        pending_statement,
                        initialization,
                    },
                    error,
                ));
            }
        };
        let mut values = values.rebind_saved();
        if let Err(error) = values.restore(arena) {
            let saved = Self {
                locals,
                values: values.suspend(),
                numeric_frames,
                control_frames: control_frames.suspend(),
                found,
                row_count,
                set_result,
                outputs,
                pending_write,
                pending_statement,
                initialization,
            };
            // SAFETY: suspension removed all references created while decoding.
            // Only the original owned images escape this failed restoration.
            unsafe { arena.rewind_to(mark) };
            return Err((saved, error));
        }
        // Publish only after every group has decoded. A failure above can clear
        // partial references without recapturing or losing the original images.
        values.discard_saved();
        control_frames.discard_saved();
        Ok(ExecutionFrame {
            locals: LocalDeclarations::Owned(locals),
            values,
            status: TriggerExecutionStatus {
                numeric_frames: numeric_frames.rebind(),
                control_frames: Some(control_frames),
                found,
                row_count,
                set_result,
                outputs,
                pending_write,
                pending_statement,
            },
            initialization,
        })
    }
}
