//! Recursive-descent parser (Pratt for expressions) into the arena AST.
//!
//! Fixed limits, checked loudly: at most [`MAX_LIST`] items per select
//! list / column list / VALUES row, and [`MAX_ROWS`] rows per INSERT.

use crate::mem::arena::Arena;
use crate::stack_format;
use crate::storage::MAX_INDEX_COLS;
use crate::util::StackStr;

use super::ast::*;
use super::lexer::{LexError, Lexer, Tok};

pub const MAX_LIST: usize = 64;
pub const MAX_CTES: usize = 16;
pub const MAX_ROWS: usize = 256;

/// Words that cannot appear as a bare column reference; mirrors the
/// reserved entries of PostgreSQL's keyword table that this grammar uses.
/// Whether a numeric token carries a `0x`/`0o`/`0b` base prefix.
fn is_base_prefixed(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() > 2 && b[0] == b'0' && matches!(b[1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
}

fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "select" | "from" | "where" | "order" | "group" | "having" | "union" | "limit"
            | "intersect" | "except"
            | "offset" | "insert" | "update" | "delete" | "create" | "drop" | "table"
            | "values" | "into" | "set" | "as" | "on" | "join" | "and" | "or" | "not"
            | "is" | "asc" | "desc" | "case" | "when" | "then" | "else" | "end"
            | "begin" | "commit" | "rollback" | "primary" | "key" | "references"
            | "default" | "unique" | "check" | "constraint" | "returning"
            | "distinct" | "between" | "like" | "ilike" | "in" | "left"
            | "right" | "full" | "inner" | "outer" | "cross" | "using"
    )
}

#[derive(Debug)]
pub struct ParseError {
    pub at: usize,
    pub message: StackStr<96>,
}

impl ParseError {
    fn new(at: usize, text: &str) -> Self {
        Self {
            at,
            message: stack_format!(96, "{}", text),
        }
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::new(e.at, e.message)
    }
}

