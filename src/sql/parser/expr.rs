//! Expression parsing: precedence climbing over the operators, and the prefix
//! forms every expression starts from.
//!
//! `expression` takes a minimum precedence and consumes operators at or above
//! it, so each level's associativity falls out of what it recurses with;
//! `prefix` handles everything an expression can begin with — a literal, a
//! column, a parenthesized expression or row, a CASE, a subquery, an array —
//! and the postfix forms (subscripts, field access, casts) that bind to it.

use crate::sql::eval::sqlstate;
use crate::sql::lexer::Tok;

use super::{is_base_prefixed, is_reserved_keyword, ParseError, Parser, MAX_LIST};
use crate::sql::ast::{BinaryOp, Expr, UnaryOp};

impl<'a> Parser<'a> {
    /// The optional `ESCAPE c` trailing a LIKE or SIMILAR TO pattern.
    fn escape_clause(&mut self) -> Result<Option<&'a Expr<'a>>, ParseError> {
        if self.eat_ident("escape")? {
            return Ok(Some(self.expression(5)?));
        }
        Ok(None)
    }

    /// Pratt expression parser.
    pub(super) fn expression(&mut self, min_prec: u8) -> Result<&'a Expr<'a>, ParseError> {
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
                    left = self.arena_expr(Expr::Case { operand: None, whens, otherwise: Some(else_v), synthetic: true })?;
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
            // `expression AT TIME ZONE zone` — desugar to the equivalent
            // `timezone(zone, expression)` function. The zone binds tightly
            // (parsed above binary-operator precedence).
            if self.peeked == Tok::Ident("at") {
                self.advance()?;
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                let zone = self.expression(8)?;
                left = self.arena_expr(Expr::Call {
                    name: "timezone",
                    args: self.arena_slice(&[zone, left])?,
                    star: false,
                    distinct: false,
                    order_by: &[],
                    over: None,
                    filter: None,
                })?;
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
                            | Tok::Ident("ilike") | Tok::Ident("similar")
                    ) {
                        return Err(self.unexpected("expected IN, BETWEEN, LIKE or SIMILAR TO after NOT"));
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
                    let left_operand = left;
                    // ASYMMETRIC is the default and says so explicitly.
                    self.eat_ident("asymmetric")?;
                    let symmetric = self.eat_ident("symmetric")?;
                    // Operands bind tighter than AND here.
                    let low = self.expression(5)?;
                    self.expect_ident("and")?;
                    let high = self.expression(5)?;
                    left = self.arena_expr(Expr::Between { operand: left, low, high, negated })?;
                    if symmetric {
                        // `x BETWEEN SYMMETRIC a AND b` holds when x lies between
                        // the two bounds in either order, which is the pair of
                        // asymmetric tests — AND-ed for NOT BETWEEN, as negating
                        // the disjunction requires.
                        let swapped = self.arena_expr(Expr::Between {
                            operand: left_operand,
                            low: high,
                            high: low,
                            negated,
                        })?;
                        left = self.arena_expr(Expr::Binary {
                            operator: if negated { BinaryOp::And } else { BinaryOp::Or },
                            left,
                            right: swapped,
                        })?;
                    }
                    continue;
                }
                let ilike = self.peeked == Tok::Ident("ilike");
                if ilike || self.peeked == Tok::Ident("like") {
                    self.advance()?;
                    // `x LIKE ANY/ALL (array)` — quantified pattern match.
                    if matches!(
                        self.peeked,
                        Tok::Ident("any") | Tok::Ident("all") | Tok::Ident("some")
                    ) {
                        let all = self.peeked == Tok::Ident("all");
                        self.advance()?;
                        self.expect_op("(")?;
                        let array = self.expression(0)?;
                        self.expect_op(")")?;
                        let operator = if ilike { BinaryOp::ILike } else { BinaryOp::Like };
                        // NOT LIKE ANY == NOT (LIKE ALL), and vice versa.
                        let inner = self.arena_expr(Expr::AnyAll {
                            operand: left,
                            operator,
                            array,
                            all: if negated { !all } else { all },
                        })?;
                        left = if negated {
                            self.arena_expr(Expr::Unary {
                                operator: crate::sql::ast::UnaryOp::Not,
                                operand: inner,
                            })?
                        } else {
                            inner
                        };
                        continue;
                    }
                    let pattern = self.expression(5)?;
                    let escape = self.escape_clause()?;
                    left = self.arena_expr(Expr::Like {
                        operand: left,
                        pattern,
                        negated,
                        case_insensitive: ilike,
                        escape,
                    })?;
                    continue;
                }
                // `x SIMILAR TO p` — SQL regular expression; desugared to the
                // scalar `similar_to(x, p)` (NOT wraps it in a boolean negation).
                if self.peeked == Tok::Ident("similar") {
                    self.advance()?;
                    self.expect_ident("to")?;
                    let pattern = self.expression(5)?;
                    // The escape character rides along as a third argument.
                    let args = match self.escape_clause()? {
                        Some(escape) => self.arena_slice(&[left, pattern, escape])?,
                        None => self.arena_slice(&[left, pattern])?,
                    };
                    let call = self.arena_expr(Expr::Call {
                        name: crate::sql::parser::SIMILAR_TO,
                        args,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    })?;
                    left = if negated {
                        self.arena_expr(Expr::Unary {
                            operator: crate::sql::ast::UnaryOp::Not,
                            operand: call,
                        })?
                    } else {
                        call
                    };
                    continue;
                }
                if negated {
                    return Err(self.unexpected("expected IN, BETWEEN, LIKE or SIMILAR TO after NOT"));
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
            // Quantified comparison: `operand operator ANY/ALL (array)` or
            // `... (subquery)`.
            if matches!(self.peeked, Tok::Ident("any") | Tok::Ident("all") | Tok::Ident("some")) {
                let all = self.peeked == Tok::Ident("all");
                self.advance()?;
                self.expect_op("(")?;
                if self.peeked == Tok::Ident("select") {
                    let select = self.select()?;
                    self.expect_op(")")?;
                    let boxed = self
                        .arena
                        .alloc(select)
                        .map_err(|_| self.err_here("statement too large for SQL arena"))?;
                    // `= ANY (sub)` is IN, `<> ALL (sub)` is NOT IN; the rest
                    // quantify over the subquery's collected result column
                    // (same truth table as the array forms, per PostgreSQL).
                    left = if operator == BinaryOp::Eq && !all {
                        self.arena_expr(Expr::InSubquery {
                            operand: left,
                            select: boxed,
                            negated: false,
                        })?
                    } else if operator == BinaryOp::NotEq && all {
                        self.arena_expr(Expr::InSubquery {
                            operand: left,
                            select: boxed,
                            negated: true,
                        })?
                    } else {
                        let array = self.arena_expr(Expr::ArraySubquery(boxed))?;
                        self.arena_expr(Expr::AnyAll { operand: left, operator, array, all })?
                    };
                    continue;
                }
                let array = self.expression(0)?;
                self.expect_op(")")?;
                left = self.arena_expr(Expr::AnyAll { operand: left, operator, array, all })?;
                continue;
            }
            let right = self.expression(operator.precedence() + 1)?;
            left = self.arena_expr(Expr::Binary { operator, left, right })?;
        }
    }

