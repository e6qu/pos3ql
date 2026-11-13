//! PostgreSQL's error context: the `CONTEXT` lines (ErrorResponse `W`) that
//! name each procedural statement, routine and query active when an error or
//! notice is raised, and the internal query (`q`, `p`) that positions an
//! error inside a query a routine runs.
//!
//! PostgreSQL computes context while raising an error, from callbacks pushed
//! by every active routine and query. Here each active routine or query is a
//! [`Frame`] linked through the Rust stack the same way. Errors propagate as
//! values, so the first frame an error leaves records the context of the
//! whole active stack in a worker-local snapshot keyed by the error's
//! SQLSTATE and message; frames further out see the snapshot is taken. The
//! wire responder and exception handlers take the snapshot with the error.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};

use crate::sql::eval::{SqlError, SqlState, sqlstate};
use crate::util::StackStr;

/// The longest context an error reports. A longer context, such as one from
/// very deep recursion, is reported as a 54000 error naming this bound.
pub(crate) const CONTEXT_BYTES: usize = 64 * 1024;

/// The longest internal query an error reports, under the same rule.
pub(crate) const INTERNAL_QUERY_BYTES: usize = 64 * 1024;

/// One active routine or query.
pub(crate) trait Frame {
    /// The text an error's cursor is positioned in while this frame runs, if
    /// a positioned error becomes this frame's internal query.
    fn query(&self) -> Option<&str> {
        None
    }

    /// Writes this frame's context line, or nothing when PostgreSQL adds
    /// none. `positioned` says the error's cursor became this frame's
    /// internal query position.
    fn write_line(&self, out: &mut dyn Write, positioned: bool) -> fmt::Result;

    /// The text of the assignment statement this frame is running, whose
    /// expressions PostgreSQL evaluates as one query.
    fn assignment(&self) -> Option<&str> {
        None
    }

    /// For a PL/pgSQL invocation: the statement it runs when a `STRICT`
    /// failure there reports its parameters, else `Some(None)`. Other frames
    /// answer `None`.
    fn strict_parameters(&self) -> Option<Option<&str>> {
        None
    }

    /// Names the dynamic query this frame runs; false if it runs none.
    fn begin_dynamic(&self, _text: *const str) -> bool {
        false
    }
}

/// A dynamic query (`EXECUTE`) whose text is known once its string has been
/// evaluated: `SQL statement "..."` from then on.
pub(crate) struct DynamicQueryFrame {
    text: Cell<Option<*const str>>,
}

impl DynamicQueryFrame {
    pub(crate) const fn new() -> Self {
        Self {
            text: Cell::new(None),
        }
    }

    fn text(&self) -> Option<&str> {
        // `begin_dynamic` stores a query that lives until the statement ends,
        // which outlasts this frame.
        self.text.get().map(|text| unsafe { &*text })
    }
}

impl Frame for DynamicQueryFrame {
    fn query(&self) -> Option<&str> {
        self.text()
    }

    fn write_line(&self, out: &mut dyn Write, positioned: bool) -> fmt::Result {
        match self.text() {
            Some(text) if !positioned => write!(out, "SQL statement \"{text}\""),
            _ => Ok(()),
        }
    }

    fn begin_dynamic(&self, text: *const str) -> bool {
        self.text.set(Some(text));
        true
    }
}

/// Names `text` as the query of the innermost frame when it is a
/// [`DynamicQueryFrame`], and positions analysis errors in it.
pub(crate) fn begin_dynamic_query(text: &str) {
    let node = HEAD.with(Cell::get);
    // Linked nodes and frames outlive their links.
    if !node.is_null() && unsafe { &*(*node).frame }.begin_dynamic(text) {
        crate::sql::error_position::set_internal_source(text);
    }
}

/// How PostgreSQL describes a query a procedural statement runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMode {
    /// `SQL statement "..."`.
    Statement,
    /// `PL/pgSQL expression "..."`.
    Expression,
    /// `PL/pgSQL assignment "..."`.
    Assignment,
}

/// A query a procedural statement runs. PostgreSQL evaluates a simple
/// expression (one value, no table, subquery, aggregate or set-returning
/// call) without its query machinery, so an error raised while one runs
/// adds no line for it; analysis errors still report it as the internal
/// query.
pub(crate) struct QueryFrame<'a> {
    text: &'a str,
    mode: QueryMode,
    simple: bool,
    /// The error being recorded was raised planning the query.
    planned: Cell<bool>,
}