pub struct Parser<'a> {
    text: &'a str,
    lexer: Lexer<'a>,
    peeked: Tok<'a>,
    peek_at: usize,
    arena: &'a Arena,
    /// Highest `$n` seen — the statement's parameter count.
    max_param: u32,
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

    fn statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        match self.peeked {
            Tok::Ident("select") | Tok::Op("(") => self.query(),
            Tok::Ident("with") => self.with_query(),
            Tok::Ident("create") => self.create(),
            Tok::Ident("drop") => self.drop_stmt(),
            Tok::Ident("insert") => self.insert(),
            Tok::Ident("update") => self.update(),
            Tok::Ident("delete") => self.delete(),
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
            } else {
                let expression = self.expression(0)?;
                let alias = self.alias()?;
                SelectItem::Expr { expression, alias }
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&items[..n])
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
        let distinct = if self.eat_ident("distinct")? {
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
        let mut group_exprs: [&'a Expr<'a>; MAX_LIST] = {
            let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
            [null_expr; MAX_LIST]
        };
        let mut n_group = 0;
        if self.eat_ident("group")? {
            self.expect_ident("by")?;
            loop {
                if n_group == MAX_LIST {
                    return Err(self.limit("GROUP BY list", MAX_LIST));
                }
                group_exprs[n_group] = self.expression(0)?;
                n_group += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let group_by = self.arena_slice(&group_exprs[..n_group])?;
        let having = if self.eat_ident("having")? {
            Some(self.expression(0)?)
        } else {
            None
        };
        Ok(Select {
            items,
            distinct,
            from,
            where_clause,
            group_by,
            having,
            order_by: &[],
            limit: None,
            offset: None,
            with: &[],
            set_body: None,
        })
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

    /// A full SELECT (core + its own ORDER BY / LIMIT / OFFSET) — used for
    /// subquery bodies, which carry their own trailing clauses.
    /// A subquery body: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET applying to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself; a genuine
    /// set-operation is carried in `set_body`.
    fn select(&mut self) -> Result<Select<'a>, ParseError> {
        let body = self.set_union()?;
        let (order_by, limit, offset) = self.order_limit()?;
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
            from: None,
            where_clause: None,
            group_by: &[],
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
    /// `WITH name AS (SELECT ...), ... <SELECT body>` (non-recursive).
    fn with_query(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("with")?;
        if self.eat_ident("recursive")? {
            return Err(self.err_here("WITH RECURSIVE is not supported"));
        }
        let placeholder: &'a Select<'a> = self
            .arena
            .alloc(Select {
                items: &[],
                distinct: false,
                from: None,
                where_clause: None,
                group_by: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with: &[],
                set_body: None,
            })
            .map_err(|_| self.err_here("statement too large for SQL arena"))?;
        let mut ctes = [Cte { name: "", query: placeholder }; MAX_CTES];
        let mut n = 0;
        loop {
            if n == MAX_CTES {
                return Err(self.limit("WITH list", MAX_CTES));
            }
            let name = self.any_ident("CTE name")?;
            self.expect_ident("as")?;
            self.expect_op("(")?;
            let q = self.select()?;
            self.expect_op(")")?;
            let boxed = self
                .arena
                .alloc(q)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            ctes[n] = Cte { name, query: boxed };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let ctes = self.arena_slice(&ctes[..n])?;
        // Body must be a plain SELECT (a set-operation body carrying WITH is
        // not supported yet).
        match self.query()? {
            Stmt::Select(mut sel) => {
                sel.with = ctes;
                Ok(Stmt::Select(sel))
            }
            _ => Err(self.err_here("WITH before a set operation is not supported")),
        }
    }

    fn query(&mut self) -> Result<Stmt<'a>, ParseError> {
        let body = self.set_union()?;
        let (order_by, limit, offset) = self.order_limit()?;
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            return Ok(Stmt::Select(sel));
        }
        Ok(Stmt::SetQuery(SetQuery { body, order_by, limit, offset }))
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
        // A parenthesized branch is itself a set-operation query.
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let inner = self.set_union()?;
            self.expect_op(")")?;
            return Ok(inner);
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
                    from: None,
                    where_clause: None,
                    group_by: &[],
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
            if is_reserved(word) {
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
            });
        }
        let first = self.any_ident("table name")?;
        let (schema, table) = if self.eat_op(".")? {
            (Some(first), self.any_ident("table name")?)
        } else {
            (None, first)
        };
        // Table function: `func(args) [AS] alias`. Only valid immediately after
        // the (possibly schema-qualified) name, before any alias.
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
        let alias = if self.eat_ident("as")? {
            Some(self.any_ident("alias")?)
        } else if let Tok::Ident(word) = self.peeked {
            if is_reserved(word) {
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
        Ok(TableRef { schema, table, alias, subquery: None, func_args, col_alias })
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
            columns[n] = self.any_ident("column alias")?;
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
            table: TableRef { schema: None, table: "", alias: None, subquery: None, func_args: None, col_alias: None },
            kind: JoinKind::Inner,
            on: None,
        };
        let mut joins = [dummy; 8];
        let mut n = 0;
        loop {
            // NATURAL joins require plan-time common-column resolution and a
            // merged `SELECT *`; not yet supported — reject clearly rather than
            // mis-parsing it as an ordinary join.
            if self.peeked == Tok::Ident("natural") {
                return Err(self.err_here("NATURAL JOIN is not supported; use JOIN ... ON/USING"));
            }
            let kind = if self.eat_op(",")? {
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
            let on = if kind == JoinKind::Cross {
                None
            } else if self.eat_ident("using")? {
                // JOIN ... USING (c, ...) is sugar for ON left.c = right.c AND …
                let left_q = if n == 0 {
                    base.alias.unwrap_or(base.table)
                } else {
                    let p = &joins[n - 1].table;
                    p.alias.unwrap_or(p.table)
                };
                let right_q = table.alias.unwrap_or(table.table);
                self.expect_op("(")?;
                let mut acc: Option<&'a Expr<'a>> = None;
                loop {
                    let col = self.any_ident("column name")?;
                    let l = self.arena_expr(Expr::Column { qualifier: Some(left_q), name: col })?;
                    let r = self.arena_expr(Expr::Column { qualifier: Some(right_q), name: col })?;
                    let eq = self.arena_expr(Expr::Binary { operator: BinaryOp::Eq, left: l, right: r })?;
                    acc = Some(match acc {
                        None => eq,
                        Some(prev) => {
                            self.arena_expr(Expr::Binary { operator: BinaryOp::And, left: prev, right: eq })?
                        }
                    });
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                acc
            } else {
                self.expect_ident("on")?;
                Some(self.expression(0)?)
            };
            joins[n] = Join { table, kind, on };
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
        let table = self.any_ident("table name")?;
        let action = if self.eat_ident("rename")? {
            if self.eat_ident("to")? {
                AlterAction::RenameTable(self.any_ident("new table name")?)
            } else {
                self.expect_ident("column")?;
                let from = self.any_ident("column name")?;
                self.expect_ident("to")?;
                let to = self.any_ident("new column name")?;
                AlterAction::RenameColumn { from, to }
            }
        } else if self.eat_ident("add")? {
            let _ = self.eat_ident("column")?;
            let name = self.any_ident("column name")?;
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
            AlterAction::DropColumn(self.any_ident("column name")?)
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

    /// Dispatches CREATE: `[OR REPLACE] VIEW` or `TABLE` ("create" consumed here).
    fn create(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("create")?;
        let or_replace = if self.eat_ident("or")? {
            self.expect_ident("replace")?;
            true
        } else {
            false
        };
        if or_replace {
            self.expect_ident("view")?;
            return self.create_view(true);
        }
        if self.eat_ident("unique")? {
            self.expect_ident("index")?;
            return self.create_index(true);
        }
        if self.eat_ident("view")? {
            return self.create_view(false);
        }
        if self.eat_ident("index")? {
            return self.create_index(false);
        }
        self.create_table()
    }

    /// CREATE [UNIQUE] INDEX name ON table (col, ...) ("create [unique] index"
    /// consumed).
    fn create_index(&mut self, unique: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("index name")?;
        self.expect_ident("on")?;
        let table = self.any_ident("table name")?;
        self.expect_op("(")?;
        let mut columns = [""; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("index columns", MAX_LIST));
            }
            columns[n] = self.any_ident("column name")?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let columns = self.arena_slice(&columns[..n])?;
        Ok(Stmt::CreateIndex { name, table, columns, unique })
    }

    /// CREATE VIEW name AS <select> ("create [or replace] view" consumed).
    fn create_view(&mut self, or_replace: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("view name")?;
        self.expect_ident("as")?;
        // Capture the raw SELECT text (re-parsed at query time).
        let start = self.peek_at;
        // Validate the body parses now, so a bad view errors at CREATE time.
        let _ = self.query()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::CreateView { name, or_replace, sql })
    }

    /// Dispatches DROP: `VIEW` or `TABLE` ("drop" consumed here).
    fn drop_stmt(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("drop")?;
        if self.eat_ident("view")? {
            let (name, if_exists) = self.drop_target("view name")?;
            return Ok(Stmt::DropView { name, if_exists });
        }
        if self.eat_ident("index")? {
            let (name, if_exists) = self.drop_target("index name")?;
            return Ok(Stmt::DropIndex { name, if_exists });
        }
        self.drop_table()
    }

    /// `[IF EXISTS] name` after a DROP keyword.
    fn drop_target(&mut self, what: &str) -> Result<(&'a str, bool), ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        Ok((self.any_ident(what)?, if_exists))
    }

    fn create_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("table")?;
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.any_ident("table name")?;
        self.expect_op("(")?;
        let mut columns = [ColumnDef { name: "", type_name: "", type_mod: -1, not_null: false, unique: false, primary: false, default: None }; MAX_LIST];
        let mut n = 0;
        let mut cons = [TableConstraint::Unique { name: None, columns: &[] }; MAX_LIST];
        let mut n_cons = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("column list", MAX_LIST));
            }
            // An optional CONSTRAINT <name> prefixes a table- or column-level
            // constraint; it names the following constraint.
            let cons_name = if self.eat_ident("constraint")? {
                Some(self.any_ident("constraint name")?)
            } else {
                None
            };
            // Table-level constraints: PRIMARY KEY / UNIQUE / CHECK / FOREIGN KEY.
            if matches!(
                self.peeked,
                Tok::Ident("primary") | Tok::Ident("unique") | Tok::Ident("check") | Tok::Ident("foreign")
            ) {
                let c = self.table_constraint(cons_name)?;
                if n_cons == MAX_LIST {
                    return Err(self.limit("constraint list", MAX_LIST));
                }
                cons[n_cons] = c;
                n_cons += 1;
                if !self.eat_op(",")? {
                    break;
                }
                continue;
            }
            if cons_name.is_some() {
                return Err(self.err_here("expected a table constraint after CONSTRAINT name"));
            }
            let col_name = self.any_ident("column name")?;
            let (type_name, type_mod) = self.type_name_mod()?;
            let mut not_null = false;
            let mut unique = false;
            let mut primary = false;
            let mut default = None;
            loop {
                // Column-level constraints may carry their own CONSTRAINT name.
                let col_cons_name = if self.eat_ident("constraint")? {
                    Some(self.any_ident("constraint name")?)
                } else {
                    None
                };
                if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    not_null = true;
                } else if self.eat_ident("null")? {
                    not_null = false;
                } else if self.eat_ident("default")? {
                    default = Some(self.expression(0)?);
                } else if self.eat_ident("unique")? {
                    unique = true;
                } else if self.eat_ident("primary")? {
                    self.expect_ident("key")?;
                    primary = true;
                    unique = true;
                    not_null = true;
                } else if self.eat_ident("check")? {
                    // Desugar a column CHECK to a table-level CHECK.
                    let c = self.check_constraint(col_cons_name)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    continue;
                } else if self.eat_ident("references")? {
                    // Desugar a column REFERENCES to a single-column FK.
                    let child = self.arena_slice(&[col_name])?;
                    let c = self.references_tail(col_cons_name, child)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    continue;
                } else if col_cons_name.is_some() {
                    return Err(self.err_here("expected a column constraint after CONSTRAINT name"));
                } else {
                    break;
                }
            }
            columns[n] = ColumnDef { name: col_name, type_name, type_mod, not_null, unique, primary, default };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let columns = self.arena_slice(&columns[..n])?;
        let constraints = self.arena_slice(&cons[..n_cons])?;
        Ok(Stmt::CreateTable(CreateTable { name, columns, constraints, if_not_exists }))
    }

    /// Parses a parenthesized, comma-separated column-name list.
    fn column_name_list(&mut self) -> Result<&'a [&'a str], ParseError> {
        self.expect_op("(")?;
        let mut columns: [&'a str; MAX_INDEX_COLS] = [""; MAX_INDEX_COLS];
        let mut k = 0;
        loop {
            if k == MAX_INDEX_COLS {
                return Err(self.limit("constraint column list", MAX_INDEX_COLS));
            }
            columns[k] = self.any_ident("column name")?;
            k += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        self.arena_slice(&columns[..k])
    }

    /// A table-level PRIMARY KEY / UNIQUE / CHECK / FOREIGN KEY constraint.
    fn table_constraint(&mut self, name: Option<&'a str>) -> Result<TableConstraint<'a>, ParseError> {
        if self.eat_ident("primary")? {
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            Ok(TableConstraint::PrimaryKey { name, columns })
        } else if self.eat_ident("unique")? {
            let columns = self.column_name_list()?;
            Ok(TableConstraint::Unique { name, columns })
        } else if self.eat_ident("check")? {
            self.check_constraint(name)
        } else {
            self.expect_ident("foreign")?;
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            self.expect_ident("references")?;
            self.references_tail(name, columns)
        }
    }

    /// A CHECK (predicate): captures the predicate's source text for durable
    /// storage alongside the parsed expression.
    fn check_constraint(&mut self, name: Option<&'a str>) -> Result<TableConstraint<'a>, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let expression = self.expression(0)?;
        let text = self.text[start..self.peek_at].trim_end();
        let text = self.arena_str(text)?;
        self.expect_op(")")?;
        Ok(TableConstraint::Check { name, expression, text })
    }

    /// The part of a FOREIGN KEY after `REFERENCES`: parent table, optional
    /// parent columns, and ON DELETE / ON UPDATE actions.
    fn references_tail(
        &mut self,
        name: Option<&'a str>,
        columns: &'a [&'a str],
    ) -> Result<TableConstraint<'a>, ParseError> {
        let parent = self.any_ident("referenced table")?;
        let parent_cols = if self.peeked == Tok::Op("(") {
            self.column_name_list()?
        } else {
            &[]
        };
        let mut on_delete = FkAction::NoAction;
        let mut on_update = FkAction::NoAction;
        while self.eat_ident("on")? {
            let is_delete = if self.eat_ident("delete")? {
                true
            } else {
                self.expect_ident("update")?;
                false
            };
            let action = self.fk_action()?;
            if is_delete {
                on_delete = action;
            } else {
                on_update = action;
            }
        }
        Ok(TableConstraint::ForeignKey {
            name,
            columns,
            parent,
            parent_cols,
            on_delete,
            on_update,
        })
    }

    fn fk_action(&mut self) -> Result<FkAction, ParseError> {
        if self.eat_ident("no")? {
            self.expect_ident("action")?;
            Ok(FkAction::NoAction)
        } else if self.eat_ident("restrict")? {
            Ok(FkAction::Restrict)
        } else if self.eat_ident("cascade")? {
            Ok(FkAction::Cascade)
        } else if self.eat_ident("set")? {
            if self.eat_ident("null")? {
                Ok(FkAction::SetNull)
            } else {
                self.expect_ident("default")?;
                Ok(FkAction::SetDefault)
            }
        } else {
            Err(self.err_here("expected NO ACTION, RESTRICT, CASCADE, SET NULL, or SET DEFAULT"))
        }
    }

    fn drop_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("table")?;
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.any_ident("table name")?;
        Ok(Stmt::DropTable(DropTable { name, if_exists }))
    }

    fn insert(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("insert")?;
        self.expect_ident("into")?;
        let table = self.any_ident("table name")?;
        let mut column_names: [&'a str; MAX_LIST] = [""; MAX_LIST];
        let mut n_cols = 0;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                if n_cols == MAX_LIST {
                    return Err(self.limit("column list", MAX_LIST));
                }
                column_names[n_cols] = self.any_ident("column name")?;
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
                target[nt] = self.any_ident("column name")?;
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
                let col = self.any_ident("column name")?;
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
        let table = self.any_ident("table name")?;
        self.expect_ident("set")?;
        let dummy: (&'a str, &'a Expr<'a>) = ("", &Expr::Null);
        let mut assignments = [dummy; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("SET list", MAX_LIST));
            }
            let col = self.any_ident("column name")?;
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
        let table = self.any_ident("table name")?;
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

    /// Pratt expression parser.
    fn expression(&mut self, min_prec: u8) -> Result<&'a Expr<'a>, ParseError> {
        let mut left = self.prefix()?;
        loop {
            // Postfix `::type` binds tightest.
            if self.peeked == Tok::Op("::") {
                self.advance()?;
                let (type_name, type_mod) = self.type_name_mod()?;
                left = self.arena_expr(Expr::Cast { operand: left, type_name, type_mod })?;
                continue;
            }
            // `IS [NOT] NULL/TRUE/FALSE/UNKNOWN/DISTINCT FROM` binds looser than
            // arithmetic and comparison (like PostgreSQL), so it applies only at
            // the comparison precedence level and below — `1 - 2 IS NOT NULL` is
            // `(1 - 2) IS NOT NULL`. The boolean tests and DISTINCT FROM desugar
            // to `CASE`/`IS NULL` so they need no dedicated AST node.
            if min_prec <= 4 && self.peeked == Tok::Ident("is") {
                self.advance()?;
                let negated = self.eat_ident("not")?;
                if self.eat_ident("null")? || self.eat_ident("unknown")? {
                    left = self.arena_expr(Expr::IsNull { operand: left, negated })?;
                    continue;
                }
                let is_true = self.eat_ident("true")?;
                if is_true || self.eat_ident("false")? {
                    // `x IS TRUE` -> CASE WHEN x THEN true ELSE false; IS FALSE
                    // tests `NOT x`; the NOT/negated flags flip the arms.
                    let cond = if is_true {
                        left
                    } else {
                        self.arena_expr(Expr::Unary { operator: UnaryOp::Not, operand: left })?
                    };
                    let then_v = self.arena_expr(Expr::Bool(!negated))?;
                    let else_v = self.arena_expr(Expr::Bool(negated))?;
                    let whens = self.arena_slice(&[(cond, then_v)])?;
                    left = self.arena_expr(Expr::Case { operand: None, whens, otherwise: Some(else_v) })?;
                    continue;
                }
                if self.eat_ident("distinct")? {
                    self.expect_ident("from")?;
                    let right = self.expression(5)?;
                    left = self.build_distinct_from(left, right, negated)?;
                    continue;
                }
                return Err(self.err_here("expected NULL, TRUE, FALSE, UNKNOWN, or DISTINCT after IS"));
            }
            // Array subscript `base[index]` (1-based).
            if self.peeked == Tok::Op("[") {
                self.advance()?;
                let index = self.expression(0)?;
                self.expect_op("]")?;
                left = self.arena_expr(Expr::Subscript { base: left, index })?;
                continue;
            }
            // `expression COLLATE collation`: we implement a single (default)
            // collation, so the clause is accepted and has no effect.
            if self.peeked == Tok::Ident("collate") {
                self.advance()?;
                // Skip an optional schema-qualified collation name.
                let _ = self.any_ident("collation name")?;
                if self.eat_op(".")? {
                    let _ = self.any_ident("collation name")?;
                }
                continue;
            }
            // `left OPERATOR([schema.]operator) right`: the explicit-operator syntax
            // psql uses (e.g. `OPERATOR(pg_catalog.~)`), at comparison
            // precedence.
            if min_prec <= 4 && self.peeked == Tok::Ident("operator") {
                self.advance()?;
                self.expect_op("(")?;
                let mut operator = self.any_op_token()?;
                if self.eat_op(".")? {
                    operator = self.any_op_token()?;
                }
                self.expect_op(")")?;
                let right = self.expression(5)?;
                left = self.build_operator(operator, left, right)?;
                continue;
            }
            // IN / BETWEEN / LIKE / ILIKE, optionally NOT-prefixed. They
            // bind like comparisons (precedence 4).
            if min_prec <= 4 {
                let negated = if self.peeked == Tok::Ident("not") {
                    self.advance()?;
                    // Infix NOT must introduce one of these forms.
                    if !matches!(
                        self.peeked,
                        Tok::Ident("in") | Tok::Ident("between") | Tok::Ident("like")
                            | Tok::Ident("ilike")
                    ) {
                        return Err(self.unexpected("expected IN, BETWEEN or LIKE after NOT"));
                    }
                    true
                } else {
                    false
                };
                if self.eat_ident("in")? {
                    self.expect_op("(")?;
                    if self.peeked == Tok::Ident("select") {
                        let select = self.select()?;
                        self.expect_op(")")?;
                        let boxed = self
                            .arena
                            .alloc(select)
                            .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                        left = self.arena_expr(Expr::InSubquery {
                            operand: left,
                            select: boxed,
                            negated,
                        })?;
                        continue;
                    }
                    let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
                    let mut list: [&'a Expr<'a>; MAX_LIST] = [null_expr; MAX_LIST];
                    let mut n = 0;
                    loop {
                        if n == MAX_LIST {
                            return Err(self.limit("IN list", MAX_LIST));
                        }
                        list[n] = self.expression(0)?;
                        n += 1;
                        if !self.eat_op(",")? {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    left = self.arena_expr(Expr::InList {
                        operand: left,
                        list: self.arena_slice(&list[..n])?,
                        negated,
                    })?;
                    continue;
                }
                if self.eat_ident("between")? {
                    // Operands bind tighter than AND here.
                    let low = self.expression(5)?;
                    self.expect_ident("and")?;
                    let high = self.expression(5)?;
                    left = self.arena_expr(Expr::Between { operand: left, low, high, negated })?;
                    continue;
                }
                let ilike = self.peeked == Tok::Ident("ilike");
                if ilike || self.peeked == Tok::Ident("like") {
                    self.advance()?;
                    let pattern = self.expression(5)?;
                    left = self.arena_expr(Expr::Like {
                        operand: left,
                        pattern,
                        negated,
                        case_insensitive: ilike,
                    })?;
                    continue;
                }
                if negated {
                    return Err(self.unexpected("expected IN, BETWEEN or LIKE after NOT"));
                }
            }
            // POSIX regex match operators bind like comparisons.
            if min_prec <= 4
                && let Tok::Op(o @ ("~" | "!~" | "~*" | "!~*")) = self.peeked
            {
                self.advance()?;
                let pattern = self.expression(5)?;
                left = self.arena_expr(Expr::Match {
                    operand: left,
                    pattern,
                    negated: o.starts_with('!'),
                    case_insensitive: o.ends_with('*'),
                })?;
                continue;
            }
            let Some(operator) = self.peek_binary_op() else {
                return Ok(left);
            };
            if operator.precedence() < min_prec {
                return Ok(left);
            }
            self.advance()?;
            // Quantified comparison: `operand operator ANY/ALL (array)`.
            if matches!(self.peeked, Tok::Ident("any") | Tok::Ident("all") | Tok::Ident("some")) {
                let all = self.peeked == Tok::Ident("all");
                self.advance()?;
                self.expect_op("(")?;
                let array = self.expression(0)?;
                self.expect_op(")")?;
                left = self.arena_expr(Expr::AnyAll { operand: left, operator, array, all })?;
                continue;
            }
            let right = self.expression(operator.precedence() + 1)?;
            left = self.arena_expr(Expr::Binary { operator, left, right })?;
        }
    }

    fn prefix(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let tok = self.peeked;
        match tok {
            Tok::Num(text) => {
                self.advance()?;
                // Base-prefixed literals (0x/0o/0b) are always integers; a plain
                // token is integral unless it has a decimal point or exponent.
                let prefixed = is_base_prefixed(text);
                let looks_integral = prefixed || !text.contains(['.', 'e', 'E']);
                if looks_integral
                    && let Some(v) = super::eval::parse_int_literal(text) {
                        return self.arena_expr(Expr::Int(v));
                    }
                // Decimal / exponent literals are NUMERIC in PostgreSQL; keep
                // the text and parse it exactly at eval time.
                self.arena_expr(Expr::NumericLit(text))
            }
            Tok::Str(s) => {
                self.advance()?;
                self.arena_expr(Expr::Str(s))
            }
            Tok::Param(n) => {
                self.advance()?;
                self.max_param = self.max_param.max(n);
                self.arena_expr(Expr::Param(n))
            }
            Tok::Op("(") => {
                self.advance()?;
                if self.peeked == Tok::Ident("select") {
                    let select = self.select()?;
                    self.expect_op(")")?;
                    let boxed = self
                        .arena
                        .alloc(select)
                        .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                    return self.arena_expr(Expr::Subquery(boxed));
                }
                let inner = self.expression(0)?;
                self.expect_op(")")?;
                // `(expression).field` composite field access (chained).
                let mut base = inner;
                while self.peeked == Tok::Op(".") {
                    self.advance()?;
                    let field = self.any_ident("field name")?;
                    base = self.arena_expr(Expr::Field { base, field })?;
                }
                Ok(base)
            }
            Tok::Op("~") => {
                self.advance()?;
                let operand = self.expression(8)?;
                self.arena_expr(Expr::Unary { operator: UnaryOp::BitNot, operand })
            }
            Tok::Op("-") => {
                self.advance()?;
                // Fold a leading minus into an integer literal, as PostgreSQL
                // does, so the negated value's magnitude picks the type:
                // `-2147483648` is int4 (INT4_MIN fits), not int8. Decimal
                // literals negate through the normal Neg path (numeric).
                if let Tok::Num(text) = self.peeked {
                    let integral = is_base_prefixed(text) || !text.contains(['.', 'e', 'E']);
                    if integral
                        && let Some(v) = super::eval::parse_int_literal(text) {
                            self.advance()?;
                            return self.arena_expr(Expr::Int(-v));
                        }
                }
                let operand = self.expression(8)?;
                self.arena_expr(Expr::Unary { operator: UnaryOp::Neg, operand })
            }
            Tok::Op("+") => {
                self.advance()?;
                self.expression(8)
            }
            Tok::Ident("not") => {
                self.advance()?;
                let operand = self.expression(3)?;
                self.arena_expr(Expr::Unary { operator: UnaryOp::Not, operand })
            }
            Tok::Ident("null") => {
                self.advance()?;
                self.arena_expr(Expr::Null)
            }
            // EXTRACT(field FROM source) — SQL-standard spelling of the
            // date_part function, kept distinct so it can return numeric.
            Tok::Ident("extract") => {
                self.advance()?;
                self.expect_op("(")?;
                let field = self.any_ident("extract field")?;
                self.expect_ident("from")?;
                let source = self.expression(0)?;
                self.expect_op(")")?;
                let field_lit = self.arena_expr(Expr::Str(field))?;
                self.arena_expr(Expr::Call {
                    name: "extract",
                    args: self.arena_slice(&[field_lit, source])?,
                    star: false,
                    distinct: false,
                    filter: None,
                    order_by: &[],
                    over: None,
                })
            }
            Tok::Ident("exists") => {
                self.advance()?;
                self.expect_op("(")?;
                let select = self.select()?;
                self.expect_op(")")?;
                let boxed = self
                    .arena
                    .alloc(select)
                    .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                self.arena_expr(Expr::Exists(boxed))
            }
            Tok::Ident("default") => {
                self.advance()?;
                self.arena_expr(Expr::DefaultMarker)
            }
            Tok::Ident("case") => {
                self.advance()?;
                let operand = if self.peeked == Tok::Ident("when") {
                    None
                } else {
                    Some(self.expression(0)?)
                };
                let dummy: (&'a Expr<'a>, &'a Expr<'a>) =
                    (self.arena_expr(Expr::Null)?, self.arena_expr(Expr::Null)?);
                let mut whens: [(&'a Expr<'a>, &'a Expr<'a>); MAX_LIST] = [dummy; MAX_LIST];
                let mut n = 0;
                while self.eat_ident("when")? {
                    if n == MAX_LIST {
                        return Err(self.limit("CASE branches", MAX_LIST));
                    }
                    let cond = self.expression(0)?;
                    self.expect_ident("then")?;
                    let result = self.expression(0)?;
                    whens[n] = (cond, result);
                    n += 1;
                }
                if n == 0 {
                    return Err(self.unexpected("CASE requires at least one WHEN"));
                }
                let otherwise = if self.eat_ident("else")? {
                    Some(self.expression(0)?)
                } else {
                    None
                };
                self.expect_ident("end")?;
                self.arena_expr(Expr::Case {
                    operand,
                    whens: self.arena_slice(&whens[..n])?,
                    otherwise,
                })
            }
            Tok::Ident("true") => {
                self.advance()?;
                self.arena_expr(Expr::Bool(true))
            }
            Tok::Ident("false") => {
                self.advance()?;
                self.arena_expr(Expr::Bool(false))
            }
            // `left`/`right` are JOIN keywords but also functions; in expression
            // position, followed by `(`, they are function calls.
            Tok::Ident(kw @ ("left" | "right")) => {
                self.advance()?;
                if self.peeked == Tok::Op("(") {
                    self.call(kw)
                } else {
                    Err(self.unexpected("expected an expression"))
                }
            }
            // SQL-standard `CAST(expression AS type[(mod)])`, equivalent to `expression::type`.
            Tok::Ident("cast") => {
                self.advance()?;
                self.expect_op("(")?;
                let operand = self.expression(0)?;
                self.expect_ident("as")?;
                let (type_name, type_mod) = self.type_name_mod()?;
                self.expect_op(")")?;
                self.arena_expr(Expr::Cast { operand, type_name, type_mod })
            }
            Tok::Ident(name) => {
                if is_reserved(name) {
                    return Err(self.unexpected("expected an expression"));
                }
                self.advance()?;
                // Qualified name: the pg_catalog / information_schema schemas are
                // transparent (pg_catalog.version() == version()), so a leading
                // recognized schema qualifier is dropped.
                let name = if (name == "pg_catalog" || name == "information_schema")
                    && self.peeked == Tok::Op(".")
                {
                    self.advance()?;
                    self.any_ident("function or column name")?
                } else {
                    name
                };
                // `ARRAY[...]` array constructor.
                if name.eq_ignore_ascii_case("array") && self.peeked == Tok::Op("[") {
                    self.advance()?;
                    let mut items: [&'a Expr<'a>; MAX_LIST] = [self.arena_expr(Expr::Null)?; MAX_LIST];
                    let mut n = 0;
                    if self.peeked != Tok::Op("]") {
                        loop {
                            if n == MAX_LIST {
                                return Err(self.limit("array elements", MAX_LIST));
                            }
                            items[n] = self.expression(0)?;
                            n += 1;
                            if !self.eat_op(",")? {
                                break;
                            }
                        }
                    }
                    self.expect_op("]")?;
                    let items = self.arena_slice(&items[..n])?;
                    return self.arena_expr(Expr::Array(items));
                }
                // `ARRAY(SELECT ...)` array-from-subquery constructor.
                if name.eq_ignore_ascii_case("array") && self.peeked == Tok::Op("(") {
                    self.advance()?;
                    let select = self.select()?;
                    self.expect_op(")")?;
                    let boxed = self
                        .arena
                        .alloc(select)
                        .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                    return self.arena_expr(Expr::ArraySubquery(boxed));
                }
                if self.peeked == Tok::Op("(") {
                    return self.call(name);
                }
                // Typed literal, SQL-standard: `DATE '2020-01-01'` is
                // exactly `'2020-01-01'::date`. Only fires when the name is a
                // known type immediately followed by a string.
                if let Tok::Str(lit) = self.peeked
                    && super::types::ColType::from_sql_name(name).is_some() {
                        self.advance()?;
                        let operand = self.arena_expr(Expr::Str(lit))?;
                        return self.arena_expr(Expr::Cast { operand, type_name: name, type_mod: -1 });
                    }
                if self.peeked == Tok::Op(".") {
                    self.advance()?;
                    let column = self.any_ident("column name")?;
                    return self.arena_expr(Expr::Column {
                        qualifier: Some(name),
                        name: column,
                    });
                }
                // SQL-standard paren-less functions.
                if matches!(
                    name,
                    "current_date" | "current_timestamp" | "current_time" | "localtimestamp"
                        | "current_user" | "session_user" | "current_catalog"
                        | "current_schema"
                ) {
                    return self.arena_expr(Expr::Call {
                        name,
                        args: &[],
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    });
                }
                self.arena_expr(Expr::Column { qualifier: None, name })
            }
            Tok::QuotedIdent(name) => {
                self.advance()?;
                if self.peeked == Tok::Op("(") {
                    return self.call(name);
                }
                if self.peeked == Tok::Op(".") {
                    self.advance()?;
                    let column = self.any_ident("column name")?;
                    return self.arena_expr(Expr::Column {
                        qualifier: Some(name),
                        name: column,
                    });
                }
                self.arena_expr(Expr::Column { qualifier: None, name })
            }
            _ => Err(self.unexpected("expected an expression")),
        }
    }

    /// Builds a simple function call `name(args)` (no star/distinct/over).
    fn plain_call(&mut self, name: &'a str, args: &[&'a Expr<'a>]) -> Result<&'a Expr<'a>, ParseError> {
        let args = self.arena_slice(args)?;
        self.arena_expr(Expr::Call { name, args, star: false, distinct: false, order_by: &[], over: None, filter: None })
    }

    fn call(&mut self, name: &'a str) -> Result<&'a Expr<'a>, ParseError> {
        self.expect_op("(")?;
        // SQL-standard `substring(str FROM start [FOR len])` and
        // `trim([both|leading|trailing] [chars] FROM str)` desugar to the plain
        // function forms.
        if name.eq_ignore_ascii_case("substring") {
            let target = self.expression(0)?;
            if self.eat_ident("from")? {
                let start = self.expression(0)?;
                let mut cargs = [target, start, target];
                let mut cn = 2;
                if self.eat_ident("for")? {
                    cargs[2] = self.expression(0)?;
                    cn = 3;
                }
                self.expect_op(")")?;
                return self.plain_call("substr", &cargs[..cn]);
            }
            // Comma form `substring(str, start[, len])`.
            let mut cargs = [target, target, target];
            let mut cn = 1;
            while self.eat_op(",")? {
                cargs[cn] = self.expression(0)?;
                cn += 1;
            }
            self.expect_op(")")?;
            return self.plain_call("substr", &cargs[..cn]);
        }
        if name.eq_ignore_ascii_case("trim") {
            let dir = if self.eat_ident("both")? {
                "btrim"
            } else if self.eat_ident("leading")? {
                "ltrim"
            } else if self.eat_ident("trailing")? {
                "rtrim"
            } else {
                "btrim"
            };
            // Optional characters expression, then `FROM str` — or just `str`.
            let first = self.expression(0)?;
            if self.eat_ident("from")? {
                let target = self.expression(0)?;
                self.expect_op(")")?;
                return self.plain_call(dir, &[target, first]);
            }
            self.expect_op(")")?;
            return self.plain_call(dir, &[first]);
        }
        if name.eq_ignore_ascii_case("position") {
            // SQL-standard `position(substr IN string)` -> strpos(string, substr)
            // (strpos takes the haystack first, the needle second). Parse the
            // needle above IN's precedence (4) so the IN keyword is not consumed
            // as an `x IN (...)` operator.
            let needle = self.expression(5)?;
            if self.eat_ident("in")? {
                let haystack = self.expression(0)?;
                self.expect_op(")")?;
                return self.plain_call("strpos", &[haystack, needle]);
            }
            let mut cargs = [needle, needle];
            let mut cn = 1;
            while self.eat_op(",")? {
                if cn < cargs.len() {
                    cargs[cn] = self.expression(0)?;
                }
                cn += 1;
            }
            self.expect_op(")")?;
            return self.plain_call(name, &cargs[..cn.min(cargs.len())]);
        }
        if name.eq_ignore_ascii_case("overlay") {
            // SQL-standard `overlay(str PLACING sub FROM start [FOR len])`.
            let target = self.expression(0)?;
            self.expect_ident("placing")?;
            let sub = self.expression(0)?;
            self.expect_ident("from")?;
            let start = self.expression(0)?;
            let mut cargs = [target, sub, start, start];
            let mut cn = 3;
            if self.eat_ident("for")? {
                cargs[3] = self.expression(0)?;
                cn = 4;
            }
            self.expect_op(")")?;
            return self.plain_call("overlay", &cargs[..cn]);
        }
        if self.peeked == Tok::Op("*") {
            self.advance()?;
            self.expect_op(")")?;
            let filter = self.parse_filter()?;
            let over = self.parse_over()?;
            return self.arena_expr(Expr::Call {
                name,
                args: &[],
                star: true,
                distinct: false,
                order_by: &[],
                over,
                filter,
            });
        }
        // `agg(DISTINCT expression)` — deduplicate argument values before aggregating.
        let distinct = if self.peeked == Tok::Ident("distinct") {
            self.advance()?;
            true
        } else {
            false
        };
        let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
        let mut args: [&'a Expr<'a>; MAX_LIST] = [null_expr; MAX_LIST];
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
        // Optional aggregate `ORDER BY` (e.g. `string_agg(x, ',' ORDER BY y)`).
        let order_by = if self.peeked == Tok::Ident("order") {
            self.advance()?;
            self.expect_ident("by")?;
            self.order_by_items()?
        } else {
            &[]
        };
        self.expect_op(")")?;
        let filter = self.parse_filter()?;
        let over = self.parse_over()?;
        let args = self.arena_slice(&args[..n])?;
        self.arena_expr(Expr::Call { name, args, star: false, distinct, order_by, over, filter })
    }

    /// Parses an optional aggregate `FILTER (WHERE cond)` clause.
    fn parse_filter(&mut self) -> Result<Option<&'a Expr<'a>>, ParseError> {
        if self.peeked != Tok::Ident("filter") {
            return Ok(None);
        }
        self.advance()?;
        self.expect_op("(")?;
        self.expect_ident("where")?;
        let cond = self.expression(0)?;
        self.expect_op(")")?;
        Ok(Some(cond))
    }

    /// Parses a comma-separated ORDER BY item list (the `ORDER BY` keyword
    /// already consumed).
    fn order_by_items(&mut self) -> Result<&'a [OrderBy<'a>], ParseError> {
        let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
        let mut ord = [OrderBy { expression: null_expr, descending: false, nulls_first: false }; MAX_LIST];
        let mut m = 0;
        loop {
            if m == MAX_LIST {
                return Err(self.limit("ORDER BY", MAX_LIST));
            }
            let expression = self.expression(0)?;
            let descending = if self.eat_ident("desc")? {
                true
            } else {
                self.eat_ident("asc")?;
                false
            };
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
            ord[m] = OrderBy { expression, descending, nulls_first };
            m += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&ord[..m])
    }

    /// Parses an optional `OVER (PARTITION BY ... ORDER BY ...)` window clause.
    /// An explicit frame (ROWS/RANGE ...) is rejected — only the default frame
    /// is supported.
    fn parse_over(&mut self) -> Result<Option<&'a WindowSpec<'a>>, ParseError> {
        if !self.eat_ident("over")? {
            return Ok(None);
        }
        self.expect_op("(")?;
        let partition_by = if self.eat_ident("partition")? {
            self.expect_ident("by")?;
            let mut parts: [&'a Expr<'a>; MAX_LIST] = [self.arena_expr(Expr::Null)?; MAX_LIST];
            let mut n = 0;
            loop {
                if n == MAX_LIST {
                    return Err(self.limit("PARTITION BY", MAX_LIST));
                }
                parts[n] = self.expression(0)?;
                n += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.arena_slice(&parts[..n])?
        } else {
            &[]
        };
        let order_by = if self.eat_ident("order")? {
            self.expect_ident("by")?;
            self.order_by_items()?
        } else {
            &[]
        };
        if matches!(self.peeked, Tok::Ident("rows") | Tok::Ident("range") | Tok::Ident("groups")) {
            return Err(self.err_here("explicit window frames (ROWS/RANGE) are not supported"));
        }
        self.expect_op(")")?;
        let spec = WindowSpec { partition_by, order_by };
        Ok(Some(
            self.arena.alloc(spec).map_err(|_| self.err_here("window spec too large for arena"))?,
        ))
    }

    /// Consumes an operator or identifier token, returning its text (used
    /// inside `OPERATOR(...)`, where a schema name and the symbol both appear).
    fn any_op_token(&mut self) -> Result<&'a str, ParseError> {
        let t = self.peeked;
        match t {
            Tok::Op(o) => {
                self.advance()?;
                Ok(o)
            }
            Tok::Ident(s) => {
                self.advance()?;
                Ok(s)
            }
            _ => Err(self.unexpected("an operator name")),
        }
    }

    /// Builds an expression for an explicit `OPERATOR(operator)` symbol.
    fn build_operator(
        &self,
        operator: &str,
        left: &'a Expr<'a>,
        right: &'a Expr<'a>,
    ) -> Result<&'a Expr<'a>, ParseError> {
        if matches!(operator, "~" | "!~" | "~*" | "!~*") {
            return self.arena_expr(Expr::Match {
                operand: left,
                pattern: right,
                negated: operator.starts_with('!'),
                case_insensitive: operator.ends_with('*'),
            });
        }
        let bop = match operator {
            "=" => BinaryOp::Eq,
            "<>" | "!=" => BinaryOp::NotEq,
            "<" => BinaryOp::Lt,
            "<=" => BinaryOp::LtEq,
            ">" => BinaryOp::Gt,
            ">=" => BinaryOp::GtEq,
            "+" => BinaryOp::Add,
            "-" => BinaryOp::Sub,
            "*" => BinaryOp::Mul,
            "/" => BinaryOp::Div,
            "%" => BinaryOp::Mod,
            "||" => BinaryOp::Concat,
            _ => return Err(self.err_here("unsupported operator in OPERATOR()")),
        };
        self.arena_expr(Expr::Binary { operator: bop, left, right })
    }

    /// Desugars `left IS [NOT] DISTINCT FROM right` into a null-safe `CASE`:
    /// both null → equal, one null → distinct, else the plain comparison.
    fn build_distinct_from(
        &self,
        left: &'a Expr<'a>,
        right: &'a Expr<'a>,
        negated: bool,
    ) -> Result<&'a Expr<'a>, ParseError> {
        let l_null = self.arena_expr(Expr::IsNull { operand: left, negated: false })?;
        let r_null = self.arena_expr(Expr::IsNull { operand: right, negated: false })?;
        let both = self.arena_expr(Expr::Binary { operator: BinaryOp::And, left: l_null, right: r_null })?;
        let either = self.arena_expr(Expr::Binary { operator: BinaryOp::Or, left: l_null, right: r_null })?;
        let cmp_op = if negated { BinaryOp::Eq } else { BinaryOp::NotEq };
        let cmp = self.arena_expr(Expr::Binary { operator: cmp_op, left, right })?;
        let both_val = self.arena_expr(Expr::Bool(negated))?;
        let either_val = self.arena_expr(Expr::Bool(!negated))?;
        let whens = self.arena_slice(&[(both, both_val), (either, either_val)])?;
        self.arena_expr(Expr::Case { operand: None, whens, otherwise: Some(cmp) })
    }

    fn peek_binary_op(&self) -> Option<BinaryOp> {
        match self.peeked {
            Tok::Op("+") => Some(BinaryOp::Add),
            Tok::Op("-") => Some(BinaryOp::Sub),
            Tok::Op("*") => Some(BinaryOp::Mul),
            Tok::Op("/") => Some(BinaryOp::Div),
            Tok::Op("%") => Some(BinaryOp::Mod),
            Tok::Op("=") => Some(BinaryOp::Eq),
            Tok::Op("<>") | Tok::Op("!=") => Some(BinaryOp::NotEq),
            Tok::Op("<") => Some(BinaryOp::Lt),
            Tok::Op("<=") => Some(BinaryOp::LtEq),
            Tok::Op(">") => Some(BinaryOp::Gt),
            Tok::Op(">=") => Some(BinaryOp::GtEq),
            Tok::Op("||") => Some(BinaryOp::Concat),
            Tok::Op("->") => Some(BinaryOp::JsonGet),
            Tok::Op("->>") => Some(BinaryOp::JsonGetText),
            Tok::Op("&") => Some(BinaryOp::BitAnd),
            Tok::Op("|") => Some(BinaryOp::BitOr),
            Tok::Op("#") => Some(BinaryOp::BitXor),
            Tok::Op("<<") => Some(BinaryOp::Shl),
            Tok::Op(">>") => Some(BinaryOp::Shr),
            Tok::Op("^") => Some(BinaryOp::Pow),
            Tok::Op("@>") => Some(BinaryOp::Contains),
            Tok::Op("<@") => Some(BinaryOp::ContainedBy),
            Tok::Op("&&") => Some(BinaryOp::Overlaps),
            Tok::Op("&<") => Some(BinaryOp::NotRightOf),
            Tok::Op("&>") => Some(BinaryOp::NotLeftOf),
            Tok::Op("-|-") => Some(BinaryOp::Adjacent),
            Tok::Ident("and") => Some(BinaryOp::And),
            Tok::Ident("or") => Some(BinaryOp::Or),
            _ => None,
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

    /// A type name in a position where a modifier is not yet honored (casts,
    /// prepared-parameter types): a modifier is rejected loudly rather than
    /// quietly dropped.
    fn type_name(&mut self) -> Result<&'a str, ParseError> {
        let (name, type_mod) = self.type_name_mod()?;
        if type_mod != -1 {
            return Err(self.unexpected("type modifier is not supported in this position yet"));
        }
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
                Ok(nums[0] as i32 + 4)
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
                Ok((((p as i32) << 16) | (s as i32)) + 4)
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
                    | "intersect" | "except"
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
            });
        }
        Ok(())
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
            }),
        }
    }

    fn arena_expr(&self, e: Expr<'a>) -> Result<&'a Expr<'a>, ParseError> {
        self.arena
            .alloc(e)
            .map(|m| &*m)
            .map_err(|_| self.err_here("statement too large for SQL arena"))
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
        }
    }

    fn err_here(&self, message: &'static str) -> ParseError {
        ParseError::new(self.peek_at, message)
    }

    fn limit(&self, what: &'static str, max: usize) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(96, "{} exceeds fixed limit of {}", what, max),
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
                assert_eq!(c.name, "t");
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
