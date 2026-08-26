//! Recursive-descent parser (Pratt for expressions) into the arena AST.
//!
//! Fixed limits, checked loudly: at most [`MAX_LIST`] items per select
//! list / column list / VALUES row, and [`MAX_ROWS`] rows per INSERT.

use crate::mem::arena::Arena;
use crate::sql::eval::sqlstate;
use crate::stack_format;
use crate::util::StackStr;

use super::ast::*;
use super::lexer::{LexError, Lexer, Tok};
use super::types::{INTERVAL_FULL_RANGE, TypeMod};

/// Names for the calls a desugaring produces, for syntax PostgreSQL does not
/// also expose as a function. A space cannot appear in an identifier, so a
/// query cannot reach these by writing them, and the function router will not
/// answer to `similar_to(...)` or `overlaps(...)` — which PostgreSQL refuses.
/// Any future desugaring of syntax-only constructs belongs here too.
pub(crate) const SIMILAR_TO: &str = "similar to";
pub(crate) const OVERLAPS_PERIODS: &str = "overlaps periods";

pub const MAX_LIST: usize = 64;

pub const MAX_CTES: usize = 16;
/// Maximum number of `FOR UPDATE`/`FOR SHARE`/… clauses on one query.
pub const MAX_LOCK_CLAUSES: usize = 8;
/// Upper bound on `WINDOW name AS (...)` definitions in one SELECT.
pub const MAX_WINDOW_DEFS: usize = 16;
/// Upper bound on warnings one statement's parse may raise.
pub const MAX_PARSE_WARNINGS: usize = 8;
type OrderLimit<'a> = (
    &'a [OrderBy<'a>],
    Option<&'a Expr<'a>>,
    Option<&'a Expr<'a>>,
    bool,
);
/// Upper bound on the number of grouping sets a single `GROUP BY` may expand to
/// (after ROLLUP/CUBE expansion and cross-multiplication). Exceeding it is a
/// loud error, never silent truncation.
pub const MAX_GROUPING_SETS: usize = 256;

/// Appends a grouping-set bitmask, failing loudly when the fixed buffer fills.
fn push_mask(
    buf: &mut [u64],
    n: &mut usize,
    mask: u64,
    err: impl FnOnce() -> ParseError,
) -> Result<(), ParseError> {
    if *n == buf.len() {
        return Err(err());
    }
    buf[*n] = mask;
    *n += 1;
    Ok(())
}
/// Upper bound on subcommands in one comma-separated ALTER TABLE.
pub const MAX_ALTER_ACTIONS: usize = 32;

/// PostgreSQL executes ALTER TABLE subcommands in a fixed pass order rather
/// than the written order: drops first, then column-type changes, then column
/// adds, then constraint adds, then column-attribute changes. This returns the
/// pass number the parser sorts by so that, e.g., an ADD CONSTRAINT can
/// reference a column ADDed later in the same statement. The standalone forms
/// (RENAME/SET SCHEMA) never share a list, so their pass is irrelevant.
fn alter_pass(action: &AlterAction) -> u8 {
    match action {
        AlterAction::DropColumn { .. } | AlterAction::DropConstraint { .. } => 0,
        AlterAction::AlterColumnType { .. } => 1,
        AlterAction::AddColumn(_) => 2,
        AlterAction::AddConstraint(_)
        | AlterAction::AlterConstraint { .. }
        | AlterAction::ValidateConstraint(_) => 3,
        AlterAction::SetDefault { .. }
        | AlterAction::DropDefault { .. }
        | AlterAction::SetNotNull { .. }
        | AlterAction::DropNotNull { .. }
        | AlterAction::AddIdentity { .. }
        | AlterAction::DropIdentity { .. } => 4,
        // Standalone forms; never appear in a multi-action list.
        AlterAction::RenameTable(_)
        | AlterAction::RenameColumn { .. }
        | AlterAction::RenameConstraint { .. }
        | AlterAction::SetTriggerEnabled { .. }
        | AlterAction::SetRowLevelSecurity(_)
        | AlterAction::AttachPartition { .. }
        | AlterAction::DetachPartition { .. }
        | AlterAction::SetSchema(_) => 5,
    }
}

pub const MAX_ROWS: usize = 256;

/// Words that cannot appear as a bare column reference; mirrors the
/// reserved entries of PostgreSQL's keyword table that this grammar uses.
/// Whether a numeric token carries a `0x`/`0o`/`0b` base prefix.
/// Whether `text` mentions the word `window` in any case — the cheap pre-filter
/// that keeps the WINDOW-clause lookahead off the path of ordinary queries.
fn mentions_window(text: &str) -> bool {
    text.as_bytes()
        .windows(6)
        .any(|w| w.eq_ignore_ascii_case(b"window"))
}

fn is_base_prefixed(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() > 2 && b[0] == b'0' && matches!(b[1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
}

/// The PostgreSQL keyword categories that constrain where a word may be used
/// unquoted. The fourth category, plain `unreserved`, behaves exactly like a
/// non-keyword in both places this matters, so it is deliberately absent —
/// `None` covers it.
///
/// Provenance: `SELECT word, catcode FROM pg_get_keywords()` on PostgreSQL
/// 18.4 (Homebrew).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keyword {
    /// `unreserved (cannot be function or type name)` — legal as a column or
    /// table name, so only identifier quoting cares.
    ColumnName,
    /// `reserved (can be function or type name)`.
    TypeFuncName,
    /// `reserved`.
    Reserved,
}

/// Categorizes `word`, or `None` when it is unreserved or not a keyword at all.
pub(crate) fn keyword_category(word: &str) -> Option<Keyword> {
    Some(match word {
        "between" | "bigint" | "bit" | "boolean" | "char" | "character" | "coalesce" | "dec"
        | "decimal" | "exists" | "extract" | "float" | "greatest" | "grouping" | "inout"
        | "int" | "integer" | "interval" | "json" | "json_array" | "json_arrayagg"
        | "json_exists" | "json_object" | "json_objectagg" | "json_query" | "json_scalar"
        | "json_serialize" | "json_table" | "json_value" | "least" | "merge_action"
        | "national" | "nchar" | "none" | "normalize" | "nullif" | "numeric" | "out"
        | "overlay" | "position" | "precision" | "real" | "row" | "setof" | "smallint"
        | "substring" | "time" | "timestamp" | "treat" | "trim" | "values" | "varchar"
        | "xmlattributes" | "xmlconcat" | "xmlelement" | "xmlexists" | "xmlforest"
        | "xmlnamespaces" | "xmlparse" | "xmlpi" | "xmlroot" | "xmlserialize" | "xmltable" => {
            Keyword::ColumnName
        }
        "authorization" | "binary" | "collation" | "concurrently" | "cross" | "current_schema"
        | "freeze" | "full" | "ilike" | "inner" | "is" | "isnull" | "join" | "left" | "like"
        | "natural" | "notnull" | "outer" | "overlaps" | "right" | "similar" | "tablesample"
        | "verbose" => Keyword::TypeFuncName,
        "all" | "analyse" | "analyze" | "and" | "any" | "array" | "as" | "asc" | "asymmetric"
        | "both" | "case" | "cast" | "check" | "collate" | "column" | "constraint" | "create"
        | "current_catalog" | "current_date" | "current_role" | "current_time"
        | "current_timestamp" | "current_user" | "default" | "deferrable" | "desc" | "distinct"
        | "do" | "else" | "end" | "except" | "false" | "fetch" | "for" | "foreign" | "from"
        | "grant" | "group" | "having" | "in" | "initially" | "intersect" | "into" | "lateral"
        | "leading" | "limit" | "localtime" | "localtimestamp" | "not" | "null" | "offset"
        | "on" | "only" | "or" | "order" | "placing" | "primary" | "references" | "returning"
        | "select" | "session_user" | "some" | "symmetric" | "system_user" | "table" | "then"
        | "to" | "trailing" | "true" | "union" | "unique" | "user" | "using" | "variadic"
        | "when" | "where" | "window" | "with" => Keyword::Reserved,
        _ => return None,
    })
}

/// Whether `word` is a keyword PostgreSQL refuses in a `ColId` position — a
/// column name, a table name, or a FROM-item alias. Its two reserved categories
/// are rejected there; the unreserved ones are accepted — `begin`, `values` and
/// `set` are all perfectly legal column names.
pub(crate) fn is_column_name_keyword(word: &str) -> bool {
    matches!(
        keyword_category(word),
        Some(Keyword::TypeFuncName | Keyword::Reserved)
    )
}

/// Whether `word` is one of PostgreSQL's fully `reserved` keywords — the ones
/// that can never name anything. Distinct from [`is_column_name_keyword`],
/// which also rejects the `can be function or type name` category: those may
/// not name a column, but `left('abc', 2)` and `array[1]` are ordinary
/// expressions, so an expression position must let them through.
pub(crate) fn is_reserved_keyword(word: &str) -> bool {
    matches!(keyword_category(word), Some(Keyword::Reserved))
}

/// Whether an identifier must be quoted to survive a round trip, mirroring
/// PostgreSQL's `quote_ident`: any keyword outside the plain unreserved
/// category would otherwise be reinterpreted.
pub(crate) fn keyword_needs_quotes(word: &str) -> bool {
    keyword_category(word).is_some()
}

#[derive(Debug)]
pub struct ParseError {
    pub at: usize,
    pub message: StackStr<96>,
    /// SQLSTATE; almost always 42601 (syntax error), but some parse-analysis
    /// errors carry their own (e.g. 42P20 for window-frame shape).
    pub sqlstate: &'static str,
}

impl ParseError {
    fn new(at: usize, text: &str) -> Self {
        Self {
            at,
            message: stack_format!(96, "{}", text),
            sqlstate: sqlstate::SYNTAX_ERROR,
        }
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::new(e.at, e.message)
    }
}

mod expr;
mod window;

mod ddl;

pub struct Parser<'a> {
    text: &'a str,
    lexer: Lexer<'a>,
    peeked: Tok<'a>,
    peek_at: usize,
    arena: &'a Arena,
    /// Highest `$n` seen — the statement's parameter count.
    max_param: u32,
    /// The `WINDOW name AS (...)` definitions of the SELECT being parsed, which
    /// `OVER name` resolves against. Scoped to that SELECT: saved and cleared
    /// around a nested one, since a subquery neither sees nor exports them.
    windows: [Option<(&'a str, &'a WindowSpec<'a>)>; MAX_WINDOW_DEFS],
    n_windows: usize,
    /// Warnings raised while parsing. PostgreSQL reports these before the
    /// statement's own output, so the engine drains them after each
    /// `next_stmt` and emits them ahead of executing it.
    warnings: [StackStr<96>; MAX_PARSE_WARNINGS],
    n_warnings: usize,
    /// True while parsing a position where `SELECT ... INTO table` is legal (a
    /// top-level query). A subquery / CTE / set-op branch clears it, so an
    /// `INTO` there is rejected as PostgreSQL rejects it.
    allow_into: bool,
    /// The `INTO table` clause a top-level SELECT carried, with the byte range
    /// of the clause itself so the query can be reconstructed without it (for
    /// reuse of the CREATE TABLE AS machinery). `None` when there was none.
    into_clause: Option<(QualName<'a>, usize, usize)>,
    /// A column/domain DEFAULT ends before a following `NOT NULL`. Ordinarily
    /// `NOT` is an infix-expression prefix (`NOT IN`, `NOT LIKE`, ...), so the
    /// expression parser needs this narrow bit of grammar context to leave the
    /// column constraint for its caller.
    stop_default_at_not_null: bool,
    /// SQL-language routine formals. Their names are resolved to the same
    /// positional parameter nodes as `$n` while the body is parsed, so a
    /// runtime value cannot lose its declared type or be mistaken for a table
    /// column merely because the caller used the named spelling.
    routine_parameters: [Option<&'a str>; crate::storage::MAX_ROUTINE_ARGUMENTS],
    routine_name: Option<&'a str>,
}

/// Parses a stored view definition across PostgreSQL's complete query body,
/// including set operations and VALUES leaves.
pub fn parse_view_select<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    parse_query(sql, arena)
}

/// Parses a query body — a SELECT, a set-operation tree (UNION/INTERSECT/
/// EXCEPT), or a VALUES list — into a `Select` (a genuine set-operation lands
/// in `set_body`). Stored views, CREATE TABLE AS, and materialized-view style
/// bodies share this boundary.
pub fn parse_query<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid query: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let sel = parser
        .query_select()
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after query"));
    }
    arena
        .alloc(sel)
        .map(|r| &*r)
        .map_err(|_| super::eval::SqlError {
            sqlstate: super::eval::SqlState::known(super::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED),
            message: crate::stack_format!(192, "query too large for SQL arena"),
        })
}

/// Parses a single scalar expression (e.g. a stored CHECK predicate) into the
/// arena. The whole input must be one expression.
pub fn parse_expr<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid expression: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let expression = parser
        .expression(0)
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after expression"));
    }
    Ok(expression)
}

