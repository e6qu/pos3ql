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

/// The length a [`SourceCopy`] of an expression node records: the node's
/// address locates a constant that keeps no source text.
const NODE: usize = usize::MAX;

/// The length a [`SourceCopy`] of a simple `CASE`'s `WHEN` value records:
/// the value's address locates its `WHEN` keyword, where PostgreSQL reports
/// the comparison with the `CASE` operand.
const CASE_WHEN: usize = usize::MAX - 1;

impl SourceCopy {
    /// Records that the sublink querying `select` was written at byte `at`.
    pub(crate) fn query(select: &crate::sql::ast::Select<'_>, at: usize) -> Self {
        Self {
            address: core::ptr::from_ref(select) as usize,
            len: NODE,
            at,
        }
    }

    /// Records that the `WHEN` comparing `value` with a simple `CASE`'s
    /// operand was written at byte `at`.
    pub(crate) fn case_when(value: &crate::sql::ast::Expr<'_>, at: usize) -> Self {
        Self {
            address: core::ptr::from_ref(value) as usize,
            len: CASE_WHEN,
            at,
        }
    }

    /// Records that the constant `expression` was written at byte `at`.
    pub(crate) fn node(expression: &crate::sql::ast::Expr<'_>, at: usize) -> Self {
        Self {
            address: core::ptr::from_ref(expression) as usize,
            len: NODE,
            at,
        }
    }
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
    /// Where the text `copies` were recorded from starts in `text`.
    copies_base: usize,
    /// Whether `text` is a query a routine runs rather than the client's.
    internal: bool,
}

const NO_SOURCE: Source = Source {
    text: core::ptr::null(),
    len: 0,
    copies: core::ptr::null(),
    copies_len: 0,
    offset_base: 0,
    copies_base: 0,
    internal: false,
};

#[derive(Clone, Copy)]
struct Pending {
    sqlstate: crate::sql::eval::SqlState,
    message_hash: u64,
    /// One-based character position, as the `P` field counts.
    position: Option<usize>,
    hint: Option<&'static str>,
    note: Option<AnalysisNote>,
}

/// What PostgreSQL adds to an analysis error: its cursor and hint.
#[derive(Clone, Copy, Default)]
pub(crate) struct Annotation {
    pub(crate) position: Option<usize>,
    pub(crate) hint: Option<&'static str>,
    pub(crate) note: Option<AnalysisNote>,
}

/// A computed DETAIL or HINT of a name-resolution error, kept beside the
/// error it explains: it reaches the client only with that error (same
/// SQLSTATE and message), so a probe that discards its error cannot attach
/// text to an unrelated later one.
#[derive(Clone, Copy)]
pub(crate) enum AnalysisNote {
    Detail(crate::util::StackStr<NOTE_BYTES>),
    Hint(crate::util::StackStr<NOTE_BYTES>),
    /// A missing column's spelling suggestion: a later search over more
    /// enclosing queries replaces it.
    Suggestion(crate::util::StackStr<NOTE_BYTES>),
}

/// Two quoted identifiers of `NAMEDATALEN - 1` bytes plus the wording.
pub(crate) const NOTE_BYTES: usize = 2 * crate::sql::lexer::MAX_IDENTIFIER_BYTES + 128;

/// Attaches a computed DETAIL to `error`.
pub(crate) fn with_detail(error: SqlError, detail: core::fmt::Arguments<'_>) -> SqlError {
    note(error, AnalysisNote::Detail(note_text(detail)))
}

/// Attaches a computed HINT to `error`.
pub(crate) fn with_computed_hint(error: SqlError, hint: core::fmt::Arguments<'_>) -> SqlError {
    note(error, AnalysisNote::Hint(note_text(hint)))
}

