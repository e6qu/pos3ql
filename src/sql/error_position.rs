//! The cursor position (the ErrorResponse `P` field) PostgreSQL reports for an
//! error its parse analysis raises: the unknown relation, column, type,
//! function or operator, or the constant whose input failed.
//!
//! Analysis here happens deep inside planning and evaluation, where the
//! statement text is out of reach. The client's statement text and the
//! parser's map of copied tokens are installed for the duration of a
//! statement; an analysis site names the AST slice its error is about, and
//! the slice's address locates it in that text. Slices from other text, such
//! as a function body or catalog definition, have no position, as in
//! PostgreSQL. The position travels beside the error and is attached only to
//! the error it was computed for (same SQLSTATE and message).

use core::cell::Cell;

use crate::sql::eval::SqlError;

/// A token the lexer copied into the arena (folded, unescaped or dequoted)
/// or whose text starts after its token: the copy's address and the token's
/// byte offset in the statement text.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceCopy {
    pub(crate) address: usize,
    pub(crate) len: usize,
    pub(crate) at: usize,
}

#[derive(Clone, Copy)]
struct Source {
    text: *const u8,
    len: usize,
    copies: *const SourceCopy,
    copies_len: usize,
    /// Where the text the statement was parsed from starts in `text`, which
    /// parsed source offsets are relative to.
    offset_base: usize,
}

const NO_SOURCE: Source = Source {
    text: core::ptr::null(),
    len: 0,
    copies: core::ptr::null(),
    copies_len: 0,
    offset_base: 0,
};

#[derive(Clone, Copy)]
struct Pending {
    sqlstate: crate::sql::eval::SqlState,
    message_hash: u64,
    /// One-based character position, as the `P` field counts.
    position: Option<usize>,
    hint: Option<&'static str>,
}

/// What PostgreSQL adds to an analysis error: its cursor and hint.
#[derive(Clone, Copy, Default)]
pub(crate) struct Annotation {
    pub(crate) position: Option<usize>,
    pub(crate) hint: Option<&'static str>,
}

const FUNCTION_HINT: &str = "No function matches the given name and argument types. You might need to add explicit type casts.";
const BINARY_OPERATOR_HINT: &str = "No operator matches the given name and argument types. You might need to add explicit type casts.";
const UNARY_OPERATOR_HINT: &str = "No operator matches the given name and argument type. You might need to add an explicit type cast.";

std::thread_local! {
    static SOURCE: Cell<Source> = const { Cell::new(NO_SOURCE) };
    /// Whether the statement is one PostgreSQL analyses as a query (or a
    /// query-bearing command), whose relation and column lookups report a
    /// position; utility commands look names up without one.
    static ANALYZED: Cell<bool> = const { Cell::new(false) };
    static PENDING: Cell<Option<Pending>> = const { Cell::new(None) };
}

struct SourceScope(Source, bool);

impl Drop for SourceScope {
    fn drop(&mut self) {
        SOURCE.with(|source| source.set(self.0));
        ANALYZED.with(|analyzed| analyzed.set(self.1));
    }
}

/// Runs `run` with `text`, the client's query text, as the statement source
/// that analysis errors are positioned in. Any position still pending from
/// an earlier statement is discarded.
/// `analyzed` says whether relation and column lookups are positioned;
/// `offset_base` is where the text the statement was parsed from starts.
pub(crate) fn with_statement_source<R>(
    text: &str,
    copies: &[SourceCopy],
    analyzed: bool,
    offset_base: usize,
    run: impl FnOnce() -> R,
) -> R {
    PENDING.with(|pending| pending.set(None));
    let _scope = SourceScope(
        SOURCE.with(|source| {
            source.replace(Source {
                text: text.as_ptr(),
                len: text.len(),
                copies: copies.as_ptr(),
                copies_len: copies.len(),
                offset_base,
            })
        }),
        ANALYZED.with(|flag| flag.replace(analyzed)),
    );
    run()
}

/// [`at_subject`] for a relation or column reference, which PostgreSQL
/// positions only while analysing a query.
pub(crate) fn at_reference(error: SqlError, subject: &str) -> SqlError {
    if ANALYZED.with(Cell::get) {
        at_subject(error, subject)
    } else {
        error
    }
}