/// Parses one complete PostgreSQL type spelling for a procedural local.
/// Keeping this at the parser boundary prevents trigger programs from
/// hand-normalizing aliases or silently losing a typmod.
pub(crate) fn parse_type_name<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<(&'a str, i32), super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid type name: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let parsed = parser
        .type_name_mod()
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after type name"));
    }
    Ok(parsed)
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str, arena: &'a Arena) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(text, arena);
        let peeked = lexer.next_token()?;
        let peek_at = lexer.token_start();
        Ok(Self {
            text,
            lexer,
            peeked,
            peek_at,
            arena,
            max_param: 0,
            windows: [None; MAX_WINDOW_DEFS],
            n_windows: 0,
            warnings: [StackStr::new(); MAX_PARSE_WARNINGS],
            n_warnings: 0,
            allow_into: false,
            into_clause: None,
            stop_default_at_not_null: false,
            routine_parameters: [None; crate::storage::MAX_ROUTINE_ARGUMENTS],
            routine_name: None,
        })
    }

    pub(crate) fn with_routine_parameters(
        mut self,
        routine_name: &'a str,
        parameters: &[crate::storage::RoutineArgumentDef],
    ) -> Result<Self, ParseError> {
        self.routine_name = Some(routine_name);
        for (index, parameter) in parameters.iter().enumerate() {
            if !parameter.name.as_str().is_empty() {
                let allocated = self.arena.alloc_str(parameter.name.as_str()).map_err(|_| {
                    ParseError::new(self.peek_at, "statement too large for SQL arena")
                })?;
                self.routine_parameters[index] = Some(allocated);
            }
        }
        Ok(self)
    }

    pub(super) fn routine_parameter(&self, name: &str) -> Option<u32> {
        self.routine_parameters
            .iter()
            .position(|parameter| parameter.is_some_and(|parameter| parameter == name))
            .map(|index| index as u32 + 1)
    }

    pub(super) fn qualified_routine_parameter(&self, qualifier: &str, name: &str) -> Option<u32> {
        self.routine_name
            .is_some_and(|routine| routine == qualifier)
            .then(|| self.routine_parameter(name))
            .flatten()
    }

    fn column_default_expression(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let prior = self.stop_default_at_not_null;
        self.stop_default_at_not_null = true;
        let parsed = self.expression(0);
        self.stop_default_at_not_null = prior;
        parsed
    }

    pub fn max_param(&self) -> u32 {
        self.max_param
    }

    /// Next statement, or None at end of input. Consumes separators.
    pub fn next_stmt(&mut self) -> Result<Option<Stmt<'a>>, ParseError> {
        while self.peeked == Tok::Op(";") {
            self.advance()?;
        }
        if self.peeked == Tok::Eof {
            return Ok(None);
        }
        let statement = self.statement()?;
        match self.peeked {
            Tok::Op(";") | Tok::Eof => Ok(Some(statement)),
            _ => Err(self.unexpected("expected ';'")),
        }
    }

    /// TRUNCATE [TABLE] name [, ...] [RESTART IDENTITY | CONTINUE IDENTITY]
    /// [CASCADE | RESTRICT]. ONLY and `*` are accepted and meaningless here
    /// (no inheritance).
    /// `[schema.]name` — a possibly-qualified relation name in a statement
    /// position (DDL targets, DML tables).
    fn qual_name(&mut self, what: &str) -> Result<QualName<'a>, ParseError> {
        let first = self.col_ident(what)?;
        if self.eat_op(".")? {
            Ok(QualName {
                schema: Some(first),
                name: self.col_ident(what)?,
            })
        } else {
            Ok(QualName::bare(first))
        }
    }

    /// DECLARE name [BINARY] [INSENSITIVE|ASENSITIVE] [[NO] SCROLL] CURSOR
    /// [{WITH|WITHOUT} HOLD] FOR select ("declare" not yet consumed).
    fn declare_cursor(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // declare
        let name = self.col_ident("cursor name")?;
        let mut binary = false;
        let mut scroll = crate::sql::cursor::CursorScroll::Default;
        loop {
            if self.eat_ident("binary")? {
                binary = true;
            } else if self.eat_ident("insensitive")? || self.eat_ident("asensitive")? {
                // Materialization makes every cursor insensitive.
            } else if self.eat_ident("scroll")? {
                scroll = crate::sql::cursor::CursorScroll::Scroll;
            } else if self.eat_ident("no")? {
                self.expect_ident("scroll")?;
                scroll = crate::sql::cursor::CursorScroll::NoScroll;
            } else {
                break;
            }
        }
        self.expect_ident("cursor")?;
        let hold = if self.eat_ident("with")? {
            self.expect_ident("hold")?;
            true
        } else {
            if self.eat_ident("without")? {
                self.expect_ident("hold")?;
            }
            false
        };
        self.expect_ident("for")?;
        // Capture the raw SELECT text; validated by parsing it now.
        let start = self.peek_at;
        let _ = self.query_select()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::DeclareCursor {
            name,
            binary,
            scroll,
            hold,
            sql,
        })
    }

    /// FETCH/MOVE [direction] [FROM|IN] cursor ("fetch"/"move" not consumed).
    fn fetch_cursor(&mut self, move_only: bool) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::cursor::FetchMotion;
        self.advance()?; // fetch | move
        let signed_count = |p: &mut Self| -> Result<i64, ParseError> {
            let negative = p.eat_op("-")?;
            match p.peeked {
                Tok::Num(text) => {
                    let v: i64 = text
                        .parse()
                        .map_err(|_| p.unexpected("expected a row count"))?;
                    p.advance()?;
                    Ok(if negative { -v } else { v })
                }
                _ => Err(p.unexpected("expected a row count")),
            }
        };
        let motion = if self.eat_ident("next")? {
            FetchMotion::Count(1)
        } else if self.eat_ident("prior")? {
            FetchMotion::Count(-1)
        } else if self.eat_ident("first")? {
            FetchMotion::Absolute(1)
        } else if self.eat_ident("last")? {
            FetchMotion::Absolute(-1)
        } else if self.eat_ident("absolute")? {
            FetchMotion::Absolute(signed_count(self)?)
        } else if self.eat_ident("relative")? {
            FetchMotion::Relative(signed_count(self)?)
        } else if self.eat_ident("forward")? {
            if self.eat_ident("all")? {
                FetchMotion::All
            } else if matches!(self.peeked, Tok::Num(_)) || self.peeked == Tok::Op("-") {
                FetchMotion::Count(signed_count(self)?)
            } else {
                FetchMotion::Count(1)
            }
        } else if self.eat_ident("backward")? {
            if self.eat_ident("all")? {
                FetchMotion::BackwardAll
            } else if matches!(self.peeked, Tok::Num(_)) || self.peeked == Tok::Op("-") {
                FetchMotion::Count(-signed_count(self)?)
            } else {
                FetchMotion::Count(-1)
            }
        } else if self.eat_ident("all")? {
            FetchMotion::All
        } else if matches!(self.peeked, Tok::Num(_)) || self.peeked == Tok::Op("-") {
            FetchMotion::Count(signed_count(self)?)
        } else {
            FetchMotion::Count(1)
        };
        if !self.eat_ident("from")? {
            let _ = self.eat_ident("in")?;
        }
        let name = self.col_ident("cursor name")?;
        Ok(Stmt::FetchCursor {
            name,
            motion,
            move_only,
        })
    }

    fn truncate(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // truncate
        let _ = self.eat_ident("table")?;
        let mut names: [QualName<'a>; 16] = [QualName::bare(""); 16];
        let mut n = 0usize;
        loop {
            let _ = self.eat_ident("only")?;
            if n == names.len() {
                return Err(self.err_here("too many tables in TRUNCATE"));
            }
            names[n] = self.qual_name("table name")?;
            n += 1;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
            }
            if self.peeked == Tok::Op(",") {
                self.advance()?;
                continue;
            }
            break;
        }
        let restart_identity = if self.eat_ident("restart")? {
            self.expect_ident("identity")?;
            true
        } else {
            if self.eat_ident("continue")? {
                self.expect_ident("identity")?;
            }
            false
        };
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        let tables = self
            .arena
            .alloc_slice_copy(&names[..n])
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::Truncate {
            tables,
            restart_identity,
            cascade,
        })
    }

    fn lock_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // lock
        let _ = self.eat_ident("table")?;
        let mut names = [QualName::bare(""); 32];
        let mut count = 0usize;
        loop {
            let _ = self.eat_ident("only")?;
            if count == names.len() {
                return Err(self.err_here("too many tables in LOCK TABLE"));
            }
            names[count] = self.qual_name("table name")?;
            count += 1;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
            }
            if self.peeked == Tok::Op(",") {
                self.advance()?;
                continue;
            }
            break;
        }
        let mode = if self.eat_ident("in")? {
            use super::ast::TableLockMode;
            let first = self.any_ident("lock mode")?;
            let mode = if first.eq_ignore_ascii_case("access") {
                if self.eat_ident("share")? {
                    TableLockMode::AccessShare
                } else {
                    self.expect_ident("exclusive")?;
                    TableLockMode::AccessExclusive
                }
            } else if first.eq_ignore_ascii_case("row") {
                if self.eat_ident("share")? {
                    TableLockMode::RowShare
                } else {
                    self.expect_ident("exclusive")?;
                    TableLockMode::RowExclusive
                }
            } else if first.eq_ignore_ascii_case("share") {
                if self.eat_ident("update")? {
                    self.expect_ident("exclusive")?;
                    TableLockMode::ShareUpdateExclusive
                } else if self.eat_ident("row")? {
                    self.expect_ident("exclusive")?;
                    TableLockMode::ShareRowExclusive
                } else {
                    TableLockMode::Share
                }
            } else if first.eq_ignore_ascii_case("exclusive") {
                TableLockMode::Exclusive
            } else {
                return Err(self.err_here("unrecognized lock mode"));
            };
            self.expect_ident("mode")?;
            mode
        } else {
            super::ast::TableLockMode::AccessExclusive
        };
        let nowait = self.eat_ident("nowait")?;
        let tables = self
            .arena
            .alloc_slice_copy(&names[..count])
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::LockTable {
            tables,
            mode,
            nowait,
        })
    }

    fn explain_bool(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("true")? || self.eat_ident("on")? {
            Ok(true)
        } else if self.eat_ident("false")? || self.eat_ident("off")? {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// EXPLAIN [ANALYZE] [VERBOSE] statement and the parenthesized option
    /// grammar shared by current PostgreSQL releases.
    fn explain(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{ExplainFormat, ExplainOptions, ExplainSerialize};

        self.advance()?; // explain
        let mut options = ExplainOptions::DEFAULT;
        let mut summary_specified = false;
        let mut timing_enabled_explicitly = false;
        if self.eat_op("(")? {
            loop {
                let option = self.any_ident("EXPLAIN option")?;
                if option.eq_ignore_ascii_case("format") {
                    let format = self.any_ident("EXPLAIN format")?;
                    options.format = if format.eq_ignore_ascii_case("text") {
                        ExplainFormat::Text
                    } else if format.eq_ignore_ascii_case("json") {
                        ExplainFormat::Json
                    } else if format.eq_ignore_ascii_case("xml") {
                        ExplainFormat::Xml
                    } else if format.eq_ignore_ascii_case("yaml") {
                        ExplainFormat::Yaml
                    } else {
                        return Err(
                            self.err_here("unrecognized value for EXPLAIN option \"format\"")
                        );
                    };
                } else if option.eq_ignore_ascii_case("serialize") {
                    options.serialize = match self.peeked {
                        Tok::Op(",") | Tok::Op(")") => ExplainSerialize::Text,
                        Tok::Ident(format) if format.eq_ignore_ascii_case("none") => {
                            self.advance()?;
                            ExplainSerialize::None
                        }
                        Tok::Ident(format) if format.eq_ignore_ascii_case("text") => {
                            self.advance()?;
                            ExplainSerialize::Text
                        }
                        Tok::Ident(format) if format.eq_ignore_ascii_case("binary") => {
                            self.advance()?;
                            ExplainSerialize::Binary
                        }
                        Tok::Ident(_) => {
                            return Err(self
                                .err_here("unrecognized value for EXPLAIN option \"serialize\""));
                        }
                        _ => {
                            return Err(self.err_here("expected EXPLAIN serialize format"));
                        }
                    };
                } else {
                    let enabled = self.explain_bool()?;
                    if option.eq_ignore_ascii_case("analyze")
                        || option.eq_ignore_ascii_case("analyse")
                    {
                        options.analyze = enabled;
                    } else if option.eq_ignore_ascii_case("verbose") {
                        options.verbose = enabled;
                    } else if option.eq_ignore_ascii_case("costs") {
                        options.costs = enabled;
                    } else if option.eq_ignore_ascii_case("settings") {
                        options.settings = enabled;
                    } else if option.eq_ignore_ascii_case("buffers") {
                        options.buffers = enabled;
                    } else if option.eq_ignore_ascii_case("wal") {
                        options.wal = enabled;
                    } else if option.eq_ignore_ascii_case("timing") {
                        options.timing = enabled;
                        timing_enabled_explicitly = enabled;
                    } else if option.eq_ignore_ascii_case("summary") {
                        options.summary = enabled;
                        summary_specified = true;
                    } else if option.eq_ignore_ascii_case("memory") {
                        options.memory = enabled;
                    } else if option.eq_ignore_ascii_case("generic_plan") {
                        options.generic_plan = enabled;
                    } else {
                        return Err(self.err_here("unrecognized EXPLAIN option"));
                    }
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        } else {
            loop {
                if self.eat_ident("analyze")? || self.eat_ident("analyse")? {
                    options.analyze = true;
                } else if self.eat_ident("verbose")? {
                    options.verbose = true;
                } else {
                    break;
                }
            }
        }
        if !summary_specified {
            options.summary = options.analyze;
        }
        if options.buffers && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "EXPLAIN option BUFFERS requires ANALYZE"),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        if options.wal && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "EXPLAIN option WAL requires ANALYZE"),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        if timing_enabled_explicitly && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "EXPLAIN option TIMING requires ANALYZE"),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        if options.serialize != ExplainSerialize::None && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "EXPLAIN option SERIALIZE requires ANALYZE"),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        if options.generic_plan && options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    96,
                    "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        let statement = self.statement()?;
        if matches!(statement, Stmt::Explain { .. }) {
            return Err(self.err_here("EXPLAIN cannot be nested"));
        }
        let statement = self
            .arena
            .alloc(statement)
            .map(|statement| &*statement)
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::Explain { options, statement })
    }

    fn statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        match self.peeked {
            Tok::Ident("select") | Tok::Ident("values") | Tok::Op("(") => self.query(),
            Tok::Ident("explain") => self.explain(),
            Tok::Ident("with") => self.with_query(),
            Tok::Ident("create") => self.create(),
            Tok::Ident("drop") => self.drop_stmt(),
            Tok::Ident("insert") => self.insert(),
            Tok::Ident("update") => self.update(),
            Tok::Ident("delete") => self.delete(),
            Tok::Ident("merge") => self.merge(),
            Tok::Ident("call") => self.call_procedure(),
            Tok::Ident("comment") => self.comment(),
            Tok::Ident("truncate") => self.truncate(),
            Tok::Ident("lock") => self.lock_table(),
            Tok::Ident("declare") => self.declare_cursor(),
            Tok::Ident("fetch") => self.fetch_cursor(false),
            Tok::Ident("move") => self.fetch_cursor(true),
            Tok::Ident("close") => {
                self.advance()?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::CloseCursor(None));
                }
                Ok(Stmt::CloseCursor(Some(self.col_ident("cursor name")?)))
            }
            Tok::Ident("begin") => {
                self.advance()?;
                let characteristics = self.transaction_modifiers(true)?;
                Ok(Stmt::Begin(characteristics))
            }
            Tok::Ident("start") => {
                self.advance()?;
                self.expect_ident("transaction")?;
                let characteristics = self.transaction_modifiers(false)?;
                Ok(Stmt::Begin(characteristics))
            }
            Tok::Ident("commit") | Tok::Ident("end") => {
                self.advance()?;
                Ok(Stmt::Commit)
            }
            Tok::Ident("rollback") | Tok::Ident("abort") => {
                self.advance()?;
                // ROLLBACK TO [SAVEPOINT] name rewinds to a savepoint; plain
                // ROLLBACK aborts the whole transaction.
                if self.eat_ident("to")? {
                    let _ = self.eat_ident("savepoint")?;
                    let name = self.any_ident("savepoint name")?;
                    Ok(Stmt::RollbackToSavepoint(name))
                } else {
                    Ok(Stmt::Rollback)
                }
            }
            Tok::Ident("savepoint") => {
                self.advance()?;
                let name = self.any_ident("savepoint name")?;
                Ok(Stmt::Savepoint(name))
            }
            Tok::Ident("release") => {
                self.advance()?;
                let _ = self.eat_ident("savepoint")?;
                let name = self.any_ident("savepoint name")?;
                Ok(Stmt::ReleaseSavepoint(name))
            }
            Tok::Ident("set") => {
                self.advance()?;
                if self.eat_ident("constraints")? {
                    let targets = if self.eat_ident("all")? {
                        ConstraintTargets::All
                    } else {
                        let mut names = [QualName::bare(""); MAX_LIST];
                        let mut count = 0;
                        loop {
                            if count == names.len() {
                                return Err(self.limit("constraint names", names.len()));
                            }
                            names[count] = self.qual_name("constraint name")?;
                            count += 1;
                            if !self.eat_op(",")? {
                                break;
                            }
                        }
                        ConstraintTargets::Named(self.arena_slice(&names[..count])?)
                    };
                    let mode = if self.eat_ident("deferred")? {
                        ConstraintMode::Deferred
                    } else {
                        self.expect_ident("immediate")?;
                        ConstraintMode::Immediate
                    };
                    return Ok(Stmt::SetConstraints { targets, mode });
                }
                let session_modifier = self.eat_ident("session")?;
                let local = if session_modifier {
                    false
                } else {
                    self.eat_ident("local")?
                };
                // SESSION is both the optional scope modifier and the first
                // word of SESSION AUTHORIZATION. Thus PostgreSQL accepts all
                // of SET SESSION AUTHORIZATION, SET LOCAL SESSION
                // AUTHORIZATION, and SET SESSION SESSION AUTHORIZATION.
                if (session_modifier && self.eat_ident("authorization")?)
                    || (self.eat_ident("session")? && {
                        self.expect_ident("authorization")?;
                        true
                    })
                {
                    let role = if self.eat_ident("default")? {
                        None
                    } else {
                        Some(self.any_ident("role name")?)
                    };
                    return Ok(Stmt::SetSessionAuthorization {
                        role,
                        local,
                        reset: false,
                    });
                }
                if self.eat_ident("role")? {
                    let role = if self.eat_ident("none")? {
                        None
                    } else {
                        Some(self.any_ident("role name")?)
                    };
                    return Ok(Stmt::SetRole {
                        role,
                        local,
                        reset: false,
                    });
                }
                // SET TRANSACTION ... / SET SESSION CHARACTERISTICS AS
                // TRANSACTION ...: retain the characteristics so execution can
                // reject isolation/read modes it cannot actually provide.
                let transaction = self.eat_ident("transaction")?;
                if transaction && self.eat_ident("snapshot")? {
                    return Ok(Stmt::SetTransactionSnapshot(
                        self.str_literal("snapshot identifier")?,
                    ));
                }
                let characteristics = if transaction {
                    true
                } else if self.eat_ident("characteristics")? {
                    self.expect_ident("as")?;
                    self.expect_ident("transaction")?;
                    true
                } else {
                    false
                };
                if characteristics {
                    let start = self.peek_at;
                    while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                        self.advance()?;
                    }
                    let characteristics = self.text[start..self.peek_at].trim();
                    if characteristics.is_empty() {
                        return Err(self.unexpected("expected transaction characteristics"));
                    }
                    return Ok(Stmt::SetTransaction(characteristics));
                }
                // Special spellings: SET TIME ZONE ..., SET NAMES ...
                let name = if self.eat_ident("time")? {
                    self.expect_ident("zone")?;
                    "timezone"
                } else if self.eat_ident("names")? {
                    "client_encoding"
                } else {
                    let n = self.any_ident("configuration parameter")?;
                    if !self.eat_op("=")? {
                        self.expect_ident("to")?;
                    }
                    n
                };
                // Capture the raw value text up to the statement terminator.
                let start = self.peek_at;
                while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                    self.advance()?;
                }
                let value = self.text[start..self.peek_at].trim();
                Ok(Stmt::Set { name, value, local })
            }
            Tok::Ident("reset") => {
                self.advance()?;
                if self.eat_ident("role")? {
                    Ok(Stmt::SetRole {
                        role: None,
                        local: false,
                        reset: true,
                    })
                } else if self.eat_ident("session")? {
                    self.expect_ident("authorization")?;
                    Ok(Stmt::SetSessionAuthorization {
                        role: None,
                        local: false,
                        reset: true,
                    })
                } else if self.eat_ident("all")? {
                    Ok(Stmt::Reset(None))
                } else {
                    Ok(Stmt::Reset(Some(
                        self.any_ident("configuration parameter")?,
                    )))
                }
            }
            Tok::Ident("show") => {
                self.advance()?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::ShowAll);
                }
                // SHOW TRANSACTION ISOLATION LEVEL, SHOW TIME ZONE — multi-word
                // spellings the SQL standard and JDBC use.
                if self.eat_ident("transaction")? {
                    self.expect_ident("isolation")?;
                    self.expect_ident("level")?;
                    return Ok(Stmt::Show("transaction_isolation"));
                }
                if self.eat_ident("time")? {
                    self.expect_ident("zone")?;
                    return Ok(Stmt::Show("timezone"));
                }
                let name = self.any_ident("configuration parameter")?;
                Ok(Stmt::Show(name))
            }
            Tok::Ident("checkpoint") => {
                self.advance()?;
                Ok(Stmt::Checkpoint)
            }
            Tok::Ident("reindex") => self.reindex(),
            Tok::Ident("vacuum") => self.vacuum_or_analyze(true),
            Tok::Ident("analyze") | Tok::Ident("analyse") => self.vacuum_or_analyze(false),
            Tok::Ident("refresh") => {
                self.advance()?;
                self.expect_ident("materialized")?;
                self.expect_ident("view")?;
                let name = self.qual_name("materialized view name")?;
                Ok(Stmt::RefreshMaterializedView { name })
            }
            Tok::Ident("alter") => self.alter_table(),
            Tok::Ident("grant") => self.grant_statement(),
            Tok::Ident("revoke") => self.revoke_statement(),
            Tok::Ident("reassign") => {
                self.advance()?;
                self.expect_ident("owned")?;
                self.expect_ident("by")?;
                let roles = self.role_name_list("role name")?;
                self.expect_ident("to")?;
                let new_owner = self.any_ident("new owner")?;
                Ok(Stmt::ReassignOwned { roles, new_owner })
            }
            Tok::Ident("copy") => self.copy_statement(),
            Tok::Ident("prepare") => self.prepare(),
            Tok::Ident("execute") => self.execute_prepared(),
            Tok::Ident("deallocate") => {
                self.advance()?;
                let _ = self.eat_ident("prepare")?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::Deallocate(None));
                }
                let name = self.any_ident("prepared statement name")?;
                Ok(Stmt::Deallocate(Some(name)))
            }
            Tok::Ident("listen") => {
                self.advance()?;
                Ok(Stmt::Listen(self.col_ident("channel name")?))
            }
            Tok::Ident("unlisten") => {
                self.advance()?;
                if self.eat_op("*")? {
                    Ok(Stmt::Unlisten(None))
                } else {
                    Ok(Stmt::Unlisten(Some(self.col_ident("channel name")?)))
                }
            }
            Tok::Ident("notify") => {
                self.advance()?;
                let channel = self.col_ident("channel name")?;
                let payload = if self.eat_op(",")? {
                    match self.peeked {
                        Tok::Str(s) => {
                            self.advance()?;
                            Some(s)
                        }
                        _ => return Err(self.unexpected("expected a payload string literal")),
                    }
                } else {
                    None
                };
                Ok(Stmt::Notify { channel, payload })
            }
            _ => Err(self.unexpected("expected a statement")),
        }
    }

    fn call_procedure(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?;
        let name = self.qual_name("procedure name")?;
        self.expect_op("(")?;
        let mut arguments = [&Expr::Null; MAX_LIST];
        let mut count = 0;
        if !self.eat_op(")")? {
            loop {
                if count == arguments.len() {
                    return Err(self.limit("procedure arguments", arguments.len()));
                }
                arguments[count] = self.expression(0)?;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        Ok(Stmt::Call {
            name,
            arguments: self.arena_slice(&arguments[..count])?,
        })
    }

    /// A comma-separated projection list (used by SELECT and RETURNING).
    fn select_items(&mut self) -> Result<&'a [SelectItem<'a>], ParseError> {
        let mut items = [SelectItem::Wildcard; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("select list", MAX_LIST));
            }
            items[n] = if self.peeked == Tok::Op("*") {
                self.advance()?;
                SelectItem::Wildcard
            } else if let Some(table) = self.table_wildcard()? {
                SelectItem::TableWildcard(table)
            } else {
                let expression = self.expression(0)?;
                let alias = self.alias()?;
                // A parenthesized `(t.*)` as a whole select item expands to
                // the table's columns, exactly like `t.*` (PostgreSQL); only
                // inside a larger expression (`(t.*)::text`, `row_to_json(t.*)`)
                // does it stay a record.
                match expression {
                    Expr::WholeRow(table) if alias.is_none() => SelectItem::TableWildcard(table),
                    // `(record).*` parsed as the `*`-sentinel field access.
                    Expr::Field { base, field: "*" } if alias.is_none() => {
                        SelectItem::RecordStar(base)
                    }
                    _ => SelectItem::Expr { expression, alias },
                }
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&items[..n])
    }

    /// `t.*` (two tokens of lookahead: restores the parser when the item
    /// turns out to be an ordinary expression).
    fn table_wildcard(&mut self) -> Result<Option<&'a str>, ParseError> {
        let table = match self.peeked {
            Tok::Ident(name) | Tok::QuotedIdent(name) => name,
            _ => return Ok(None),
        };
        let mark = self.lexer.mark();
        let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
        self.advance()?;
        if self.peeked == Tok::Op(".") {
            self.advance()?;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
                return Ok(Some(table));
            }
        }
        self.lexer.reset(mark);
        self.peeked = saved_peeked;
        self.peek_at = saved_peek_at;
        Ok(None)
    }

    fn returning(&mut self) -> Result<&'a [SelectItem<'a>], ParseError> {
        if self.eat_ident("returning")? {
            self.select_items()
        } else {
            Ok(&[])
        }
    }

    /// A SELECT through HAVING, without the trailing ORDER BY / LIMIT / OFFSET
    /// (those belong to the enclosing query so a set operation can share them).
    fn select_core(&mut self) -> Result<Select<'a>, ParseError> {
        self.expect_ident("select")?;
        // The WINDOW clause is written after HAVING, but the names it defines
        // are used in the select list above it, so it is parsed ahead of the
        // list. The scope stays live past this function because the trailing
        // ORDER BY — parsed by our caller — may also use those names; the
        // caller restores the enclosing query's windows once it is done.
        self.n_windows = 0;
        self.prescan_windows()?;
        let mut distinct_on: &'a [&'a Expr<'a>] = &[];
        let distinct = if self.eat_ident("distinct")? {
            // `DISTINCT ON (expr, ...)`: keep the first row per distinct key.
            if self.eat_ident("on")? {
                self.expect_op("(")?;
                let mut exprs = [self.arena_expr(Expr::Null)?; MAX_LIST];
                let mut n = 0;
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("DISTINCT ON list", MAX_LIST));
                    }
                    exprs[n] = self.expression(0)?;
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                distinct_on = self.arena_slice(&exprs[..n])?;
            }
            true
        } else {
            let _ = self.eat_ident("all")?;
            false
        };
        let items = self.select_items()?;

        // `SELECT ... INTO [TABLE] name ...` — a CREATE TABLE AS spelled the
        // old way. Legal only at the top level; a subquery / CTE / set-op branch
        // rejects it as PostgreSQL does. The clause's byte range is recorded so
        // the query can be reconstructed without it.
        if self.peeked == Tok::Ident("into") {
            let into_start = self.peek_at;
            self.advance()?;
            if !self.allow_into {
                return Err(self.err_here("SELECT ... INTO is not allowed here"));
            }
            if self.into_clause.is_some() {
                return Err(self.err_here("multiple INTO clauses in one query"));
            }
            let _ = self.eat_ident("table")?;
            let name = self.qual_name("table name")?;
            let into_end = self.peek_at;
            self.into_clause = Some((name, into_start, into_end));
        }

        let from = if self.eat_ident("from")? {
            Some(self.from_clause()?)
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let (group_by, grouping_sets) = if self.eat_ident("group")? {
            self.expect_ident("by")?;
            self.group_by_clause()?
        } else {
            (&[][..], &[][..])
        };
        let having = if self.eat_ident("having")? {
            Some(self.expression(0)?)
        } else {
            None
        };
        // Consume the clause in its written position; the lookahead above left
        // the cursor before it.
        if self.eat_ident("window")? {
            self.window_definitions()?;
        }
        Ok(Select {
            items,
            distinct,
            distinct_on,
            from,
            where_clause,
            group_by,
            grouping_sets,
            having,
            order_by: &[],
            limit: None,
            offset: None,
            with_ties: false,
            with: &[],
            set_body: None,
            locking: &[],
        })
    }

    /// Parses this SELECT's `WINDOW` clause ahead of the cursor, then restores
    /// the cursor, so `OVER name` resolves while the select list — written
    /// before the clause — is being parsed.
    fn prescan_windows(&mut self) -> Result<(), ParseError> {
        // The overwhelming majority of queries have no WINDOW clause at all;
        // skip the token scan unless the word appears somewhere ahead.
        if !mentions_window(&self.text[self.peek_at..]) {
            return Ok(());
        }
        let mark = self.lexer.mark();
        let (peeked, peek_at) = (self.peeked, self.peek_at);
        let mut depth = 0usize;
        // `AS window` is a column label, not this clause — a reserved word is
        // allowed there, as PostgreSQL allows `SELECT 1 AS window`.
        let mut after_as = false;
        loop {
            if matches!(self.peeked, Tok::Ident("window")) && after_as {
                after_as = false;
                self.advance()?;
                continue;
            }
            after_as = matches!(self.peeked, Tok::Ident("as"));
            match self.peeked {
                Tok::Eof => break,
                Tok::Op("(") => depth += 1,
                // Leaving this SELECT's parentheses: the clause is not ours.
                Tok::Op(")") if depth == 0 => break,
                Tok::Op(")") => depth -= 1,
                Tok::Op(";") if depth == 0 => break,
                // A set operation ends this leaf; the next has its own clause.
                Tok::Ident("union" | "intersect" | "except") if depth == 0 => break,
                Tok::Ident("window") if depth == 0 => {
                    self.advance()?;
                    self.window_definitions()?;
                    break;
                }
                _ => {}
            }
            self.advance()?;
        }
        self.lexer.reset(mark);
        (self.peeked, self.peek_at) = (peeked, peek_at);
        Ok(())
    }

    /// Trailing ORDER BY / LIMIT / OFFSET (any may be absent).
    fn order_limit(&mut self) -> Result<OrderLimit<'a>, ParseError> {
        let mut order = [OrderBy {
            expression: &Expr::Null,
            descending: false,
            nulls_first: false,
        }; MAX_LIST];
        let mut n_order = 0;
        if self.eat_ident("order")? {
            self.expect_ident("by")?;
            loop {
                if n_order == MAX_LIST {
                    return Err(self.limit("order by list", MAX_LIST));
                }
                let expression = self.expression(0)?;
                let descending = if self.eat_ident("desc")? {
                    true
                } else {
                    self.eat_ident("asc")?;
                    false
                };
                // Optional NULLS FIRST/LAST; PostgreSQL defaults NULLS LAST
                // for ASC and NULLS FIRST for DESC.
                let nulls_first = if self.eat_ident("nulls")? {
                    if self.eat_ident("first")? {
                        true
                    } else {
                        self.expect_ident("last")?;
                        false
                    }
                } else {
                    descending
                };
                order[n_order] = OrderBy {
                    expression,
                    descending,
                    nulls_first,
                };
                n_order += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let order_by = self.arena_slice(&order[..n_order])?;
        // LIMIT and OFFSET accept either order, as in PostgreSQL.
        let mut limit = None;
        let mut offset = None;
        let mut with_ties = false;
        loop {
            if limit.is_none() && self.eat_ident("limit")? {
                // `LIMIT ALL` is the standard spelling of "no limit"; it leaves
                // the clause unset rather than binding an expression.
                if self.eat_ident("all")? {
                    continue;
                }
                limit = Some(self.expression(0)?);
            } else if offset.is_none() && self.eat_ident("offset")? {
                offset = Some(self.expression(0)?);
                // Accept the noise words ROW/ROWS.
                let _ = self.eat_ident("rows")? || self.eat_ident("row")?;
            } else if limit.is_none() && self.eat_ident("fetch")? {
                // `FETCH { FIRST | NEXT } [count] { ROW | ROWS } { ONLY | WITH
                // TIES }` — the SQL-standard spelling of LIMIT. The count
                // defaults to 1 when omitted.
                if !self.eat_ident("first")? {
                    self.expect_ident("next")?;
                }
                if self.eat_ident("row")? || self.eat_ident("rows")? {
                    limit = Some(&Expr::Int(1));
                } else {
                    limit = Some(self.expression(0)?);
                    if !(self.eat_ident("rows")? || self.eat_ident("row")?) {
                        return Err(self.err_here("expected ROW or ROWS after FETCH count"));
                    }
                }
                if self.eat_ident("with")? {
                    self.expect_ident("ties")?;
                    with_ties = true;
                } else {
                    self.expect_ident("only")?;
                }
            } else {
                break;
            }
        }
        if with_ties && order_by.is_empty() {
            return Err(self.err_here("WITH TIES cannot be specified without ORDER BY clause"));
        }
        Ok((order_by, limit, offset, with_ties))
    }

    /// A subquery body: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET applying to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself; a genuine
    /// set-operation is carried in `set_body`.
    fn select(&mut self) -> Result<Select<'a>, ParseError> {
        // This is the nesting boundary for every subquery, so it is where a
        // nested SELECT's named windows stop being visible.
        let enclosing_windows = (self.windows, self.n_windows);
        // A subquery / CTE / set-op branch is not a place `SELECT ... INTO` may
        // appear; forbid it here (select_core checks the flag).
        let saved_allow = self.allow_into;
        self.allow_into = false;
        let body = self.set_union()?;
        let (order_by, limit, offset, with_ties) = self.order_limit()?;
        // Row-locking clauses come last, after ORDER BY / LIMIT / OFFSET.
        let locking = self.locking_clauses()?;
        self.allow_into = saved_allow;
        (self.windows, self.n_windows) = enclosing_windows;
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            sel.with_ties = with_ties;
            sel.locking = locking;
            return Ok(sel);
        }
        Ok(Select {
            items: &[],
            distinct: false,
            distinct_on: &[],
            from: None,
            where_clause: None,
            group_by: &[],
            grouping_sets: &[],
            having: None,
            order_by,
            limit,
            offset,
            with_ties,
            with: &[],
            set_body: Some(body),
            locking,
        })
    }

    /// Parses any query expression into the Select-shaped representation used
    /// by stored definitions and query-bearing commands. WITH is part of that
    /// expression in PostgreSQL, not a top-level-only statement prefix.
    fn query_select(&mut self) -> Result<Select<'a>, ParseError> {
        if self.peeked != Tok::Ident("with") {
            return self.select();
        }
        match self.with_query()? {
            Stmt::Select(select) => Ok(select),
            Stmt::SetQuery(query) => Ok(Select {
                items: &[],
                distinct: false,
                distinct_on: &[],
                from: None,
                where_clause: None,
                group_by: &[],
                grouping_sets: &[],
                having: None,
                order_by: query.order_by,
                limit: query.limit,
                offset: query.offset,
                with_ties: query.with_ties,
                with: query.with,
                set_body: Some(query.body),
                locking: query.locking,
            }),
            _ => Err(self.err_here("query must end in SELECT, TABLE, or VALUES")),
        }
    }

    /// Parses the trailing `FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE }
    /// [OF t, …] [NOWAIT | SKIP LOCKED]` row-locking clauses (zero or more).
    fn locking_clauses(&mut self) -> Result<&'a [LockClause<'a>], ParseError> {
        let mut clauses = [LockClause {
            strength: LockStrength::Update,
            of: &[],
            wait: LockWait::Wait,
        }; MAX_LOCK_CLAUSES];
        let mut n = 0;
        while self.eat_ident("for")? {
            if n == MAX_LOCK_CLAUSES {
                return Err(self.limit("locking clauses", MAX_LOCK_CLAUSES));
            }
            let strength = if self.eat_ident("update")? {
                LockStrength::Update
            } else if self.eat_ident("no")? {
                self.expect_ident("key")?;
                self.expect_ident("update")?;
                LockStrength::NoKeyUpdate
            } else if self.eat_ident("share")? {
                LockStrength::Share
            } else if self.eat_ident("key")? {
                self.expect_ident("share")?;
                LockStrength::KeyShare
            } else {
                return Err(
                    self.err_here("expected UPDATE, NO KEY UPDATE, SHARE, or KEY SHARE after FOR")
                );
            };
            let mut of = [""; MAX_LIST];
            let mut nof = 0;
            if self.eat_ident("of")? {
                loop {
                    if nof == MAX_LIST {
                        return Err(self.limit("FOR ... OF list", MAX_LIST));
                    }
                    of[nof] = self.col_ident("table name")?;
                    nof += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            let wait = if self.eat_ident("nowait")? {
                LockWait::NoWait
            } else if self.eat_ident("skip")? {
                self.expect_ident("locked")?;
                LockWait::SkipLocked
            } else {
                LockWait::Wait
            };
            clauses[n] = LockClause {
                strength,
                of: self.arena_slice(&of[..nof])?,
                wait,
            };
            n += 1;
        }
        self.arena_slice(&clauses[..n])
    }

    /// A top-level query: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET that apply to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself.
    /// `WITH [RECURSIVE] name [(col, ...)] AS (SELECT ...), ... <SELECT body>`.
    fn with_query(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("with")?;
        let recursive = self.eat_ident("recursive")?;
        let placeholder: &'a Select<'a> = self
            .arena
            .alloc(Select {
                items: &[],
                distinct: false,
                distinct_on: &[],
                from: None,
                where_clause: None,
                group_by: &[],
                grouping_sets: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with_ties: false,
                with: &[],
                set_body: None,
                locking: &[],
            })
            .map_err(|_| self.err_here("statement too large for SQL arena"))?;
        let mut ctes = [Cte {
            name: "",
            columns: &[],
            recursive: false,
            materialization: crate::sql::ast::CteMaterialization::Default,
            query: placeholder,
            dml: None,
        }; MAX_CTES];
        let mut n = 0;
        loop {
            if n == MAX_CTES {
                return Err(self.limit("WITH list", MAX_CTES));
            }
            let name = self.col_ident("CTE name")?;
            // Optional output-column rename list `name(c1, c2, ...)`.
            let columns = self.column_alias_list()?.unwrap_or(&[]);
            self.expect_ident("as")?;
            let materialization = if self.eat_ident("materialized")? {
                crate::sql::ast::CteMaterialization::Materialized
            } else if self.eat_ident("not")? {
                self.expect_ident("materialized")?;
                crate::sql::ast::CteMaterialization::NotMaterialized
            } else {
                crate::sql::ast::CteMaterialization::Default
            };
            self.expect_op("(")?;
            // A data-modifying CTE body is an INSERT/UPDATE/DELETE (run once,
            // its RETURNING becomes the relation); anything else is a query.
            let (boxed, dml) = if matches!(
                self.peeked,
                Tok::Ident("insert") | Tok::Ident("update") | Tok::Ident("delete")
            ) {
                let stmt = self.statement()?;
                let boxed_stmt = self
                    .arena
                    .alloc(stmt)
                    .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                (placeholder, Some(&*boxed_stmt))
            } else {
                let q = self.query_select()?;
                let boxed = self
                    .arena
                    .alloc(q)
                    .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                (&*boxed, None)
            };
            self.expect_op(")")?;
            ctes[n] = Cte {
                name,
                columns,
                recursive,
                materialization,
                query: boxed,
                dml,
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let ctes = self.arena_slice(&ctes[..n])?;
        match self.statement()? {
            Stmt::Select(mut sel) => {
                sel.with = ctes;
                Ok(Stmt::Select(sel))
            }
            Stmt::SetQuery(mut q) => {
                q.with = ctes;
                Ok(Stmt::SetQuery(q))
            }
            statement @ (Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_)) => {
                let statement = self
                    .arena
                    .alloc(statement)
                    .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                Ok(Stmt::With {
                    ctes,
                    statement: &*statement,
                })
            }
            _ => {
                Err(self
                    .err_here("WITH must be followed by SELECT, INSERT, UPDATE, DELETE, or MERGE"))
            }
        }
    }

    fn query(&mut self) -> Result<Stmt<'a>, ParseError> {
        let enclosing_windows = (self.windows, self.n_windows);
        // A top-level query may carry `SELECT ... INTO table`; capture the whole
        // statement's byte span so it can be rewritten to CREATE TABLE AS.
        let stmt_start = self.peek_at;
        let saved_allow = self.allow_into;
        let saved_into = self.into_clause.take();
        self.allow_into = true;
        let body = self.set_union()?;
        let (order_by, limit, offset, with_ties) = self.order_limit()?;
        let locking = self.locking_clauses()?;
        let stmt_end = self.peek_at;
        self.allow_into = saved_allow;
        let into = self.into_clause.take();
        self.into_clause = saved_into;
        (self.windows, self.n_windows) = enclosing_windows;
        if let Some((name, into_start, into_end)) = into {
            // Reconstruct the query without its INTO clause and hand it to the
            // CREATE TABLE AS machinery.
            let sql = self
                .arena
                .alloc_str_display(format_args!(
                    "{} {}",
                    self.text[stmt_start..into_start].trim_end(),
                    self.text[into_end..stmt_end].trim_start()
                ))
                .map_err(|_| self.err_here("SELECT INTO query too large for the SQL arena"))?;
            return Ok(Stmt::CreateTableAs {
                name,
                columns: &[],
                sql: sql.trim(),
                with_data: true,
                if_not_exists: false,
                materialized: false,
            });
        }
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            sel.with_ties = with_ties;
            sel.locking = locking;
            return Ok(Stmt::Select(sel));
        }
        Ok(Stmt::SetQuery(SetQuery {
            with: &[],
            body,
            order_by,
            limit,
            offset,
            with_ties,
            locking,
        }))
    }

    /// UNION / EXCEPT level (lowest precedence, left-associative).
    fn set_union(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        let mut left = self.set_intersect()?;
        loop {
            let operator = if self.eat_ident("union")? {
                SetOp::Union
            } else if self.eat_ident("except")? {
                SetOp::Except
            } else {
                break;
            };
            let all = self.set_all()?;
            let right = self.set_intersect()?;
            left = self.alloc_set(SetTree::Op {
                operator,
                all,
                left,
                right,
            })?;
        }
        Ok(left)
    }

    /// INTERSECT level (binds tighter than UNION / EXCEPT).
    fn set_intersect(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        let mut left = self.set_leaf()?;
        while self.eat_ident("intersect")? {
            let all = self.set_all()?;
            let right = self.set_leaf()?;
            left = self.alloc_set(SetTree::Op {
                operator: SetOp::Intersect,
                all,
                left,
                right,
            })?;
        }
        Ok(left)
    }

    fn set_leaf(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        // A parenthesized branch is itself a set-operation query, and may
        // carry its own trailing ORDER BY / LIMIT / OFFSET (applied to the
        // branch before the outer set operator combines it).
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let enclosing_windows = (self.windows, self.n_windows);
            let inner = self.set_union()?;
            let (order_by, limit, offset, with_ties) = self.order_limit()?;
            (self.windows, self.n_windows) = enclosing_windows;
            self.expect_op(")")?;
            if order_by.is_empty() && limit.is_none() && offset.is_none() {
                return Ok(inner);
            }
            let sel = match inner {
                SetTree::Select(s) => {
                    let mut sel = **s;
                    sel.order_by = order_by;
                    sel.limit = limit;
                    sel.offset = offset;
                    sel.with_ties = with_ties;
                    sel
                }
                op => Select {
                    items: &[],
                    distinct: false,
                    distinct_on: &[],
                    from: None,
                    where_clause: None,
                    group_by: &[],
                    grouping_sets: &[],
                    having: None,
                    order_by,
                    limit,
                    offset,
                    with_ties,
                    with: &[],
                    set_body: Some(op),
                    locking: &[],
                },
            };
            let boxed = self
                .arena
                .alloc(sel)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            return self.alloc_set(SetTree::Select(boxed));
        }
        // `VALUES (row), (row), ...` is a set-operator branch: desugar to
        // `SELECT row UNION ALL SELECT row ...` (each row a FROM-less SELECT).
        if self.peeked == Tok::Ident("values") {
            self.advance()?;
            let mut tree: Option<&'a SetTree<'a>> = None;
            loop {
                self.expect_op("(")?;
                let mut items: [SelectItem<'a>; MAX_LIST] = [SelectItem::Wildcard; MAX_LIST];
                let mut n = 0;
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("VALUES columns", MAX_LIST));
                    }
                    // A VALUES column with no outer alias is named `columnN`,
                    // as PostgreSQL names it (a UNION-ALL takes its output names
                    // from the first branch, so naming every row is harmless).
                    let expression = self.expression(0)?;
                    let alias = self
                        .arena
                        .alloc_str_display(format_args!("column{}", n + 1))
                        .map_err(|_| self.err_here("VALUES too large"))?;
                    items[n] = SelectItem::Expr {
                        expression,
                        alias: Some(alias),
                    };
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let sel = Select {
                    items: self.arena_slice(&items[..n])?,
                    distinct: false,
                    distinct_on: &[],
                    from: None,
                    where_clause: None,
                    group_by: &[],
                    grouping_sets: &[],
                    having: None,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    with_ties: false,
                    with: &[],
                    set_body: None,
                    locking: &[],
                };
                let leaf = self.alloc_set(SetTree::Select(
                    self.arena
                        .alloc(sel)
                        .map_err(|_| self.err_here("VALUES too large"))?,
                ))?;
                tree = Some(match tree {
                    None => leaf,
                    Some(l) => self.alloc_set(SetTree::Op {
                        operator: SetOp::Union,
                        all: true,
                        left: l,
                        right: leaf,
                    })?,
                });
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(tree.expect("at least one VALUES row"));
        }
        let core = self.select_core()?;
        let core = self
            .arena
            .alloc(core)
            .map_err(|_| self.err_here("statement too large for SQL arena"))?;
        self.alloc_set(SetTree::Select(core))
    }

    /// `ALL` or `DISTINCT` after a set operator (DISTINCT is the default).
    fn set_all(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("all")? {
            Ok(true)
        } else {
            let _ = self.eat_ident("distinct")?;
            Ok(false)
        }
    }

    fn alloc_set(&mut self, tree: SetTree<'a>) -> Result<&'a SetTree<'a>, ParseError> {
        self.arena
            .alloc(tree)
            .map_err(|_| self.err_here("statement too large for SQL arena"))
            .map(|t| t as &_)
    }

    fn table_ref(&mut self) -> Result<TableRef<'a>, ParseError> {
        // `LATERAL` may precede a subquery or a function FROM item, letting it
        // reference the FROM items to its left. It applies to whichever kind of
        // item follows, so it is captured here and stamped on the result.
        let lateral = self.eat_ident("lateral")?;
        // ROWS FROM composes function scans in lockstep. Keep the member calls
        // typed as function-only TableRefs so ordinary relations, subqueries,
        // and nested groups cannot enter this state.
        if self.peeked == Tok::Ident("rows") {
            let mark = self.lexer.mark();
            let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
            self.advance()?;
            if self.eat_ident("from")? {
                self.expect_op("(")?;
                let mut functions = [TableRef {
                    schema: None,
                    table: "",
                    alias: None,
                    subquery: None,
                    func_args: None,
                    rows_from: None,
                    col_alias: None,
                    cte: None,
                    with_ordinality: false,
                    lateral: false,
                    authorization_role: None,
                }; MAX_LIST];
                let mut count = 0usize;
                loop {
                    if count == functions.len() {
                        return Err(self.limit("ROWS FROM functions", functions.len()));
                    }
                    let function = self.table_ref()?;
                    if function.func_args.is_none()
                        || function.rows_from.is_some()
                        || function.subquery.is_some()
                        || function.cte.is_some()
                        || function.with_ordinality
                        || function.lateral
                    {
                        return Err(self.err_here("ROWS FROM requires function calls"));
                    }
                    functions[count] = function;
                    count += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let with_ordinality = if self.eat_ident("with")? {
                    self.expect_ident("ordinality")?;
                    true
                } else {
                    false
                };
                let alias = if self.eat_ident("as")? {
                    Some(self.col_ident("alias")?)
                } else if let Tok::Ident(word) = self.peeked {
                    if is_column_name_keyword(word) {
                        None
                    } else {
                        self.advance()?;
                        Some(word)
                    }
                } else {
                    None
                };
                let col_alias = self.column_alias_list()?;
                return Ok(TableRef {
                    schema: None,
                    table: "",
                    alias,
                    subquery: None,
                    func_args: None,
                    rows_from: Some(self.arena_slice(&functions[..count])?),
                    col_alias,
                    cte: None,
                    with_ordinality,
                    lateral,
                    authorization_role: None,
                });
            }
            self.lexer.reset(mark);
            self.peeked = saved_peeked;
            self.peek_at = saved_peek_at;
        }
        // Derived table: `(SELECT ...) [AS] alias`. PostgreSQL requires the
        // alias, so a missing one is a syntax error.
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let select = self.query_select()?;
            self.expect_op(")")?;
            let boxed = self
                .arena
                .alloc(select)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            let _ = self.eat_ident("as")?;
            let Tok::Ident(word) = self.peeked else {
                return Err(self.err_here("subquery in FROM must have an alias"));
            };
            if is_column_name_keyword(word) {
                return Err(self.err_here("subquery in FROM must have an alias"));
            }
            self.advance()?;
            // Optional column-alias list `alias(c1, c2, ...)` renames the
            // derived table's output columns.
            let col_alias = self.column_alias_list()?;
            return Ok(TableRef {
                schema: None,
                table: "",
                alias: Some(word),
                subquery: Some(boxed),
                func_args: None,
                rows_from: None,
                col_alias,
                cte: None,
                with_ordinality: false,
                lateral,
                authorization_role: None,
            });
        }
        let first = self.col_ident("table name")?;
        let (schema, table) = if self.eat_op(".")? {
            (Some(first), self.col_ident("table name")?)
        } else {
            (None, first)
        };
        // Table function: `func(args) [WITH ORDINALITY] [AS] alias`. Only valid
        // immediately after the (possibly schema-qualified) name.
        let func_args = if self.peeked == Tok::Op("(") {
            self.advance()?;
            let mut args: [&'a Expr<'a>; MAX_LIST] = [self.arena_expr(Expr::Null)?; MAX_LIST];
            let mut n = 0;
            if self.peeked != Tok::Op(")") {
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("function arguments", MAX_LIST));
                    }
                    args[n] = self.expression(0)?;
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            Some(self.arena_slice(&args[..n])?)
        } else {
            None
        };
        // `WITH ORDINALITY` follows the argument list, before any alias.
        let with_ordinality = if func_args.is_some() && self.eat_ident("with")? {
            self.expect_ident("ordinality")?;
            true
        } else {
            false
        };
        let alias = if self.eat_ident("as")? {
            Some(self.col_ident("alias")?)
        } else if let Tok::Ident(word) = self.peeked {
            if is_column_name_keyword(word) {
                None
            } else {
                self.advance()?;
                Some(word)
            }
        } else {
            None
        };
        // A column-alias list `alias(col, ...)` after a table function renames
        // its output columns (the count is validated against the function's
        // arity at planning time, where PostgreSQL's 42P10 error is raised).
        let col_alias = if func_args.is_some() {
            self.column_alias_list()?
        } else {
            None
        };
        Ok(TableRef {
            schema,
            table,
            alias,
            subquery: None,
            func_args,
            rows_from: None,
            col_alias,
            cte: None,
            with_ordinality,
            lateral,
            authorization_role: None,
        })
    }

    /// Parses an optional column-alias list `(col1, col2, ...)` following a FROM
    /// item's correlation name. Returns `None` when there is no list.
    fn column_alias_list(&mut self) -> Result<Option<&'a [&'a str]>, ParseError> {
        if self.peeked != Tok::Op("(") {
            return Ok(None);
        }
        self.advance()?;
        let mut columns: [&'a str; MAX_LIST] = [""; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("column aliases", MAX_LIST));
            }
            columns[n] = self.col_ident("column alias")?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Some(self.arena_slice(&columns[..n])?))
    }

    #[expect(
        clippy::wrong_self_convention,
        reason = "parses the FROM clause; not a conversion"
    )]
    fn from_clause(&mut self) -> Result<FromClause<'a>, ParseError> {
        let base = self.table_ref()?;
        let dummy = Join {
            table: TableRef {
                schema: None,
                table: "",
                alias: None,
                subquery: None,
                func_args: None,
                rows_from: None,
                col_alias: None,
                cte: None,
                with_ordinality: false,
                lateral: false,
                authorization_role: None,
            },
            kind: JoinKind::Inner,
            on: None,
            using_columns: None,
            natural: false,
        };
        let mut joins = [dummy; crate::sql::query::MAX_JOIN_TABLES - 1];
        let mut n = 0;
        loop {
            let natural = self.eat_ident("natural")?;
            let kind = if natural {
                if self.eat_ident("inner")? {
                    self.expect_ident("join")?;
                    JoinKind::Inner
                } else if self.eat_ident("left")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Left
                } else if self.eat_ident("right")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Right
                } else if self.eat_ident("full")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Full
                } else {
                    self.expect_ident("join")?;
                    JoinKind::Inner
                }
            } else if self.eat_op(",")? {
                JoinKind::Cross
            } else if self.eat_ident("cross")? {
                self.expect_ident("join")?;
                JoinKind::Cross
            } else if self.eat_ident("inner")? {
                self.expect_ident("join")?;
                JoinKind::Inner
            } else if self.eat_ident("left")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Left
            } else if self.eat_ident("right")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Right
            } else if self.eat_ident("full")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Full
            } else if self.eat_ident("join")? {
                JoinKind::Inner
            } else {
                break;
            };
            if n == joins.len() {
                return Err(self.limit("joins", joins.len()));
            }
            let table = self.table_ref()?;
            let mut using_columns = None;
            let on = if natural || kind == JoinKind::Cross {
                None
            } else if self.eat_ident("using")? {
                // The merged-column semantics (single output column, resolved
                // against the whole left join tree) are applied at plan time,
                // where the joined tables' columns are known.
                self.expect_op("(")?;
                let mut cols = [""; MAX_USING_COLUMNS];
                let mut n_cols = 0;
                loop {
                    if n_cols == cols.len() {
                        return Err(self.limit("USING columns", cols.len()));
                    }
                    cols[n_cols] = self.col_ident("column name")?;
                    n_cols += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                using_columns = Some(self.arena_slice(&cols[..n_cols])?);
                None
            } else {
                self.expect_ident("on")?;
                Some(self.expression(0)?)
            };
            joins[n] = Join {
                table,
                kind,
                on,
                using_columns,
                natural,
            };
            n += 1;
        }
        Ok(FromClause {
            base,
            joins: self.arena_slice(&joins[..n])?,
        })
    }

    /// `COPY table [(col, ...)] FROM STDIN | TO STDOUT [[WITH] (options)]`.
    /// The resolved option state is typed before execution so streaming COPY
    /// cannot accept an option then lose it between protocol messages.
    fn copy_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("copy")?;
        // `COPY (query) TO STDOUT`: a parenthesized query stands in for a table
        // (a column list requires a table, so `(` here is always a query).
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let start = self.peek_at;
            let _ = self.query_select()?;
            let end = self.peek_at;
            self.expect_op(")")?;
            let query = self.text[start..end].trim();
            if !self.eat_ident("to")? {
                return Err(self.unexpected("COPY (query) supports only TO STDOUT"));
            }
            self.expect_ident("stdout")
                .map_err(|_| self.unexpected("COPY (query) TO supports only STDOUT"))?;
            let options = self.copy_options()?;
            self.validate_copy_options(&options)?;
            return Ok(Stmt::Copy(crate::sql::ast::CopyStmt {
                table: QualName {
                    schema: None,
                    name: "",
                },
                columns: &[],
                query: Some(query),
                to: true,
                options,
                where_clause: None,
                where_text: None,
            }));
        }
        let table = self.qual_name("table name")?;
        let mut columns = [""; crate::storage::MAX_COLUMNS];
        let mut n_columns = 0usize;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                if n_columns == columns.len() {
                    return Err(self.unexpected("too many COPY columns"));
                }
                columns[n_columns] = self.col_ident("column name")?;
                n_columns += 1;
                if self.peeked == Tok::Op(",") {
                    self.advance()?;
                    continue;
                }
                break;
            }
            if self.peeked != Tok::Op(")") {
                return Err(self.unexpected("expected ')'"));
            }
            self.advance()?;
        }
        let to = if self.eat_ident("to")? {
            self.expect_ident("stdout")
                .map_err(|_| self.unexpected("COPY TO supports only STDOUT"))?;
            true
        } else if self.eat_ident("from")? {
            self.expect_ident("stdin")
                .map_err(|_| self.unexpected("COPY FROM supports only STDIN"))?;
            false
        } else {
            return Err(self.unexpected("expected FROM or TO"));
        };
        let options = self.copy_options()?;
        self.validate_copy_options(&options)?;
        let (where_clause, where_text) = if self.eat_ident("where")? {
            let start = self.peek_at;
            let predicate = self.expression(0)?;
            (
                Some(predicate),
                Some(self.text[start..self.peek_at].trim_end()),
            )
        } else {
            (None, None)
        };
        let columns = self.arena_slice(&columns[..n_columns])?;
        Ok(Stmt::Copy(crate::sql::ast::CopyStmt {
            table,
            columns,
            query: None,
            to,
            options,
            where_clause,
            where_text,
        }))
    }

    /// The COPY option list: both the modern `WITH (FORMAT csv, HEADER, ...)`
    /// list and the legacy bare `[WITH] CSV HEADER DELIMITER 'x' ...` shorthand
    /// real tools still emit. Binary format and unknown options fail loudly
    /// rather than mis-read data.
    fn copy_options(&mut self) -> Result<CopyOptions<'a>, ParseError> {
        let mut options = CopyOptions::TEXT;
        let _ = self.eat_ident("with")?;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                self.copy_modern_option(&mut options)?;
                if self.peeked == Tok::Op(",") {
                    self.advance()?;
                    continue;
                }
                break;
            }
            if self.peeked != Tok::Op(")") {
                return Err(self.unexpected("expected ')'"));
            }
            self.advance()?;
        } else {
            while self.copy_legacy_option(&mut options)? {}
        }
        Ok(options)
    }

    /// One option inside `COPY ... WITH ( ... )`.
    fn copy_modern_option(&mut self, options: &mut CopyOptions<'a>) -> Result<(), ParseError> {
        let option = self.any_ident("COPY option")?;
        match option {
            "format" => {
                let format = self.any_ident("COPY format")?;
                options.format = match format {
                    "text" => CopyFormat::Text,
                    "csv" => CopyFormat::Csv,
                    "binary" => CopyFormat::Binary,
                    other => {
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(96, "COPY format \"{}\" does not exist", other),
                            sqlstate: sqlstate::UNDEFINED_OBJECT,
                        });
                    }
                };
            }
            "delimiter" => options.delimiter = Some(self.copy_char("DELIMITER")?),
            "null" => options.null_string = Some(self.copy_string("NULL")?),
            "quote" => options.quote = Some(self.copy_char("QUOTE")?),
            "escape" => options.escape = Some(self.copy_char("ESCAPE")?),
            "header" => options.header = self.copy_header()?,
            "encoding" => self.copy_encoding()?,
            "force_quote" => {
                if self.eat_op("*")? {
                    options.force_quote_all = true;
                } else {
                    options.force_quote = self.copy_column_list()?;
                }
            }
            "force_not_null" => options.force_not_null = self.copy_column_list()?,
            "force_null" => options.force_null = self.copy_column_list()?,
            "on_error" => {
                options.on_error = match self.any_ident("COPY ON_ERROR action")? {
                    "stop" => crate::sql::ast::CopyErrorAction::Stop,
                    "ignore" => crate::sql::ast::CopyErrorAction::Ignore,
                    action => return Err(self.copy_unsupported_option(action)),
                };
            }
            "reject_limit" => {
                let Tok::Num(text) = self.peeked else {
                    return Err(self.unexpected("expected positive COPY REJECT_LIMIT"));
                };
                self.advance()?;
                options.reject_limit = text.parse::<u64>().ok().filter(|limit| *limit > 0);
                if options.reject_limit.is_none() {
                    return Err(self.unexpected("COPY REJECT_LIMIT must be a positive bigint"));
                }
            }
            "log_verbosity" => {
                options.log_verbosity = match self.any_ident("COPY LOG_VERBOSITY")? {
                    "default" => crate::sql::ast::CopyLogVerbosity::Default,
                    "verbose" => crate::sql::ast::CopyLogVerbosity::Verbose,
                    "silent" => crate::sql::ast::CopyLogVerbosity::Silent,
                    value => return Err(self.copy_unsupported_option(value)),
                };
            }
            "default" => options.default_string = Some(self.copy_string("DEFAULT")?),
            "freeze" | "oids" => {
                return Err(self.copy_unsupported_option(option));
            }
            _ => return Err(self.copy_unsupported_option(option)),
        }
        Ok(())
    }

    /// One legacy bare option; returns whether one was consumed.
    fn copy_legacy_option(&mut self, options: &mut CopyOptions<'a>) -> Result<bool, ParseError> {
        if self.eat_ident("binary")? {
            options.format = CopyFormat::Binary;
        } else if self.eat_ident("csv")? {
            options.format = CopyFormat::Csv;
        } else if self.eat_ident("header")? {
            options.header = crate::sql::ast::CopyHeader::Skip;
        } else if self.eat_ident("delimiter")? {
            let _ = self.eat_ident("as")?;
            options.delimiter = Some(self.copy_char("DELIMITER")?);
        } else if self.eat_ident("null")? {
            let _ = self.eat_ident("as")?;
            options.null_string = Some(self.copy_string("NULL")?);
        } else if self.eat_ident("quote")? {
            let _ = self.eat_ident("as")?;
            options.quote = Some(self.copy_char("QUOTE")?);
        } else if self.eat_ident("escape")? {
            let _ = self.eat_ident("as")?;
            options.escape = Some(self.copy_char("ESCAPE")?);
        } else if self.eat_ident("encoding")? {
            let _ = self.eat_ident("as")?;
            self.copy_encoding()?;
        } else if self.eat_ident("force")? {
            if self.eat_ident("quote")? {
                if self.eat_op("*")? {
                    options.force_quote_all = true;
                } else {
                    options.force_quote = self.copy_column_list()?;
                }
            } else if self.eat_ident("not")? {
                self.expect_ident("null")?;
                options.force_not_null = self.copy_column_list()?;
            } else {
                self.expect_ident("null")?;
                options.force_null = self.copy_column_list()?;
            }
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    /// The CSV-only options are meaningless in text format, exactly as
    /// PostgreSQL rejects them.
    fn validate_copy_options(&self, options: &CopyOptions) -> Result<(), ParseError> {
        // Binary format takes none of the text/CSV field options.
        if options.is_binary() {
            let text_only = if options.delimiter.is_some() {
                Some("DELIMITER")
            } else if options.null_string.is_some() {
                Some("NULL")
            } else if !matches!(options.header, crate::sql::ast::CopyHeader::None) {
                Some("HEADER")
            } else if options.default_string.is_some() {
                Some("DEFAULT")
            } else {
                None
            };
            if let Some(name) = text_only {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "cannot specify {} in BINARY mode", name),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            }
        }
        if !options.is_csv() {
            let csv_only = if options.quote.is_some() {
                Some("QUOTE")
            } else if options.escape.is_some() {
                Some("ESCAPE")
            } else if options.force_quote_all || !options.force_quote.is_empty() {
                Some("FORCE_QUOTE")
            } else if !options.force_not_null.is_empty() {
                Some("FORCE_NOT_NULL")
            } else if !options.force_null.is_empty() {
                Some("FORCE_NULL")
            } else {
                None
            };
            if let Some(name) = csv_only {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "COPY {} requires CSV mode", name),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            }
        }
        if matches!(options.on_error, crate::sql::ast::CopyErrorAction::Ignore)
            && options.is_binary()
        {
            return Err(self.copy_unsupported("COPY ON_ERROR ignore in BINARY mode"));
        }
        if options.reject_limit.is_some()
            && !matches!(options.on_error, crate::sql::ast::CopyErrorAction::Ignore)
        {
            return Err(self.unexpected("COPY REJECT_LIMIT requires ON_ERROR ignore"));
        }
        if !matches!(
            options.log_verbosity,
            crate::sql::ast::CopyLogVerbosity::Default
        ) && !matches!(options.on_error, crate::sql::ast::CopyErrorAction::Ignore)
        {
            return Err(self.unexpected("COPY LOG_VERBOSITY requires ON_ERROR ignore"));
        }
        Ok(())
    }

    /// A single-byte character option (`DELIMITER`, `QUOTE`, `ESCAPE`).
    fn copy_char(&mut self, what: &'static str) -> Result<u8, ParseError> {
        let s = self.copy_string(what)?;
        let bytes = s.as_bytes();
        if bytes.len() != 1 {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "COPY {} must be a single one-byte character", what),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
            });
        }
        Ok(bytes[0])
    }

    /// A required single-quoted string literal (e.g. an enum label).
    pub(super) fn str_literal(&mut self, what: &'static str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Str(s) => {
                self.advance()?;
                Ok(s)
            }
            _ => Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "expected {} as a quoted string literal", what),
                sqlstate: sqlstate::SYNTAX_ERROR,
            }),
        }
    }

    /// A string-literal option value.
    fn copy_string(&mut self, what: &'static str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Str(s) => {
                self.advance()?;
                Ok(s)
            }
            _ => Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "COPY {} requires a quoted string", what),
                sqlstate: sqlstate::SYNTAX_ERROR,
            }),
        }
    }

    /// `HEADER` with an optional boolean (`true`/`false`/`on`/`off`); bare = true.
    fn copy_header(&mut self) -> Result<crate::sql::ast::CopyHeader, ParseError> {
        if self.eat_ident("match")? {
            return Ok(crate::sql::ast::CopyHeader::Match);
        }
        if self.eat_ident("true")? || self.eat_ident("on")? {
            Ok(crate::sql::ast::CopyHeader::Skip)
        } else if self.eat_ident("false")? || self.eat_ident("off")? {
            Ok(crate::sql::ast::CopyHeader::None)
        } else if let Tok::Str(s) = self.peeked {
            self.advance()?;
            match s {
                "true" | "on" | "1" => Ok(crate::sql::ast::CopyHeader::Skip),
                "false" | "off" | "0" => Ok(crate::sql::ast::CopyHeader::None),
                "match" => Ok(crate::sql::ast::CopyHeader::Match),
                _ => Err(self.unexpected("COPY HEADER requires a boolean or MATCH")),
            }
        } else {
            Ok(crate::sql::ast::CopyHeader::Skip)
        }
    }

    /// `ENCODING 'utf8'` — only UTF-8 is supported; anything else is loud.
    /// Compared case-insensitively, ignoring `-`/`_`, without allocating.
    fn copy_encoding(&mut self) -> Result<(), ParseError> {
        let encoding = self.copy_string("ENCODING")?;
        let mut norm = [0u8; 12];
        let mut n = 0usize;
        for &b in encoding.as_bytes() {
            if b == b'-' || b == b'_' {
                continue;
            }
            if n == norm.len() {
                return Err(self.copy_unsupported("a COPY ENCODING other than UTF8"));
            }
            norm[n] = b.to_ascii_lowercase();
            n += 1;
        }
        if matches!(&norm[..n], b"utf8" | b"unicode") {
            Ok(())
        } else {
            Err(self.copy_unsupported("a COPY ENCODING other than UTF8"))
        }
    }

    /// A parenthesized column-name list `( a, b, ... )` for a FORCE option.
    fn copy_column_list(&mut self) -> Result<&'a [&'a str], ParseError> {
        if self.peeked != Tok::Op("(") {
            return Err(self.unexpected("expected '(' for a COPY column list"));
        }
        self.advance()?;
        let mut names = [""; crate::storage::MAX_COLUMNS];
        let mut n = 0usize;
        loop {
            if n == names.len() {
                return Err(self.unexpected("too many COPY columns"));
            }
            names[n] = self.col_ident("column name")?;
            n += 1;
            if self.peeked == Tok::Op(",") {
                self.advance()?;
                continue;
            }
            break;
        }
        if self.peeked != Tok::Op(")") {
            return Err(self.unexpected("expected ')'"));
        }
        self.advance()?;
        self.arena_slice(&names[..n])
    }

    fn copy_unsupported_option(&self, option: &str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(96, "COPY option \"{}\" is not supported", option),
            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
        }
    }

    fn copy_unsupported(&self, what: &str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(96, "{} is not supported", what),
            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
        }
    }

    /// The tail of `ALTER [COLUMN] col [SET DATA] TYPE newtype [USING expr]`,
    /// after the `TYPE` keyword.
    fn alter_column_type(&mut self, column: &'a str) -> Result<AlterAction<'a>, ParseError> {
        let (type_name, type_mod) = self.type_name_mod()?;
        let collation = if self.eat_ident("collate")? {
            Some(self.collation_name()?)
        } else {
            None
        };
        let using = if self.eat_ident("using")? {
            Some(self.expression(0)?)
        } else {
            None
        };
        Ok(AlterAction::AlterColumnType {
            column,
            type_name,
            type_mod,
            collation,
            using,
        })
    }

    /// VACUUM / ANALYZE with their shared shape: an optional parenthesized or
    /// bare option list, then an optional comma-separated table list, each
    /// table with an optional column list.
    fn vacuum_or_analyze(&mut self, is_vacuum: bool) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // the VACUUM / ANALYZE keyword
        let mut run_analyze = !is_vacuum;
        if self.eat_op("(")? {
            // A parenthesized option list — consume up to the matching ')'.
            let mut depth = 1;
            while depth > 0 {
                if self.peeked == Tok::Eof {
                    return Err(self.unexpected("unterminated option list"));
                }
                if matches!(self.peeked, Tok::Ident(word) if word.eq_ignore_ascii_case("analyze") || word.eq_ignore_ascii_case("analyse"))
                {
                    run_analyze = true;
                }
                if self.eat_op("(")? {
                    depth += 1;
                } else if self.eat_op(")")? {
                    depth -= 1;
                } else {
                    self.advance()?;
                }
            }
        } else {
            // The bare-keyword form: FULL / FREEZE / VERBOSE / ANALYZE.
            loop {
                if self.eat_ident("full")?
                    || self.eat_ident("freeze")?
                    || self.eat_ident("verbose")?
                {
                    continue;
                }
                if self.eat_ident("analyze")? || self.eat_ident("analyse")? {
                    run_analyze = true;
                    continue;
                }
                break;
            }
        }
        // An optional table list, each with an optional column list.
        let empty_target = crate::sql::ast::MaintenanceTarget {
            table: QualName::bare(""),
            columns: &[],
        };
        let mut targets = [empty_target; 64];
        let mut target_count = 0usize;
        if !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            loop {
                if target_count == targets.len() {
                    return Err(self.unexpected("too many maintenance targets"));
                }
                let table = self.qual_name("table name")?;
                let mut column_names = [""; crate::storage::MAX_COLUMNS];
                let mut column_count = 0usize;
                if self.eat_op("(")? {
                    loop {
                        if column_count == column_names.len() {
                            return Err(self.unexpected("too many maintenance columns"));
                        }
                        column_names[column_count] = self.col_ident("column name")?;
                        column_count += 1;
                        if !self.eat_op(",")? {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                }
                targets[target_count] = crate::sql::ast::MaintenanceTarget {
                    table,
                    columns: self.arena_slice(&column_names[..column_count])?,
                };
                target_count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let targets = self.arena_slice(&targets[..target_count])?;
        Ok(if is_vacuum {
            Stmt::Vacuum {
                targets,
                analyze: run_analyze,
            }
        } else {
            Stmt::Analyze(targets)
        })
    }

    fn role_name_list(&mut self, what: &str) -> Result<&'a [&'a str], ParseError> {
        let mut names = [""; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == names.len() {
                return Err(self.limit("roles", names.len()));
            }
            names[count] = self.any_ident(what)?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&names[..count])
    }

    fn privilege_start(&self) -> bool {
        matches!(
            self.peeked,
            Tok::Ident(
                "all"
                    | "select"
                    | "insert"
                    | "update"
                    | "delete"
                    | "truncate"
                    | "references"
                    | "trigger"
                    | "usage"
                    | "create"
                    | "execute"
                    | "maintain"
            )
        )
    }

    fn grant_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("grant")?;
        if !self.privilege_start() {
            return self.grant_role_after_keyword();
        }
        let privileges = self.privilege_list()?;
        self.expect_ident("on")?;
        let target = self.privilege_target()?;
        self.expect_ident("to")?;
        let grantees = self.role_name_list("grantee")?;
        let grant_option = if self.eat_ident("with")? {
            self.expect_ident("grant")?;
            self.expect_ident("option")?;
            true
        } else {
            false
        };
        Ok(Stmt::GrantPrivileges {
            privileges,
            target,
            grantees,
            grant_option,
        })
    }

    fn grant_role_after_keyword(&mut self) -> Result<Stmt<'a>, ParseError> {
        let roles = self.role_name_list("role name")?;
        self.expect_ident("to")?;
        let members = self.role_name_list("member role name")?;
        let mut options = crate::sql::ast::RoleGrantOptions::DEFAULT;
        if self.eat_ident("with")? {
            loop {
                if self.eat_ident("admin")? {
                    let _ = self.eat_ident("option")?;
                    options.admin = true;
                } else if self.eat_ident("inherit")? {
                    options.inherit = self.role_option_boolean()?;
                } else if self.eat_ident("set")? {
                    options.set = self.role_option_boolean()?;
                } else {
                    break;
                }
                let _ = self.eat_op(",")?;
            }
        }
        Ok(Stmt::GrantRole {
            roles,
            members,
            options,
        })
    }

    fn privilege_list(&mut self) -> Result<&'a [crate::sql::ast::Privilege], ParseError> {
        use crate::sql::ast::Privilege;
        let mut privileges = [Privilege::All; 12];
        let mut count = 0usize;
        loop {
            if count == privileges.len() {
                return Err(self.limit("privileges", privileges.len()));
            }
            privileges[count] = if self.eat_ident("all")? {
                let _ = self.eat_ident("privileges")?;
                Privilege::All
            } else if self.eat_ident("select")? {
                Privilege::Select
            } else if self.eat_ident("insert")? {
                Privilege::Insert
            } else if self.eat_ident("update")? {
                Privilege::Update
            } else if self.eat_ident("delete")? {
                Privilege::Delete
            } else if self.eat_ident("truncate")? {
                Privilege::Truncate
            } else if self.eat_ident("references")? {
                Privilege::References
            } else if self.eat_ident("trigger")? {
                Privilege::Trigger
            } else if self.eat_ident("usage")? {
                Privilege::Usage
            } else if self.eat_ident("create")? {
                Privilege::Create
            } else if self.eat_ident("execute")? {
                Privilege::Execute
            } else if self.eat_ident("maintain")? {
                Privilege::Maintain
            } else {
                return Err(self.unexpected("expected an object privilege"));
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&privileges[..count])
    }

    fn privilege_target(&mut self) -> Result<crate::sql::ast::PrivilegeTarget<'a>, ParseError> {
        use crate::sql::ast::{PrivilegeObjectKind, PrivilegeTarget, RoutineTargetKind};
        let kind = if self.eat_ident("all")? {
            if self.eat_ident("tables")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllTablesInSchema
            } else if self.eat_ident("sequences")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllSequencesInSchema
            } else if self.eat_ident("functions")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllFunctionsInSchema
            } else {
                return Err(self.unexpected("expected TABLES, SEQUENCES, or FUNCTIONS after ALL"));
            }
        } else if self.eat_ident("function")? {
            return self.routine_privilege_target(RoutineTargetKind::Function);
        } else if self.eat_ident("procedure")? {
            return self.routine_privilege_target(RoutineTargetKind::Procedure);
        } else if self.eat_ident("routine")? {
            return self.routine_privilege_target(RoutineTargetKind::Either);
        } else if self.eat_ident("table")? {
            PrivilegeObjectKind::Table
        } else if self.eat_ident("sequence")? {
            PrivilegeObjectKind::Sequence
        } else if self.eat_ident("schema")? {
            PrivilegeObjectKind::Schema
        } else if self.eat_ident("tablespace")? {
            PrivilegeObjectKind::Tablespace
        } else if self.eat_ident("type")? || self.eat_ident("domain")? {
            PrivilegeObjectKind::Type
        } else {
            // TABLE is PostgreSQL's default object kind.
            PrivilegeObjectKind::Table
        };
        let mut names = [QualName::bare(""); MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == names.len() {
                return Err(self.limit("privilege targets", names.len()));
            }
            names[count] = if matches!(
                kind,
                PrivilegeObjectKind::Schema
                    | PrivilegeObjectKind::Tablespace
                    | PrivilegeObjectKind::AllTablesInSchema
                    | PrivilegeObjectKind::AllSequencesInSchema
                    | PrivilegeObjectKind::AllFunctionsInSchema
            ) {
                QualName::bare(self.col_ident("schema name")?)
            } else {
                self.qual_name("object name")?
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(PrivilegeTarget::Objects {
            kind,
            names: self.arena_slice(&names[..count])?,
        })
    }

    fn routine_privilege_target(
        &mut self,
        kind: crate::sql::ast::RoutineTargetKind,
    ) -> Result<crate::sql::ast::PrivilegeTarget<'a>, ParseError> {
        use crate::sql::ast::{PrivilegeTarget, RoutineIdentity};
        let mut identities = [RoutineIdentity {
            name: QualName::bare(""),
            argument_types: &[],
            signature_is_explicit: true,
        }; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == identities.len() {
                return Err(self.limit("routine privilege targets", identities.len()));
            }
            let name = self.qual_name("routine name")?;
            self.expect_op("(")?;
            let mut argument_types = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut argument_count = 0usize;
            if !self.eat_op(")")? {
                loop {
                    if argument_count == argument_types.len() {
                        return Err(self.limit("routine arguments", argument_types.len()));
                    }
                    argument_types[argument_count] = self.any_ident("routine argument type")?;
                    argument_count += 1;
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            identities[count] = RoutineIdentity {
                name,
                argument_types: self.arena_slice(&argument_types[..argument_count])?,
                signature_is_explicit: true,
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(PrivilegeTarget::Routines {
            kind,
            identities: self.arena_slice(&identities[..count])?,
        })
    }

    fn role_option_boolean(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("true")? {
            Ok(true)
        } else if self.eat_ident("false")? {
            Ok(false)
        } else {
            Err(self.unexpected("expected TRUE or FALSE"))
        }
    }

    fn revoke_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("revoke")?;
        if self.eat_ident("admin")? {
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            return self.revoke_role_after_keyword(true);
        }
        let grant_option_only = if self.eat_ident("grant")? {
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            true
        } else {
            false
        };
        if !self.privilege_start() {
            return self.revoke_role_after_keyword(false);
        }
        let privileges = self.privilege_list()?;
        self.expect_ident("on")?;
        let target = self.privilege_target()?;
        self.expect_ident("from")?;
        let grantees = self.role_name_list("grantee")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::RevokePrivileges {
            grant_option_only,
            privileges,
            target,
            grantees,
            cascade,
        })
    }

    fn revoke_role_after_keyword(
        &mut self,
        admin_option_only: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        let roles = self.role_name_list("role name")?;
        self.expect_ident("from")?;
        let members = self.role_name_list("member role name")?;
        let _ = self.eat_ident("cascade")? || self.eat_ident("restrict")?;
        Ok(Stmt::RevokeRole {
            roles,
            members,
            admin_option_only,
        })
    }

    /// `REINDEX [ ( options ) ] target [ CONCURRENTLY ] [ name ]`.
    fn reindex(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("reindex")?;
        let mut build = IndexBuildMode::Blocking;
        let mut saw_concurrently = false;
        let mut tablespace = None;
        let mut verbose = false;
        let mut saw_verbose = false;
        if self.eat_op("(")? {
            loop {
                let option = self.any_ident("REINDEX option")?;
                if option.eq_ignore_ascii_case("concurrently") {
                    if core::mem::replace(&mut saw_concurrently, true) {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    let value = if self.peeked == Tok::Op(",") || self.peeked == Tok::Op(")") {
                        true
                    } else {
                        let _ = self.eat_op("=")?;
                        self.role_option_boolean()?
                    };
                    build = if value {
                        IndexBuildMode::Concurrent
                    } else {
                        IndexBuildMode::Blocking
                    };
                } else if option.eq_ignore_ascii_case("verbose") {
                    if core::mem::replace(&mut saw_verbose, true) {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    verbose = if self.peeked == Tok::Op(",") || self.peeked == Tok::Op(")") {
                        true
                    } else {
                        let _ = self.eat_op("=")?;
                        self.role_option_boolean()?
                    };
                } else if option.eq_ignore_ascii_case("tablespace") {
                    if tablespace.is_some() {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    let _ = self.eat_op("=")?;
                    tablespace = Some(self.any_ident("tablespace name")?);
                } else {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(96, "unrecognized REINDEX option \"{}\"", option),
                        sqlstate: sqlstate::SYNTAX_ERROR,
                    });
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let target = if self.eat_ident("index")? {
            ReindexTarget::Index
        } else if self.eat_ident("table")? {
            ReindexTarget::Table
        } else if self.eat_ident("schema")? {
            ReindexTarget::Schema
        } else if self.eat_ident("database")? {
            ReindexTarget::Database
        } else if self.eat_ident("system")? {
            ReindexTarget::System
        } else {
            return Err(self.err_here("expected INDEX, TABLE, SCHEMA, DATABASE, or SYSTEM"));
        };
        if self.eat_ident("concurrently")? {
            if saw_concurrently {
                return Err(self.err_here("CONCURRENTLY specified more than once"));
            }
            build = IndexBuildMode::Concurrent;
        }
        let name = if matches!(target, ReindexTarget::Database | ReindexTarget::System)
            && matches!(self.peeked, Tok::Op(";") | Tok::Eof)
        {
            None
        } else if matches!(target, ReindexTarget::Database | ReindexTarget::System) {
            Some(QualName::bare(self.any_ident("database name")?))
        } else {
            Some(self.qual_name("reindex target")?)
        };
        Ok(Stmt::Reindex {
            target,
            name,
            options: ReindexOptions {
                build,
                tablespace,
                verbose,
            },
        })
    }

    fn subscription_publication_change(
        &mut self,
    ) -> Result<(&'a [&'a str], SubscriptionPublicationRefresh), ParseError> {
        let mut publications = [""; MAX_LIST];
        let mut count = 0;
        loop {
            if count == publications.len() {
                return Err(self.limit("subscription publications", publications.len()));
            }
            publications[count] = self.any_ident("publication name")?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let mut refresh = true;
        let mut copy_data = true;
        let mut saw_refresh = false;
        let mut saw_copy_data = false;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            loop {
                let option = self.any_ident("publication option")?;
                if option.eq_ignore_ascii_case("refresh") {
                    if core::mem::replace(&mut saw_refresh, true) {
                        return Err(self.err_here("duplicate publication option refresh"));
                    }
                    let _ = self.eat_op("=")?;
                    refresh = self.role_option_boolean()?;
                } else if option.eq_ignore_ascii_case("copy_data") {
                    if core::mem::replace(&mut saw_copy_data, true) {
                        return Err(self.err_here("duplicate publication option copy_data"));
                    }
                    let _ = self.eat_op("=")?;
                    copy_data = self.role_option_boolean()?;
                } else {
                    return Err(self.err_here("unrecognized publication option"));
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        if !refresh && saw_copy_data {
            return Err(self.err_here("copy_data requires refresh = true"));
        }
        Ok((
            self.arena_slice(&publications[..count])?,
            if refresh {
                SubscriptionPublicationRefresh::Refresh { copy_data }
            } else {
                SubscriptionPublicationRefresh::NoRefresh
            },
        ))
    }

    fn subscription_settings_patch(&mut self) -> Result<SubscriptionSettingsPatch<'a>, ParseError> {
        let mut patch = SubscriptionSettingsPatch {
            slot: None,
            binary: None,
            streaming: None,
            synchronous_commit: None,
            two_phase: None,
            disable_on_error: None,
            password_required: None,
            run_as_owner: None,
            origin: None,
            failover: None,
        };
        self.expect_op("(")?;
        loop {
            let key = self.any_ident("subscription setting")?;
            if key.eq_ignore_ascii_case("slot_name") {
                if patch.slot.is_some() {
                    return Err(self.err_here("duplicate subscription setting slot_name"));
                }
                self.expect_op("=")?;
                patch.slot = Some(if self.eat_ident("none")? {
                    SubscriptionSlotSetting::Absent
                } else {
                    let value = match self.peeked {
                        Tok::Ident(value) | Tok::Str(value) => value,
                        _ => return Err(self.err_here("slot_name requires a name or NONE")),
                    };
                    self.advance()?;
                    SubscriptionSlotSetting::Named(value)
                });
            } else if key.eq_ignore_ascii_case("binary") {
                if patch.binary.is_some() {
                    return Err(self.err_here("duplicate subscription setting binary"));
                }
                patch.binary = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("streaming") {
                if patch.streaming.is_some() {
                    return Err(self.err_here("duplicate subscription setting streaming"));
                }
                patch.streaming = Some(self.subscription_streaming()?);
            } else if key.eq_ignore_ascii_case("synchronous_commit") {
                if patch.synchronous_commit.is_some() {
                    return Err(self.err_here("duplicate subscription setting synchronous_commit"));
                }
                patch.synchronous_commit = Some(self.subscription_synchronous_commit()?);
            } else if key.eq_ignore_ascii_case("two_phase") {
                if patch.two_phase.is_some() {
                    return Err(self.err_here("duplicate subscription setting two_phase"));
                }
                patch.two_phase = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("disable_on_error") {
                if patch.disable_on_error.is_some() {
                    return Err(self.err_here("duplicate subscription setting disable_on_error"));
                }
                patch.disable_on_error = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("password_required") {
                if patch.password_required.is_some() {
                    return Err(self.err_here("duplicate subscription setting password_required"));
                }
                patch.password_required = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("run_as_owner") {
                if patch.run_as_owner.is_some() {
                    return Err(self.err_here("duplicate subscription setting run_as_owner"));
                }
                patch.run_as_owner = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("origin") {
                if patch.origin.is_some() {
                    return Err(self.err_here("duplicate subscription setting origin"));
                }
                patch.origin = Some(self.subscription_origin()?);
            } else if key.eq_ignore_ascii_case("failover") {
                if patch.failover.is_some() {
                    return Err(self.err_here("duplicate subscription setting failover"));
                }
                patch.failover = Some(self.subscription_bool_option(key)?);
            } else {
                return Err(self.err_here("unrecognized subscription setting"));
            }
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        Ok(patch)
    }

    fn subscription_lsn(value: &str) -> Option<u64> {
        let (high, low) = value.split_once('/')?;
        if high.is_empty() || low.is_empty() || high.len() > 8 || low.len() > 8 {
            return None;
        }
        Some((u64::from_str_radix(high, 16).ok()? << 32) | u64::from_str_radix(low, 16).ok()?)
    }

    fn alter_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("alter")?;
        use crate::sql::ast::AlterOwnerKind;
        if self.eat_ident("default")? {
            self.expect_ident("privileges")?;
            return self.alter_default_privileges();
        }
        if self.eat_ident("role")? || self.eat_ident("user")? || self.eat_ident("group")? {
            return self.alter_role();
        }
        if self.eat_ident("publication")? {
            return self.alter_publication();
        }
        if self.eat_ident("subscription")? {
            let name = self.any_ident("subscription name")?;
            let action = if self.eat_ident("owner")? {
                self.expect_ident("to")?;
                AlterSubscriptionAction::SetOwner(self.any_ident("role name")?)
            } else if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                AlterSubscriptionAction::Rename(self.any_ident("new subscription name")?)
            } else if self.eat_ident("enable")? {
                AlterSubscriptionAction::Enable
            } else if self.eat_ident("disable")? {
                AlterSubscriptionAction::Disable
            } else if self.eat_ident("connection")? {
                AlterSubscriptionAction::SetConnection(
                    self.str_literal("subscription connection string")?,
                )
            } else if self.eat_ident("set")? {
                if self.peeked == Tok::Op("(") {
                    AlterSubscriptionAction::SetOptions(self.subscription_settings_patch()?)
                } else {
                    self.expect_ident("publication")?;
                    let (publications, refresh) = self.subscription_publication_change()?;
                    AlterSubscriptionAction::SetPublications {
                        publications,
                        refresh,
                    }
                }
            } else if self.eat_ident("add")? {
                self.expect_ident("publication")?;
                let (publications, refresh) = self.subscription_publication_change()?;
                AlterSubscriptionAction::AddPublications {
                    publications,
                    refresh,
                }
            } else if self.eat_ident("drop")? {
                self.expect_ident("publication")?;
                let (publications, refresh) = self.subscription_publication_change()?;
                AlterSubscriptionAction::DropPublications {
                    publications,
                    refresh,
                }
            } else if self.eat_ident("refresh")? {
                self.expect_ident("publication")?;
                let copy_data = if self.eat_ident("with")? {
                    self.expect_op("(")?;
                    self.expect_ident("copy_data")?;
                    let _ = self.eat_op("=")?;
                    let copy_data = self.role_option_boolean()?;
                    self.expect_op(")")?;
                    copy_data
                } else {
                    true
                };
                AlterSubscriptionAction::RefreshPublications { copy_data }
            } else if self.eat_ident("skip")? {
                self.expect_op("(")?;
                self.expect_ident("lsn")?;
                self.expect_op("=")?;
                let lsn = if self.eat_ident("none")? {
                    None
                } else {
                    let value = match self.peeked {
                        Tok::Ident(value) | Tok::Str(value) => value,
                        _ => return Err(self.err_here("subscription skip LSN is invalid")),
                    };
                    self.advance()?;
                    Some(
                        Self::subscription_lsn(value)
                            .ok_or_else(|| self.err_here("subscription skip LSN is invalid"))?,
                    )
                };
                self.expect_op(")")?;
                AlterSubscriptionAction::Skip { lsn }
            } else {
                return Err(self.err_here("invalid ALTER SUBSCRIPTION action"));
            };
            return Ok(Stmt::AlterSubscription { name, action });
        }
        if self.eat_ident("trigger")? {
            return self.alter_trigger();
        }
        if self.eat_ident("policy")? {
            return self.alter_policy();
        }
        if self.eat_ident("statistics")? {
            return self.alter_statistics();
        }
        if self.eat_ident("index")? {
            return self.alter_index();
        }
        if self.eat_ident("tablespace")? {
            return self.alter_tablespace();
        }
        if self.eat_ident("extension")? {
            return self.alter_extension();
        }
        if self.eat_ident("aggregate")? {
            return self.alter_aggregate();
        }
        if self.eat_ident("function")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Function);
        }
        if self.eat_ident("procedure")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Procedure);
        }
        if self.eat_ident("routine")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Either);
        }
        if self.eat_ident("schema")? {
            let name = QualName {
                schema: None,
                name: self.col_ident("schema name")?,
            };
            return self.alter_owner(AlterOwnerKind::Schema, name, false);
        }
        if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.qual_name("materialized view name")?;
            if self.peeked == Tok::Ident("depends") || self.peeked == Tok::Ident("no") {
                if if_exists {
                    return Err(self.err_here("IF EXISTS is not allowed with DEPENDS ON EXTENSION"));
                }
                let enabled = !self.eat_ident("no")?;
                self.expect_ident("depends")?;
                self.expect_ident("on")?;
                self.expect_ident("extension")?;
                return Ok(Stmt::AlterMaterializedViewExtensionDependency {
                    name,
                    extension: self.col_ident("extension name")?,
                    enabled,
                });
            }
            return self.alter_owner(AlterOwnerKind::MaterializedView, name, if_exists);
        }
        if self.eat_ident("view")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.qual_name("view name")?;
            return self.alter_owner(AlterOwnerKind::View, name, if_exists);
        }
        if self.eat_ident("sequence")? {
            return self.alter_sequence();
        }
        if self.eat_ident("domain")? {
            return self.alter_domain();
        }
        if self.eat_ident("type")? {
            return self.alter_type();
        }
        self.expect_ident("table")?;
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let only = self.eat_ident("only")?;
        let table = self.qual_name("table name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(AlterOwnerKind::Table, table, if_exists);
        }
        // RENAME … and SET SCHEMA are standalone forms: PostgreSQL does not
        // combine them with a comma-separated subcommand list, so they parse to
        // a single-element list on their own.
        if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            let action = AlterAction::SetSchema(self.col_ident("schema name")?);
            return Ok(Stmt::AlterTable(AlterTable {
                table,
                if_exists,
                only,
                actions: self.arena_slice(&[action])?,
            }));
        }
        if self.eat_ident("rename")? {
            let action = if self.eat_ident("to")? {
                AlterAction::RenameTable(self.col_ident("new table name")?)
            } else if self.eat_ident("constraint")? {
                let from = self.col_ident("constraint name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new constraint name")?;
                AlterAction::RenameConstraint { from, to }
            } else {
                let _ = self.eat_ident("column")?;
                let from = self.col_ident("column name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new column name")?;
                AlterAction::RenameColumn { from, to }
            };
            return Ok(Stmt::AlterTable(AlterTable {
                table,
                if_exists,
                only,
                actions: self.arena_slice(&[action])?,
            }));
        }
        if self.eat_ident("force")? {
            self.expect_ident("row")?;
            self.expect_ident("level")?;
            self.expect_ident("security")?;
            return Ok(Stmt::AlterTable(AlterTable {
                table,
                if_exists,
                only,
                actions: self.arena_slice(&[AlterAction::SetRowLevelSecurity(
                    RowLevelSecurityAlteration::Force,
                )])?,
            }));
        }
        if self.eat_ident("no")? {
            self.expect_ident("force")?;
            self.expect_ident("row")?;
            self.expect_ident("level")?;
            self.expect_ident("security")?;
            return Ok(Stmt::AlterTable(AlterTable {
                table,
                if_exists,
                only,
                actions: self.arena_slice(&[AlterAction::SetRowLevelSecurity(
                    RowLevelSecurityAlteration::NoForce,
                )])?,
            }));
        }
        let trigger_mode = if self.eat_ident("enable")? {
            if self.eat_ident("row")? {
                self.expect_ident("level")?;
                self.expect_ident("security")?;
                return Ok(Stmt::AlterTable(AlterTable {
                    table,
                    if_exists,
                    only,
                    actions: self.arena_slice(&[AlterAction::SetRowLevelSecurity(
                        RowLevelSecurityAlteration::Enable,
                    )])?,
                }));
            }
            if self.eat_ident("replica")? {
                Some(crate::sql::ast::TriggerEnableMode::Replica)
            } else if self.eat_ident("always")? {
                Some(crate::sql::ast::TriggerEnableMode::Always)
            } else {
                Some(crate::sql::ast::TriggerEnableMode::Origin)
            }
        } else if self.eat_ident("disable")? {
            if self.eat_ident("row")? {
                self.expect_ident("level")?;
                self.expect_ident("security")?;
                return Ok(Stmt::AlterTable(AlterTable {
                    table,
                    if_exists,
                    only,
                    actions: self.arena_slice(&[AlterAction::SetRowLevelSecurity(
                        RowLevelSecurityAlteration::Disable,
                    )])?,
                }));
            }
            Some(crate::sql::ast::TriggerEnableMode::Disabled)
        } else {
            None
        };
        if let Some(enabled) = trigger_mode {
            self.expect_ident("trigger")?;
            let target = match (enabled, self.peeked) {
                (
                    crate::sql::ast::TriggerEnableMode::Origin
                    | crate::sql::ast::TriggerEnableMode::Disabled,
                    Tok::Ident("all"),
                ) => {
                    self.advance()?;
                    crate::sql::ast::TriggerEnableTarget::All
                }
                (
                    crate::sql::ast::TriggerEnableMode::Origin
                    | crate::sql::ast::TriggerEnableMode::Disabled,
                    Tok::Ident("user"),
                ) => {
                    self.advance()?;
                    crate::sql::ast::TriggerEnableTarget::User
                }
                _ => crate::sql::ast::TriggerEnableTarget::Name(self.col_ident("trigger name")?),
            };
            let action = AlterAction::SetTriggerEnabled { target, enabled };
            return Ok(Stmt::AlterTable(AlterTable {
                table,
                if_exists,
                only,
                actions: self.arena_slice(&[action])?,
            }));
        }
        // Otherwise a comma-separated list of ADD / DROP / ALTER subcommands.
        let mut buffer = [AlterAction::DropDefault { column: "" }; MAX_ALTER_ACTIONS];
        let mut count = 0usize;
        loop {
            if count == MAX_ALTER_ACTIONS {
                return Err(self.err_here("too many actions in one ALTER TABLE"));
            }
            buffer[count] = self.alter_table_cmd()?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        // PostgreSQL executes the subcommands in a fixed pass order, not the
        // written order, so a constraint can reference a column added later in
        // the same statement. A stable insertion sort by pass keeps the written
        // order within a pass and allocates nothing.
        for i in 1..count {
            let mut j = i;
            while j > 0 && alter_pass(&buffer[j - 1]) > alter_pass(&buffer[j]) {
                buffer.swap(j - 1, j);
                j -= 1;
            }
        }
        Ok(Stmt::AlterTable(AlterTable {
            table,
            if_exists,
            only,
            actions: self.arena_slice(&buffer[..count])?,
        }))
    }

    fn alter_default_privileges(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{DefaultPrivilegeAction, DefaultPrivilegeObjectKind};

        let roles = if self.eat_ident("for")? {
            if !self.eat_ident("role")? {
                self.expect_ident("user")?;
            }
            self.role_name_list("role name")?
        } else {
            &[]
        };
        let schemas = if self.eat_ident("in")? {
            self.expect_ident("schema")?;
            self.role_name_list("schema name")?
        } else {
            &[]
        };

        let grant = if self.eat_ident("grant")? {
            true
        } else {
            self.expect_ident("revoke")?;
            false
        };
        let grant_option_only = if !grant && self.eat_ident("grant")? {
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            true
        } else {
            false
        };
        let privileges = self.privilege_list()?;
        self.expect_ident("on")?;
        let kind = if self.eat_ident("tables")? {
            DefaultPrivilegeObjectKind::Tables
        } else if self.eat_ident("sequences")? {
            DefaultPrivilegeObjectKind::Sequences
        } else if self.eat_ident("functions")? || self.eat_ident("routines")? {
            DefaultPrivilegeObjectKind::Functions
        } else if self.eat_ident("types")? {
            DefaultPrivilegeObjectKind::Types
        } else if self.eat_ident("schemas")? {
            DefaultPrivilegeObjectKind::Schemas
        } else {
            return Err(self
                .unexpected("expected TABLES, SEQUENCES, FUNCTIONS, ROUTINES, TYPES, or SCHEMAS"));
        };
        if !schemas.is_empty() && kind == DefaultPrivilegeObjectKind::Schemas {
            return Err(ParseError {
                at: self.peek_at,
                message: crate::util::StackStr::from_str(
                    "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
                ),
                sqlstate: sqlstate::INVALID_GRANT_OPERATION,
            });
        }
        if grant {
            self.expect_ident("to")?;
            let grantees = self.role_name_list("grantee")?;
            let grant_option = if self.eat_ident("with")? {
                self.expect_ident("grant")?;
                self.expect_ident("option")?;
                true
            } else {
                false
            };
            Ok(Stmt::AlterDefaultPrivileges {
                roles,
                schemas,
                action: DefaultPrivilegeAction::Grant {
                    privileges,
                    kind,
                    grantees,
                    grant_option,
                },
            })
        } else {
            self.expect_ident("from")?;
            let grantees = self.role_name_list("grantee")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            Ok(Stmt::AlterDefaultPrivileges {
                roles,
                schemas,
                action: DefaultPrivilegeAction::Revoke {
                    grant_option_only,
                    privileges,
                    kind,
                    grantees,
                    cascade,
                },
            })
        }
    }

    pub(super) fn alter_owner(
        &mut self,
        kind: crate::sql::ast::AlterOwnerKind,
        name: QualName<'a>,
        if_exists: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("owner")?;
        self.expect_ident("to")?;
        let role = self.any_ident("role name")?;
        Ok(Stmt::AlterOwner {
            kind,
            name,
            role,
            if_exists,
        })
    }

    /// One ADD / DROP / ALTER subcommand of an ALTER TABLE (the comma-listable
    /// forms; RENAME and SET SCHEMA are handled by the caller).
    fn alter_table_cmd(&mut self) -> Result<AlterAction<'a>, ParseError> {
        if self.eat_ident("attach")? {
            self.expect_ident("partition")?;
            let child = self.qual_name("partition name")?;
            let bound = self.partition_bound()?;
            Ok(AlterAction::AttachPartition { child, bound })
        } else if self.eat_ident("detach")? {
            self.expect_ident("partition")?;
            let child = self.qual_name("partition name")?;
            if self.eat_ident("concurrently")? || self.eat_ident("finalize")? {
                return Err(self.err_here("concurrent partition detach is not supported"));
            }
            Ok(AlterAction::DetachPartition { child })
        } else if self.eat_ident("add")? {
            // ADD [CONSTRAINT name] <table constraint> vs ADD [COLUMN] <def>.
            if self.eat_ident("constraint")? {
                let cname = self.col_ident("constraint name")?;
                return Ok(AlterAction::AddConstraint(
                    self.table_constraint(Some(cname), true)?,
                ));
            }
            if matches!(
                self.peeked,
                Tok::Ident("primary")
                    | Tok::Ident("unique")
                    | Tok::Ident("check")
                    | Tok::Ident("foreign")
            ) {
                return Ok(AlterAction::AddConstraint(
                    self.table_constraint(None, true)?,
                ));
            }
            let _ = self.eat_ident("column")?;
            let name = self.col_ident("column name")?;
            let (type_name, type_mod) = self.type_name_mod()?;
            let collation = if self.eat_ident("collate")? {
                self.collation_name()?
            } else {
                crate::sql::ast::Collation::Default
            };
            let mut not_null = false;
            let mut unique = false;
            let mut default = None;
            let mut default_text = None;
            let mut generated_text = None;
            let mut identity = None;
            loop {
                if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    not_null = true;
                } else if self.eat_ident("null")? {
                    not_null = false;
                } else if self.eat_ident("default")? {
                    let start = self.peek_at;
                    default = Some(self.column_default_expression()?);
                    default_text = Some(self.text[start..self.peek_at].trim_end());
                } else if self.eat_ident("generated")? {
                    match self.generated_clause()? {
                        crate::sql::ast::ColGen::Generated(text) => generated_text = Some(text),
                        crate::sql::ast::ColGen::Identity(spec) => identity = Some(spec),
                    }
                } else if self.eat_ident("unique")? {
                    unique = true;
                } else {
                    break;
                }
            }
            Ok(AlterAction::AddColumn(ColumnDef {
                name,
                type_name,
                type_mod,
                collation,
                not_null,
                unique,
                primary: false,
                default,
                default_text,
                generated_text,
                identity,
            }))
        } else if self.eat_ident("drop")? {
            if self.eat_ident("constraint")? {
                let if_exists = self.eat_ident("if")? && {
                    self.expect_ident("exists")?;
                    true
                };
                let name = self.col_ident("constraint name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                Ok(AlterAction::DropConstraint {
                    name,
                    if_exists,
                    cascade,
                })
            } else {
                let _ = self.eat_ident("column")?;
                let if_exists = self.eat_ident("if")? && {
                    self.expect_ident("exists")?;
                    true
                };
                let name = self.col_ident("column name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                Ok(AlterAction::DropColumn {
                    name,
                    if_exists,
                    cascade,
                })
            }
        } else if self.eat_ident("validate")? {
            self.expect_ident("constraint")?;
            Ok(AlterAction::ValidateConstraint(
                self.col_ident("constraint name")?,
            ))
        } else if self.eat_ident("alter")? {
            if self.eat_ident("constraint")? {
                let name = self.col_ident("constraint name")?;
                let mut alteration = ConstraintAlteration {
                    deferrable: None,
                    initially: None,
                    enforced: None,
                };
                loop {
                    if self.eat_ident("deferrable")? {
                        if alteration.deferrable.replace(true).is_some() {
                            return Err(self.err_here("duplicate DEFERRABLE clause"));
                        }
                    } else if self.eat_ident("not")? {
                        if self.eat_ident("deferrable")? {
                            if alteration.deferrable.replace(false).is_some() {
                                return Err(self.err_here("duplicate NOT DEFERRABLE clause"));
                            }
                        } else {
                            self.expect_ident("enforced")?;
                            if alteration.enforced.replace(false).is_some() {
                                return Err(self.err_here("duplicate NOT ENFORCED clause"));
                            }
                        }
                    } else if self.eat_ident("initially")? {
                        let mode = if self.eat_ident("deferred")? {
                            ConstraintMode::Deferred
                        } else {
                            self.expect_ident("immediate")?;
                            ConstraintMode::Immediate
                        };
                        if alteration.initially.replace(mode).is_some() {
                            return Err(self.err_here("duplicate INITIALLY clause"));
                        }
                    } else if self.eat_ident("enforced")? {
                        if alteration.enforced.replace(true).is_some() {
                            return Err(self.err_here("duplicate ENFORCED clause"));
                        }
                    } else {
                        break;
                    }
                }
                if alteration.deferrable.is_none()
                    && alteration.initially.is_none()
                    && alteration.enforced.is_none()
                {
                    return Err(self.err_here("expected a constraint attribute"));
                }
                if alteration.deferrable == Some(false)
                    && alteration.initially == Some(ConstraintMode::Deferred)
                {
                    return Err(
                        self.err_here("constraint declared INITIALLY DEFERRED must be DEFERRABLE")
                    );
                }
                return Ok(AlterAction::AlterConstraint { name, alteration });
            }
            // ALTER [COLUMN] col { SET DEFAULT e | DROP DEFAULT | SET NOT NULL
            // | DROP NOT NULL }.
            let _ = self.eat_ident("column")?;
            let column = self.col_ident("column name")?;
            // `TYPE t` and `SET DATA TYPE t` are the same column-type change;
            // `SET DEFAULT`/`SET NOT NULL` and `DROP DEFAULT`/`DROP NOT NULL`
            // are the other four ALTER COLUMN forms.
            if self.eat_ident("type")? {
                self.alter_column_type(column)
            } else if self.eat_ident("set")? {
                if self.eat_ident("data")? {
                    self.expect_ident("type")?;
                    self.alter_column_type(column)
                } else if self.eat_ident("default")? {
                    let start = self.peek_at;
                    let value = self.expression(0)?;
                    let value_text = self.text[start..self.peek_at].trim_end();
                    Ok(AlterAction::SetDefault {
                        column,
                        value,
                        value_text,
                    })
                } else {
                    self.expect_ident("not")?;
                    self.expect_ident("null")?;
                    Ok(AlterAction::SetNotNull { column })
                }
            } else if self.eat_ident("drop")? {
                if self.eat_ident("default")? {
                    Ok(AlterAction::DropDefault { column })
                } else if self.eat_ident("identity")? {
                    let if_exists = if self.eat_ident("if")? {
                        self.expect_ident("exists")?;
                        true
                    } else {
                        false
                    };
                    Ok(AlterAction::DropIdentity { column, if_exists })
                } else {
                    self.expect_ident("not")?;
                    self.expect_ident("null")?;
                    Ok(AlterAction::DropNotNull { column })
                }
            } else if self.eat_ident("add")? {
                // ALTER COLUMN col ADD GENERATED ... AS IDENTITY.
                self.expect_ident("generated")?;
                match self.generated_clause()? {
                    crate::sql::ast::ColGen::Identity(spec) => {
                        Ok(AlterAction::AddIdentity { column, spec })
                    }
                    crate::sql::ast::ColGen::Generated(_) => {
                        Err(self.err_here("a column cannot be turned into a generated column"))
                    }
                }
            } else {
                Err(self.unexpected("expected TYPE, SET, DROP or ADD"))
            }
        } else {
            Err(self.unexpected("expected ADD, DROP or ALTER"))
        }
    }

    fn prepare(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("prepare")?;
        let name = self.any_ident("prepared statement name")?;
        // Declared parameter types, if any; they constrain EXECUTE arguments.
        let mut ptypes: [&'a str; MAX_LIST] = [""; MAX_LIST];
        let mut np = 0;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                if np == MAX_LIST {
                    return Err(self.limit("PREPARE parameter types", MAX_LIST));
                }
                ptypes[np] = self.type_name()?;
                np += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        self.expect_ident("as")?;
        let start = self.peek_at;
        // Validate the body by parsing it; the raw text is what is stored.
        let _ = self.statement()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::Prepare {
            name,
            sql,
            param_types: self.arena_slice(&ptypes[..np])?,
        })
    }

    fn execute_prepared(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("execute")?;
        let name = self.any_ident("prepared statement name")?;
        let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
        let mut args: [&'a Expr<'a>; MAX_LIST] = [null_expr; MAX_LIST];
        let mut n = 0;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            if self.peeked != Tok::Op(")") {
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("EXECUTE arguments", MAX_LIST));
                    }
                    args[n] = self.expression(0)?;
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
        }
        Ok(Stmt::ExecutePrepared {
            name,
            args: self.arena_slice(&args[..n])?,
        })
    }

    fn insert(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("insert")?;
        self.expect_ident("into")?;
        let table = self.qual_name("table name")?;
        let mut column_names: [&'a str; MAX_LIST] = [""; MAX_LIST];
        let mut n_cols = 0;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                if n_cols == MAX_LIST {
                    return Err(self.limit("column list", MAX_LIST));
                }
                column_names[n_cols] = self.col_ident("column name")?;
                n_cols += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        // OVERRIDING { SYSTEM | USER } VALUE for identity columns.
        let overriding = if self.eat_ident("overriding")? {
            let mode = if self.eat_ident("system")? {
                crate::sql::ast::Overriding::System
            } else {
                self.expect_ident("user")?;
                crate::sql::ast::Overriding::User
            };
            self.expect_ident("value")?;
            mode
        } else {
            crate::sql::ast::Overriding::None
        };
        // Source is either VALUES (...), ... or a SELECT.
        let mut rows: [&'a [&'a Expr<'a>]; MAX_ROWS] = [&[]; MAX_ROWS];
        let mut n_rows = 0;
        let mut select = None;
        if self.peeked == Tok::Ident("select") {
            let sel = self.select()?;
            select = Some(
                self.arena
                    .alloc(sel)
                    .map_err(|_| self.err_here("statement too large for SQL arena"))?
                    as &_,
            );
        } else if self.eat_ident("default")? {
            // `DEFAULT VALUES` inserts one row of nothing but defaults, which
            // is exactly a row of `DEFAULT` markers over no named columns.
            self.expect_ident("values")?;
            rows[0] = &[];
            n_rows = 1;
        } else {
            self.expect_ident("values")?;
            loop {
                if n_rows == MAX_ROWS {
                    return Err(self.limit("VALUES rows", MAX_ROWS));
                }
                self.expect_op("(")?;
                let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
                let mut row: [&'a Expr<'a>; MAX_LIST] = [null_expr; MAX_LIST];
                let mut n = 0;
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("VALUES row", MAX_LIST));
                    }
                    row[n] = self.expression(0)?;
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                rows[n_rows] = self.arena_slice(&row[..n])?;
                n_rows += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let on_conflict = self.on_conflict()?;
        let returning = self.returning()?;
        Ok(Stmt::Insert(Insert {
            table,
            columns: self.arena_slice(&column_names[..n_cols])?,
            rows: self.arena_slice(&rows[..n_rows])?,
            select,
            on_conflict,
            returning,
            overriding,
        }))
    }

    /// `ON CONFLICT [(columns) | ON CONSTRAINT name] DO {NOTHING | UPDATE SET a
    /// = e, ... [WHERE cond]}`.
    fn on_conflict(&mut self) -> Result<Option<OnConflict<'a>>, ParseError> {
        if !self.eat_ident("on")? {
            return Ok(None);
        }
        self.expect_ident("conflict")?;
        let null_expression = self.arena_expr(Expr::Null)?;
        let mut target: [crate::sql::ast::OnConflictTarget<'a>; MAX_LIST] =
            [crate::sql::ast::OnConflictTarget {
                column: None,
                expression: null_expression,
                expression_text: "",
            }; MAX_LIST];
        let mut nt = 0;
        let mut constraint = None;
        if self.eat_op("(")? {
            loop {
                if nt == MAX_LIST {
                    return Err(self.limit("conflict target", MAX_LIST));
                }
                let start = self.peek_at;
                let expression = self.expression(0)?;
                let expression_text = self.text[start..self.peek_at].trim_end();
                target[nt] = crate::sql::ast::OnConflictTarget {
                    column: match expression {
                        Expr::Column {
                            qualifier: None,
                            name,
                        } => Some(*name),
                        _ => None,
                    },
                    expression,
                    expression_text,
                };
                nt += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        } else if self.eat_ident("on")? {
            // `ON CONFLICT ON CONSTRAINT name`.
            self.expect_ident("constraint")?;
            constraint = Some(self.col_ident("constraint name")?);
        }
        self.expect_ident("do")?;
        let (update, update_where) = if self.eat_ident("nothing")? {
            (None, None)
        } else {
            self.expect_ident("update")?;
            self.expect_ident("set")?;
            let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
            let mut assigns: [(&'a str, &'a Expr<'a>); MAX_LIST] = [("", null_expr); MAX_LIST];
            let mut na = 0;
            loop {
                if na == MAX_LIST {
                    return Err(self.limit("assignments", MAX_LIST));
                }
                let col = self.col_ident("column name")?;
                self.expect_op("=")?;
                let value = self.expression(0)?;
                assigns[na] = (col, value);
                na += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            let where_clause = if self.eat_ident("where")? {
                Some(self.expression(0)?)
            } else {
                None
            };
            (Some(self.arena_slice(&assigns[..na])?), where_clause)
        };
        Ok(Some(OnConflict {
            target: self.arena_slice(&target[..nt])?,
            constraint,
            update,
            update_where,
        }))
    }

    fn update(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("update")?;
        let table = self.qual_name("table name")?;
        let alias =
            if self.eat_ident("as")? || matches!(self.peeked, Tok::Ident(word) if word != "set") {
                Some(self.col_ident("table alias")?)
            } else {
                None
            };
        self.expect_ident("set")?;
        let dummy: (&'a str, &'a Expr<'a>) = ("", &Expr::Null);
        let mut assignments = [dummy; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("SET list", MAX_LIST));
            }
            let col = self.col_ident("column name")?;
            self.expect_op("=")?;
            let value = self.expression(0)?;
            assignments[n] = (col, value);
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let from = if self.eat_ident("from")? {
            let fc = self.from_clause()?;
            Some(
                &*self
                    .arena
                    .alloc(fc)
                    .map_err(|_| self.err_here("FROM too large for SQL arena"))?,
            )
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Update(Update {
            table,
            alias,
            assignments: self.arena_slice(&assignments[..n])?,
            from,
            where_clause,
            returning,
        }))
    }

    /// `MERGE INTO target [AS alias] USING source [AS alias] ON cond
    /// { WHEN [NOT] MATCHED [AND cond] THEN action }...`.
    fn merge(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{Merge, MergeWhen};
        self.expect_ident("merge")?;
        self.expect_ident("into")?;
        let target = self.qual_name("target table")?;
        let target_alias =
            if self.eat_ident("as")? || matches!(self.peeked, Tok::Ident(w) if w != "using") {
                Some(self.col_ident("target alias")?)
            } else {
                None
            };
        self.expect_ident("using")?;
        let source = self.table_ref()?;
        self.expect_ident("on")?;
        let on = self.expression(0)?;
        let dummy = MergeWhen {
            matched: true,
            cond: None,
            action: crate::sql::ast::MergeAction::Delete,
        };
        let mut whens = [dummy; MAX_LIST];
        let mut n = 0;
        while self.eat_ident("when")? {
            if n == MAX_LIST {
                return Err(self.limit("WHEN clauses", MAX_LIST));
            }
            let matched = if self.eat_ident("not")? {
                self.expect_ident("matched")?;
                false
            } else {
                self.expect_ident("matched")?;
                true
            };
            let cond = if self.eat_ident("and")? {
                Some(self.expression(0)?)
            } else {
                None
            };
            self.expect_ident("then")?;
            let action = self.merge_action(matched)?;
            whens[n] = MergeWhen {
                matched,
                cond,
                action,
            };
            n += 1;
        }
        if n == 0 {
            return Err(self.err_here("MERGE requires at least one WHEN clause"));
        }
        Ok(Stmt::Merge(Merge {
            target,
            target_alias,
            source,
            on,
            whens: self.arena_slice(&whens[..n])?,
        }))
    }

    /// One MERGE action after `THEN`. `matched` selects the allowed set.
    fn merge_action(
        &mut self,
        matched: bool,
    ) -> Result<crate::sql::ast::MergeAction<'a>, ParseError> {
        use crate::sql::ast::MergeAction;
        if self.eat_ident("do")? {
            self.expect_ident("nothing")?;
            return Ok(MergeAction::DoNothing);
        }
        if matched {
            if self.eat_ident("update")? {
                self.expect_ident("set")?;
                let dummy: (&'a str, &'a Expr<'a>) = ("", &Expr::Null);
                let mut assignments = [dummy; MAX_LIST];
                let mut n = 0;
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("SET list", MAX_LIST));
                    }
                    let col = self.col_ident("column name")?;
                    self.expect_op("=")?;
                    assignments[n] = (col, self.expression(0)?);
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                Ok(MergeAction::Update(self.arena_slice(&assignments[..n])?))
            } else {
                self.expect_ident("delete")?;
                Ok(MergeAction::Delete)
            }
        } else {
            self.expect_ident("insert")?;
            // INSERT [(cols)] { VALUES (exprs) | DEFAULT VALUES }.
            let mut columns: &'a [&'a str] = &[];
            if self.peeked == Tok::Op("(") {
                self.advance()?;
                let mut names = [""; MAX_LIST];
                let mut c = 0;
                loop {
                    if c == MAX_LIST {
                        return Err(self.limit("column list", MAX_LIST));
                    }
                    names[c] = self.col_ident("column name")?;
                    c += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                columns = self.arena_slice(&names[..c])?;
            }
            if self.eat_ident("default")? {
                self.expect_ident("values")?;
                return Ok(MergeAction::Insert {
                    columns,
                    values: &[],
                    default_values: true,
                });
            }
            self.expect_ident("values")?;
            self.expect_op("(")?;
            let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
            let mut vals = [null_expr; MAX_LIST];
            let mut v = 0;
            loop {
                if v == MAX_LIST {
                    return Err(self.limit("VALUES list", MAX_LIST));
                }
                vals[v] = self.expression(0)?;
                v += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            Ok(MergeAction::Insert {
                columns,
                values: self.arena_slice(&vals[..v])?,
                default_values: false,
            })
        }
    }

    /// `COMMENT ON <object> IS { 'text' | NULL }`.
    fn comment(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{CommentRelKind, CommentTarget};
        self.expect_ident("comment")?;
        self.expect_ident("on")?;
        let target = if self.eat_ident("table")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Table,
                name: self.qual_name("table name")?,
            }
        } else if self.eat_ident("view")? {
            CommentTarget::Relation {
                kind: CommentRelKind::View,
                name: self.qual_name("view name")?,
            }
        } else if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            CommentTarget::Relation {
                kind: CommentRelKind::MaterializedView,
                name: self.qual_name("materialized view name")?,
            }
        } else if self.eat_ident("index")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Index,
                name: self.qual_name("index name")?,
            }
        } else if self.eat_ident("sequence")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Sequence,
                name: self.qual_name("sequence name")?,
            }
        } else if self.eat_ident("schema")? {
            CommentTarget::Schema(self.col_ident("schema name")?)
        } else if self.eat_ident("tablespace")? {
            CommentTarget::Tablespace(self.col_ident("tablespace name")?)
        } else if self.eat_ident("extension")? {
            CommentTarget::Extension(self.col_ident("extension name")?)
        } else if self.eat_ident("trigger")? {
            let name = self.col_ident("trigger name")?;
            self.expect_ident("on")?;
            CommentTarget::Trigger(crate::sql::ast::TriggerIdentity {
                name,
                table: self.qual_name("trigger table")?,
            })
        } else if self.eat_ident("type")? {
            CommentTarget::Type {
                name: self.comment_type_name()?,
                domain_only: false,
            }
        } else if self.eat_ident("domain")? {
            CommentTarget::Type {
                name: self.comment_type_name()?,
                domain_only: true,
            }
        } else if self.eat_ident("column")? {
            // `[schema.]table.column`: the last dotted part is the column.
            let first = self.col_ident("column reference")?;
            self.expect_op(".")?;
            let second = self.col_ident("column reference")?;
            if self.eat_op(".")? {
                let third = self.col_ident("column reference")?;
                CommentTarget::Column {
                    relation: QualName {
                        schema: Some(first),
                        name: second,
                    },
                    column: third,
                }
            } else {
                CommentTarget::Column {
                    relation: QualName::bare(first),
                    column: second,
                }
            }
        } else {
            return Err(self.err_here("unsupported COMMENT ON object type"));
        };
        self.expect_ident("is")?;
        let text = match self.expression(0)? {
            Expr::Str(s) => Some(*s),
            Expr::Null => None,
            _ => return Err(self.err_here("COMMENT value must be a string literal or NULL")),
        };
        Ok(Stmt::Comment { target, text })
    }

    /// A type name in `COMMENT ON TYPE/DOMAIN`: keep a user schema qualifier,
    /// while canonicalizing PostgreSQL's multi-word built-in spellings.
    fn comment_type_name(&mut self) -> Result<&'a str, ParseError> {
        let first = self.any_ident("type name")?;
        if self.eat_op(".")? {
            let second = self.any_ident("type name")?;
            if self.eat_op("[")? {
                self.expect_op("]")?;
                return self.arena_str(stack_format!(144, "{}.{}[]", first, second).as_str());
            }
            return self.arena_str(stack_format!(144, "{}.{}", first, second).as_str());
        }
        let mut name = first;
        if name == "double" {
            self.expect_ident("precision")?;
            name = "float8";
        } else if name == "bit" && self.eat_ident("varying")? {
            name = "varbit";
        } else if (name == "character" || name == "char") && self.eat_ident("varying")? {
            name = "varchar";
        } else if name == "timestamp" || name == "time" {
            if self.eat_ident("with")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                name = if name == "timestamp" {
                    "timestamptz"
                } else {
                    "timetz"
                };
            } else if self.eat_ident("without")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
            }
        }
        if self.eat_op("[")? {
            self.expect_op("]")?;
            return self.arena_str(stack_format!(144, "{}[]", name).as_str());
        }
        Ok(name)
    }

    fn delete(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("delete")?;
        self.expect_ident("from")?;
        let table = self.qual_name("table name")?;
        let alias = if self.eat_ident("as")?
            || matches!(self.peeked, Tok::Ident(word) if word != "using" && word != "where" && word != "returning")
        {
            Some(self.col_ident("table alias")?)
        } else {
            None
        };
        let using = if self.eat_ident("using")? {
            let fc = self.from_clause()?;
            Some(
                &*self
                    .arena
                    .alloc(fc)
                    .map_err(|_| self.err_here("USING too large for SQL arena"))?,
            )
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Delete(Delete {
            table,
            alias,
            using,
            where_clause,
            returning,
        }))
    }

    fn where_clause(&mut self) -> Result<Option<&'a Expr<'a>>, ParseError> {
        if self.eat_ident("where")? {
            Ok(Some(self.expression(0)?))
        } else {
            Ok(None)
        }
    }

    /// Multi-word type names are normalized: `double precision` → float8.
    /// Base type name plus its PostgreSQL atttypmod (-1 when there is no
    /// `(...)`). varchar/char carry a length; numeric/decimal carry
    /// (precision[, scale]); any other type with a modifier is a loud error.
    fn type_name_mod(&mut self) -> Result<(&'a str, i32), ParseError> {
        let mut name = self.any_ident("type name")?;
        // System-qualified built-ins normalize to their bare spelling. A user
        // schema is part of the type identity and must survive parsing; dropping
        // it aliases same-named domains/enums in different schemas.
        if self.peeked == Tok::Op(".") {
            let schema = name;
            self.advance()?;
            let base = self.any_ident("type name")?;
            name = if schema == "pg_catalog" || schema == "information_schema" {
                base
            } else {
                self.arena
                    .alloc_str(stack_format!(128, "{}.{}", schema, base).as_str())
                    .map_err(|_| self.err_here("type name too long"))?
            };
        }
        if name == "double" {
            self.expect_ident("precision")?;
            name = "float8";
        }
        // `bit varying [(n)]` is the `varbit` type.
        if name == "bit" && self.eat_ident("varying")? {
            name = "varbit";
        }
        // `character varying` / `char varying` is `varchar`.
        if (name == "character" || name == "char") && self.eat_ident("varying")? {
            name = "varchar";
        }
        if name == "timestamp" || name == "time" {
            if self.eat_ident("with")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                name = if name == "timestamp" {
                    "timestamptz"
                } else {
                    "timetz"
                };
            } else if self.eat_ident("without")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
            }
        }
        let type_mod = if self.peeked == Tok::Op("(") {
            self.type_modifier(name)?
        } else if name == "char" || name == "character" {
            // Bare `char`/`character` is char(1) in PostgreSQL (`'ab'::char`
            // is 'a'); only the internal name `bpchar` means unlimited.
            TypeMod::Length(1).encode()
        } else {
            -1
        };
        // Repeated `[]` spell the same scalar-element array type.
        if self.peeked == Tok::Op("[") {
            self.advance()?;
            self.expect_op("]")?;
            while self.peeked == Tok::Op("[") {
                self.advance()?;
                self.expect_op("]")?;
            }
            let array = self
                .arena
                .alloc_str(stack_format!(132, "{}[]", name).as_str())
                .map_err(|_| self.err_here("type name too long"))?;
            return Ok((array, type_mod));
        }
        Ok((name, type_mod))
    }

    /// A type name for a prepared-statement parameter: PostgreSQL parses and
    /// then ignores any modifier here (`PREPARE q(varchar(2))` does not
    /// truncate its argument — verified against 18.4), so the modifier is
    /// accepted and dropped.
    fn type_name(&mut self) -> Result<&'a str, ParseError> {
        let (name, _type_mod) = self.type_name_mod()?;
        Ok(name)
    }

    /// Parses `(n)` or `(p[,s])` and encodes PostgreSQL's atttypmod. Only
    /// varchar/char (length) and numeric/decimal (precision, scale) take one.
    fn type_modifier(&mut self, base: &str) -> Result<i32, ParseError> {
        self.expect_op("(")?;
        let mut nums = [0i64; 2];
        let mut n = 0;
        loop {
            match self.peeked {
                Tok::Num(t) => {
                    if n == 2 {
                        return Err(self.unexpected("too many type-modifier arguments"));
                    }
                    let Ok(v) = t.parse::<i64>() else {
                        return Err(self.unexpected("type modifier must be an integer"));
                    };
                    nums[n] = v;
                    n += 1;
                    self.advance()?;
                }
                Tok::Op(",") => self.advance()?,
                Tok::Op(")") => {
                    self.advance()?;
                    break;
                }
                _ => return Err(self.unexpected("expected a type modifier")),
            }
        }
        match base {
            "varchar" | "char" | "character" | "bpchar" => {
                if n != 1 {
                    return Err(self.unexpected("length for character type takes one argument"));
                }
                if !(1..=10_485_760).contains(&nums[0]) {
                    return Err(self.unexpected("length for character type must be 1..10485760"));
                }
                Ok(TypeMod::Length(nums[0] as usize).encode())
            }
            "numeric" | "decimal" | "dec" => {
                if n < 1 {
                    return Err(self.unexpected("numeric type modifier requires a precision"));
                }
                let p = nums[0];
                let s = if n == 2 { nums[1] } else { 0 };
                if !(1..=1000).contains(&p) {
                    return Err(self.unexpected("numeric precision must be between 1 and 1000"));
                }
                if !(0..=p).contains(&s) {
                    return Err(self.unexpected("numeric scale must be between 0 and precision"));
                }
                Ok(TypeMod::NumericPS {
                    precision: p as u16,
                    scale: s as u16,
                }
                .encode())
            }
            "bit" | "varbit" => {
                if n != 1 {
                    return Err(self.unexpected("length for bit type takes one argument"));
                }
                if nums[0] < 1 {
                    return Err(self.unexpected("length for bit type must be at least 1"));
                }
                Ok(TypeMod::Length(nums[0] as usize).encode())
            }
            // Fractional-second precision, 0..=6. A larger value is clamped to
            // 6, and PostgreSQL warns when it does so.
            "timestamp" | "timestamptz" | "time" | "timetz" | "interval" => {
                if n != 1 {
                    return Err(self.unexpected("precision for this type takes one argument"));
                }
                if nums[0] < 0 {
                    return Err(self.unexpected("precision must be between 0 and 6"));
                }
                if nums[0] > 6 {
                    // PostgreSQL names the SQL type, not the alias written:
                    // `timestamptz(7)` is reported as TIMESTAMP(7) WITH TIME ZONE.
                    let (sql_name, zoned) = match base {
                        "timestamp" => ("TIMESTAMP", false),
                        "timestamptz" => ("TIMESTAMP", true),
                        "time" => ("TIME", false),
                        "timetz" => ("TIME", true),
                        _ => ("INTERVAL", false),
                    };
                    self.warn(stack_format!(
                        96,
                        "{}({}){} precision reduced to maximum allowed, 6",
                        sql_name,
                        nums[0],
                        if zoned { " WITH TIME ZONE" } else { "" }
                    ));
                }
                let precision = nums[0].min(6) as u8;
                // A plain `interval(p)` carries the full field range beside its
                // precision; the other temporal types carry the precision bare.
                if base == "interval" {
                    Ok(TypeMod::IntervalMod {
                        range: INTERVAL_FULL_RANGE,
                        precision: Some(precision),
                    }
                    .encode())
                } else {
                    Ok(TypeMod::TemporalPrecision(precision).encode())
                }
            }
            _ => Err(self.unexpected("type modifier is not supported for this type yet")),
        }
    }

    fn alias(&mut self) -> Result<Option<&'a str>, ParseError> {
        if self.eat_ident("as")? {
            return Ok(Some(self.any_ident("alias")?));
        }
        // Bare alias: an identifier that is not a clause keyword.
        if let Tok::Ident(name) = self.peeked {
            let reserved = matches!(
                name,
                "from"
                    | "where"
                    | "order"
                    | "limit"
                    | "group"
                    | "having"
                    | "union"
                    | "intersect"
                    | "except"
                    | "window"
                    | "for"
                    | "fetch"
                    | "into"
                    | "and"
                    | "or"
                    | "is"
                    | "as"
                    | "asc"
                    | "desc"
                    | "offset"
            );
            if !reserved {
                self.advance()?;
                return Ok(Some(name));
            }
        }
        if let Tok::QuotedIdent(name) = self.peeked {
            self.advance()?;
            return Ok(Some(name));
        }
        Ok(None)
    }

    fn transaction_modifiers(&mut self, allow_work: bool) -> Result<&'a str, ParseError> {
        if allow_work && !self.eat_ident("work")? {
            let _ = self.eat_ident("transaction")?;
        }
        let start = self.peek_at;
        while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            self.advance()?;
        }
        Ok(self.text[start..self.peek_at].trim())
    }

    // --- token helpers ---

    fn advance(&mut self) -> Result<(), ParseError> {
        self.peeked = self.lexer.next_token()?;
        self.peek_at = self.lexer.token_start();
        Ok(())
    }

    fn eat_op(&mut self, operator: &str) -> Result<bool, ParseError> {
        if self.peeked == Tok::Op(operator) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect_op(&mut self, operator: &str) -> Result<(), ParseError> {
        if !self.eat_op(operator)? {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "expected '{}'", operator),
                sqlstate: sqlstate::SYNTAX_ERROR,
            });
        }
        Ok(())
    }

    fn eat_ident(&mut self, word: &str) -> Result<bool, ParseError> {
        if self.peeked == Tok::Ident(word) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect_ident(&mut self, word: &str) -> Result<(), ParseError> {
        if !self.eat_ident(word)? {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "expected '{}'", word),
                sqlstate: sqlstate::SYNTAX_ERROR,
            });
        }
        Ok(())
    }

    /// [`Self::any_ident`] for a `ColId` position — a column name, a table name,
    /// or a bare alias. An unquoted keyword PostgreSQL rejects there is a syntax
    /// error; quoting it (`"select"`) always makes it a plain identifier.
    fn col_ident(&mut self, what: &str) -> Result<&'a str, ParseError> {
        if let Tok::Ident(word) = self.peeked
            && is_column_name_keyword(word)
        {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "syntax error at or near \"{}\"", word),
                sqlstate: sqlstate::SYNTAX_ERROR,
            });
        }
        self.any_ident(what)
    }

    /// Records a warning for the engine to emit before this statement runs.
    /// Overflowing the fixed buffer drops the extra warnings rather than
    /// failing the statement — PostgreSQL still executes it too.
    fn warn(&mut self, message: StackStr<96>) {
        if self.n_warnings < MAX_PARSE_WARNINGS {
            self.warnings[self.n_warnings] = message;
            self.n_warnings += 1;
        }
    }

    /// Takes the warnings raised since the last call, in the order parsed.
    pub fn take_warnings(&mut self) -> ([StackStr<96>; MAX_PARSE_WARNINGS], usize) {
        let taken = self.n_warnings;
        self.n_warnings = 0;
        (self.warnings, taken)
    }

    /// Unquoted or quoted identifier.
    fn any_ident(&mut self, what: &str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Ident(name) | Tok::QuotedIdent(name) => {
                self.advance()?;
                Ok(name)
            }
            _ => Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "expected {}", what),
                sqlstate: sqlstate::SYNTAX_ERROR,
            }),
        }
    }

    /// `schema.table` composed into one arena string — the qualifier form a
    /// three-part star (`schema.table.*`) resolves through.
    pub(super) fn composed_qualifier(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<&'a str, ParseError> {
        let text = crate::stack_format!(130, "{}.{}", schema, table);
        self.arena.alloc_str(text.as_str()).map_err(|_| ParseError {
            at: self.peek_at,
            message: crate::stack_format!(96, "statement too large for SQL arena"),
            sqlstate: crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        })
    }

    fn arena_expr(&self, e: Expr<'a>) -> Result<&'a Expr<'a>, ParseError> {
        self.arena
            .alloc(e)
            .map(|m| &*m)
            .map_err(|_| self.err_here("statement too large for SQL arena"))
    }

    /// Parses the body of a `GROUP BY` clause (the keywords already consumed)
    /// into a flat, deduplicated list of grouping expressions and a set of
    /// grouping-set bitmasks over that list. A plain `GROUP BY a, b` returns an
    /// empty mask set (meaning a single implicit all-columns set);
    /// `ROLLUP`/`CUBE`/`GROUPING SETS` return explicit masks, cross-multiplied
    /// across comma-separated top-level elements exactly as PostgreSQL does.
    fn group_by_clause(&mut self) -> Result<(&'a [&'a Expr<'a>], &'a [u64]), ParseError> {
        let null_expr = self.arena_expr(Expr::Null)?;
        let mut flat: [&'a Expr<'a>; MAX_LIST] = [null_expr; MAX_LIST];
        let mut n_flat = 0usize;
        // Running cross-product of grouping-set masks; starts as one empty set.
        let mut acc = [0u64; MAX_GROUPING_SETS];
        let mut n_acc = 1usize;
        let mut scratch = [0u64; MAX_GROUPING_SETS];
        let mut explicit = false;
        loop {
            let mut elem = [0u64; MAX_GROUPING_SETS];
            let mut n_elem = 0usize;
            if self.peeked == Tok::Ident("rollup") || self.peeked == Tok::Ident("cube") {
                let is_cube = self.peeked == Tok::Ident("cube");
                self.advance()?;
                self.expect_op("(")?;
                let mut terms = [0u64; MAX_LIST];
                let n_terms = self.grouping_term_list(&mut flat, &mut n_flat, &mut terms)?;
                self.expect_op(")")?;
                if is_cube {
                    if n_terms > 20 {
                        return Err(self.err_here("CUBE with too many columns"));
                    }
                    for subset in 0u32..(1u32 << n_terms) {
                        let mut m = 0u64;
                        for (t, &tm) in terms[..n_terms].iter().enumerate() {
                            if subset & (1 << t) != 0 {
                                m |= tm;
                            }
                        }
                        push_mask(&mut elem, &mut n_elem, m, || {
                            self.err_here("too many grouping sets")
                        })?;
                    }
                } else {
                    for keep in (0..=n_terms).rev() {
                        let mut m = 0u64;
                        for &tm in &terms[..keep] {
                            m |= tm;
                        }
                        push_mask(&mut elem, &mut n_elem, m, || {
                            self.err_here("too many grouping sets")
                        })?;
                    }
                }
                explicit = true;
            } else if self.peeked == Tok::Ident("grouping") {
                self.advance()?;
                self.expect_ident("sets")?;
                self.expect_op("(")?;
                loop {
                    self.grouping_set_member(&mut flat, &mut n_flat, &mut elem, &mut n_elem)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                explicit = true;
            } else {
                let m = self.grouping_term(&mut flat, &mut n_flat)?;
                push_mask(&mut elem, &mut n_elem, m, || {
                    self.err_here("too many grouping sets")
                })?;
            }
            // Cross product: acc × elem.
            let mut n_new = 0usize;
            for &a in &acc[..n_acc] {
                for &e in &elem[..n_elem] {
                    push_mask(&mut scratch, &mut n_new, a | e, || {
                        self.err_here("too many grouping sets")
                    })?;
                }
            }
            acc[..n_new].copy_from_slice(&scratch[..n_new]);
            n_acc = n_new;
            if !self.eat_op(",")? {
                break;
            }
        }
        let group_by = self.arena_slice(&flat[..n_flat])?;
        let grouping_sets = if explicit {
            self.arena_slice(&acc[..n_acc])?
        } else {
            &[][..]
        };
        Ok((group_by, grouping_sets))
    }

    /// Interns a grouping expression into `flat` (deduplicated by structural
    /// equality) and returns its single-bit mask.
    fn intern_group(
        &mut self,
        flat: &mut [&'a Expr<'a>; MAX_LIST],
        n_flat: &mut usize,
        e: &'a Expr<'a>,
    ) -> Result<u64, ParseError> {
        for (i, existing) in flat[..*n_flat].iter().enumerate() {
            if **existing == *e {
                return Ok(1u64 << i);
            }
        }
        if *n_flat == MAX_LIST {
            return Err(self.limit("GROUP BY list", MAX_LIST));
        }
        let bit = 1u64 << *n_flat;
        flat[*n_flat] = e;
        *n_flat += 1;
        Ok(bit)
    }

    /// Parses a single grouping term — either a bare expression or a
    /// parenthesized `(a, b, ...)` compound (one grouping level spanning
    /// several columns) — and returns the OR of its column bits.
    fn grouping_term(
        &mut self,
        flat: &mut [&'a Expr<'a>; MAX_LIST],
        n_flat: &mut usize,
    ) -> Result<u64, ParseError> {
        // A parenthesized list groups several columns into one level. A bare
        // parenthesized single expression is just that expression.
        if self.peeked == Tok::Op("(") && self.paren_is_group_list()? {
            self.advance()?;
            let mut mask = 0u64;
            if self.peeked != Tok::Op(")") {
                loop {
                    let e = self.expression(0)?;
                    mask |= self.intern_group(flat, n_flat, e)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            Ok(mask)
        } else {
            let e = self.expression(0)?;
            self.intern_group(flat, n_flat, e)
        }
    }

    /// Parses a comma-separated list of grouping terms (inside `ROLLUP(...)` /
    /// `CUBE(...)`), storing one mask per term. Returns the term count.
    fn grouping_term_list(
        &mut self,
        flat: &mut [&'a Expr<'a>; MAX_LIST],
        n_flat: &mut usize,
        terms: &mut [u64; MAX_LIST],
    ) -> Result<usize, ParseError> {
        let mut n = 0usize;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("GROUP BY list", MAX_LIST));
            }
            terms[n] = self.grouping_term(flat, n_flat)?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(n)
    }

    /// Parses one member of a `GROUPING SETS (...)` list into `elem` — a single
    /// set `(a, b)` / `()` / bare expr, or a nested `ROLLUP`/`CUBE` that
    /// expands to several sets.
    fn grouping_set_member(
        &mut self,
        flat: &mut [&'a Expr<'a>; MAX_LIST],
        n_flat: &mut usize,
        elem: &mut [u64; MAX_GROUPING_SETS],
        n_elem: &mut usize,
    ) -> Result<(), ParseError> {
        if self.peeked == Tok::Ident("rollup") || self.peeked == Tok::Ident("cube") {
            let is_cube = self.peeked == Tok::Ident("cube");
            self.advance()?;
            self.expect_op("(")?;
            let mut terms = [0u64; MAX_LIST];
            let n_terms = self.grouping_term_list(flat, n_flat, &mut terms)?;
            self.expect_op(")")?;
            if is_cube {
                if n_terms > 20 {
                    return Err(self.err_here("CUBE with too many columns"));
                }
                for subset in 0u32..(1u32 << n_terms) {
                    let mut m = 0u64;
                    for (t, &tm) in terms[..n_terms].iter().enumerate() {
                        if subset & (1 << t) != 0 {
                            m |= tm;
                        }
                    }
                    push_mask(elem, n_elem, m, || self.err_here("too many grouping sets"))?;
                }
            } else {
                for keep in (0..=n_terms).rev() {
                    let mut m = 0u64;
                    for &tm in &terms[..keep] {
                        m |= tm;
                    }
                    push_mask(elem, n_elem, m, || self.err_here("too many grouping sets"))?;
                }
            }
            Ok(())
        } else {
            let m = self.grouping_term(flat, n_flat)?;
            push_mask(elem, n_elem, m, || self.err_here("too many grouping sets"))
        }
    }

    /// With `(` peeked at grouping-term position, reports whether it opens a
    /// multi-column grouping list — `()` or `(a, b, ...)` — as opposed to a
    /// scalar parenthesized expression like `(a + b)` or `(x + 1) * 2`. It
    /// Whether the token after the peeked one is `::` — a cloned-lexer
    /// lookahead, used to keep a unary minus from folding into a literal the
    /// cast binds tighter to.
    pub(super) fn next_is_cast(&self) -> Result<bool, ParseError> {
        let mut lexer = self.lexer.clone();
        Ok(matches!(lexer.next_token()?, Tok::Op("::")))
    }

    /// scans a cloned lexer to the matching close paren: a top-level comma is
    /// never valid inside a scalar `( ... )`, so seeing one (or an immediate
    /// close, the empty grand-total level) unambiguously marks a grouping list.
    fn paren_is_group_list(&self) -> Result<bool, ParseError> {
        let mut lexer = self.lexer.clone();
        let mut depth = 1usize; // the peeked `(` is already consumed by the real lexer
        let mut tokens = 0usize;
        loop {
            let tok = lexer.next_token()?;
            tokens += 1;
            match tok {
                Tok::Op("(") | Tok::Op("[") => depth += 1,
                Tok::Op(")") | Tok::Op("]") => {
                    depth -= 1;
                    // Matching close with no top-level comma: an empty list
                    // `()` (the first token closed it) is a grand-total level;
                    // anything else was a scalar `( ... )`.
                    if depth == 0 {
                        return Ok(tokens == 1);
                    }
                }
                Tok::Op(",") if depth == 1 => return Ok(true),
                Tok::Eof => return Ok(false),
                _ => {}
            }
        }
    }

    fn arena_slice<T: Copy>(&self, items: &[T]) -> Result<&'a [T], ParseError> {
        self.arena
            .alloc_slice_copy(items)
            .map(|m| &*m)
            .map_err(|_| self.err_here("statement too large for SQL arena"))
    }

    fn arena_str(&self, s: &str) -> Result<&'a str, ParseError> {
        self.arena
            .alloc_str(s)
            .map_err(|_| self.err_here("statement too large for SQL arena"))
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(96, "syntax error: {}", expected),
            sqlstate: sqlstate::SYNTAX_ERROR,
        }
    }

    fn err_here(&self, message: &'static str) -> ParseError {
        ParseError::new(self.peek_at, message)
    }

    fn limit(&self, what: &'static str, max: usize) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(96, "{} exceeds fixed limit of {}", what, max),
            sqlstate: sqlstate::SYNTAX_ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    fn with_parser<R>(text: &str, f: impl FnOnce(&mut Parser) -> R) -> R {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 18).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut p = Parser::new(text, &arena).unwrap();
            f(&mut p)
        })
    }

    #[test]
    fn select_literals_with_aliases() {
        with_parser("SELECT 1, 'x' AS name, 2.5 half", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.items.len(), 3);
            let SelectItem::Expr { expression, alias } = s.items[1] else {
                panic!()
            };
            assert_eq!(*expression, Expr::Str("x"));
            assert_eq!(alias, Some("name"));
            let SelectItem::Expr { alias, .. } = s.items[2] else {
                panic!()
            };
            assert_eq!(alias, Some("half"));
            assert!(p.next_stmt().unwrap().is_none());
        });
    }

    #[test]
    fn quantified_subqueries_are_typed_at_the_parse_boundary() {
        with_parser(
            "SELECT ROW(1, 2) < ANY (SELECT 1, 3), 1 = ANY (SELECT 1)",
            |p| {
                let Stmt::Select(select) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let SelectItem::Expr { expression, .. } = select.items[0] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::QuantifiedSubquery {
                        operator: BinaryOp::Lt,
                        all: false,
                        ..
                    }
                ));
                let SelectItem::Expr { expression, .. } = select.items[1] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::InSubquery { negated: false, .. }
                ));
            },
        );
    }

    #[test]
    fn cte_materialization_and_query_bodies_are_typed_at_parse_time() {
        with_parser(
            "WITH a AS MATERIALIZED (SELECT 1), \
                  b AS NOT MATERIALIZED (SELECT * FROM a) \
             SELECT * FROM b",
            |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected WITH query")
                };
                assert_eq!(
                    select.with[0].materialization,
                    crate::sql::ast::CteMaterialization::Materialized
                );
                assert_eq!(
                    select.with[1].materialization,
                    crate::sql::ast::CteMaterialization::NotMaterialized
                );
            },
        );
        for query in [
            "CREATE TABLE copied AS WITH value AS (SELECT 1 AS id) SELECT id FROM value",
            "CREATE VIEW copied_view AS WITH value AS (SELECT 1 AS id) SELECT id FROM value",
            "COPY (WITH value AS (SELECT 1 AS id) SELECT id FROM value) TO STDOUT",
            "DECLARE copied_cursor CURSOR FOR WITH value AS (SELECT 1 AS id) SELECT id FROM value",
        ] {
            with_parser(query, |parser| {
                assert!(parser.next_stmt().unwrap().is_some(), "rejected {query}")
            });
        }
    }

    #[test]
    fn partition_declarations_are_typed_at_the_parse_boundary() {
        with_parser(
            "CREATE TABLE sales (sold_on date, amount int) PARTITION BY RANGE (sold_on); \
             CREATE TABLE sales_2026 PARTITION OF sales FOR VALUES FROM ('2026-01-01') TO ('2027-01-01'); \
             CREATE TABLE sales_other PARTITION OF sales DEFAULT",
            |p| {
                let Stmt::CreateTable(parent) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(
                    matches!(parent.partition, PartitionClause::By { strategy: PartitionStrategy::Range, columns } if columns == ["sold_on"])
                );
                let Stmt::CreateTable(child) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(
                    matches!(child.partition, PartitionClause::Of { bound: PartitionBound::Range { from, to }, .. } if from.len() == 1 && to.len() == 1)
                );
                let Stmt::CreateTable(default) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    default.partition,
                    PartitionClause::Of {
                        bound: PartitionBound::Default,
                        ..
                    }
                ));
            },
        );
    }

    #[test]
    fn default_partition_uses_postgresqls_bound_syntax() {
        with_parser(
            "CREATE TABLE leaf PARTITION OF parent FOR VALUES DEFAULT",
            |p| assert!(p.next_stmt().is_err()),
        );
    }

    #[test]
    fn publication_options_parse_without_heap_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE PUBLICATION changes FOR ALL TABLES WITH (publish = 'INSERT, Update, delete')",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                all_tables,
                publish,
                ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("publication statement did not parse")
            };
            assert!(all_tables);
            assert!(publish.insert && publish.update && publish.delete);
            assert!(!publish.truncate);
        });
    }

    #[test]
    fn duplicate_publication_options_fail_at_the_parse_boundary() {
        for sql in [
            "CREATE PUBLICATION changes WITH (publish = 'insert', publish = 'update')",
            "CREATE PUBLICATION changes WITH (publish_via_partition_root = true, publish_via_partition_root = false)",
            "ALTER PUBLICATION changes SET (publish = 'insert', publish = 'delete')",
            "ALTER PUBLICATION changes SET (publish_via_partition_root = true, publish_via_partition_root = false)",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "publication parser", 1 << 18).unwrap();
            let mut parser = Parser::new(sql, &arena).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.message.as_str(), "conflicting or redundant options");
            });
        }
    }

    #[test]
    fn empty_publication_parse_without_heap_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "empty publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new("CREATE PUBLICATION changes", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                all_tables, tables, ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("empty CREATE PUBLICATION did not parse")
            };
            assert!(!all_tables);
            assert!(tables.is_empty());
        });
    }

    #[test]
    fn alter_publication_is_a_typed_operation_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "ALTER PUBLICATION changes ADD TABLE public.orders, archive.orders",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterPublication { name, action }) = parser.next_stmt().unwrap() else {
                panic!("ALTER PUBLICATION did not parse")
            };
            assert_eq!(name, "changes");
            let crate::sql::ast::AlterPublicationAction::AddTargets { tables, schemas } = action
            else {
                panic!("membership action lost its operation")
            };
            assert_eq!(tables.len(), 2);
            assert!(schemas.is_empty());
            assert_eq!(tables[0].relation.schema, Some("public"));
            assert_eq!(tables[1].relation.schema, Some("archive"));
        });
    }

    #[test]
    fn publication_column_lists_remain_attached_to_their_relation_without_allocation() {
        with_parser(
            "CREATE PUBLICATION changes FOR TABLE public.orders (id, total) WHERE (id > 0), archive.orders",
            |parser| {
                let Some(Stmt::CreatePublication { tables, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication column list did not parse")
                };
                assert_eq!(tables.len(), 2);
                assert_eq!(tables[0].relation.schema, Some("public"));
                assert_eq!(tables[0].relation.name, "orders");
                assert_eq!(tables[0].columns, ["id", "total"]);
                assert_eq!(tables[0].filter_text, Some("id > 0"));
                assert!(matches!(tables[0].filter, Some(Expr::Binary { .. })));
                assert!(tables[1].columns.is_empty());
                assert!(tables[1].filter.is_none());
            },
        );
    }

    #[test]
    fn alter_publication_set_table_does_not_consume_the_set_keyword_twice() {
        with_parser("ALTER PUBLICATION changes SET TABLE orders", |parser| {
            let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("SET TABLE did not parse")
            };
            let crate::sql::ast::AlterPublicationAction::SetTargets { tables, schemas } = action
            else {
                panic!("SET TABLE parsed as another ALTER PUBLICATION action")
            };
            assert_eq!(tables[0].relation, QualName::bare("orders"));
            assert!(tables[0].columns.is_empty());
            assert!(schemas.is_empty());
        });
    }

    #[test]
    fn alter_subscription_definition_operations_are_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter subscription parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "ALTER SUBSCRIPTION apply_changes CONNECTION 'host=127.0.0.2 port=5432'; \
             ALTER SUBSCRIPTION apply_changes SET PUBLICATION sales, inventory WITH (refresh = false)",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterSubscription { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("ALTER SUBSCRIPTION CONNECTION did not parse")
            };
            assert_eq!(
                action,
                crate::sql::ast::AlterSubscriptionAction::SetConnection("host=127.0.0.2 port=5432")
            );
            let Some(Stmt::AlterSubscription { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("ALTER SUBSCRIPTION SET PUBLICATION did not parse")
            };
            let crate::sql::ast::AlterSubscriptionAction::SetPublications {
                publications,
                refresh,
            } = action
            else {
                panic!("SET PUBLICATION lost its typed publication list")
            };
            assert_eq!(publications, ["sales", "inventory"]);
            assert_eq!(
                refresh,
                crate::sql::ast::SubscriptionPublicationRefresh::NoRefresh
            );
        });
    }

    #[test]
    fn alter_type_set_schema_is_a_typed_operation_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter type parser", 1 << 18).unwrap();
        let mut parser = Parser::new("ALTER TYPE public.state SET SCHEMA archive", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterType { name, action }) = parser.next_stmt().unwrap() else {
                panic!("ALTER TYPE did not parse")
            };
            assert_eq!(
                name,
                QualName {
                    schema: Some("public"),
                    name: "state"
                }
            );
            assert_eq!(
                action,
                crate::sql::ast::AlterTypeAction::SetSchema("archive")
            );
        });
    }

    #[test]
    fn row_trigger_lifecycle_is_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER audit_change BEFORE INSERT OR UPDATE OR DELETE ON public.orders \
             FOR EACH ROW EXECUTE FUNCTION audit_row('audit'); \
             ALTER TABLE public.orders DISABLE TRIGGER audit_change; \
             DROP TRIGGER IF EXISTS audit_change ON public.orders",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("CREATE TRIGGER did not parse")
            };
            assert_eq!(trigger.name, "audit_change");
            assert_eq!(trigger.timing, crate::sql::ast::TriggerTiming::Before);
            assert_eq!(
                trigger.events,
                [
                    crate::sql::ast::TriggerEvent::Insert,
                    crate::sql::ast::TriggerEvent::Update,
                    crate::sql::ast::TriggerEvent::Delete,
                ]
            );
            assert_eq!(
                trigger.table,
                QualName {
                    schema: Some("public"),
                    name: "orders"
                }
            );
            assert_eq!(trigger.function, QualName::bare("audit_row"));
            assert_eq!(trigger.arguments, ["audit"]);
            let Some(Stmt::AlterTable(alter)) = parser.next_stmt().unwrap() else {
                panic!("ALTER TABLE trigger mode did not parse")
            };
            assert_eq!(
                alter.table,
                QualName {
                    schema: Some("public"),
                    name: "orders"
                }
            );
            assert_eq!(
                alter.actions,
                [crate::sql::ast::AlterAction::SetTriggerEnabled {
                    target: crate::sql::ast::TriggerEnableTarget::Name("audit_change"),
                    enabled: crate::sql::ast::TriggerEnableMode::Disabled,
                }]
            );
            let Some(Stmt::DropTrigger {
                trigger,
                if_exists,
                cascade,
            }) = parser.next_stmt().unwrap()
            else {
                panic!("DROP TRIGGER did not parse")
            };
            assert!(if_exists);
            assert!(!cascade);
            assert_eq!(
                trigger,
                crate::sql::ast::TriggerIdentity {
                    name: "audit_change",
                    table: QualName {
                        schema: Some("public"),
                        name: "orders"
                    },
                }
            );
        });
    }

    #[test]
    fn instead_of_view_trigger_is_typed_and_illegal_forms_fail_at_parse_boundary() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "instead of trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER write_view INSTEAD OF INSERT OR UPDATE OR DELETE ON public.orders_view \
             FOR EACH ROW EXECUTE FUNCTION write_orders()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("INSTEAD OF trigger did not parse")
            };
            assert_eq!(trigger.timing, crate::sql::ast::TriggerTiming::InsteadOf);
            assert_eq!(trigger.level, crate::sql::ast::TriggerLevel::Row);
        });
        for source in [
            "CREATE TRIGGER bad INSTEAD OF INSERT ON orders_view FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad INSTEAD OF TRUNCATE ON orders_view FOR EACH ROW EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad INSTEAD OF UPDATE OF id ON orders_view FOR EACH ROW EXECUTE FUNCTION f()",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "invalid instead of trigger", 1 << 18).unwrap();
            let mut parser = Parser::new(source, &arena).unwrap();
            assert!(parser.next_stmt().is_err(), "{source}");
        }
    }

    #[test]
    fn transition_table_trigger_is_typed_and_illegal_forms_fail_at_parse_boundary() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "transition trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER audit_change AFTER UPDATE ON public.orders \
             REFERENCING OLD TABLE AS old_orders NEW TABLE AS new_orders \
             FOR EACH STATEMENT EXECUTE FUNCTION audit_statement()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("CREATE TRIGGER did not parse")
            };
            assert_eq!(
                trigger.transition_tables,
                crate::sql::ast::TriggerTransitionTables::OldNew {
                    old: "old_orders",
                    new: "new_orders",
                }
            );
        });
        for source in [
            "CREATE TRIGGER bad BEFORE UPDATE ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER INSERT ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER UPDATE ON orders REFERENCING OLD TABLE AS rows NEW TABLE AS rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER UPDATE OF amount ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena =
                Arena::new(&mut budget, "invalid transition trigger parser", 1 << 18).unwrap();
            let mut parser = Parser::new(source, &arena).unwrap();
            assert!(parser.next_stmt().is_err(), "{source}");
        }
    }

    #[test]
    fn publication_schema_targets_are_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "publication schema parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE PUBLICATION changes FOR TABLE public.orders, TABLES IN SCHEMA archive, current_schema()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                tables, schemas, ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("publication schema targets did not parse")
            };
            assert_eq!(tables[0].relation.schema, Some("public"));
            assert_eq!(tables[0].relation.name, "orders");
            assert!(tables[0].columns.is_empty());
            assert_eq!(schemas, ["archive", "public"]);
        });
    }

    #[test]
    fn publication_owner_change_is_typed_without_allocation() {
        with_parser(
            "ALTER PUBLICATION changes OWNER TO replication_owner",
            |parser| {
                let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication owner change did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterPublicationAction::SetOwner("replication_owner")
                );
            },
        );
    }

    #[test]
    fn publication_rename_is_typed_without_allocation() {
        with_parser(
            "ALTER PUBLICATION changes RENAME TO renamed_changes",
            |parser| {
                let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication rename did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterPublicationAction::Rename("renamed_changes")
                );
            },
        );
    }

    #[test]
    fn index_rename_is_typed_without_allocation() {
        with_parser(
            "ALTER INDEX IF EXISTS public.old_index RENAME TO new_index",
            |parser| {
                let Some(Stmt::AlterIndex {
                    name,
                    if_exists,
                    action,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("index rename did not parse")
                };
                assert_eq!(
                    name,
                    QualName {
                        schema: Some("public"),
                        name: "old_index"
                    }
                );
                assert!(if_exists);
                assert_eq!(
                    action,
                    crate::sql::ast::AlterIndexAction::Rename("new_index")
                );
            },
        );
    }

    #[test]
    fn index_and_tablespace_lifecycle_is_typed_without_allocation() {
        with_parser(
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS value_idx ON ONLY public.items \
             (id DESC NULLS LAST, value COLLATE \"C\" text_ops) INCLUDE (payload) \
             NULLS NOT DISTINCT WITH (fillfactor=80, deduplicate_items=off) \
             TABLESPACE fast WHERE id > 0; \
             ALTER INDEX value_idx ATTACH PARTITION value_idx_p0; \
             REINDEX (VERBOSE, TABLESPACE fast, CONCURRENTLY) INDEX value_idx; \
             DROP INDEX CONCURRENTLY IF EXISTS value_idx; \
             CREATE TABLESPACE fast OWNER postgres LOCATION '/object-prefix/fast' \
             WITH (random_page_cost=0, seq_page_cost=1e300, effective_io_concurrency=8); \
             ALTER TABLESPACE fast RESET (random_page_cost, effective_io_concurrency); \
             GRANT CREATE ON TABLESPACE fast TO PUBLIC; \
             COMMENT ON TABLESPACE fast IS 'placement'; \
             DROP TABLESPACE IF EXISTS fast",
            |parser| {
                let Some(Stmt::CreateIndex {
                    build,
                    scope,
                    if_not_exists,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    options,
                    tablespace,
                    unique,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE INDEX did not produce a typed index command")
                };
                assert_eq!(build, IndexBuildMode::Concurrent);
                assert_eq!(scope, IndexTargetScope::Only);
                assert!(if_not_exists && nulls_not_distinct && unique);
                assert_eq!(columns.len(), 2);
                assert_eq!(include_columns, ["payload"]);
                assert_eq!(options.fillfactor, Some(80));
                assert_eq!(tablespace, Some("fast"));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterIndex {
                        action: AlterIndexAction::AttachPartition(_),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Reindex { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropIndex {
                        build: IndexBuildMode::Concurrent,
                        ..
                    })
                ));
                let Some(Stmt::CreateTablespace { options, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE TABLESPACE did not produce typed options")
                };
                assert_eq!(options.random_page_cost.unwrap().value(), 0.0);
                assert_eq!(options.seq_page_cost.unwrap().value(), 1e300);
                assert_eq!(options.effective_io_concurrency, Some(8));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTablespace {
                        action: AlterTablespaceAction::ResetOptions(_),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::GrantPrivileges { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: CommentTarget::Tablespace("fast"),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropTablespace {
                        name: "fast",
                        if_exists: true
                    })
                ));
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
    }

    #[test]
    fn grouping_sets_expansion() {
        // Plain GROUP BY: no explicit sets, all columns implied.
        with_parser("SELECT a FROM t GROUP BY a, b", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 2);
            assert!(s.grouping_sets.is_empty());
        });
        // ROLLUP(a, b) -> {a,b}, {a}, {} (bits index group_by = [a, b]).
        with_parser("SELECT a FROM t GROUP BY ROLLUP(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 2);
            assert_eq!(s.grouping_sets, &[0b11, 0b01, 0b00]);
        });
        // CUBE(a, b) -> all four subsets.
        with_parser("SELECT a FROM t GROUP BY CUBE(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.grouping_sets.len(), 4);
            for expected in [0b00, 0b01, 0b10, 0b11] {
                assert!(s.grouping_sets.contains(&expected));
            }
        });
        // Explicit GROUPING SETS, including the empty grand-total set.
        with_parser(
            "SELECT a FROM t GROUP BY GROUPING SETS ((a, b), (a), ())",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(s.grouping_sets, &[0b11, 0b01, 0b00]);
            },
        );
        // Cross product: a, ROLLUP(b, c) -> a always set, times {bc},{b},{}.
        with_parser("SELECT a FROM t GROUP BY a, ROLLUP(b, c)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 3); // a, b, c
            assert_eq!(s.grouping_sets, &[0b111, 0b011, 0b001]);
        });
        // A parenthesized scalar must not be read as a grouping list.
        with_parser("SELECT a FROM t GROUP BY (a + 1) * 2", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 1);
            assert!(s.grouping_sets.is_empty());
        });
    }

    #[test]
    fn derived_table_column_alias_list() {
        with_parser("SELECT * FROM (VALUES (1,'a')) AS v(id, name)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("v"));
            assert_eq!(base.col_alias, Some(&["id", "name"][..]));
            assert!(base.subquery.is_some());
        });
    }

    #[test]
    fn table_function_column_alias() {
        with_parser("SELECT * FROM generate_series(1,3) AS g(x)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("g"));
            assert_eq!(base.col_alias, Some(&["x"][..]));
            assert!(base.func_args.is_some());
        });
    }

    #[test]
    fn rows_from_is_a_nonempty_typed_function_group() {
        with_parser(
            "SELECT * FROM LATERAL ROWS FROM (generate_series(1,3), unnest(ARRAY[4,5])) \
             WITH ORDINALITY AS expanded(series, value, ordinality)",
            |p| {
                let Stmt::Select(select) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let source = &select.from.unwrap().base;
                assert!(source.lateral);
                assert!(source.with_ordinality);
                assert_eq!(source.alias, Some("expanded"));
                assert_eq!(
                    source.col_alias,
                    Some(&["series", "value", "ordinality"][..])
                );
                let functions = source.rows_from.expect("typed ROWS FROM group");
                assert_eq!(functions.len(), 2);
                assert_eq!(functions[0].table, "generate_series");
                assert_eq!(functions[1].table, "unnest");
                assert!(functions.iter().all(TableRef::is_function_source));
            },
        );
        for sql in [
            "SELECT * FROM ROWS FROM ()",
            "SELECT * FROM ROWS FROM (ordinary_table)",
            "SELECT * FROM ROWS FROM (ROWS FROM (generate_series(1,2)))",
        ] {
            with_parser(sql, |p| assert!(p.next_stmt().is_err(), "{sql}"));
        }
    }

    #[test]
    fn precedence_and_parens() {
        with_parser("SELECT 1 + 2 * 3, (1 + 2) * 3", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let SelectItem::Expr { expression, .. } = s.items[0] else {
                panic!()
            };
            // 1 + (2 * 3)
            let Expr::Binary {
                operator: BinaryOp::Add,
                left,
                right,
            } = expression
            else {
                panic!()
            };
            assert_eq!(**left, Expr::Int(1));
            assert!(matches!(
                right,
                Expr::Binary {
                    operator: BinaryOp::Mul,
                    ..
                }
            ));
            let SelectItem::Expr { expression, .. } = s.items[1] else {
                panic!()
            };
            assert!(matches!(
                expression,
                Expr::Binary {
                    operator: BinaryOp::Mul,
                    ..
                }
            ));
        });
    }

    #[test]
    fn full_select_shape() {
        with_parser(
            "SELECT a, b FROM t WHERE a > 1 AND b = 'x' ORDER BY a DESC, b LIMIT 10",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(s.from.unwrap().base.table, "t");
                assert!(matches!(
                    s.where_clause.unwrap(),
                    Expr::Binary {
                        operator: BinaryOp::And,
                        ..
                    }
                ));
                assert_eq!(s.order_by.len(), 2);
                assert!(s.order_by[0].descending);
                assert!(!s.order_by[1].descending);
                assert_eq!(s.limit, Some(&Expr::Int(10)));
            },
        );
    }

    #[test]
    fn ddl_and_dml() {
        with_parser(
            "CREATE TABLE t (id int NOT NULL, name text, score double precision);
             INSERT INTO t (id, name) VALUES (1, 'a'), (2, NULL);
             UPDATE t SET name = 'b' WHERE id = 1;
             DELETE FROM t WHERE id = 2;
             DROP TABLE IF EXISTS t",
            |p| {
                let Stmt::CreateTable(c) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(c.name, QualName::bare("t"));
                assert_eq!(c.columns.len(), 3);
                assert!(c.columns[0].not_null);
                assert_eq!(c.columns[2].type_name, "float8");

                let Stmt::Insert(i) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(i.columns, &["id", "name"]);
                assert_eq!(i.rows.len(), 2);
                assert_eq!(*i.rows[1][1], Expr::Null);

                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Update(_)));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Delete(_)));
                let Stmt::DropTable(d) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(d.if_exists);
            },
        );
    }

    #[test]
    fn constraint_attributes_reject_unrepresentable_states_at_parse_time() {
        for sql in [
            "CREATE TABLE t (id integer UNIQUE ENFORCED)",
            "CREATE TABLE t (id integer CHECK (id > 0) DEFERRABLE)",
            "CREATE TABLE t (id integer UNIQUE INITIALLY DEFERRED)",
            "CREATE TABLE t (id integer, CHECK (id > 0) NOT VALID)",
            "CREATE TABLE t (id integer, EXCLUDE USING btree (id WITH =))",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}")
            });
        }
        with_parser(
            "ALTER TABLE t ADD CONSTRAINT positive CHECK (id > 0) NOT VALID",
            |parser| assert!(parser.next_stmt().unwrap().is_some()),
        );
    }

    #[test]
    fn multi_word_type_aliases_retain_array_suffixes() {
        with_parser(
            "CREATE TABLE type_alias_arrays (values double precision[], moments timestamp with time zone[])",
            |p| {
                let Stmt::CreateTable(create) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(create.columns[0].type_name, "float8[]");
                assert_eq!(create.columns[1].type_name, "timestamptz[]");
            },
        );
    }

    #[test]
    fn casts_is_null_and_txn() {
        with_parser(
            "SELECT 1::bigint, NULL IS NULL, 2 IS NOT NULL; BEGIN; COMMIT; ROLLBACK",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let SelectItem::Expr { expression, .. } = s.items[0] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::Cast {
                        type_name: "bigint",
                        ..
                    }
                ));
                let SelectItem::Expr { expression, .. } = s.items[2] else {
                    panic!()
                };
                assert!(matches!(expression, Expr::IsNull { negated: true, .. }));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Begin("")));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Commit));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Rollback));
            },
        );
    }

    #[test]
    fn syntax_errors_carry_position() {
        with_parser("SELECT FROM", |p| {
            let err = p.next_stmt().unwrap_err();
            assert_eq!(err.at, 7);
        });
    }

    #[test]
    fn statistics_grammar_produces_only_valid_typed_states() {
        with_parser(
            "CREATE STATISTICS s (ndistinct, dependencies, mcv) ON a, (lower(b)) FROM app.t; \
             ALTER STATISTICS s SET STATISTICS DEFAULT; \
             ALTER STATISTICS s SET STATISTICS 10000; \
             DROP STATISTICS IF EXISTS s CASCADE",
            |parser| {
                let Stmt::CreateStatistics(create) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected CREATE STATISTICS")
                };
                let StatisticsKeys::Multivariate { kinds, keys } = create.keys else {
                    panic!("expected multivariate statistics")
                };
                assert!(kinds.ndistinct() && kinds.dependencies() && kinds.mcv());
                assert_eq!(keys.len(), 2);
                assert!(matches!(keys[1], StatisticsKey::Expression(_)));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterStatistics {
                        action: AlterStatisticsAction::SetTarget(StatisticsTarget::Default),
                        ..
                    }
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterStatistics {
                        action: AlterStatisticsAction::SetTarget(StatisticsTarget::Value(10000)),
                        ..
                    }
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::DropStatistics {
                        if_exists: true,
                        cascade: true,
                        ..
                    }
                ));
            },
        );
        for sql in [
            "CREATE STATISTICS ON a FROM t",
            "CREATE STATISTICS s (ndistinct) ON (lower(a)) FROM t",
            "CREATE STATISTICS s (mcv, mcv) ON a, b FROM t",
            "CREATE STATISTICS IF NOT EXISTS ON a, b FROM t",
            "ALTER STATISTICS s SET STATISTICS 10001",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}")
            });
        }
    }
}