/// Attaches PostgreSQL's spelling suggestion for a missing column to
/// `error`, replacing an earlier suggestion; an explanation already attached
/// (why an existing column is out of reach) stays.
pub(crate) fn with_column_suggestion(
    error: SqlError,
    suggestion: &crate::sql::column_suggestion::ColumnSuggestion<'_>,
) -> SqlError {
    if error.sqlstate.as_str() != crate::sql::eval::sqlstate::UNDEFINED_COLUMN
        || error.sqlstate.internal_control().is_some()
    {
        return error;
    }
    let message_hash = message_hash(&error.message);
    PENDING.with(|pending| {
        let known = pending
            .get()
            .filter(|known| known.sqlstate == error.sqlstate && known.message_hash == message_hash);
        if known.is_some_and(|known| {
            matches!(
                known.note,
                Some(AnalysisNote::Detail(_) | AnalysisNote::Hint(_))
            ) || known.hint.is_some()
        }) {
            return;
        }
        pending.set(Some(Pending {
            sqlstate: error.sqlstate,
            message_hash,
            position: known.and_then(|known| known.position),
            hint: None,
            note: {
                let mut suggestion = *suggestion;
                crate::sql::column_suggestion::offer_set_branches(&mut suggestion);
                suggestion.hint().map(AnalysisNote::Suggestion)
            },
        }))
    });
    error
}

/// Whether `error` already carries a detail, hint or suggestion.
fn has_pending_note(error: &SqlError) -> bool {
    let message_hash = message_hash(&error.message);
    PENDING.with(|pending| {
        pending.get().is_some_and(|known| {
            known.sqlstate == error.sqlstate
                && known.message_hash == message_hash
                && (known.note.is_some() || known.hint.is_some())
        })
    })
}

fn note_text(text: core::fmt::Arguments<'_>) -> crate::util::StackStr<NOTE_BYTES> {
    use core::fmt::Write as _;
    let mut out = crate::util::StackStr::new();
    let _ = out.write_fmt(text);
    out
}

fn note(error: SqlError, note: AnalysisNote) -> SqlError {
    if error.sqlstate.internal_control().is_some() {
        return error;
    }
    let message_hash = message_hash(&error.message);
    PENDING.with(|pending| {
        let known = pending
            .get()
            .filter(|known| known.sqlstate == error.sqlstate && known.message_hash == message_hash);
        pending.set(Some(Pending {
            sqlstate: error.sqlstate,
            message_hash,
            position: known.and_then(|known| known.position),
            hint: known.and_then(|known| known.hint),
            note: Some(note),
        }))
    });
    error
}

const FUNCTION_HINT: &str = "No function matches the given name and argument types. You might need to add explicit type casts.";
const AMBIGUOUS_FUNCTION_HINT: &str =
    "Could not choose a best candidate function. You might need to add explicit type casts.";
const BINARY_OPERATOR_HINT: &str = "No operator matches the given name and argument types. You might need to add explicit type casts.";
const AMBIGUOUS_OPERATOR_HINT: &str =
    "Could not choose a best candidate operator. You might need to add explicit type casts.";
const UNARY_OPERATOR_HINT: &str = "No operator matches the given name and argument type. You might need to add an explicit type cast.";

std::thread_local! {
    static SOURCE: Cell<Source> = const { Cell::new(NO_SOURCE) };
    /// Whether the statement is one PostgreSQL analyses as a query (or a
    /// query-bearing command), whose relation and column lookups report a
    /// position; utility commands look names up without one.
    static ANALYZED: Cell<bool> = const { Cell::new(false) };
    static PENDING: Cell<Option<Pending>> = const { Cell::new(None) };
    /// The client's whole query string, PostgreSQL's `debug_query_string`:
    /// the outermost statement source while it runs.
    static CLIENT_QUERY: Cell<(*const u8, usize)> = const { Cell::new((core::ptr::null(), 0)) };
}

struct ClientQueryScope(Option<(*const u8, usize)>);

impl Drop for ClientQueryScope {
    fn drop(&mut self) {
        if let Some(prior) = self.0 {
            CLIENT_QUERY.with(|client| client.set(prior));
        }
    }
}

