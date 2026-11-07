//! Compiled syntax borrows parsing storage; source identity and layout do not.

use super::super::numeric_loops::{self, BlockEdge};
use super::*;
use crate::mem::lease_pool::SharedSlice;

#[derive(Clone)]
pub(in crate::sql::exec) struct ProgramSource {
    bytes: SharedSlice<u8>,
    kind: PlpgsqlProgramKind,
}

impl PartialEq for ProgramSource {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && SharedSlice::ptr_eq(&self.bytes, &other.bytes)
    }
}
impl Eq for ProgramSource {}

impl ProgramSource {
    pub(in crate::sql::exec) fn capture(
        text: &str,
        kind: PlpgsqlProgramKind,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let mut bytes = pool.lease(text.len()).map_err(source_full)?;
        for chunk in text.as_bytes().chunks(4096) {
            super::super::super::query::check_timeout()?;
            bytes.extend_from_slice(chunk).map_err(source_full)?;
        }
        Ok(Self {
            bytes: bytes.freeze(),
            kind,
        })
    }

    pub(in crate::sql::exec) fn parse<'a>(
        &self,
        parsing: &'a Arena,
    ) -> Result<(ParsedTriggerProgram<'a>, CompiledProgram<'a>), SqlError> {
        let text = core::str::from_utf8(&self.bytes).expect("program sources copy UTF-8 text");
        let text = parsing.alloc_str(text).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "PL/pgSQL source parsing exceeds statement memory"
            )
        })?;
        let program = parse_plpgsql_program(text, parsing, self.kind)?;
        let code = CompiledProgram::new(self.clone(), program.body, program.locals, parsing)?;
        Ok((program, code))
    }
}

fn source_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL source image: {}",
        error
    )
}

#[derive(Clone)]
pub(in crate::sql::exec) struct ProgramLayout {
    pub(super) source: ProgramSource,
    pub(super) depth: usize,
    pub(super) handlers: usize,
}

pub(in crate::sql::exec) struct CompiledProgram<'a> {
    blocks: &'a [TriggerBlock<'a>],
    layout: ProgramLayout,
    pub(in crate::sql::exec) declarations: &'a [ParsedTriggerLocalDecl<'a>],
}

impl<'a> CompiledProgram<'a> {
    fn new(
        source: ProgramSource,
        block: TriggerBlock<'a>,
        declarations: &'a [ParsedTriggerLocalDecl<'a>],
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
            declarations,
        })
    }

    pub(in crate::sql::exec) fn layout(&self) -> ProgramLayout {
        self.layout.clone()
    }

    pub(in crate::sql::exec) fn source(&self) -> &ProgramSource {
        &self.layout.source
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
        "procedural execution source or workspace was replaced"
    )
}
