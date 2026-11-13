//! Parse-frame references locate backend plans without persisting AST pointers.

use core::cell::Cell;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql_err;

use super::eval::{SqlError, sqlstate};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Expression,
    Query,
    Statement,
}

#[derive(Clone, Copy)]
pub(super) enum Origin {
    Routine { oid: i32, version: u64 },
    Anonymous,
}

#[derive(Clone, Copy)]
pub(super) struct Site<'a> {
    pub kind: Kind,
    pub key: usize,
    pub position: usize,
    pub text: &'a str,
    pub exception_variables: bool,
    /// How many routine parameters the text names `$1` onwards.
    pub positionals: u32,
}

struct Capture<'a> {
    body: &'a str,
    arena: &'a Arena,
    sites: ArenaList<'a, Site<'a>>,
}

thread_local! {
    static CAPTURE: Cell<*mut ()> = const { Cell::new(core::ptr::null_mut()) };
}

pub(super) fn capture<'a, T>(
    body: &'a str,
    arena: &'a Arena,
    parse: impl FnOnce() -> Result<T, SqlError>,
) -> Result<(T, &'a [Site<'a>]), SqlError> {
    struct Restore(*mut ());
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.with(|slot| slot.set(self.0));
        }
    }
    let mut frame = Capture {
        body,
        arena,
        sites: ArenaList::new(arena),
    };
    let previous = CAPTURE.with(|slot| slot.replace((&mut frame as *mut Capture<'_>).cast()));
    let restore = Restore(previous);
    let value = parse();
    drop(restore);
    Ok((value?, frame.sites.as_slice()))
}

pub(super) fn record(kind: Kind, key: usize, text: &str, original: &str) -> Result<(), SqlError> {
    record_with(kind, key, text, original, None)
}

/// [`record`] for a query rewritten into `arena`, the arena the program is
/// parsed in: the site keeps the parsed text itself, so the positions of its
/// parsed names locate in the text a procedural error reports.
pub(super) fn record_rewritten(
    kind: Kind,
    key: usize,
    text: &str,
    original: &str,
    arena: &Arena,
) -> Result<(), SqlError> {
    record_with(kind, key, text, original, Some(arena))
}

fn record_with(
    kind: Kind,
    key: usize,
    text: &str,
    original: &str,
    text_arena: Option<&Arena>,
) -> Result<(), SqlError> {
    CAPTURE.with(|slot| {
        let pointer = slot.get();
        if pointer.is_null() {
            return Ok(());
        }
        // The closure-only capture boundary owns this frame until parsing ends.
        // Neither the frame pointer nor an arbitrary caller borrow can escape.
        let frame = unsafe { &mut *pointer.cast::<Capture<'_>>() };
        let start = (original.as_ptr() as usize)
            .checked_sub(frame.body.as_ptr() as usize)
            .ok_or_else(invalid_source)?;
        let end = start
            .checked_add(original.len())
            .ok_or_else(invalid_source)?;
        let original = frame.body.get(start..end).ok_or_else(invalid_source)?;
        let text = if text == original {
            original
        } else if text_arena.is_some_and(|arena| core::ptr::eq(arena, frame.arena)) {
            // SAFETY: `text` lives in the capture's own arena, which outlives
            // every site the capture returns.
            unsafe { core::mem::transmute::<&str, &str>(text) }
        } else {
            frame
                .arena
                .alloc_str(text)
                .map_err(|_| super::query::arena_full_pub())?
        };
        let exception_variables = references_exception_variables(text, frame.arena)?;
        frame
            .sites
            .push(Site {
                kind,
                key,
                position: start,
                text,
                exception_variables,
                positionals: super::parser::procedural_positionals(),
            })
            .map_err(|_| super::query::arena_full_pub())
    })
}

// Keep only a flag; identifier unescaping and case folding use temporary space.
pub(super) fn references_exception_variables(text: &str, arena: &Arena) -> Result<bool, SqlError> {
    arena
        .with_remaining_region(|scratch| {
            use super::lexer::{Lexer, Tok};
            let mut lexer = Lexer::new(text, scratch);
            loop {
                match lexer.next_token().map_err(|error| {
                    super::parse_error_to_sql(&super::parser::ParseError::lexical(error, text))
                })? {
                    Tok::Ident("sqlstate" | "sqlerrm")
                    | Tok::QuotedIdent("sqlstate" | "sqlerrm") => return Ok(true),
                    Tok::Eof => return Ok(false),
                    _ => {}
                }
            }
        })
        .map_err(|_| super::query::arena_full_pub())?
}

fn invalid_source() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "PL/pgSQL input plan has no original body span"
    )
}

pub(super) fn parse<'a>(
    site: Site<'a>,
    storage: &crate::storage::Storage,
    txid: u32,
    row: &dyn super::eval::ColumnLookup<'a>,
    relations: Option<&'a [(&'a str, &'a super::ast::MaterializedCte<'a>)]>,
    arena: &'a Arena,
    dependencies: Option<&mut crate::storage::StoredQueryDependencies>,
) -> Result<super::parser::Parser<'a>, SqlError> {
    let mut dependencies = dependencies;
    // Only the site's own text names the routine's parameters `$n`; what
    // analysing it reads (stored views and routines) does not.
    let positional = |parse: &mut dyn FnMut() -> Result<(), SqlError>| {
        super::parser::with_procedural_positionals(site.positionals, parse)
    };
    let mut parser = super::parser::Parser::new(site.text, arena)
        .map_err(|error| super::parse_error_to_sql(&error))?;
    parser.capture_type_inputs();
    let catalog = super::query::storage_catalog(storage, arena, txid);
    let columns = super::eval::RuntimeColumnTypes {
        row,
        catalog: Some(&catalog),
    };
    match site.kind {
        Kind::Expression => {
            let mut parsed = None;
            positional(&mut || {
                parsed = Some(
                    parser
                        .expression(0)
                        .map_err(|error| super::parse_error_to_sql(&error))?,
                );
                Ok(())
            })?;
            let expression = parsed.expect("parsed site expression");
            let expression = match relations {
                Some(relations) => super::query::bind_materialized_expression(
                    expression, relations, storage, txid, arena,
                )?,
                None => expression,
            };
            super::query::capture_contextual_expression_type_inputs(
                &parser, expression, storage, txid, arena, &columns, row,
            )?;
            if let Some(dependencies) = dependencies.as_mut() {
                super::query::collect_runtime_expression(
                    expression,
                    storage,
                    txid,
                    dependencies,
                    arena,
                    &columns,
                )?;
            }
        }
        Kind::Query | Kind::Statement => loop {
            let mut next = None;
            positional(&mut || {
                next = parser
                    .next_stmt()
                    .map_err(|error| super::parse_error_to_sql(&error))?;
                Ok(())
            })?;
            let Some(statement) = next else {
                break;
            };
            {
                let statement = arena
                    .alloc(statement)
                    .map_err(|_| super::query::arena_full_pub())?;
                let statement = match relations {
                    Some(relations) => super::query::bind_materialized_statement(
                        statement, relations, storage, txid, arena,
                    )?,
                    None => statement,
                };
                super::query::capture_procedural_statement_type_inputs(
                    &parser, statement, storage, txid, arena, row,
                )?;
                if let Some(dependencies) = dependencies.as_mut() {
                    super::query::collect_runtime_statement(
                        statement,
                        storage,
                        txid,
                        dependencies,
                        arena,
                        Some(&columns),
                    )?;
                }
            }
        },
    }
    Ok(parser)
}