/// Runs `run` with the client's whole query string, if a client statement is
/// running.
pub(crate) fn with_client_query<R>(run: impl FnOnce(Option<&str>) -> R) -> R {
    let (text, len) = CLIENT_QUERY.with(Cell::get);
    if text.is_null() {
        return run(None);
    }
    // with_statement_source keeps the client's text alive while it is installed.
    let bytes = unsafe { core::slice::from_raw_parts(text, len) };
    run(core::str::from_utf8(bytes).ok())
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
    crate::sql::error_context::reset();
    let _client = ClientQueryScope(CLIENT_QUERY.with(|client| {
        client
            .get()
            .0
            .is_null()
            .then(|| client.replace((text.as_ptr(), text.len())))
    }));
    let _scope = SourceScope(
        SOURCE.with(|source| {
            source.replace(Source {
                text: text.as_ptr(),
                len: text.len(),
                copies: copies.as_ptr(),
                copies_len: copies.len(),
                offset_base,
                copies_base: 0,
                internal: false,
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
/// A slice inside a token the lexer copied verbatim, such as a word of a
/// dollar-quoted function body: the copy's text appears in the statement
/// just after the token's opening delimiter.
fn within_copy(
    copies: &[SourceCopy],
    source: Source,
    address: usize,
    len: usize,
) -> Option<(usize, Source)> {
    const MAX_DELIMITER: usize = 64;
    let text = unsafe { core::slice::from_raw_parts(source.text, source.len) };
    copies.iter().find_map(|copy| {
        if copy.len >= CASE_WHEN
            || address < copy.address
            || address + len > copy.address + copy.len
        {
            return None;
        }
        // The copy lives in the statement arena for the statement's duration.
        let copied = unsafe { core::slice::from_raw_parts(copy.address as *const u8, copy.len) };
        let at = copy.at + source.copies_base;
        (0..=MAX_DELIMITER)
            .find(|delimiter| text.get(at + delimiter..at + delimiter + copy.len) == Some(copied))
            .map(|delimiter| (at + delimiter + (address - copy.address), source))
    })
}

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
        return Some((copy.at + source.copies_base, source));
    }
    let start = source.text as usize;
    if address < start || address + subject.len() > start + source.len {
        return within_copy(copies, source, address, subject.len());
    }
    let at = address - start;
    // A quoted token parsed without a copy map (a routine's query) is a
    // slice inside its quotes; PostgreSQL points at the opening quote.
    let text = unsafe { core::slice::from_raw_parts(source.text, source.len) };
    let quoted = at > 0
        && matches!(text[at - 1], b'\'' | b'"')
        && text.get(at + subject.len()) == Some(&text[at - 1]);
    Some((if quoted { at - 1 } else { at }, source))
}

/// Where `expression` starts in the installed statement text, as
/// PostgreSQL's `exprLocation` places an error about the whole expression.
fn expression_start(expression: &crate::sql::ast::Expr<'_>) -> Option<(usize, Source)> {
    use crate::sql::ast::Expr;
    let source = SOURCE.with(Cell::get);
    if source.text.is_null() {
        return None;
    }
    let parsed = |position: crate::sql::ast::SourcePosition| {
        position
            .get()
            .map(|at| at + source.offset_base)
            .filter(|at| *at <= source.len)
            .map(|at| (at, source))
    };
    match expression {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Param(_)
        | Expr::Subquery(_) => {
            let address = core::ptr::from_ref(expression) as usize;
            // with_statement_source keeps the copy map alive with its text.
            let copies = unsafe { core::slice::from_raw_parts(source.copies, source.copies_len) };
            copies
                .iter()
                .find(|copy| copy.len == NODE && copy.address == address)
                .map(|copy| (copy.at + source.copies_base, source))
        }
        Expr::Str(text) | Expr::NumericLit(text) | Expr::BitLit(text) => locate(text),
        Expr::Column { qualifier, name }
        | Expr::RoutineParam {
            qualifier, name, ..
        } => locate(qualifier.unwrap_or(name)),
        Expr::Binary { left, .. } => expression_start(left),
        Expr::Cast {
            operand,
            source_position,
            ..
        } => {
            let operand = expression_start(operand);
            match (operand, parsed(*source_position)) {
                (Some(operand), Some(cast)) if cast.0 < operand.0 => Some(cast),
                (Some(operand), _) => Some(operand),
                (None, cast) => cast,
            }
        }
        Expr::Unary {
            source_position, ..
        }
        | Expr::Call {
            source_position, ..
        } => parsed(*source_position),
        Expr::RecordFieldIndex {
            base: Expr::RowSubquery { select, .. },
            index,
        } => match select.items.get(usize::from(*index)) {
            Some(crate::sql::ast::SelectItem::Expr { expression, .. }) => {
                expression_start(expression)
            }
            _ => None,
        },
        Expr::IsNull { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::InList { operand, .. }
        | Expr::Like { operand, .. } => expression_start(operand),
        _ => None,
    }
}

/// Marks `error` as being about the whole of `expression`, at its start.
pub(crate) fn at_expression(error: SqlError, expression: &crate::sql::ast::Expr<'_>) -> SqlError {
    let position = expression_start(expression).map(|(at, source)| character_position(source, at));
    record(&error, position, None);
    error
}

/// [`at_expression`] with PostgreSQL's hint.
pub(crate) fn at_expression_with_hint(
    error: SqlError,
    expression: &crate::sql::ast::Expr<'_>,
    hint: &'static str,
) -> SqlError {
    let position = expression_start(expression).map(|(at, source)| character_position(source, at));
    record(&error, position, Some(hint));
    error
}

/// Marks `error` as being about the node `expression` at the position the
/// parser recorded for it: a subquery at its parenthesis, an `IN` or
/// quantified sublink at its keyword or operator.
pub(crate) fn at_node(error: SqlError, expression: &crate::sql::ast::Expr<'_>) -> SqlError {
    let node = core::ptr::from_ref(expression) as usize;
    let query = sublink_query(expression).map(|select| core::ptr::from_ref(select) as usize);
    let source = SOURCE.with(Cell::get);
    let position = (!source.copies.is_null())
        .then(|| {
            // with_statement_source keeps the copy map alive with its text.
            let copies = unsafe { core::slice::from_raw_parts(source.copies, source.copies_len) };
            let at = |address: usize| {
                copies
                    .iter()
                    .find(|copy| copy.len == NODE && copy.address == address)
                    .map(|copy| character_position(source, copy.at + source.copies_base))
            };
            at(node).or_else(|| query.and_then(at))
        })
        .flatten();
    record(&error, position, None);
    error
}

/// [`at_node`] for a missing comparison operator of an `IN` or quantified
/// sublink, with PostgreSQL's hint; other errors pass through.
pub(crate) fn operator_at_node(
    error: SqlError,
    expression: &crate::sql::ast::Expr<'_>,
) -> SqlError {
    if error.sqlstate != crate::sql::eval::sqlstate::UNDEFINED_FUNCTION
        || !error
            .message
            .as_str()
            .starts_with("operator does not exist: ")
    {
        return error;
    }
    let error = at_node(error, expression);
    record(&error, None, Some(BINARY_OPERATOR_HINT));
    error
}

/// Positions a missing comparison operator between a simple `CASE`'s
/// operand and its `WHEN` `value` at the `WHEN` keyword, with PostgreSQL's
/// hint; other errors pass through.
pub(crate) fn operator_at_case_when(
    error: SqlError,
    value: &crate::sql::ast::Expr<'_>,
) -> SqlError {
    if error.sqlstate != crate::sql::eval::sqlstate::UNDEFINED_FUNCTION
        || !error
            .message
            .as_str()
            .starts_with("operator does not exist: ")
    {
        return error;
    }
    let address = core::ptr::from_ref(value) as usize;
    let source = SOURCE.with(Cell::get);
    let position = (!source.copies.is_null())
        .then(|| {
            // with_statement_source keeps the copy map alive with its text.
            let copies = unsafe { core::slice::from_raw_parts(source.copies, source.copies_len) };
            copies
                .iter()
                .find(|copy| copy.len == CASE_WHEN && copy.address == address)
                .map(|copy| character_position(source, copy.at + source.copies_base))
        })
        .flatten();
    record(&error, position, Some(BINARY_OPERATOR_HINT));
    error
}

/// Marks `error` as being about a select-list item: `t.*` at its
/// qualifier, a bare `*` where the parser read it.
pub(crate) fn at_item(error: SqlError, item: &crate::sql::ast::SelectItem<'_>) -> SqlError {
    use crate::sql::ast::SelectItem;
    match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            at_expression(error, expression)
        }
        SelectItem::TableWildcard(qualifier) => at_reference(error, qualifier),
        SelectItem::Wildcard(at) => at_parsed_offset(error, at.get()),
    }
}

/// The query of a sublink expression.
pub(crate) fn sublink_query<'e, 'a>(
    expression: &'e crate::sql::ast::Expr<'a>,
) -> Option<&'e crate::sql::ast::Select<'a>> {
    use crate::sql::ast::Expr;
    match expression {
        Expr::Subquery(select)
        | Expr::ArraySubquery(select)
        | Expr::Exists(select)
        | Expr::RowSubquery { select, .. }
        | Expr::InSubquery { select, .. }
        | Expr::QuantifiedSubquery { select, .. } => Some(select),
        _ => None,
    }
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

/// PostgreSQL's hint for a type without a default operator class.
pub(crate) const DEFAULT_OPERATOR_CLASS_HINT: &str = "You must specify an operator class for the index or define a default operator class for the data type.";

/// Attaches PostgreSQL's `hint` to `error`.
pub(crate) fn with_hint(error: SqlError, hint: &'static str) -> SqlError {
    record(&error, None, Some(hint));
    error
}

/// Positions `error` where the parser recorded `position`.
pub(crate) fn at_source_position(
    error: SqlError,
    position: crate::sql::ast::SourcePosition,
) -> SqlError {
    at_parsed_offset(error, position.get())
}

/// A position at byte `at` of the parsed source.
fn at_parsed_offset(error: SqlError, at: Option<usize>) -> SqlError {
    let source = SOURCE.with(Cell::get);
    let position = at
        .map(|at| at + source.offset_base)
        .filter(|at| !source.text.is_null() && *at <= source.len)
        .map(|at| character_position(source, at));
    record(&error, position, None);
    error
}

/// A position at byte `at` of the parsed source with PostgreSQL's hint.
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
        // An aggregate placed where its query cannot compute it.
        Expr::Call {
            source_position, ..
        } if state == sqlstate::GROUPING_ERROR && message.starts_with("aggregate function") => {
            at_parsed_offset(error, source_position.get())
        }
        Expr::Cast {
            type_name,
            operand,
            source_position,
            ..
        } => {
            if state == sqlstate::UNDEFINED_OBJECT && message.starts_with("type \"") {
                at_subject(error, type_name)
            } else if state == sqlstate::CANNOT_COERCE && message.starts_with("cannot cast type ") {
                at_parsed_offset(error, source_position.get())
            } else if let Expr::Str(literal) = operand
                && (state.starts_with("22")
                    // An object-identifier type's input looks the name up:
                    // any failure of it is the literal's.
                    || crate::sql::types::ColType::from_sql_name(type_name)
                        .is_some_and(crate::sql::types::ColType::is_reg_object)
                        && error.sqlstate.internal_control().is_none())
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
            args,
            ..
        } if state == sqlstate::UNDEFINED_FUNCTION
            && message.starts_with("operator ")
            && crate::sql::ast::catalog_operator_call(name).is_some() =>
        {
            let hint = if args.len() == 1 {
                UNARY_OPERATOR_HINT
            } else {
                BINARY_OPERATOR_HINT
            };
            annotated(error, source_position.get(), hint)
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
        Expr::Call {
            name,
            source_position,
            ..
        } if state == sqlstate::AMBIGUOUS_FUNCTION
            && message.starts_with("function ")
            && message.ends_with(") is not unique") =>
        {
            match source_position.get() {
                Some(at) => annotated(error, Some(at), AMBIGUOUS_FUNCTION_HINT),
                None => {
                    let error = at_subject(error, name);
                    annotated(error, None, AMBIGUOUS_FUNCTION_HINT)
                }
            }
        }
        Expr::Binary {
            source_position, ..
        } if state == sqlstate::UNDEFINED_FUNCTION && message.starts_with("operator ") => {
            annotated(error, source_position.get(), BINARY_OPERATOR_HINT)
        }
        Expr::Binary {
            source_position, ..
        } if state == sqlstate::AMBIGUOUS_FUNCTION
            && message.starts_with("operator is not unique: ") =>
        {
            annotated(error, source_position.get(), AMBIGUOUS_OPERATOR_HINT)
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
            note: known.and_then(|known| known.note),
        }))
    });
}

