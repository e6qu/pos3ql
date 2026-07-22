//! Expression parsing: precedence climbing over the operators, and the prefix
//! forms every expression starts from.
//!
//! `expression` takes a minimum precedence and consumes operators at or above
//! it, so each level's associativity falls out of what it recurses with;
//! `prefix` handles everything an expression can begin with — a literal, a
//! column, a parenthesized expression or row, a CASE, a subquery, an array —
//! and the postfix forms (subscripts, field access, casts) that bind to it.

use crate::sql::lexer::Tok;

use super::{is_base_prefixed, is_reserved_keyword, ParseError, Parser, MAX_LIST};
use crate::sql::ast::{BinaryOp, Expr, UnaryOp};

impl<'a> Parser<'a> {
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
                    left = self.arena_expr(Expr::Like {
                        operand: left,
                        pattern,
                        negated,
                        case_insensitive: ilike,
                    })?;
                    continue;
                }
                // `x SIMILAR TO p` — SQL regular expression; desugared to the
                // scalar `similar_to(x, p)` (NOT wraps it in a boolean negation).
                if self.peeked == Tok::Ident("similar") {
                    self.advance()?;
                    self.expect_ident("to")?;
                    let pattern = self.expression(5)?;
                    let call = self.arena_expr(Expr::Call {
                        name: "similar_to",
                        args: self.arena_slice(&[left, pattern])?,
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
                            "overlaps",
                            &[items[0], items[1], other_start, other_end],
                        );
                    }
                    base = self.plain_call("row", &items[..n])?;
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
                return self.plain_call("substring", &cargs[..cn]);
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
        self.arena_expr(Expr::Case { operand: None, whens, otherwise: Some(cmp) })
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