pub(super) fn reject_atomic<'a>(
    site: Site<'a>,
    storage: &crate::storage::Storage,
    txid: u32,
    row: &dyn super::eval::ColumnLookup<'a>,
    relations: Option<&'a [(&'a str, &'a super::ast::MaterializedCte<'a>)]>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let parser = parse(site, storage, txid, row, relations, arena, None)?;
        if parser.type_input_candidates().iter().any(|candidate| {
            super::type_inputs::candidate_target_oid(storage, txid, candidate).is_some()
        }) {
            return Err(super::exec::engine_required(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "checked type inputs in data-change triggers require a resumable execution context"
            )));
        }
        Ok(())
    })();
    // Only an owned error escapes this metadata-only decision.
    unsafe { arena.rewind_to(mark) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    #[test]
    fn exception_variable_flags_follow_identifiers_without_retaining_lexer_scratch() {
        let mut budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut budget, "handler variable tokens", 16 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (source, expected) in [
                ("SQLERRM || sqlstate", true),
                ("\"sqlerrm\"", true),
                ("\"SQLERRM\"", false),
                ("'sqlerrm' /* SQLSTATE */", false),
                ("$x$SQLERRM$x$ -- sqlstate", false),
                ("\"sql\"\"errm\"", false),
                ("repeat('x',8192)", false),
            ] {
                let used = arena.used();
                assert_eq!(
                    references_exception_variables(source, &arena).unwrap(),
                    expected,
                    "{source}"
                );
                assert_eq!(arena.used(), used);
            }
        });
    }

    #[test]
    fn parse_frames_copy_synthetic_sources_and_restore_nested_authority() {
        let mut budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut budget, "PL source frames", 16 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let body = arena.alloc_str("RETURN 1; RETURN 2").unwrap();
            let nested_body = arena.alloc_str("RETURN 3").unwrap();
            let mut synthetic = *b"SELECT 1";
            let (_, outer) = capture(body, &arena, || {
                record(Kind::Expression, 1, &body[7..8], &body[7..8])?;
                let (_, nested) = capture(nested_body, &arena, || {
                    record(
                        Kind::Query,
                        2,
                        core::str::from_utf8(&synthetic).unwrap(),
                        nested_body,
                    )
                })?;
                assert_eq!(nested.len(), 1);
                assert_eq!(nested[0].text, "SELECT 1");
                synthetic.fill(b'x');
                assert_eq!(nested[0].text, "SELECT 1");
                let foreign = *b"RETURN 1";
                assert!(
                    record(
                        Kind::Statement,
                        3,
                        "SELECT 1",
                        core::str::from_utf8(&foreign).unwrap()
                    )
                    .is_err()
                );
                let failed: Result<((), &[Site<'_>]), SqlError> =
                    capture(nested_body, &arena, || Err(invalid_source()));
                assert!(failed.is_err());
                record(Kind::Expression, 4, &body[17..18], &body[17..18])
            })
            .unwrap();
            assert_eq!(outer.len(), 2);
            assert_eq!((outer[0].position, outer[0].text), (7, "1"));
            assert_eq!((outer[1].position, outer[1].text), (17, "2"));
            assert_eq!((outer[0].key, outer[1].key), (1, 4));
            record(Kind::Statement, 9, "not retained", "outside a parse frame").unwrap();
        });
    }
}
