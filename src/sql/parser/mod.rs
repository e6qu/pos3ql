//! Recursive-descent parser (Pratt for expressions) into the arena AST.
//!
//! Fixed limits, checked loudly: at most [`MAX_LIST`] items per select
//! list / column list / VALUES row, and [`MAX_ROWS`] rows per INSERT.

use crate::sql::eval::sqlstate;
use crate::mem::arena::Arena;
use crate::stack_format;
use crate::util::StackStr;

use super::ast::*;
use super::lexer::{LexError, Lexer, Tok};
use super::types::{TypeMod, INTERVAL_FULL_RANGE};

/// Names for the calls a desugaring produces, for syntax PostgreSQL does not
/// also expose as a function. A space cannot appear in an identifier, so a
/// query cannot reach these by writing them, and the function router will not
/// answer to `similar_to(...)` or `overlaps(...)` — which PostgreSQL refuses.
/// Any future desugaring of syntax-only constructs belongs here too.
pub(crate) const SIMILAR_TO: &str = "similar to";
pub(crate) const OVERLAPS_PERIODS: &str = "overlaps periods";

pub const MAX_LIST: usize = 64;

pub const MAX_CTES: usize = 16;
/// Upper bound on `WINDOW name AS (...)` definitions in one SELECT.
pub const MAX_WINDOW_DEFS: usize = 16;
/// Upper bound on warnings one statement's parse may raise.
pub const MAX_PARSE_WARNINGS: usize = 8;
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
pub const MAX_ROWS: usize = 256;

/// Words that cannot appear as a bare column reference; mirrors the
/// reserved entries of PostgreSQL's keyword table that this grammar uses.
/// Whether a numeric token carries a `0x`/`0o`/`0b` base prefix.
/// Whether `text` mentions the word `window` in any case — the cheap pre-filter
/// that keeps the WINDOW-clause lookahead off the path of ordinary queries.
fn mentions_window(text: &str) -> bool {
    text.as_bytes().windows(6).any(|w| w.eq_ignore_ascii_case(b"window"))
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
                | "xmlnamespaces" | "xmlparse" | "xmlpi" | "xmlroot" | "xmlserialize" | "xmltable" => Keyword::ColumnName,
            "authorization" | "binary" | "collation" | "concurrently" | "cross"
                | "current_schema" | "freeze" | "full" | "ilike" | "inner" | "is" | "isnull" | "join"
                | "left" | "like" | "natural" | "notnull" | "outer" | "overlaps" | "right" | "similar"
                | "tablesample" | "verbose" => Keyword::TypeFuncName,
            "all" | "analyse" | "analyze" | "and" | "any" | "array" | "as" | "asc"
                | "asymmetric" | "both" | "case" | "cast" | "check" | "collate" | "column"
                | "constraint" | "create" | "current_catalog" | "current_date" | "current_role"
                | "current_time" | "current_timestamp" | "current_user" | "default" | "deferrable"
                | "desc" | "distinct" | "do" | "else" | "end" | "except" | "false" | "fetch" | "for"
                | "foreign" | "from" | "grant" | "group" | "having" | "in" | "initially" | "intersect"
                | "into" | "lateral" | "leading" | "limit" | "localtime" | "localtimestamp" | "not"
                | "null" | "offset" | "on" | "only" | "or" | "order" | "placing" | "primary"
                | "references" | "returning" | "select" | "session_user" | "some" | "symmetric"
                | "system_user" | "table" | "then" | "to" | "trailing" | "true" | "union" | "unique"
                | "user" | "using" | "variadic" | "when" | "where" | "window" | "with" => Keyword::Reserved,
        _ => return None,
    })
}

/// Whether `word` is a keyword PostgreSQL refuses in a `ColId` position — a
/// column name, a table name, or a FROM-item alias. Its two reserved categories
/// are rejected there; the unreserved ones are accepted — `begin`, `values` and
/// `set` are all perfectly legal column names.
pub(crate) fn is_column_name_keyword(word: &str) -> bool {
    matches!(keyword_category(word), Some(Keyword::TypeFuncName | Keyword::Reserved))
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
}