/// Removes the cursor position recorded for `error`, keeping its hint, so an
/// internal query can report it instead.
pub(crate) fn take_position(error: &SqlError) -> Option<usize> {
    let message_hash = message_hash(&error.message);
    PENDING.with(|pending| {
        let mut known = pending.get().filter(|known| {
            known.sqlstate == error.sqlstate && known.message_hash == message_hash
        })?;
        let position = known.position.take();
        pending.set(Some(known));
        position
    })
}

/// Positions a syntax error at byte `cursor` of a query a routine runs, the
/// text it was parsed from. Client statements report their syntax error
/// cursor directly.
pub(crate) fn internal_parse_cursor(error: &SqlError, cursor: usize) {
    let source = SOURCE.with(Cell::get);
    if source.internal && cursor <= source.len {
        record(error, Some(character_position(source, cursor)), None);
    }
}

/// Runs `probe`, an evaluation whose own errors are discarded, keeping the
/// position, hint and diagnostic recorded for the error being reported.
pub(crate) fn preserving_pending<R>(probe: impl FnOnce() -> R) -> R {
    let pending = PENDING.with(Cell::get);
    let diagnostic = crate::sql::eval::take_diagnostic();
    let result = probe();
    PENDING.with(|slot| slot.set(pending));
    let _ = crate::sql::eval::take_diagnostic();
    if let Some(diagnostic) = diagnostic {
        crate::sql::eval::stash_full_diagnostic(diagnostic);
    }
    result
}