impl<'a> QueryFrame<'a> {
    pub(crate) const fn new(text: &'a str, mode: QueryMode, simple: bool) -> Self {
        Self {
            text,
            mode,
            simple,
            planned: Cell::new(false),
        }
    }

    /// Marks the error about to be recorded as raised while planning, when a
    /// simple expression is named like any other query.
    pub(crate) fn planned(&self) {
        self.planned.set(true);
    }
}

impl Frame for QueryFrame<'_> {
    fn query(&self) -> Option<&str> {
        Some(self.text)
    }

    fn write_line(&self, out: &mut dyn Write, positioned: bool) -> fmt::Result {
        if positioned || (self.simple && !self.planned.get()) {
            return Ok(());
        }
        let what = match self.mode {
            QueryMode::Statement => "SQL statement",
            QueryMode::Expression => "PL/pgSQL expression",
            QueryMode::Assignment => "PL/pgSQL assignment",
        };
        write!(out, "{what} \"{}\"", self.text)
    }
}

/// A SQL-language function's body: `SQL function "f" statement N`, or
/// `during startup` before its first statement runs.
pub(crate) struct SqlFunctionFrame<'a> {
    pub(crate) name: &'a str,
    pub(crate) body: &'a str,
    /// One-based index of the running statement; zero before the first.
    pub(crate) statement: Cell<usize>,
    pub(crate) inlining: SqlFunctionInlining,
}

/// How PostgreSQL's planner treats a SQL-language function call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlFunctionInlining {
    /// The body runs as its own statements.
    Executed,
    /// The body is inlined into its caller's query: the call has no frame.
    Inlined,
    /// The body is inlined with constant arguments and folded while it is
    /// inlined: `SQL function "f" during inlining`.
    Folded,
}

impl Frame for SqlFunctionFrame<'_> {
    fn query(&self) -> Option<&str> {
        (self.inlining != SqlFunctionInlining::Inlined).then_some(self.body)
    }

    fn write_line(&self, out: &mut dyn Write, _positioned: bool) -> fmt::Result {
        match (self.inlining, self.statement.get()) {
            (SqlFunctionInlining::Inlined, _) => Ok(()),
            (SqlFunctionInlining::Folded, _) => {
                write!(out, "SQL function \"{}\" during inlining", self.name)
            }
            (SqlFunctionInlining::Executed, 0) => {
                write!(out, "SQL function \"{}\" during startup", self.name)
            }
            (SqlFunctionInlining::Executed, statement) => write!(
                out,
                "SQL function \"{}\" statement {}",
                self.name, statement
            ),
        }
    }
}

/// A SQL-language function body validated at `CREATE FUNCTION`: an error
/// that cannot point into the statement names the function.
pub(crate) struct SqlFunctionValidationFrame<'a> {
    pub(crate) name: &'a str,
}

impl Frame for SqlFunctionValidationFrame<'_> {
    fn write_line(&self, out: &mut dyn Write, positioned: bool) -> fmt::Result {
        if positioned {
            return Ok(());
        }
        write!(out, "SQL function \"{}\"", self.name)
    }
}

/// One COPY FROM input row: `CopyFromErrorCallback`'s context line.
/// The connection creates one per input row and reads it after an error:
/// a column still set names the input conversion that failed, the only error
/// `ON_ERROR ignore` skips.
pub(crate) struct CopyFrame<'a> {
    pub(crate) relation: Cell<StackStr<64>>,
    pub(crate) line: u64,
    pub(crate) binary: bool,
    /// The input row while it still describes the error (`line_buf_valid`).
    pub(crate) text: Cell<Option<&'a str>>,
    /// The column being converted and its input, `None` for NULL.
    pub(crate) column: Cell<Option<(StackStr<64>, Option<&'a str>)>>,
}

impl CopyFrame<'_> {
    pub(crate) const fn new(line: u64, binary: bool) -> Self {
        Self {
            relation: Cell::new(StackStr::new()),
            line,
            binary,
            text: Cell::new(None),
            column: Cell::new(None),
        }
    }
}

/// The input conversion a COPY row failed in, kept past the row's input.
#[derive(Clone, Copy)]
pub(crate) struct CopyConversion {
    relation: StackStr<64>,
    line: u64,
    column: StackStr<64>,
    /// The input as `CopyLimitPrintoutLength` shows it; `None` for NULL.
    value: Option<StackStr<128>>,
}