/// Parses a stored view definition (a single SELECT) into a `Select` in the
/// arena, for expansion as a derived table. Set-operation view bodies are not
/// supported yet.
pub fn parse_view_select<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::sqlstate::SYNTAX_ERROR,
        message: crate::stack_format!(192, "invalid view definition: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let statement = parser
        .next_stmt()
        .map_err(|e| to_sql(e.message.as_str()))?
        .ok_or_else(|| to_sql("empty"))?;
    match statement {
        Stmt::Select(s) => arena.alloc(s).map(|r| &*r).map_err(|_| super::eval::SqlError {
            sqlstate: super::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            message: crate::stack_format!(192, "view too large for SQL arena"),
        }),
        _ => Err(to_sql("view body must be a plain SELECT")),
    }
}

/// Parses a single scalar expression (e.g. a stored CHECK predicate) into the
/// arena. The whole input must be one expression.
pub fn parse_expr<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::sqlstate::SYNTAX_ERROR,
        message: crate::stack_format!(192, "invalid expression: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let expression = parser.expression(0).map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after expression"));
    }
    Ok(expression)
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
        })
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
            Ok(QualName { schema: Some(first), name: self.col_ident(what)? })
        } else {
            Ok(QualName::bare(first))
        }
    }

    /// DECLARE name [BINARY] [INSENSITIVE|ASENSITIVE] [[NO] SCROLL] CURSOR
    /// [{WITH|WITHOUT} HOLD] FOR select ("declare" not yet consumed). BINARY
    /// is refused loudly (binary-format simple-query rows are not produced).
    fn declare_cursor(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // declare
        let name = self.col_ident("cursor name")?;
        let mut scroll = false;
        loop {
            if self.eat_ident("binary")? {
                return Err(ParseError {
                    at: self.peek_at,
                    message: crate::stack_format!(96, "BINARY cursors are not supported"),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else if self.eat_ident("insensitive")? || self.eat_ident("asensitive")? {
                // Materialization makes every cursor insensitive.
            } else if self.eat_ident("scroll")? {
                scroll = true;
            } else if self.eat_ident("no")? {
                self.expect_ident("scroll")?;
                scroll = false;
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
        let _ = self.query()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::DeclareCursor { name, scroll, hold, sql })
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
        Ok(Stmt::FetchCursor { name, motion, move_only })
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
        Ok(Stmt::Truncate { tables, restart_identity, cascade })
    }

    fn statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        match self.peeked {
            Tok::Ident("select") | Tok::Op("(") => self.query(),
            Tok::Ident("with") => self.with_query(),
            Tok::Ident("create") => self.create(),
            Tok::Ident("drop") => self.drop_stmt(),
            Tok::Ident("insert") => self.insert(),
            Tok::Ident("update") => self.update(),
            Tok::Ident("delete") => self.delete(),
            Tok::Ident("truncate") => self.truncate(),
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
                self.skip_transaction_modifiers()?;
                Ok(Stmt::Begin)
            }
            Tok::Ident("start") => {
                self.advance()?;
                self.expect_ident("transaction")?;
                self.skip_transaction_modifiers()?;
                Ok(Stmt::Begin)
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
                // SESSION / LOCAL scope prefixes are both treated as session.
                let _ = self.eat_ident("session")? || self.eat_ident("local")?;
                // SET TRANSACTION ... / SET SESSION CHARACTERISTICS AS
                // TRANSACTION ...: set the (default) transaction characteristics.
                // The engine provides one isolation level, as BEGIN does, so the
                // clause is consumed and acknowledged.
                if self.eat_ident("transaction")? || self.eat_ident("characteristics")? {
                    while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                        self.advance()?;
                    }
                    return Ok(Stmt::SetTransaction);
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
                Ok(Stmt::Set { name, value })
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
            Tok::Ident("alter") => self.alter_table(),
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
            _ => Err(self.unexpected("expected a statement")),
        }
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
                    Expr::WholeRow(table) if alias.is_none() => {
                        SelectItem::TableWildcard(table)
                    }
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
            with: &[],
            set_body: None,
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
    #[allow(clippy::type_complexity)]
    fn order_limit(
        &mut self,
    ) -> Result<(&'a [OrderBy<'a>], Option<&'a Expr<'a>>, Option<&'a Expr<'a>>), ParseError> {
        let mut order = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false }; MAX_LIST];
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
                order[n_order] = OrderBy { expression, descending, nulls_first };
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
            } else {
                break;
            }
        }
        Ok((order_by, limit, offset))
    }

    /// A subquery body: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET applying to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself; a genuine
    /// set-operation is carried in `set_body`.
    fn select(&mut self) -> Result<Select<'a>, ParseError> {
        // This is the nesting boundary for every subquery, so it is where a
        // nested SELECT's named windows stop being visible.
        let enclosing_windows = (self.windows, self.n_windows);
        let body = self.set_union()?;
        let (order_by, limit, offset) = self.order_limit()?;
        (self.windows, self.n_windows) = enclosing_windows;
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
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
            with: &[],
            set_body: Some(body),
        })
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
                with: &[],
                set_body: None,
            })
            .map_err(|_| self.err_here("statement too large for SQL arena"))?;
        let mut ctes = [Cte { name: "", columns: &[], recursive: false, query: placeholder }; MAX_CTES];
        let mut n = 0;
        loop {
            if n == MAX_CTES {
                return Err(self.limit("WITH list", MAX_CTES));
            }
            let name = self.col_ident("CTE name")?;
            // Optional output-column rename list `name(c1, c2, ...)`.
            let columns = self.column_alias_list()?.unwrap_or(&[]);
            self.expect_ident("as")?;
            self.expect_op("(")?;
            let q = self.select()?;
            self.expect_op(")")?;
            let boxed = self
                .arena
                .alloc(q)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            ctes[n] = Cte { name, columns, recursive, query: boxed };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let ctes = self.arena_slice(&ctes[..n])?;
        match self.query()? {
            Stmt::Select(mut sel) => {
                sel.with = ctes;
                Ok(Stmt::Select(sel))
            }
            Stmt::SetQuery(mut q) => {
                q.with = ctes;
                Ok(Stmt::SetQuery(q))
            }
            _ => Err(self.err_here("WITH must be followed by SELECT")),
        }
    }

    fn query(&mut self) -> Result<Stmt<'a>, ParseError> {
        let enclosing_windows = (self.windows, self.n_windows);
        let body = self.set_union()?;
        let (order_by, limit, offset) = self.order_limit()?;
        (self.windows, self.n_windows) = enclosing_windows;
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            return Ok(Stmt::Select(sel));
        }
        Ok(Stmt::SetQuery(SetQuery { with: &[], body, order_by, limit, offset }))
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
            left = self.alloc_set(SetTree::Op { operator, all, left, right })?;
        }
        Ok(left)
    }

    /// INTERSECT level (binds tighter than UNION / EXCEPT).
    fn set_intersect(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        let mut left = self.set_leaf()?;
        while self.eat_ident("intersect")? {
            let all = self.set_all()?;
            let right = self.set_leaf()?;
            left = self.alloc_set(SetTree::Op { operator: SetOp::Intersect, all, left, right })?;
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
            let (order_by, limit, offset) = self.order_limit()?;
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
                    with: &[],
                    set_body: Some(op),
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
                    items[n] = SelectItem::Expr { expression: self.expression(0)?, alias: None };
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
                    with: &[],
                    set_body: None,
                };
                let leaf = self.alloc_set(SetTree::Select(
                    self.arena.alloc(sel).map_err(|_| self.err_here("VALUES too large"))?,
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
        // Derived table: `(SELECT ...) [AS] alias`. PostgreSQL requires the
        // alias, so a missing one is a syntax error.
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let select = self.select()?;
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
                col_alias,
                cte: None,
                with_ordinality: false,
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
            col_alias,
            cte: None,
            with_ordinality,
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

    #[expect(clippy::wrong_self_convention, reason = "parses the FROM clause; not a conversion")]
    fn from_clause(&mut self) -> Result<FromClause<'a>, ParseError> {
        let base = self.table_ref()?;
        let dummy = Join {
            table: TableRef { schema: None, table: "", alias: None, subquery: None, func_args: None, col_alias: None, cte: None, with_ordinality: false },
            kind: JoinKind::Inner,
            on: None,
            using_columns: None,
            natural: false,
        };
        let mut joins = [dummy; 8];
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
            joins[n] = Join { table, kind, on, using_columns, natural };
            n += 1;
        }
        Ok(FromClause {
            base,
            joins: self.arena_slice(&joins[..n])?,
        })
    }

    fn alter_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("alter")?;
        self.expect_ident("table")?;
        let table = self.qual_name("table name")?;
        let action = if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterAction::SetSchema(self.col_ident("schema name")?)
        } else if self.eat_ident("rename")? {
            if self.eat_ident("to")? {
                AlterAction::RenameTable(self.col_ident("new table name")?)
            } else {
                self.expect_ident("column")?;
                let from = self.col_ident("column name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new column name")?;
                AlterAction::RenameColumn { from, to }
            }
        } else if self.eat_ident("add")? {
            let _ = self.eat_ident("column")?;
            let name = self.col_ident("column name")?;
            let (type_name, type_mod) = self.type_name_mod()?;
            let mut not_null = false;
            let mut unique = false;
            let mut default = None;
            loop {
                if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    not_null = true;
                } else if self.eat_ident("null")? {
                    not_null = false;
                } else if self.eat_ident("default")? {
                    default = Some(self.expression(0)?);
                } else if self.eat_ident("unique")? {
                    unique = true;
                } else {
                    break;
                }
            }
            AlterAction::AddColumn(ColumnDef {
                name,
                type_name,
                type_mod,
                not_null,
                unique,
                primary: false,
                default,
            })
        } else if self.eat_ident("drop")? {
            let _ = self.eat_ident("column")?;
            AlterAction::DropColumn(self.col_ident("column name")?)
        } else {
            return Err(self.unexpected("expected RENAME, ADD or DROP"));
        };
        Ok(Stmt::AlterTable(AlterTable { table, action }))
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
        Ok(Stmt::Prepare { name, sql, param_types: self.arena_slice(&ptypes[..np])? })
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
        Ok(Stmt::ExecutePrepared { name, args: self.arena_slice(&args[..n])? })
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
        }))
    }

    /// `ON CONFLICT [(columns)] DO {NOTHING | UPDATE SET a = e, ... [WHERE cond]}`.
    fn on_conflict(&mut self) -> Result<Option<OnConflict<'a>>, ParseError> {
        if !self.eat_ident("on")? {
            return Ok(None);
        }
        self.expect_ident("conflict")?;
        let mut target: [&'a str; MAX_LIST] = [""; MAX_LIST];
        let mut nt = 0;
        if self.eat_op("(")? {
            loop {
                if nt == MAX_LIST {
                    return Err(self.limit("conflict target", MAX_LIST));
                }
                target[nt] = self.col_ident("column name")?;
                nt += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
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
            update,
            update_where,
        }))
    }

    fn update(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("update")?;
        let table = self.qual_name("table name")?;
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
            Some(&*self.arena.alloc(fc).map_err(|_| self.err_here("FROM too large for SQL arena"))?)
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Update(Update {
            table,
            assignments: self.arena_slice(&assignments[..n])?,
            from,
            where_clause,
            returning,
        }))
    }

    fn delete(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("delete")?;
        self.expect_ident("from")?;
        let table = self.qual_name("table name")?;
        let using = if self.eat_ident("using")? {
            let fc = self.from_clause()?;
            Some(&*self.arena.alloc(fc).map_err(|_| self.err_here("USING too large for SQL arena"))?)
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Delete(Delete { table, using, where_clause, returning }))
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
        // A schema-qualified type (`pg_catalog.int4`, `pg_catalog.regtype`):
        // drop the schema and keep the bare type name.
        if self.peeked == Tok::Op(".") {
            self.advance()?;
            name = self.any_ident("type name")?;
        }
        if name == "double" {
            self.expect_ident("precision")?;
            return Ok(("float8", -1));
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
                return Ok((if name == "timestamp" { "timestamptz" } else { "timetz" }, -1));
            }
            if self.eat_ident("without")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                return Ok((name, -1));
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
        // A trailing `[]` (repeatable) makes it a one-dimensional array type.
        if self.peeked == Tok::Op("[") {
            self.advance()?;
            self.expect_op("]")?;
            while self.peeked == Tok::Op("[") {
                self.advance()?;
                self.expect_op("]")?;
            }
            let array = self
                .arena
                .alloc_str(stack_format!(72, "{}[]", name).as_str())
                .map_err(|_| self.err_here("type name too long"))?;
            return Ok((array, -1));
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
                Ok(TypeMod::NumericPS { precision: p as u16, scale: s as u16 }.encode())
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
                "from" | "where" | "order" | "limit" | "group" | "having" | "union"
                    | "intersect" | "except" | "window"
                    | "and" | "or" | "is" | "as" | "asc" | "desc" | "offset"
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

    fn skip_transaction_modifiers(&mut self) -> Result<(), ParseError> {
        // BEGIN [WORK | TRANSACTION] [ISOLATION LEVEL ...] — accepted, the
        // engine provides its one isolation level regardless.
        while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            self.advance()?;
        }
        Ok(())
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
                        push_mask(&mut elem, &mut n_elem, m, || self.err_here("too many grouping sets"))?;
                    }
                } else {
                    for keep in (0..=n_terms).rev() {
                        let mut m = 0u64;
                        for &tm in &terms[..keep] {
                            m |= tm;
                        }
                        push_mask(&mut elem, &mut n_elem, m, || self.err_here("too many grouping sets"))?;
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
                push_mask(&mut elem, &mut n_elem, m, || self.err_here("too many grouping sets"))?;
            }
            // Cross product: acc × elem.
            let mut n_new = 0usize;
            for &a in &acc[..n_acc] {
                for &e in &elem[..n_elem] {
                    push_mask(&mut scratch, &mut n_new, a | e, || self.err_here("too many grouping sets"))?;
                }
            }
            acc[..n_new].copy_from_slice(&scratch[..n_new]);
            n_acc = n_new;
            if !self.eat_op(",")? {
                break;
            }
        }
        let group_by = self.arena_slice(&flat[..n_flat])?;
        let grouping_sets = if explicit { self.arena_slice(&acc[..n_acc])? } else { &[][..] };
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
        let mut p = Parser::new(text, &arena).unwrap();
        f(&mut p)
    }

    #[test]
    fn select_literals_with_aliases() {
        with_parser("SELECT 1, 'x' AS name, 2.5 half", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.items.len(), 3);
            let SelectItem::Expr { expression, alias } = s.items[1] else { panic!() };
            assert_eq!(*expression, Expr::Str("x"));
            assert_eq!(alias, Some("name"));
            let SelectItem::Expr { alias, .. } = s.items[2] else { panic!() };
            assert_eq!(alias, Some("half"));
            assert!(p.next_stmt().unwrap().is_none());
        });
    }

    #[test]
    fn grouping_sets_expansion() {
        // Plain GROUP BY: no explicit sets, all columns implied.
        with_parser("SELECT a FROM t GROUP BY a, b", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            assert_eq!(s.group_by.len(), 2);
            assert!(s.grouping_sets.is_empty());
        });
        // ROLLUP(a, b) -> {a,b}, {a}, {} (bits index group_by = [a, b]).
        with_parser("SELECT a FROM t GROUP BY ROLLUP(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            assert_eq!(s.group_by.len(), 2);
            assert_eq!(s.grouping_sets, &[0b11, 0b01, 0b00]);
        });
        // CUBE(a, b) -> all four subsets.
        with_parser("SELECT a FROM t GROUP BY CUBE(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            let mut got = s.grouping_sets.to_vec();
            got.sort_unstable();
            assert_eq!(got, vec![0b00, 0b01, 0b10, 0b11]);
        });
        // Explicit GROUPING SETS, including the empty grand-total set.
        with_parser("SELECT a FROM t GROUP BY GROUPING SETS ((a, b), (a), ())", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            assert_eq!(s.grouping_sets, &[0b11, 0b01, 0b00]);
        });
        // Cross product: a, ROLLUP(b, c) -> a always set, times {bc},{b},{}.
        with_parser("SELECT a FROM t GROUP BY a, ROLLUP(b, c)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            assert_eq!(s.group_by.len(), 3); // a, b, c
            assert_eq!(s.grouping_sets, &[0b111, 0b011, 0b001]);
        });
        // A parenthesized scalar must not be read as a grouping list.
        with_parser("SELECT a FROM t GROUP BY (a + 1) * 2", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            assert_eq!(s.group_by.len(), 1);
            assert!(s.grouping_sets.is_empty());
        });
    }

    #[test]
    fn derived_table_column_alias_list() {
        with_parser("SELECT * FROM (VALUES (1,'a')) AS v(id, name)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("v"));
            assert_eq!(base.col_alias, Some(&["id", "name"][..]));
            assert!(base.subquery.is_some());
        });
    }

    #[test]
    fn table_function_column_alias() {
        with_parser("SELECT * FROM generate_series(1,3) AS g(x)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("g"));
            assert_eq!(base.col_alias, Some(&["x"][..]));
            assert!(base.func_args.is_some());
        });
    }

    #[test]
    fn precedence_and_parens() {
        with_parser("SELECT 1 + 2 * 3, (1 + 2) * 3", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            let SelectItem::Expr { expression, .. } = s.items[0] else { panic!() };
            // 1 + (2 * 3)
            let Expr::Binary { operator: BinaryOp::Add, left, right } = expression else { panic!() };
            assert_eq!(**left, Expr::Int(1));
            assert!(matches!(right, Expr::Binary { operator: BinaryOp::Mul, .. }));
            let SelectItem::Expr { expression, .. } = s.items[1] else { panic!() };
            assert!(matches!(expression, Expr::Binary { operator: BinaryOp::Mul, .. }));
        });
    }

    #[test]
    fn full_select_shape() {
        with_parser(
            "SELECT a, b FROM t WHERE a > 1 AND b = 'x' ORDER BY a DESC, b LIMIT 10",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
                assert_eq!(s.from.unwrap().base.table, "t");
                assert!(matches!(
                    s.where_clause.unwrap(),
                    Expr::Binary { operator: BinaryOp::And, .. }
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
                let Stmt::CreateTable(c) = p.next_stmt().unwrap().unwrap() else { panic!() };
                assert_eq!(c.name, QualName::bare("t"));
                assert_eq!(c.columns.len(), 3);
                assert!(c.columns[0].not_null);
                assert_eq!(c.columns[2].type_name, "float8");

                let Stmt::Insert(i) = p.next_stmt().unwrap().unwrap() else { panic!() };
                assert_eq!(i.columns, &["id", "name"]);
                assert_eq!(i.rows.len(), 2);
                assert_eq!(*i.rows[1][1], Expr::Null);

                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Update(_)));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Delete(_)));
                let Stmt::DropTable(d) = p.next_stmt().unwrap().unwrap() else { panic!() };
                assert!(d.if_exists);
            },
        );
    }

    #[test]
    fn casts_is_null_and_txn() {
        with_parser("SELECT 1::bigint, NULL IS NULL, 2 IS NOT NULL; BEGIN; COMMIT; ROLLBACK", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else { panic!() };
            let SelectItem::Expr { expression, .. } = s.items[0] else { panic!() };
            assert!(matches!(expression, Expr::Cast { type_name: "bigint", .. }));
            let SelectItem::Expr { expression, .. } = s.items[2] else { panic!() };
            assert!(matches!(expression, Expr::IsNull { negated: true, .. }));
            assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Begin));
            assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Commit));
            assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Rollback));
        });
    }

    #[test]
    fn syntax_errors_carry_position() {
        with_parser("SELECT FROM", |p| {
            let err = p.next_stmt().unwrap_err();
            assert_eq!(err.at, 7);
        });
    }
}