    pub(super) fn prefix(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let tok = self.peeked;
        match tok {
            Tok::Num(text) => {
                self.advance()?;
                // Base-prefixed literals (0x/0o/0b) are always integers; a plain
                // token is integral unless it has a decimal point or exponent.
                let prefixed = is_base_prefixed(text);
                let looks_integral = prefixed || !text.contains(['.', 'e', 'E']);
                if looks_integral
                    && let Some(v) = crate::sql::eval::parse_int_literal(text) {
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
            Tok::Bit(s) => {
                self.advance()?;
                self.arena_expr(Expr::BitLit(s))
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
                // A parenthesized comma list is either an OVERLAPS period pair
                // `(start, end) OVERLAPS (start, end)` (desugared to the internal
                // `overlaps(s1, e1, s2, e2)` call) or, in every other position,
                // an implicit row constructor equivalent to `ROW(...)`.
                let mut base = inner;
                if self.peeked == Tok::Op(",") {
                    let mut items = [inner; MAX_LIST];
                    let mut n = 1usize;
                    while self.peeked == Tok::Op(",") {
                        self.advance()?;
                        if n == MAX_LIST {
                            return Err(self.limit("row constructor", MAX_LIST));
                        }
                        items[n] = self.expression(0)?;
                        n += 1;
                    }
                    self.expect_op(")")?;
                    if self.peeked == Tok::Ident("overlaps") {
                        if n != 2 {
                            return Err(self.err_here("OVERLAPS requires a (start, end) pair"));
                        }
                        self.advance()?;
                        self.expect_op("(")?;
                        let other_start = self.expression(0)?;
                        self.expect_op(",")?;
                        let other_end = self.expression(0)?;
                        self.expect_op(")")?;
                        return self.plain_call(
                            crate::sql::parser::OVERLAPS_PERIODS,
                            &[items[0], items[1], other_start, other_end],
                        );
                    }
                    base = self.plain_call("row", &items[..n])?;
                    // A bare row constructor is not a field-access target:
                    // PostgreSQL's grammar reaches `.field` through a
                    // parenthesized expression, so `(1,2).f1` is a syntax error
                    // while `((1,2)).f1` — where the outer parens make it one —
                    // is the spelling that works.
                    if self.peeked == Tok::Op(".") {
                        return Err(self.err_here("syntax error at or near \".\""));
                    }
                } else {
                    self.expect_op(")")?;
                }
                // `(expression).field` composite field access (chained), or
                // `(expression).*` record expansion. The star is carried as a
                // `Field` with the sentinel field name `*` (no real column can
                // be named that); `select_items` turns a top-level one into a
                // `RecordStar` item and every other position rejects it.
                while self.peeked == Tok::Op(".") {
                    self.advance()?;
                    if self.peeked == Tok::Op("*") {
                        self.advance()?;
                        return self.arena_expr(Expr::Field { base, field: "*" });
                    }
                    let field = self.any_ident("field name")?;
                    base = self.arena_expr(Expr::Field { base, field })?;
                }
                Ok(base)
            }
            // PostgreSQL's prefix arithmetic operators are spelled as the
            // functions they name, so they parse straight into those calls.
            Tok::Op("|/") | Tok::Op("||/") | Tok::Op("@") => {
                let operator = match self.peeked {
                    Tok::Op("|/") => UnaryOp::SquareRoot,
                    Tok::Op("||/") => UnaryOp::CubeRoot,
                    _ => UnaryOp::AbsoluteValue,
                };
                self.advance()?;
                // PostgreSQL puts "any other operator" below binary + - * /,
                // so `@ -3 + 1` is `@ (-3 + 1)` and `|/ 4 * 2` is `|/ (4 * 2)`
                // — the operand takes everything that binds tighter than this
                // level, which is where `||` and the bitwise operators sit too.
                let operand = self.expression(6)?;
                self.arena_expr(Expr::Unary { operator, operand })
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
                    // A following `::` binds tighter than the minus, so
                    // `-32768::int2` is `-(32768::int2)` — PostgreSQL raises
                    // "smallint out of range" there, and folding would hide it.
                    let cast_follows = self.next_is_cast()?;
                    if integral
                        && !cast_follows
                        && let Some(v) = crate::sql::eval::parse_int_literal(text) {
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
                    synthetic: false,
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
                // The reserved-word test below runs after the cursor has moved,
                // so the word's own offset is kept for the error.
                let name_at = self.peek_at;
                self.advance()?;
                // Qualified name: the pg_catalog / information_schema schemas are
                // transparent (pg_catalog.version() == version()), so a leading
                // recognized schema qualifier is dropped.
                let (name, stripped_schema) = if (name == "pg_catalog"
                    || name == "information_schema")
                    && self.peeked == Tok::Op(".")
                {
                    let stripped = name;
                    self.advance()?;
                    (self.any_ident("function or column name")?, Some(stripped))
                } else {
                    (name, None)
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
                // Every construct this arm recognizes has now had its chance, so a
                // still-unconsumed reserved word cannot begin an expression —
                // `ARRAY` is itself reserved, which is why this cannot come
                // first. A `can be function or type name` keyword may continue:
                // it is exactly the category PostgreSQL allows to name one.
                if self.peeked == Tok::Op("(") {
                    return self.call(name);
                }
                // Typed literal, SQL-standard: `DATE '2020-01-01'` is
                // exactly `'2020-01-01'::date`. Only fires when the name is a
                // known type immediately followed by a string.
                if let Tok::Str(lit) = self.peeked
                    && crate::sql::types::ColType::from_sql_name(name).is_some() {
                        self.advance()?;
                        // `INTERVAL '1' DAY`: the SQL-standard trailing unit
                        // qualifier interprets an otherwise-unitless value in
                        // that field, so it is folded into the literal before
                        // the cast rather than left dangling.
                        let lit = if name.eq_ignore_ascii_case("interval") {
                            self.interval_with_qualifier(lit)?
                        } else {
                            lit
                        };
                        let operand = self.arena_expr(Expr::Str(lit))?;
                        return self.arena_expr(Expr::Cast { operand, type_name: name, type_mod: -1 });
                    }
                if self.peeked == Tok::Op(".") {
                    self.advance()?;
                    if self.peeked == Tok::Op("*") {
                        self.advance()?;
                        return self.arena_expr(Expr::WholeRow(name));
                    }
                    let column = self.any_ident("column name")?;
                    // A third part makes it `schema.table.column` — as does a
                    // transparently stripped `pg_catalog.`/`information_schema.`
                    // prefix, which must still validate as the schema part.
                    if self.peeked == Tok::Op(".") {
                        self.advance()?;
                        if self.peeked == Tok::Op("*") {
                            self.advance()?;
                            // `schema.table.*`: a whole-row reference under a
                            // composed qualifier, which scope resolution binds
                            // to the unaliased base table of that schema.
                            return self.arena_expr(Expr::WholeRow(
                                self.composed_qualifier(name, column)?,
                            ));
                        }
                        let third = self.any_ident("column name")?;
                        return self.arena_expr(Expr::SchemaColumn {
                            schema: name,
                            table: column,
                            name: third,
                        });
                    }
                    if let Some(schema) = stripped_schema {
                        return self.arena_expr(Expr::SchemaColumn {
                            schema,
                            table: name,
                            name: column,
                        });
                    }
                    return self.arena_expr(Expr::Column {
                        qualifier: Some(name),
                        name: column,
                    });
                }
                // SQL-standard functions written without parentheses. The
                // temporal ones also take an optional precision.
                if matches!(
                    name,
                    "current_date" | "current_timestamp" | "current_time" | "localtimestamp"
                        | "localtime" | "current_user" | "session_user" | "user"
                        | "current_catalog" | "current_schema"
                ) {
                    let mut args: &[&'a Expr<'a>] = &[];
                    if self.peeked == Tok::Op("(")
                        && matches!(name, "current_timestamp" | "current_time" | "localtimestamp" | "localtime")
                    {
                        self.advance()?;
                        let precision = self.expression(0)?;
                        self.expect_op(")")?;
                        args = self.arena_slice(&[precision])?;
                    }
                    return self.arena_expr(Expr::Call {
                        name,
                        args,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    });
                }
                // Only now, every construct this arm knows having had its
                // chance: a still-unconsumed reserved word cannot begin an
                // expression. It cannot come earlier — `ARRAY` is reserved and
                // so is `current_date`, yet both are ordinary expressions.
                if is_reserved_keyword(name) {
                    return Err(ParseError { at: name_at, ..self.unexpected("expected an expression") });
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
                    if self.peeked == Tok::Op("*") {
                        self.advance()?;
                        return self.arena_expr(Expr::WholeRow(name));
                    }
                    let column = self.any_ident("column name")?;
                    if self.peeked == Tok::Op(".") {
                        self.advance()?;
                        if self.peeked == Tok::Op("*") {
                            self.advance()?;
                            return self.arena_expr(Expr::WholeRow(
                                self.composed_qualifier(name, column)?,
                            ));
                        }
                        let third = self.any_ident("column name")?;
                        return self.arena_expr(Expr::SchemaColumn {
                            schema: name,
                            table: column,
                            name: third,
                        });
                    }
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
    pub(super) fn plain_call(&mut self, name: &'a str, args: &[&'a Expr<'a>]) -> Result<&'a Expr<'a>, ParseError> {
        let args = self.arena_slice(args)?;
        self.arena_expr(Expr::Call { name, args, star: false, distinct: false, order_by: &[], over: None, filter: None })
    }

    pub(super) fn call(&mut self, name: &'a str) -> Result<&'a Expr<'a>, ParseError> {
        self.expect_op("(")?;
        // SQL-standard `substring(str FROM start [FOR len])` and
        // `trim([both|leading|trailing] [chars] FROM str)` desugar to the plain
        // function forms.
        if name.eq_ignore_ascii_case("substring") {
            // Above the precedence `SIMILAR TO` binds at, so that the SIMILAR
            // in `substring(x SIMILAR p ESCAPE e)` is this form's keyword
            // rather than an infix operator applied to the target.
            let target = self.expression(5)?;
            if self.eat_ident("from")? {
                let start = self.expression(0)?;
                let mut cargs = [target, start, target];
                let mut cn = 2;
                if self.eat_ident("for")? {
                    cargs[2] = self.expression(0)?;
                    cn = 3;
                }
                self.expect_op(")")?;
                return self.plain_call("substring", &cargs[..cn]);
            }
            // `substring(str SIMILAR pattern ESCAPE e)`: SQL:2003's spelling of
            // the SQL-regular-expression form that `FROM pattern FOR e` already
            // spells, so it is the same call — the extraction semantics live in
            // one place rather than being written twice for two syntaxes.
            if self.eat_ident("similar")? {
                let pattern = self.expression(0)?;
                self.expect_ident("escape")?;
                let escape = self.expression(0)?;
                self.expect_op(")")?;
                return self.plain_call("substring", &[target, pattern, escape]);
            }
            // `substring(str FOR len)` — no FROM; PostgreSQL implies FROM 1.
            if self.eat_ident("for")? {
                let len = self.expression(0)?;
                let one = self.arena_expr(Expr::Int(1))?;
                self.expect_op(")")?;
                return self.plain_call("substring", &[target, one, len]);
            }
            // Comma form `substring(str, start[, len])`.
            let mut cargs = [target, target, target];
            let mut cn = 1;
            while self.eat_op(",")? {
                cargs[cn] = self.expression(0)?;
                cn += 1;
            }
            self.expect_op(")")?;
            return self.plain_call("substring", &cargs[..cn]);
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
                // Kept under its own name, not desugared to `strpos`: the two
                // compute the same thing, but PostgreSQL labels the output
                // column `position`, and the label comes from the call name.
                return self.plain_call("position", &[haystack, needle]);
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
        if name.eq_ignore_ascii_case("make_interval") {
            // `make_interval([years =>] i, [months =>] i, [weeks =>] i,
            //  [days =>] i, [hours =>] i, [mins =>] i, [secs =>] d)` — the seven
            // named fields, any subset, positional or `field => value`. Desugar
            // to a fixed seven-argument positional call (missing fields = 0) so
            // the AST and evaluator stay unaware of argument names.
            const FIELDS: [&str; 7] =
                ["years", "months", "weeks", "days", "hours", "mins", "secs"];
            let zero = self.arena_expr(Expr::Int(0))?;
            let mut slots: [&'a Expr<'a>; 7] = [zero; 7];
            let mut pos = 0usize;
            let mut seen_named = false;
            if self.peeked != Tok::Op(")") {
                loop {
                    let first = self.expression(0)?;
                    if self.eat_op("=>")? {
                        // Named argument: the parsed expression must be a bare
                        // field name; map it to its fixed slot.
                        let field = match first {
                            Expr::Column { qualifier: None, name } => *name,
                            _ => return Err(self.err_here("make_interval argument name must be a field")),
                        };
                        let idx = FIELDS.iter().position(|f| f.eq_ignore_ascii_case(field));
                        let idx = match idx {
                            Some(i) => i,
                            None => return Err(self.err_here("unknown make_interval field")),
                        };
                        slots[idx] = self.expression(0)?;
                        seen_named = true;
                    } else {
                        if seen_named {
                            return Err(self.err_here("positional argument after named argument"));
                        }
                        if pos >= FIELDS.len() {
                            return Err(self.err_here("too many arguments to make_interval"));
                        }
                        slots[pos] = first;
                        pos += 1;
                    }
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            return self.plain_call("make_interval", &slots);
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
        // Ordered-set aggregate: `agg(direct_args) WITHIN GROUP (ORDER BY ...)`.
        // The WITHIN GROUP ordering is the aggregated input; it is carried in the
        // same `order_by` slot.
        let order_by = if self.peeked == Tok::Ident("within") {
            self.advance()?;
            self.expect_ident("group")?;
            self.expect_op("(")?;
            self.expect_ident("order")?;
            self.expect_ident("by")?;
            let items = self.order_by_items()?;
            self.expect_op(")")?;
            items
        } else {
            order_by
        };
        let filter = self.parse_filter()?;
        let over = self.parse_over()?;
        let args = self.arena_slice(&args[..n])?;
        self.arena_expr(Expr::Call { name, args, star: false, distinct, order_by, over, filter })
    }

    /// Consumes an operator or identifier token, returning its text (used
    /// inside `OPERATOR(...)`, where a schema name and the symbol both appear).
    pub(super) fn any_op_token(&mut self) -> Result<&'a str, ParseError> {
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
    pub(super) fn build_operator(
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
    /// Applies an SQL-standard interval unit qualifier (`INTERVAL '1' DAY`) to
    /// the literal, folding the trailing field into the string. Returns the
    /// literal unchanged when no qualifier follows.
    ///
    /// A single field interprets a bare numeric value in that unit and, for
    /// every field but SECOND, truncates it toward zero to that field's
    /// resolution — `'2.5' HOUR` is two hours, not two and a half. The
    /// hyphenated `YEAR TO MONTH` range form and a qualifier on an already
    /// unit-bearing string are not yet handled and are refused rather than
    /// quietly mis-parsed.
    fn interval_with_qualifier(&mut self, lit: &'a str) -> Result<&'a str, ParseError> {
        let Some((word, keep_fraction)) = self.peek_interval_field() else {
            return Ok(lit);
        };
        self.advance()?;
        if self.eat_ident("to")? {
            // YEAR TO MONTH carries a self-contained `Y-M` format; the clock
            // ranges (DAY/HOUR/MINUTE TO ...) carry a `D H:M:S` value truncated
            // to the trailing field. Each field pair is a valid ordering only
            // from a coarser field to a finer one.
            if word == "year" {
                if self.eat_ident("month")? {
                    return self.year_to_month(lit);
                }
                return Err(self.err_here("expected MONTH after YEAR TO"));
            }
            let Some(start) = clock_field_ordinal(word) else {
                return Err(self.err_here("INTERVAL range must run from a coarser field to a finer"));
            };
            let Some((end_word, _)) = self.peek_interval_field() else {
                return Err(self.err_here("expected an interval field after TO"));
            };
            let Some(end) = clock_field_ordinal(end_word) else {
                return Err(self.err_here("INTERVAL range must run from a coarser field to a finer"));
            };
            if end <= start {
                return Err(self.err_here("INTERVAL range must run from a coarser field to a finer"));
            }
            self.advance()?;
            return self.clock_range(lit, start, end);
        }
        let value = lit.trim();
        let numeric = value.strip_prefix(['-', '+']).unwrap_or(value);
        let is_number = !numeric.is_empty()
            && numeric.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && numeric.bytes().filter(|&b| b == b'.').count() <= 1;
        if !is_number {
            return Err(self.err_here(
                "INTERVAL unit qualifier on a non-numeric literal is not supported yet",
            ));
        }
        // Truncate toward zero for the coarser fields by dropping any fraction.
        let magnitude = if keep_fraction {
            value
        } else {
            value.split_once('.').map_or(value, |(head, _)| head)
        };
        let combined = crate::stack_format!(64, "{} {}", magnitude, word);
        self.arena
            .alloc_str(combined.as_str())
            .map_err(|_| self.err_here("interval literal too large for SQL arena"))
    }

    /// `INTERVAL '1-2' YEAR TO MONTH`: the `Y-M` value is years and months, a
    /// bare number is months (the trailing field), and the leading sign applies
    /// to both. The month part must be 0..11, since twelve months is a year;
    /// PostgreSQL rejects `1-13` as out of range. The value is rewritten to the
    /// `N year M month` form the interval parser already understands.
    fn year_to_month(&mut self, lit: &'a str) -> Result<&'a str, ParseError> {
        let out_of_range = || ParseError {
            at: self.peek_at,
            message: crate::stack_format!(96, "interval field value out of range: \"{}\"", lit),
            sqlstate: sqlstate::INTERVAL_FIELD_OVERFLOW,
        };
        // A part that is not a number at all is malformed input (22007); a
        // number that is simply too large for its field is out of range (22015).
        let bad_syntax = || ParseError {
            at: self.peek_at,
            message: crate::stack_format!(96, "invalid input syntax for type interval: \"{}\"", lit),
            sqlstate: sqlstate::INVALID_DATETIME_FORMAT,
        };
        let value = lit.trim();
        let (sign, rest) = match value.strip_prefix('-') {
            Some(r) => ("-", r),
            None => ("", value.strip_prefix('+').unwrap_or(value)),
        };
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let combined = match rest.split_once('-') {
            Some((years, months)) => {
                if !all_digits(years) || !all_digits(months) {
                    return Err(bad_syntax());
                }
                // Twelve or more months would carry into the year field, which
                // the two-field form does not permit.
                if months.parse::<u32>().map_err(|_| out_of_range())? > 11 {
                    return Err(out_of_range());
                }
                crate::stack_format!(48, "{s}{y} year {s}{m} month", s = sign, y = years, m = months)
            }
            None => {
                if !all_digits(rest) {
                    return Err(bad_syntax());
                }
                crate::stack_format!(48, "{s}{m} month", s = sign, m = rest)
            }
        };
        self.arena
            .alloc_str(combined.as_str())
            .map_err(|_| self.err_here("interval literal too large for SQL arena"))
    }

    /// A clock `TO`-range qualifier — `INTERVAL '1 2:03:04' DAY TO SECOND` and
    /// its kin. The value is a day count (only when the range starts at DAY)
    /// followed by an `H:M:S` clock, truncated to the trailing field. Rather
    /// than teach the interval parser the colon rules — a two-part clock is
    /// `H:M` or `M:S` depending on the leading field, a three-part clock is
    /// always `H:M:S`, and a bare number takes the trailing field — the value
    /// is decoded here into components and re-emitted as the unambiguous
    /// `N day N hour N minute N second` form the parser already sums. Day and
    /// clock carry independent signs, as PostgreSQL keeps them.
    fn clock_range(&mut self, lit: &'a str, start: u8, end: u8) -> Result<&'a str, ParseError> {
        let bad = || ParseError {
            at: self.peek_at,
            message: crate::stack_format!(96, "invalid input syntax for type interval: \"{}\"", lit),
            sqlstate: sqlstate::INVALID_DATETIME_FORMAT,
        };
        let value = lit.trim();

        // A single scalar with no day separator and no clock takes the trailing
        // field: `'5' DAY TO HOUR` is five hours, `'100' MINUTE TO SECOND` a
        // hundred seconds.
        if !value.contains([' ', ':']) {
            return self.scalar_in_field(lit, value, end);
        }

        // Components in the order the parser will read them back.
        let mut days = 0i64;
        let mut hours = 0i64;
        let mut minutes = 0i64;
        let mut sec_whole = 0i64;
        let mut sec_frac: &str = "";

        // The clock, and the day count when the range starts at DAY.
        let clock = if start == FIELD_DAY {
            let (day_str, clock) = value.split_once(' ').ok_or_else(bad)?;
            days = parse_signed_int(day_str.trim()).ok_or_else(bad)?;
            clock.trim()
        } else {
            value
        };

        // The clock's own sign, independent of the day's.
        let (clock_neg, clock) = match clock.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, clock.strip_prefix('+').unwrap_or(clock)),
        };
        // The field a two-part clock begins at: HOUR after a day or when the
        // range starts at DAY/HOUR, MINUTE when the range starts at MINUTE.
        let clock_start = if start == FIELD_MINUTE { FIELD_MINUTE } else { FIELD_HOUR };
        let mut parts = clock.split(':');
        let a = parts.next().unwrap_or("");
        let b = parts.next();
        let c = parts.next();
        if parts.next().is_some() {
            return Err(bad());
        }
        match (b, c) {
            // Three parts are always hours:minutes:seconds.
            (Some(bb), Some(cc)) => {
                hours = parse_uint(a).ok_or_else(bad)?;
                minutes = parse_uint(bb).ok_or_else(bad)?;
                let (w, f) = split_seconds(cc).ok_or_else(bad)?;
                sec_whole = w;
                sec_frac = f;
            }
            // Two parts start at the clock's leading field.
            (Some(bb), None) => {
                if clock_start == FIELD_HOUR {
                    hours = parse_uint(a).ok_or_else(bad)?;
                    minutes = parse_uint(bb).ok_or_else(bad)?;
                } else {
                    minutes = parse_uint(a).ok_or_else(bad)?;
                    let (w, f) = split_seconds(bb).ok_or_else(bad)?;
                    sec_whole = w;
                    sec_frac = f;
                }
            }
            // One part after a day is the field just below DAY, i.e. hours.
            (None, None) => {
                hours = parse_uint(a).ok_or_else(bad)?;
            }
            (None, Some(_)) => unreachable!("split yields no third part without a second"),
        }
        if clock_neg {
            hours = -hours;
            minutes = -minutes;
        }

        // Truncate to the trailing field by zeroing everything finer.
        if end < FIELD_HOUR {
            hours = 0;
        }
        if end < FIELD_MINUTE {
            minutes = 0;
        }
        if end < FIELD_SECOND {
            sec_whole = 0;
            sec_frac = "";
        }

        // Seconds carry the clock's sign on the whole `S.f` value together, so a
        // fractional-only negative second (`-0.5`) keeps its sign.
        let sec_sign = if clock_neg { "-" } else { "" };
        let combined = crate::stack_format!(
            96,
            "{} day {} hour {} minute {}{}.{} second",
            days,
            hours,
            minutes,
            sec_sign,
            sec_whole,
            if sec_frac.is_empty() { "0" } else { sec_frac }
        );
        self.arena
            .alloc_str(combined.as_str())
            .map_err(|_| self.err_here("interval literal too large for SQL arena"))
    }

    /// A bare number interpreted in a single field for a range qualifier — the
    /// trailing field of a `TO` range. Truncates toward zero for every field
    /// but SECOND, as the single-field qualifier does.
    fn scalar_in_field(
        &mut self,
        lit: &'a str,
        value: &str,
        field: u8,
    ) -> Result<&'a str, ParseError> {
        let numeric = value.strip_prefix(['-', '+']).unwrap_or(value);
        let is_number = !numeric.is_empty()
            && numeric.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && numeric.bytes().filter(|&b| b == b'.').count() <= 1;
        if !is_number {
            return Err(ParseError {
                at: self.peek_at,
                message: crate::stack_format!(96, "invalid input syntax for type interval: \"{}\"", lit),
                sqlstate: sqlstate::INVALID_DATETIME_FORMAT,
            });
        }
        let (word, keep_fraction) = clock_field_word(field);
        let magnitude = if keep_fraction {
            value
        } else {
            value.split_once('.').map_or(value, |(head, _)| head)
        };
        let combined = crate::stack_format!(64, "{} {}", magnitude, word);
        self.arena
            .alloc_str(combined.as_str())
            .map_err(|_| self.err_here("interval literal too large for SQL arena"))
    }

    /// The interval field keyword under the cursor, as `(unit word, keeps a
    /// fractional part)`. SECOND keeps its fraction; the coarser fields do not.
    fn peek_interval_field(&self) -> Option<(&'static str, bool)> {
        let Tok::Ident(word) = self.peeked else {
            return None;
        };
        Some(match word {
            "year" => ("year", false),
            "month" => ("month", false),
            "day" => ("day", false),
            "hour" => ("hour", false),
            "minute" => ("minute", false),
            "second" => ("second", true),
            _ => return None,
        })
    }

    pub(super) fn build_distinct_from(
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
        self.arena_expr(Expr::Case { operand: None, whens, otherwise: Some(cmp), synthetic: true })
    }

    pub(super) fn peek_binary_op(&self) -> Option<BinaryOp> {
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
            Tok::Op("#>") => Some(BinaryOp::JsonPath),
            Tok::Op("#>>") => Some(BinaryOp::JsonPathText),
            Tok::Op("#-") => Some(BinaryOp::JsonDeletePath),
            Tok::Op("?") => Some(BinaryOp::JsonExists),
            Tok::Op("?|") => Some(BinaryOp::JsonExistsAny),
            Tok::Op("?&") => Some(BinaryOp::JsonExistsAll),
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
}

/// Ordinals for the clock interval fields, coarse to fine. YEAR and MONTH are
/// not here — their range form is handled separately.
const FIELD_DAY: u8 = 0;
const FIELD_HOUR: u8 = 1;
const FIELD_MINUTE: u8 = 2;
const FIELD_SECOND: u8 = 3;

/// The clock field an interval keyword names, or `None` for YEAR/MONTH and
/// anything that is not a field.
fn clock_field_ordinal(word: &str) -> Option<u8> {
    Some(match word {
        "day" => FIELD_DAY,
        "hour" => FIELD_HOUR,
        "minute" => FIELD_MINUTE,
        "second" => FIELD_SECOND,
        _ => return None,
    })
}

/// The unit word for a clock field ordinal, and whether it keeps a fraction.
fn clock_field_word(field: u8) -> (&'static str, bool) {
    match field {
        FIELD_DAY => ("day", false),
        FIELD_HOUR => ("hour", false),
        FIELD_MINUTE => ("minute", false),
        _ => ("second", true),
    }
}

/// A non-negative integer, or `None` for anything else.
fn parse_uint(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A signed integer (optional leading `+`/`-`), or `None`.
fn parse_signed_int(s: &str) -> Option<i64> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v: i64 = parse_uint(rest)?;
    Some(if neg { -v } else { v })
}

/// Splits a seconds field into `(whole, fraction-digits)`; the fraction is the
/// text after the decimal point, empty when there is none.
fn split_seconds(s: &str) -> Option<(i64, &str)> {
    match s.split_once('.') {
        Some((w, f)) => {
            if !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((parse_uint(w)?, f))
        }
        None => Some((parse_uint(s)?, "")),
    }
}