impl CopyFrame<'_> {
    /// The failed conversion, when the row's error came from one.
    pub(crate) fn conversion(&self) -> Option<CopyConversion> {
        let (column, value) = self.column.get()?;
        Some(CopyConversion {
            relation: self.relation.get(),
            line: self.line,
            column,
            value: value.map(|value| {
                let mut shown = StackStr::new();
                let _ = write!(shown, "{}", CopyData(value));
                shown
            }),
        })
    }
}

impl CopyConversion {
    /// The row's `CONTEXT` line.
    pub(crate) fn context(&self) -> StackStr<512> {
        let mut line = StackStr::new();
        let _ = write!(
            line,
            "COPY {}, line {}, column {}",
            self.relation.as_str(),
            self.line,
            self.column.as_str()
        );
        let _ = match &self.value {
            Some(value) => write!(line, ": \"{}\"", value.as_str()),
            None => line.write_str(": null input"),
        };
        line
    }

    /// `ON_ERROR ignore`'s verbose notice for the skipped row.
    pub(crate) fn skipped_notice(&self) -> StackStr<512> {
        let mut notice = StackStr::new();
        let _ = write!(
            notice,
            "skipping row due to data type incompatibility at line {} for column \"{}\": ",
            self.line,
            self.column.as_str()
        );
        let _ = match &self.value {
            Some(value) => write!(notice, "\"{}\"", value.as_str()),
            None => notice.write_str("null input"),
        };
        notice
    }
}

/// `CopyLimitPrintoutLength`: at most 100 bytes of COPY data, cut at a
/// character boundary and marked with `...`.
struct CopyData<'a>(&'a str);

impl fmt::Display for CopyData<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX_COPY_DATA_DISPLAY: usize = 100;
        if self.0.len() <= MAX_COPY_DATA_DISPLAY {
            return out.write_str(self.0);
        }
        let mut end = MAX_COPY_DATA_DISPLAY;
        while !self.0.is_char_boundary(end) {
            end -= 1;
        }
        write!(out, "{}...", &self.0[..end])
    }
}

impl Frame for CopyFrame<'_> {
    fn write_line(&self, out: &mut dyn Write, _positioned: bool) -> fmt::Result {
        write!(
            out,
            "COPY {}, line {}",
            self.relation.get().as_str(),
            self.line
        )?;
        match (self.binary, self.column.get(), self.text.get()) {
            (true, Some((column, _)), _) => write!(out, ", column {}", column.as_str()),
            (true, None, _) | (false, None, None) => Ok(()),
            (false, Some((column, Some(value))), _) => {
                write!(out, ", column {}: \"{}\"", column.as_str(), CopyData(value))
            }
            (false, Some((column, None)), _) => {
                write!(out, ", column {}: null input", column.as_str())
            }
            (false, None, Some(text)) => write!(out, ": \"{}\"", CopyData(text)),
        }
    }
}

/// A PL/pgSQL function compiled for a call it cannot serve, as PostgreSQL's
/// `plpgsql_compile_callback` reports such an error.
pub(crate) struct PlpgsqlCompilationFrame<'a> {
    pub(crate) name: &'a str,
}

impl Frame for PlpgsqlCompilationFrame<'_> {
    fn write_line(&self, out: &mut dyn Write, _positioned: bool) -> fmt::Result {
        write!(
            out,
            "compilation of PL/pgSQL function \"{}\" near line 1",
            self.name
        )
    }
}

/// A PL/pgSQL declaration's type that does not parse, as PostgreSQL's
/// `parse_datatype` names it.
pub(crate) struct PlpgsqlTypeNameFrame<'a> {
    pub(crate) text: &'a str,
}

impl Frame for PlpgsqlTypeNameFrame<'_> {
    fn write_line(&self, out: &mut dyn Write, _positioned: bool) -> fmt::Result {
        write!(out, "invalid type name \"{}\"", self.text)
    }
}