/// The byte offset of `subject` in the installed statement text: a token the
/// parser copied, or a slice of the text itself.
fn locate(subject: &str) -> Option<(usize, Source)> {
    let source = SOURCE.with(Cell::get);
    if source.text.is_null() {
        return None;
    }
    let address = subject.as_ptr() as usize;
    // with_statement_source owns the only installation path, and its guard
    // restores the prior source before the caller can free the slices.
    let copies = unsafe { core::slice::from_raw_parts(source.copies, source.copies_len) };
    if let Some(copy) = copies
        .iter()
        .find(|copy| copy.address == address && copy.len == subject.len())
    {
        return Some((copy.at, source));
    }
    let start = source.text as usize;
    (address >= start && address + subject.len() <= start + source.len)
        .then(|| (address - start, source))
}

/// One-based character position of byte offset `at` in the source text.
fn character_position(source: Source, at: usize) -> usize {
    let text = unsafe { core::slice::from_raw_parts(source.text, source.len) };
    1 + text[..at.min(text.len())]
        .iter()
        .filter(|byte| (**byte & 0xC0) != 0x80)
        .count()
}

fn message_hash(message: &dyn core::fmt::Display) -> u64 {
    struct Hasher(u64);
    impl core::fmt::Write for Hasher {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for byte in text.bytes() {
                // FNV-1a.
                self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
            Ok(())
        }
    }
    let mut hasher = Hasher(0xcbf2_9ce4_8422_2325);
    let _ = core::fmt::Write::write_fmt(&mut hasher, format_args!("{message}"));
    hasher.0
}

/// Marks `error` as being about `subject`, a slice of the statement's AST.
pub(crate) fn at_subject(error: SqlError, subject: &str) -> SqlError {
    let position = locate(subject).map(|(at, source)| character_position(source, at));
    record(&error, position, None);
    error
}

/// [`at_subject`] or [`at_offset`] with PostgreSQL's hint for the error.
fn annotated(error: SqlError, at: Option<usize>, hint: &'static str) -> SqlError {
    let source = SOURCE.with(Cell::get);
    let position = at
        .map(|at| at + source.offset_base)
        .filter(|at| !source.text.is_null() && *at <= source.len)
        .map(|at| character_position(source, at));
    record(&error, position, Some(hint));
    error
}

/// Positions an error raised analysing `expression` at the part PostgreSQL
/// names: a cast's unknown type or failed constant, an unknown function or
/// an unknown operator.
pub(crate) fn expression_error(
    error: SqlError,
    expression: &crate::sql::ast::Expr<'_>,
) -> SqlError {
    use crate::sql::ast::Expr;
    use crate::sql::eval::sqlstate;
    let state = error.sqlstate.as_str();
    let message = error.message.as_str();
    match expression {
        Expr::Column { qualifier, name } => column_reference(error, *qualifier, name),
        Expr::Cast {
            type_name, operand, ..
        } => {
            if state == sqlstate::UNDEFINED_OBJECT && message.starts_with("type \"") {
                at_subject(error, type_name)
            } else if let Expr::Str(literal) = operand
                && state.starts_with("22")
                && !is_type_modifier_failure(message)
            {
                at_subject(error, literal)
            } else {
                error
            }
        }
        Expr::Call {
            name,
            source_position,
            ..
        } if state == sqlstate::UNDEFINED_FUNCTION && message.starts_with("function ") => {
            match source_position.get() {
                Some(at) => annotated(error, Some(at), FUNCTION_HINT),
                None => {
                    let error = at_subject(error, name);
                    annotated(error, None, FUNCTION_HINT)
                }
            }
        }
        Expr::Binary {
            source_position, ..
        } if state == sqlstate::UNDEFINED_FUNCTION && message.starts_with("operator ") => {
            annotated(error, source_position.get(), BINARY_OPERATOR_HINT)
        }
        Expr::Unary {
            source_position, ..
        } if state == sqlstate::UNDEFINED_FUNCTION && message.starts_with("operator ") => {
            annotated(error, source_position.get(), UNARY_OPERATOR_HINT)
        }
        _ => error,
    }
}

/// Records `position` and `hint` for `error`; the innermost analysis site
/// that saw it names its subject, so an existing record keeps its parts.
fn record(error: &SqlError, position: Option<usize>, hint: Option<&'static str>) {
    if error.sqlstate.internal_control().is_some() || (position.is_none() && hint.is_none()) {
        return;
    }
    let message_hash = message_hash(&error.message);
    PENDING.with(|pending| {
        let known = pending
            .get()
            .filter(|known| known.sqlstate == error.sqlstate && known.message_hash == message_hash);
        pending.set(Some(Pending {
            sqlstate: error.sqlstate,
            message_hash,
            position: known.and_then(|known| known.position).or(position),
            hint: known.and_then(|known| known.hint).or(hint),
        }))
    });
}

