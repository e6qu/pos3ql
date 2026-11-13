//! Compiled syntax borrows parsing storage; source identity and layout do not.

use super::super::numeric_loops::{self, BlockEdge};
use super::*;
use crate::mem::lease_pool::SharedSlice;

#[derive(Clone)]
pub(in crate::sql::exec) struct ProgramSource {
    bytes: SharedSlice<u8>,
    kind: PlpgsqlProgramKind,
    /// How many parameters the text names `$1` onwards.
    positionals: u32,
}

impl PartialEq for ProgramSource {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.positionals == other.positionals
            && SharedSlice::ptr_eq(&self.bytes, &other.bytes)
    }
}
impl Eq for ProgramSource {}

impl ProgramSource {
    pub(in crate::sql::exec) fn capture(
        text: &str,
        kind: PlpgsqlProgramKind,
        positionals: u32,
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
            positionals,
        })
    }

    /// Parses the saved text. `written` is the text as the statement wrote
    /// it, when it is still installed: errors are positioned there.
    pub(in crate::sql::exec) fn parse<'a>(
        &self,
        parsing: &'a Arena,
        written: Option<&str>,
    ) -> Result<(ParsedTriggerProgram<'a>, CompiledProgram<'a>), SqlError> {
        let text = core::str::from_utf8(&self.bytes).expect("program sources copy UTF-8 text");
        let text = parsing.alloc_str(text).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "PL/pgSQL source parsing exceeds statement memory"
            )
        })?;
        let program = match written {
            Some(written) => crate::sql::error_position::with_copied_source(written, text, || {
                parse_routine_program(text, parsing, self.kind, self.positionals)
            }),
            None => parse_routine_program(text, parsing, self.kind, self.positionals),
        }?;
        let code = CompiledProgram::new(self.clone(), text, program.body, program.locals, parsing)?;
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
    /// The program text every statement and declaration source slices.
    text: &'a str,
    blocks: &'a [TriggerBlock<'a>],
    layout: ProgramLayout,
    pub(in crate::sql::exec) declarations: &'a [ParsedTriggerLocalDecl<'a>],
}

impl<'a> CompiledProgram<'a> {
    /// The program text every statement and declaration source slices.
    pub(in crate::sql::exec) const fn text(&self) -> &'a str {
        self.text
    }

    fn new(
        source: ProgramSource,
        text: &'a str,
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
            .alloc_slice_with(count, |_| TriggerBlock {
                statements: &[],
                sources: &[],
            })
            .map_err(|_| frame_full())?;
        let mut next = 0;
        compile(block, blocks, &mut next);
        assert_eq!(next, count);
        Ok(Self {
            text,
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

    pub(in crate::sql::exec) fn kind(&self) -> PlpgsqlProgramKind {
        self.layout.source.kind
    }

    /// The one-based line of `source`, a slice of the program text, as
    /// PostgreSQL numbers a routine body's lines.
    pub(in crate::sql::exec) fn line_of(&self, source: &str) -> Option<usize> {
        let offset = (source.as_ptr() as usize).checked_sub(self.text.as_ptr() as usize)?;
        let before = self.text.as_bytes().get(..offset)?;
        Some(1 + before.iter().filter(|byte| **byte == b'\n').count())
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