struct Node {
    frame: *const (dyn Frame + 'static),
    previous: *const Node,
}

std::thread_local! {
    static HEAD: Cell<*const Node> = const { Cell::new(core::ptr::null()) };
    static SNAPSHOT: RefCell<Snapshot> = const { RefCell::new(Snapshot::EMPTY) };
}

/// Runs `run` with `frame` active.
pub(crate) fn with_frame<R>(frame: &dyn Frame, run: impl FnOnce() -> R) -> R {
    let node = Node {
        // The node is unlinked below before `frame`'s borrow ends.
        frame: unsafe {
            core::mem::transmute::<*const (dyn Frame + '_), *const (dyn Frame + 'static)>(frame)
        },
        previous: HEAD.with(Cell::get),
    };
    struct Unlink(*const Node);
    impl Drop for Unlink {
        fn drop(&mut self) {
            HEAD.with(|head| head.set(self.0));
        }
    }
    HEAD.with(|head| head.set(&node));
    let _unlink = Unlink(node.previous);
    run()
}

/// Like [`with_frame`], recording the context of an error `run` returns
/// while `frame` is still active.
pub(crate) fn within<T>(
    frame: &dyn Frame,
    run: impl FnOnce() -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    with_frame(frame, || run().map_err(capture))
}

/// Runs `run` outside the innermost frame, a query whose statement goes on
/// after the query ends, recording the context of an error it returns.
pub(crate) fn outside_query<T>(run: impl FnOnce() -> Result<T, SqlError>) -> Result<T, SqlError> {
    let head = HEAD.with(Cell::get);
    if head.is_null() {
        return run();
    }
    struct Relink(*const Node);
    impl Drop for Relink {
        fn drop(&mut self) {
            HEAD.with(|head| head.set(self.0));
        }
    }
    // The innermost node stays linked to its frame until `Relink` restores it.
    HEAD.with(|current| current.set(unsafe { (*head).previous }));
    let _relink = Relink(head);
    run().map_err(capture)
}

struct Snapshot {
    key: Option<(SqlState, u64)>,
    context: StackStr<CONTEXT_BYTES>,
    query: StackStr<INTERNAL_QUERY_BYTES>,
    /// One-based character position in `query`; `None` when the error has
    /// no internal query.
    position: Option<usize>,
}

impl Snapshot {
    const EMPTY: Self = Self {
        key: None,
        context: StackStr::new(),
        query: StackStr::new(),
        position: None,
    };
}

fn message_hash(message: &dyn fmt::Display) -> u64 {
    struct Hasher(u64);
    impl Write for Hasher {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            for byte in text.bytes() {
                // FNV-1a.
                self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
            Ok(())
        }
    }
    let mut hasher = Hasher(0xcbf2_9ce4_8422_2325);
    let _ = write!(hasher, "{message}");
    hasher.0
}

fn key(error: &SqlError) -> (SqlState, u64) {
    (error.sqlstate, message_hash(&error.message))
}

fn same_key(left: Option<(SqlState, u64)>, right: (SqlState, u64)) -> bool {
    left.is_some_and(|(state, hash)| state == right.0 && hash == right.1)
}

/// Separates lines written by successive frames.
struct Lines<'o> {
    out: &'o mut dyn Write,
    started: bool,
    pending_line: bool,
}

impl Write for Lines<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.is_empty() {
            return Ok(());
        }
        if self.pending_line {
            if self.started {
                self.out.write_char('\n')?;
            }
            self.started = true;
            self.pending_line = false;
        }
        self.out.write_str(text)
    }
}

/// Walks the active frames from the innermost, writing their lines.
/// `position` is the error's cursor, which the first frame that runs a
/// query takes as its internal query position.
fn write_frames(out: &mut dyn Write, mut position: Option<usize>) -> Result<Internal, fmt::Error> {
    let mut lines = Lines {
        out,
        started: false,
        pending_line: false,
    };
    let mut internal = None;
    let mut node = HEAD.with(Cell::get);
    while !node.is_null() {
        // Every linked node and frame outlives its link (see `with_frame`).
        let current = unsafe { &*node };
        let frame = unsafe { &*current.frame };
        let positioned = match (position, frame.query()) {
            (Some(at), Some(query)) => {
                internal = Some((query as *const str, at));
                position = None;
                true
            }
            _ => false,
        };
        lines.pending_line = true;
        frame.write_line(&mut lines, positioned)?;
        node = current.previous;
    }
    Ok(Internal {
        query: internal,
        unclaimed: position,
    })
}

struct Internal {
    query: Option<(*const str, usize)>,
    unclaimed: Option<usize>,
}

