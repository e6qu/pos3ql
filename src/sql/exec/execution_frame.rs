//! Lexical positions, declared types and live values share one execution owner.

use super::*;

pub(super) struct ExecutionFrame<'a> {
    pub(super) locals: MutableSlice<TriggerLocalDecl>,
    pub(super) values: local_values::LocalValues<'a>,
    pub(super) status: TriggerExecutionStatus<'a>,
}

pub(super) struct SuspendedExecutionFrame {
    locals: MutableSlice<TriggerLocalDecl>,
    values: local_values::LocalValues<'static>,
    numeric_frames: NumericLoopFrames<'static>,
    control_frames: control_flow::SuspendedControlFrames,
    found: Option<bool>,
    row_count: i64,
    set_result: Option<PlpgsqlSetResult>,
    outputs: MutableSlice<OutputBinding>,
}

impl<'a> ExecutionFrame<'a> {
    pub(super) fn new(
        arena: &Arena,
        program: &CompiledProgram<'_>,
        declarations: &[TriggerLocalDecl],
        output_count: usize,
        controls: &std::sync::Arc<LeasePool>,
        rows: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let mut locals = controls
            .lease(declarations.len())
            .map_err(control_flow::execution_full)?;
        locals
            .extend_from_slice(declarations)
            .map_err(control_flow::execution_full)?;
        Ok(Self {
            values: local_values::LocalValues::new(controls, rows, locals.len())?,
            status: TriggerExecutionStatus::new(
                arena,
                program,
                &locals,
                output_count,
                controls,
                rows,
            )?,
            locals,
        })
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed capture returns the live execution owner without allocating"
    )]
    pub(super) fn suspend(mut self) -> Result<SuspendedExecutionFrame, (Self, SqlError)> {
        let captured = self.values.capture().and_then(|()| {
            self.status
                .control_frames
                .as_mut()
                .expect("one procedural frame owner")
                .capture()
        });
        if let Err(error) = captured {
            return Err((self, error));
        }
        let TriggerExecutionStatus {
            numeric_frames,
            control_frames,
            found,
            row_count,
            set_result,
            outputs,
        } = self.status;
        Ok(SuspendedExecutionFrame {
            locals: self.locals,
            values: self.values.suspend(),
            numeric_frames: numeric_frames.suspend(),
            control_frames: control_frames
                .expect("one procedural frame owner")
                .suspend(),
            found,
            row_count,
            set_result,
            outputs,
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
    ) -> Result<ExecutionFrame<'out>, (Self, SqlError)> {
        let Self {
            locals,
            values,
            numeric_frames,
            control_frames,
            found,
            row_count,
            set_result,
            outputs,
        } = self;
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
                    },
                    error,
                ));
            }
        };
        let mut values = values.rebind_saved();
        if let Err(error) = values.restore(arena) {
            return Err((
                Self {
                    locals,
                    values: values.suspend(),
                    numeric_frames,
                    control_frames: control_frames.suspend(),
                    found,
                    row_count,
                    set_result,
                    outputs,
                },
                error,
            ));
        }
        // Publish only after every group has decoded. A failure above can clear
        // partial references without recapturing or losing the original images.
        values.discard_saved();
        control_frames.discard_saved();
        Ok(ExecutionFrame {
            locals,
            values,
            status: TriggerExecutionStatus {
                numeric_frames: numeric_frames.rebind(),
                control_frames: Some(control_frames),
                found,
                row_count,
                set_result,
                outputs,
            },
        })
    }
}