/// Restores a position [`take_position`] removed.
pub(crate) fn restore_position(error: &SqlError, position: usize) {
    record(error, Some(position), None);
}

/// Runs `run` with `text`, a query a routine runs, as the source its
/// analysis errors are positioned in.
pub(crate) fn with_internal_source<R>(text: &str, run: impl FnOnce() -> R) -> R {
    let _scope = SourceScope(
        SOURCE.with(|source| {
            source.replace(Source {
                text: text.as_ptr(),
                len: text.len(),
                copies: (&[] as &[SourceCopy]).as_ptr(),
                copies_len: 0,
                offset_base: 0,
                copies_base: 0,
                internal: true,
            })
        }),
        ANALYZED.with(|flag| flag.replace(true)),
    );
    run()
}

/// Runs `run` positioning subjects inside `copy`, a verbatim copy of
/// `written`, where `written` appears in the installed statement text.
pub(crate) fn with_copied_source<R>(written: &str, copy: &str, run: impl FnOnce() -> R) -> R {
    let Some((at, current)) = locate(written) else {
        return run();
    };
    let copies = [SourceCopy {
        address: copy.as_ptr() as usize,
        len: copy.len(),
        at,
    }];
    let _scope = SourceScope(
        SOURCE.with(|installed| {
            installed.replace(Source {
                copies: copies.as_ptr(),
                copies_len: copies.len(),
                ..current
            })
        }),
        ANALYZED.with(Cell::get),
    );
    run()
}

