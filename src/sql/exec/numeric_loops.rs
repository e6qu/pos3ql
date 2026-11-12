//! Numeric loop locals use one reusable frame per active nesting level.

use super::local_values::LocalValues;
use super::*;

pub(super) struct NumericLoopWorkspace<'a> {
    depth: usize,
    pub(super) locals: MutableSlice<TriggerLocalDecl>,
    pub(super) values: LocalValues<'a>,
}

impl NumericLoopWorkspace<'_> {
    pub(super) fn suspend(self) -> NumericLoopWorkspace<'static> {
        NumericLoopWorkspace {
            depth: self.depth,
            locals: self.locals,
            values: self.values.suspend(),
        }
    }
}

impl NumericLoopWorkspace<'static> {
    pub(super) fn rebind<'out>(self) -> NumericLoopWorkspace<'out> {
        NumericLoopWorkspace {
            depth: self.depth,
            locals: self.locals,
            values: self.values.rebind_saved(),
        }
    }
}

pub(super) struct NumericLoopFrames<'a> {
    slots: MutableSlice<Option<NumericLoopWorkspace<'a>>>,
    active: usize,
}

impl<'a> NumericLoopFrames<'a> {
    pub(super) fn new(
        arena: &Arena,
        block: TriggerBlock<'_>,
        locals: &[TriggerLocalDecl],
        pool: &std::sync::Arc<LeasePool>,
        rows: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let depth = numeric_depth(block);
        if depth == 0 {
            return Ok(Self {
                slots: pool.lease(0).map_err(control_flow::execution_full)?,
                active: 0,
            });
        }
        arena
            .with_remaining_region(|layout| {
                let counts = layout
                    .alloc_slice_with(depth, |_| 0usize)
                    .map_err(|_| frame_full())?;
                frame_layout(block, locals.len(), 0, counts);
                let mut slots = pool.lease(depth).map_err(control_flow::execution_full)?;
                for (depth, count) in counts.iter().copied().enumerate() {
                    let mut locals = pool.lease(count).map_err(control_flow::execution_full)?;
                    for _ in 0..count {
                        locals
                            .push_reserved(empty_plpgsql_local())
                            .map_err(control_flow::execution_full)?;
                    }
                    let values = LocalValues::new(pool, rows, count)?;
                    slots
                        .push_reserved(Some(NumericLoopWorkspace {
                            depth,
                            locals,
                            values,
                        }))
                        .map_err(control_flow::execution_full)?;
                }
                Ok(Self { slots, active: 0 })
            })
            .map_err(|_| frame_full())?
    }

    pub(super) fn suspend(self) -> NumericLoopFrames<'static> {
        NumericLoopFrames {
            slots: self.slots.map_in_place(|slot| {
                slot.map(|workspace| NumericLoopWorkspace {
                    depth: workspace.depth,
                    locals: workspace.locals,
                    values: workspace.values.suspend_unused(),
                })
            }),
            active: self.active,
        }
    }

    pub(super) fn take(&mut self) -> Result<NumericLoopWorkspace<'a>, SqlError> {
        let workspace = self
            .slots
            .get_mut(self.active)
            .and_then(Option::take)
            .ok_or_else(frame_full)?;
        self.active += 1;
        Ok(workspace)
    }

    pub(super) fn restore(&mut self, mut workspace: NumericLoopWorkspace<'a>) {
        let depth = workspace.depth;
        assert_eq!(self.active, depth + 1, "numeric frame stack order");
        assert!(self.slots[depth].is_none());
        // Inactive values cannot retain references into a completed body's
        // temporary arena suffix. The scope owns both of its buffer ranges.
        workspace.locals.fill(empty_plpgsql_local());
        workspace.values.fill(Datum::Null);
        self.active -= 1;
        self.slots[depth] = Some(workspace);
    }
}

impl NumericLoopFrames<'static> {
    pub(super) fn rebind<'out>(self) -> NumericLoopFrames<'out> {
        NumericLoopFrames {
            slots: self.slots.map_in_place(|slot| {
                slot.map(|workspace| NumericLoopWorkspace {
                    depth: workspace.depth,
                    locals: workspace.locals,
                    values: workspace.values.rebind_unused(),
                })
            }),
            active: self.active,
        }
    }
}