/// The position and hint recorded for the error being reported, if it is
/// the error they were computed for.
pub(crate) fn take_annotation(sqlstate: &str, message: &dyn core::fmt::Display) -> Annotation {
    let Some(pending) = PENDING.with(|pending| pending.take()) else {
        return Annotation::default();
    };
    if pending.sqlstate == sqlstate && pending.message_hash == message_hash(message) {
        Annotation {
            position: pending.position,
            hint: pending.hint,
        }
    } else {
        Annotation::default()
    }
}

/// Positions a failed column reference `qualifier.name` at its start, for
/// the errors PostgreSQL positions there: an unknown, ambiguous or ungrouped
/// column and a missing or invalid FROM-clause entry.
pub(crate) fn column_reference(error: SqlError, qualifier: Option<&str>, name: &str) -> SqlError {
    use crate::sql::eval::sqlstate;
    if matches!(
        error.sqlstate.as_str(),
        sqlstate::UNDEFINED_COLUMN
            | sqlstate::UNDEFINED_TABLE
            | sqlstate::AMBIGUOUS_COLUMN
            | sqlstate::GROUPING_ERROR
    ) {
        at_reference(error, qualifier.unwrap_or(name))
    } else {
        error
    }
}

/// A cast's type modifier is applied after the constant's input, when the
/// statement runs, so PostgreSQL reports these failures without a position.
fn is_type_modifier_failure(message: &str) -> bool {
    message == "numeric field overflow"
        || message.starts_with("value too long for type")
        || message.starts_with("bit string length")
        || message.starts_with("bit string too long")
}

/// The longest message an error reports in full: a function signature with
/// PostgreSQL's 100 arguments (`FUNC_MAX_ARGS`), each a quoted,
/// schema-qualified array type name of two `NAMEDATALEN - 1` identifiers with
/// every quote doubled, plus the surrounding wording.
pub(crate) const FULL_MESSAGE_BYTES: usize = 1024
    + crate::storage::MAX_ROUTINE_ARGUMENTS * (4 * crate::sql::lexer::MAX_IDENTIFIER_BYTES + 16);

struct FullMessage {
    text: crate::util::StackStr<FULL_MESSAGE_BYTES>,
    sqlstate: Option<crate::sql::eval::SqlState>,
    message_hash: u64,
}

std::thread_local! {
    static FULL_MESSAGE: core::cell::RefCell<FullMessage> = const {
        core::cell::RefCell::new(FullMessage {
            text: crate::util::StackStr::new(),
            sqlstate: None,
            message_hash: 0,
        })
    };
}

/// An error whose message may exceed [`SqlError`]'s inline capacity. The
/// error carries the message's start; the complete text is kept beside it
/// and reported in its place, as for a positioned error.
pub(crate) fn full_message_error(
    sqlstate: &'static str,
    write: impl FnOnce(&mut dyn core::fmt::Write) -> Result<(), SqlError>,
) -> SqlError {
    FULL_MESSAGE.with(|full| {
        let mut full = full.borrow_mut();
        full.text.clear();
        full.sqlstate = None;
        if let Err(error) = write(&mut full.text) {
            return error;
        }
        if full.text.is_truncated() {
            return crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "error message exceeds {} bytes",
                FULL_MESSAGE_BYTES
            );
        }
        let error = crate::sql_err!(sqlstate, "{}", full.text.as_str());
        if error.message.is_truncated() {
            full.sqlstate = Some(error.sqlstate);
            full.message_hash = message_hash(&error.message);
        }
        error
    })
}

/// Runs `report` with the complete text of the error being reported when
/// [`full_message_error`] kept one for it, or `None`.
pub(crate) fn with_full_message<R>(
    sqlstate: &str,
    message: &dyn core::fmt::Display,
    report: impl FnOnce(Option<&str>) -> R,
) -> R {
    FULL_MESSAGE.with(|full| {
        let mut full = full.borrow_mut();
        let matches = full.sqlstate.is_some_and(|state| state == sqlstate)
            && full.message_hash == message_hash(message);
        full.sqlstate = None;
        report(matches.then_some(full.text.as_str()))
    })
}