/// Records the context of `error`, raised while the active frames run,
/// unless it already has one. Returns the error to propagate: a context or
/// internal query too long to report becomes a 54000 error.
pub(crate) fn capture(error: SqlError) -> SqlError {
    if error.sqlstate.internal_control().is_some() || HEAD.with(Cell::get).is_null() {
        return error;
    }
    let error_key = key(&error);
    if SNAPSHOT.with(|snapshot| same_key(snapshot.borrow().key, error_key)) {
        return error;
    }
    let position = crate::sql::error_position::take_position(&error);
    SNAPSHOT.with(|snapshot| {
        let mut snapshot = snapshot.borrow_mut();
        snapshot.key = None;
        snapshot.context.clear();
        snapshot.query.clear();
        snapshot.position = None;
        let written = write_frames(&mut snapshot.context, position);
        let Ok(internal) = written else {
            return context_full();
        };
        if snapshot.context.is_truncated() {
            return context_full();
        }
        if let Some(at) = internal.unclaimed {
            crate::sql::error_position::restore_position(&error, at);
        }
        if let Some((query, at)) = internal.query {
            // The frame that owns `query` is still active.
            let query = unsafe { &*query };
            snapshot.query = StackStr::from_str(query);
            if snapshot.query.is_truncated() {
                return internal_query_full();
            }
            snapshot.position = Some(at);
        }
        snapshot.key = Some(error_key);
        error
    })
}

fn context_full() -> SqlError {
    crate::sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "error context exceeds {} bytes",
        CONTEXT_BYTES
    )
}

fn internal_query_full() -> SqlError {
    crate::sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "error internal query exceeds {} bytes",
        INTERNAL_QUERY_BYTES
    )
}

/// An error's recorded context and internal query.
pub(crate) struct Reported<'s> {
    pub(crate) context: &'s str,
    /// The internal query and the one-based position in it.
    pub(crate) internal: Option<(&'s str, usize)>,
}

/// Runs `report` with the context recorded for the error being reported,
/// then forgets it.
pub(crate) fn with_reported<R>(
    sqlstate: &str,
    message: &dyn fmt::Display,
    report: impl FnOnce(Option<Reported<'_>>) -> R,
) -> R {
    SNAPSHOT.with(|snapshot| {
        let mut snapshot = snapshot.borrow_mut();
        let matches = snapshot
            .key
            .is_some_and(|(state, hash)| state == sqlstate && hash == message_hash(message));
        snapshot.key = None;
        if !matches {
            return report(None);
        }
        let reported = Reported {
            context: snapshot.context.as_str(),
            internal: snapshot
                .position
                .map(|position| (snapshot.query.as_str(), position)),
        };
        report(Some(reported))
    })
}

/// Takes the context recorded for `error`, for an exception handler.
pub(crate) fn take<R>(error: &SqlError, take: impl FnOnce(Option<Reported<'_>>) -> R) -> R {
    with_reported(error.sqlstate.as_str(), &error.message, take)
}

/// Records `reported` as the context of `error`, which an exception handler
/// raises again.
pub(crate) fn restore(error: &SqlError, context: &str, internal: Option<(&str, usize)>) {
    SNAPSHOT.with(|snapshot| {
        let mut snapshot = snapshot.borrow_mut();
        snapshot.context = StackStr::from_str(context);
        snapshot.query = StackStr::from_str(internal.map_or("", |(query, _)| query));
        snapshot.position = internal.map(|(_, position)| position);
        snapshot.key = Some(key(error));
    });
}

/// Forgets any context left by an earlier statement.
pub(crate) fn reset() {
    SNAPSHOT.with(|snapshot| snapshot.borrow_mut().key = None);
}

/// The context of the active frames, as a notice or `PG_CONTEXT` reports it.
pub(crate) struct Live;

impl fmt::Display for Live {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_frames(out, None).map(|_| ())
    }
}

/// The assignment statement the innermost frame is running.
pub(crate) fn innermost_assignment<'s>() -> Option<&'s str> {
    let node = HEAD.with(Cell::get);
    if node.is_null() {
        return None;
    }
    // The innermost frame outlives the expression evaluation asking for it.
    let frame = unsafe { &*(*node).frame };
    frame
        .assignment()
        .map(|text| unsafe { &*(text as *const str) })
}

/// The statement whose `STRICT` failure reports its parameters: the
/// innermost PL/pgSQL invocation's, when `#print_strict_params` is on.
pub(crate) fn strict_parameters_statement<'s>() -> Option<&'s str> {
    let mut node = HEAD.with(Cell::get);
    while !node.is_null() {
        // Active frames outlive the statement asking for them.
        let frame = unsafe { &*(*node).frame };
        if let Some(statement) = frame.strict_parameters() {
            return statement.map(|text| unsafe { &*(text as *const str) });
        }
        node = unsafe { (*node).previous };
    }
    None
}

/// Whether any frame is active, so a notice has a context.
pub(crate) fn is_active() -> bool {
    !HEAD.with(Cell::get).is_null()
}