fn frame_layout(block: TriggerBlock<'_>, count: usize, depth: usize, counts: &mut [usize]) {
    visit_nested_blocks(block, |block, edge| match edge.declared() {
        // Every counter or declaration is a new slot after the enclosing
        // variables, so `label.name` still reaches a shadowed one.
        0 => frame_layout(block, count, depth, counts),
        declared => {
            counts[depth] = counts[depth].max(count + declared);
            frame_layout(block, count + declared, depth + 1, counts);
        }
    });
}

fn frame_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL numeric loop frames exceed statement memory"
    )
}

fn numeric_depth(block: TriggerBlock<'_>) -> usize {
    let mut depth = 0;
    visit_nested_blocks(block, |block, edge| {
        depth = depth.max(numeric_depth(block) + usize::from(edge.declared() != 0));
    });
    depth
}

#[derive(Clone, Copy)]
pub(super) enum BlockEdge<'a> {
    Ordinary,
    Numeric,
    /// A nested block that declares variables.
    Scope(&'a [ParsedTriggerLocalDecl<'a>]),
    Handler,
}

impl BlockEdge<'_> {
    /// How many variables entering the block declares.
    fn declared(self) -> usize {
        match self {
            Self::Numeric => 1,
            Self::Scope(declarations) => declarations.len(),
            Self::Ordinary | Self::Handler => 0,
        }
    }
}

// Layout passes share this exhaustive set of lexical block edges.
pub(super) fn visit_nested_blocks<'a>(
    block: TriggerBlock<'a>,
    mut visit: impl FnMut(TriggerBlock<'a>, BlockEdge<'a>),
) {
    for statement in block.statements {
        match statement {
            TriggerStatement::For(program) => visit(
                program.block,
                if matches!(program.source, TriggerForSource::Numeric { .. }) {
                    BlockEdge::Numeric
                } else {
                    BlockEdge::Ordinary
                },
            ),
            TriggerStatement::Block(program) => visit(
                program.block,
                if program.declarations.is_empty() {
                    BlockEdge::Ordinary
                } else {
                    BlockEdge::Scope(program.declarations)
                },
            ),
            TriggerStatement::If(program) => {
                for branch in program.branches {
                    visit(branch.block, BlockEdge::Ordinary);
                }
            }
            TriggerStatement::Case(program) => {
                for branch in program.branches {
                    visit(branch.block, BlockEdge::Ordinary);
                }
                if let TriggerCaseElse::Block(block) = program.otherwise {
                    visit(block, BlockEdge::Ordinary);
                }
            }
            TriggerStatement::Exception(program) => {
                visit(program.protected, BlockEdge::Ordinary);
                for handler in program.handlers {
                    visit(handler.block, BlockEdge::Handler);
                }
            }
            TriggerStatement::While(program) => visit(program.block, BlockEdge::Ordinary),
            TriggerStatement::Loop(block) => visit(*block, BlockEdge::Ordinary),
            TriggerStatement::Null
            | TriggerStatement::TransactionControl(_)
            | TriggerStatement::LocalAssign(_)
            | TriggerStatement::SelectInto(_)
            | TriggerStatement::DynamicQuery(_)
            | TriggerStatement::DynamicSelectInto(_)
            | TriggerStatement::Utility { .. }
            | TriggerStatement::CursorOpen(_)
            | TriggerStatement::CursorMotion(_)
            | TriggerStatement::CursorClose(_)
            | TriggerStatement::Perform(_)
            | TriggerStatement::Assert(_)
            | TriggerStatement::Raise(_)
            | TriggerStatement::GetDiagnostics(_)
            | TriggerStatement::GetStackedDiagnostics(_)
            | TriggerStatement::Dml { .. }
            | TriggerStatement::LoopControl(_)
            | TriggerStatement::ReturnNext(_)
            | TriggerStatement::ReturnQuery(_)
            | TriggerStatement::ReturnDynamicQuery(_)
            | TriggerStatement::Return(_) => {}
        }
    }
}

#[cfg(test)]
mod ownership_tests;