/// Runs `run` positioning relation and column lookups in the installed
/// statement text, for a utility statement that analyses queries written
/// inside it (`CREATE RULE`).
pub(crate) fn with_analysis<R>(run: impl FnOnce() -> R) -> R {
    let _scope = SourceScope(
        SOURCE.with(Cell::get),
        ANALYZED.with(|flag| flag.replace(true)),
    );
    run()
}

/// Runs `run` positioning relation and column lookups in the installed
/// statement text, for queries parsed from `text`, a slice of it written
/// inside a utility statement (a `CREATE FUNCTION ... BEGIN ATOMIC` body):
/// their parsed offsets count from the slice's start.
pub(crate) fn with_analysis_in<R>(text: &str, run: impl FnOnce() -> R) -> R {
    with_analysis_of_parsed(text, None, run)
}

/// [`with_analysis_in`] for a statement parsed on its own from `text`, whose
/// parser recorded `copies` relative to `text`.
pub(crate) fn with_analysis_of_parsed<R>(
    text: &str,
    copies: Option<&[SourceCopy]>,
    run: impl FnOnce() -> R,
) -> R {
    let current = SOURCE.with(Cell::get);
    let start = current.text as usize;
    let address = text.as_ptr() as usize;
    let mut source = current;
    if !current.text.is_null() && address >= start && address + text.len() <= start + current.len {
        source.offset_base = address - start;
        if let Some(copies) = copies {
            source.copies = copies.as_ptr();
            source.copies_len = copies.len();
            source.copies_base = address - start;
        }
    }
    let _scope = SourceScope(
        SOURCE.with(|installed| installed.replace(source)),
        ANALYZED.with(|flag| flag.replace(true)),
    );
    run()
}

