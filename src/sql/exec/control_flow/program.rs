//! Compiled syntax borrows parsing storage; source identity and layout do not.

use super::super::numeric_loops::{self, BlockEdge};
use super::*;
use crate::mem::arena::FrozenArenaSliceHandle;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::sql::exec) struct ProgramSource {
    bytes: FrozenArenaSliceHandle<u8>,
    kind: PlpgsqlProgramKind,
}

impl ProgramSource {
    pub(in crate::sql::exec) fn capture(
        text: &str,
        kind: PlpgsqlProgramKind,
        arena: &Arena,
    ) -> Result<Self, SqlError> {
        let bytes = text.as_bytes();
        let handle = arena
            .retain_slice_with(bytes.len(), |index| bytes[index])
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "PL/pgSQL source image exceeds statement memory"
                )
            })?;
        Ok(Self {
            bytes: arena.freeze_retained_slice(handle).map_err(|_| changed())?,
            kind,
        })
    }

    fn text(self, arena: &Arena) -> Result<&str, SqlError> {
        let bytes = arena.retained_slice(self.bytes).map_err(|_| changed())?;
        core::str::from_utf8(bytes).map_err(|_| changed())
    }

    pub(super) fn require(self, arena: &Arena) -> Result<(), SqlError> {
        arena
            .retained_slice(self.bytes)
            .map(|_| ())
            .map_err(|_| changed())
    }

    pub(in crate::sql::exec) fn parse<'a>(
        self,
        source: &'a Arena,
        parsing: &'a Arena,
    ) -> Result<(ParsedTriggerProgram<'a>, CompiledProgram<'a>), SqlError> {
        let program = parse_plpgsql_program(self.text(source)?, parsing, self.kind)?;
        let code = CompiledProgram::new(self, program.body, parsing)?;
        Ok((program, code))
    }
}

#[derive(Clone, Copy)]
pub(in crate::sql::exec) struct ProgramLayout {
    pub(super) source: ProgramSource,
    pub(super) depth: usize,
    pub(super) handlers: usize,
}

#[derive(Clone, Copy)]
pub(in crate::sql::exec) struct CompiledProgram<'a> {
    blocks: &'a [TriggerBlock<'a>],
    layout: ProgramLayout,
}

impl<'a> CompiledProgram<'a> {
    fn new(
        source: ProgramSource,
        block: TriggerBlock<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
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
            layout: ProgramLayout {
                source,
                depth,
                handlers,
            },
        })
    }

    pub(in crate::sql::exec) fn layout(&self) -> ProgramLayout {
        self.layout
    }

    pub(in crate::sql::exec) fn source(&self) -> ProgramSource {
        self.layout.source
    }

    pub(in crate::sql::exec) fn root(&self) -> TriggerBlock<'a> {
        self.blocks[0]
    }

    pub(super) fn block(&self, index: BlockIndex) -> TriggerBlock<'a> {
        self.blocks[index.0]
    }

    pub(super) fn index(&self, block: TriggerBlock<'_>) -> BlockIndex {
        BlockIndex(
            self.blocks
                .iter()
                .position(|candidate| core::ptr::eq(candidate.statements, block.statements))
                .expect("entered block belongs to the compiled procedural program"),
        )
    }
}

pub(super) fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "procedural execution source was reset or replaced"
    )
}