/// Names `text`, a dynamic query a routine has just built, as the internal
/// source of the enclosing [`with_internal_source`] scope.
pub(crate) fn set_internal_source(text: &str) {
    SOURCE.with(|source| {
        let mut current = source.get();
        if current.internal {
            current.text = text.as_ptr();
            current.len = text.len();
            source.set(current);
        }
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
            note: pending.note,
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
    if is_record_field_failure(&error) {
        return error;
    }
    if matches!(
        error.sqlstate.as_str(),
        sqlstate::UNDEFINED_COLUMN
            | sqlstate::UNDEFINED_TABLE
            | sqlstate::AMBIGUOUS_COLUMN
            | sqlstate::GROUPING_ERROR
    ) {
        let error = at_reference(error, qualifier.unwrap_or(name));
        // A reference no FROM item answers still searches the earlier
        // branches of an enclosing set operation for a suggestion.
        if crate::sql::column_suggestion::set_branches_in_reach() && !has_pending_note(&error) {
            with_column_suggestion(
                error,
                &crate::sql::column_suggestion::ColumnSuggestion::new(qualifier, name),
            )
        } else {
            error
        }
    } else {
        error
    }
}

/// A procedural record's missing field: PostgreSQL resolves the field while
/// planning the expression that names it and reports it without a position.
pub(crate) fn is_record_field_failure(error: &SqlError) -> bool {
    let message = error.message.as_str();
    error.sqlstate == crate::sql::eval::sqlstate::UNDEFINED_COLUMN
        && message.starts_with("record \"")
        && message.contains("\" has no field \"")
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
    sqlstate: impl Into<crate::sql::eval::SqlState>,
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

/// Keeps `text`, a complete message saved from an earlier report, for
/// `error`, whose inline message is its start.
pub(crate) fn keep_full_message(error: &SqlError, text: &str) {
    FULL_MESSAGE.with(|full| {
        let mut full = full.borrow_mut();
        full.text.clear();
        let _ = core::fmt::Write::write_str(&mut full.text, text);
        full.sqlstate = Some(error.sqlstate);
        full.message_hash = message_hash(&error.message);
    })
}

/// Runs `read` with the complete message kept for `error`, if any, keeping
/// it for the error's report.
pub(crate) fn peek_full_message<R>(error: &SqlError, read: impl FnOnce(Option<&str>) -> R) -> R {
    FULL_MESSAGE.with(|full| {
        let full = full.borrow();
        let matches = full.sqlstate.is_some_and(|state| state == error.sqlstate)
            && full.message_hash == message_hash(&error.message);
        read(matches.then_some(full.text.as_str()))
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

/// A syntax error in a routine body written in the statement text, at its
/// place in that text, as PostgreSQL's `function_parse_error_transpose`
/// reports it. `body` is the parsed body and `error` indexes it.
pub(crate) fn body_parse_error(error: &crate::sql::parser::ParseError, body: &str) -> SqlError {
    let sql_error = crate::sql::parse_error_to_sql(error);
    let position = error.cursor.and_then(|cursor| body_position(cursor, body));
    record(&sql_error, position, error.hint);
    sql_error
}

/// An analysis error positioned in a routine body (the internal source it
/// was analysed under), moved to the body's place in the statement text.
/// `body` is the body as written in that text.
pub(crate) fn body_analysis_error(error: SqlError, analysed: &str, body: &str) -> SqlError {
    let Some(position) = take_position(&error) else {
        return error;
    };
    let cursor = analysed
        .char_indices()
        .nth(position.saturating_sub(1))
        .map_or(analysed.len(), |(at, _)| at);
    if let Some(position) = body_position(cursor, body) {
        record(&error, Some(position), None);
    }
    error
}

/// The one-based character position in the statement text of byte `cursor`
/// of `body`, a string constant written in it.
fn body_position(cursor: usize, body: &str) -> Option<usize> {
    {
        let (token_at, source) = locate(body)?;
        let text = unsafe { core::slice::from_raw_parts(source.text, source.len) };
        let at = match text.get(token_at) {
            // A dollar-quoted body is written verbatim after its tag.
            Some(b'$') => {
                let tag = text[token_at + 1..].iter().position(|byte| *byte == b'$')?;
                token_at + tag + 2 + cursor
            }
            // A plain string literal: each doubled quote is one body quote.
            Some(b'\'') => {
                let mut at = token_at + 1;
                for _ in 0..cursor {
                    at += if text.get(at) == Some(&b'\'') { 2 } else { 1 };
                }
                at
            }
            _ => return None,
        };
        (at <= source.len).then(|| character_position(source, at))
    }
}
