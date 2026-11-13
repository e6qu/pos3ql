//! Operators, SQL-syntax functions and the parser's expansions of row
//! comparisons and `IN` lists, printed as `ruleutils.c` prints the nodes
//! PostgreSQL's parser builds for them (`get_oper_expr`,
//! `get_func_sql_syntax`, `ScalarArrayOpExpr`, `RowCompareExpr`).

use super::{Context, Output, Parent, push_identifier};
use crate::sql::SqlError;
use crate::sql::ast::{BinaryOp, CallKind, Expr, SourcePosition, UnaryOp};
use crate::sql::types::oid;

/// `float8[]`, the direct argument of a multiple-fraction percentile.
const FLOAT8_ARRAY: i32 = 1022;

/// Text types PostgreSQL's text operators relabel to `text`.
fn text_like(type_oid: i32) -> bool {
    matches!(type_oid, oid::TEXT | oid::VARCHAR | oid::BPCHAR)
}

fn integer_rank(type_oid: i32) -> Option<u8> {
    Some(match type_oid {
        oid::INT2 => 1,
        oid::INT4 => 2,
        oid::INT8 => 3,
        _ => return None,
    })
}

/// Types with their own `||` operator, which an unknown operand takes.
fn own_concat(type_oid: i32) -> bool {
    super::element_of(type_oid).is_some()
        || matches!(
            type_oid,
            oid::JSONB | oid::TSVECTOR | oid::TSQUERY | oid::BYTEA | oid::BIT | oid::VARBIT
        )
}

/// A prefix operator's spelling and the operand type it coerces to.
pub(super) fn prefix_operator(operator: UnaryOp) -> Option<(&'static str, Option<i32>)> {
    Some(match operator {
        UnaryOp::Neg => ("-", None),
        UnaryOp::BitNot => ("~", None),
        UnaryOp::AbsoluteValue => ("@", None),
        UnaryOp::SquareRoot => ("|/", Some(oid::FLOAT8)),
        UnaryOp::CubeRoot => ("||/", Some(oid::FLOAT8)),
        UnaryOp::TextSearchNot => ("!!", Some(oid::TSQUERY)),
        UnaryOp::Not => return None,
    })
}

/// Whether an `IN` list item is a plain column, which PostgreSQL compares
/// on its own rather than in the `= ANY (ARRAY[...])` of the other items.
fn in_list_var(expression: &Expr<'_>) -> bool {
    matches!(
        expression,
        Expr::Column { .. } | Expr::SchemaColumn { .. } | Expr::RoutineParam { .. }
    )
}

/// `transformAExprIn`'s shape: whether the non-column items form one
/// array comparison, and how many comparisons the list becomes.
pub(super) fn in_list_shape(list: &[&Expr<'_>]) -> (bool, usize) {
    let vars = list.iter().filter(|item| in_list_var(item)).count();
    let others = list.len() - vars;
    if others > 1 {
        (true, vars + 1)
    } else {
        (false, list.len())
    }
}

impl Context<'_> {
    /// The operand types a binary operator coerces its operands to, as
    /// PostgreSQL's operator resolution selects them; `None` keeps an
    /// operand as it is.
    pub(super) fn binary_operands(
        &self,
        operator: BinaryOp,
        left: &Expr<'_>,
        right: &Expr<'_>,
    ) -> Result<(Option<i32>, Option<i32>), SqlError> {
        let (l, r) = (self.type_of(left)?, self.type_of(right)?);
        let same = |l: Option<i32>, r: Option<i32>| match (l, r) {
            (Some(l), None) => (Some(l), Some(l)),
            (None, Some(r)) => (Some(r), Some(r)),
            (None, None) => (Some(oid::TEXT), Some(oid::TEXT)),
            (l, r) => (l, r),
        };
        let text = |found: Option<i32>| match found {
            None => Some(oid::TEXT),
            Some(found) if text_like(found) => Some(oid::TEXT),
            found => found,
        };
        Ok(match operator {
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq => {
                if operator == BinaryOp::Sub && l == Some(oid::JSONB) && r.is_none() {
                    return Ok((l, Some(oid::TEXT)));
                }
                let object = |found: Option<i32>| {
                    found.is_some_and(|found| {
                        found == oid::OID
                            || crate::sql::types::ColType::from_oid(found)
                                .is_some_and(crate::sql::types::ColType::is_reg_object)
                    })
                };
                match (l, r) {
                    // An integer compared with an object identifier becomes one.
                    (Some(_), Some(other)) if object(l) && integer_rank(other).is_some() => (l, l),
                    (Some(other), Some(_)) if object(r) && integer_rank(other).is_some() => (r, r),
                    // `bpchar` against `text` compares as text.
                    (Some(l), Some(r))
                        if text_like(l) && text_like(r) && (l == oid::TEXT || r == oid::TEXT) =>
                    {
                        (Some(oid::TEXT), Some(oid::TEXT))
                    }
                    _ => self.operand_types(left, right)?,
                }
            }
            BinaryOp::Concat => match (l, r) {
                (Some(l), Some(r)) if text_like(l) || text_like(r) => (
                    if text_like(l) {
                        Some(oid::TEXT)
                    } else {
                        Some(l)
                    },
                    if text_like(r) {
                        Some(oid::TEXT)
                    } else {
                        Some(r)
                    },
                ),
                (Some(l), None) => (
                    text(Some(l)),
                    if own_concat(l) {
                        Some(l)
                    } else {
                        Some(oid::TEXT)
                    },
                ),
                (None, Some(r)) => (
                    if own_concat(r) {
                        Some(r)
                    } else {
                        Some(oid::TEXT)
                    },
                    text(Some(r)),
                ),
                (None, None) => (Some(oid::TEXT), Some(oid::TEXT)),
                (l, r) => (l, r),
            },
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                match (l.and_then(integer_rank), r.and_then(integer_rank)) {
                    (Some(lr), Some(rr)) => {
                        let wider = if lr >= rr { l } else { r };
                        (wider, wider)
                    }
                    _ => same(l, r),
                }
            }
            BinaryOp::Shl | BinaryOp::Shr if l.and_then(integer_rank).is_some() => {
                (l, Some(oid::INT4))
            }
            BinaryOp::Pow => {
                if l == Some(oid::NUMERIC) || r == Some(oid::NUMERIC) {
                    (Some(oid::NUMERIC), Some(oid::NUMERIC))
                } else {
                    (Some(oid::FLOAT8), Some(oid::FLOAT8))
                }
            }
            BinaryOp::JsonGet | BinaryOp::JsonGetText | BinaryOp::JsonExists => {
                (l, r.or(Some(oid::TEXT)))
            }
            BinaryOp::JsonPath
            | BinaryOp::JsonPathText
            | BinaryOp::JsonDeletePath
            | BinaryOp::JsonExistsAny
            | BinaryOp::JsonExistsAll => (l, r.or(Some(oid::TEXT_ARRAY))),
            BinaryOp::JsonPathExists => (l, r.or(Some(oid::JSONPATH))),
            BinaryOp::TextSearchMatch => match (l, r) {
                (Some(oid::JSONB), None) => (l, Some(oid::JSONPATH)),
                (Some(oid::TSVECTOR), None) => (l, Some(oid::TSQUERY)),
                (Some(oid::TSQUERY), None) => (l, Some(oid::TSVECTOR)),
                (None, Some(oid::TSQUERY)) => (Some(oid::TSVECTOR), r),
                (None, Some(oid::TSVECTOR)) => (Some(oid::TSQUERY), r),
                (l, r) if l.is_none_or(text_like) && r.is_none_or(text_like) => {
                    (Some(oid::TEXT), Some(oid::TEXT))
                }
                (l, r) => (text(l), text(r)),
            },
            BinaryOp::StartsWith
            | BinaryOp::PatternLt
            | BinaryOp::PatternLtEq
            | BinaryOp::PatternGt
            | BinaryOp::PatternGtEq
            | BinaryOp::Like
            | BinaryOp::ILike
            | BinaryOp::NotLike
            | BinaryOp::NotILike
            | BinaryOp::RegexMatch
            | BinaryOp::RegexIMatch
            | BinaryOp::RegexNotMatch
            | BinaryOp::RegexNotIMatch => {
                if l == Some(oid::BYTEA) || r == Some(oid::BYTEA) {
                    (Some(oid::BYTEA), Some(oid::BYTEA))
                } else {
                    (Some(oid::TEXT), Some(oid::TEXT))
                }
            }
            _ => same(l, r),
        })
    }

    /// A binary operator: `(left op right)`.
    pub(super) fn binary(
        &self,
        operator: BinaryOp,
        left: &Expr<'_>,
        right: &Expr<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let Some(symbol) = operator.operator_name() else {
            return output.unsupported();
        };
        let (left_type, right_type) = self.binary_operands(operator, left, right)?;
        self.open(output)?;
        let parent = Parent::Operator(symbol);
        self.wrapped(left, Some(left_type), parent, true, output)?;
        output.push(" ")?;
        output.push(symbol)?;
        output.push(" ")?;
        self.wrapped(right, Some(right_type), parent, false, output)?;
        self.close(output)
    }

    /// A prefix operator: `(op operand)`.
    pub(super) fn prefix(
        &self,
        operator: UnaryOp,
        operand: &Expr<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let Some((symbol, expected)) = prefix_operator(operator) else {
            return output.unsupported();
        };
        self.open(output)?;
        output.push(symbol)?;
        output.push(" ")?;
        self.wrapped(operand, Some(expected), Parent::Prefix, false, output)?;
        self.close(output)
    }

    /// `x [NOT] LIKE p ESCAPE e`: the pattern goes through `like_escape`.
    pub(super) fn like_escape(
        &self,
        operand: &Expr<'_>,
        pattern: &Expr<'_>,
        escape: &Expr<'_>,
        symbol: &'static str,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let kind = if self.type_of(operand)? == Some(oid::BYTEA) {
            oid::BYTEA
        } else {
            oid::TEXT
        };
        self.open(output)?;
        self.wrapped(
            operand,
            Some(Some(kind)),
            Parent::Operator(symbol),
            true,
            output,
        )?;
        output.push(" ")?;
        output.push(symbol)?;
        output.push(" like_escape(")?;
        self.write(pattern, Some(kind), output)?;
        output.push(", ")?;
        self.write(escape, Some(kind), output)?;
        output.push(")")?;
        self.close(output)
    }

    /// `x [NOT] SIMILAR TO p [ESCAPE e]`: `x ~ similar_to_escape(p[, e])`.
    pub(super) fn similar(
        &self,
        args: &[&Expr<'_>],
        negated: bool,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let [operand, rest @ ..] = args else {
            return output.unsupported();
        };
        let symbol = if negated { "!~" } else { "~" };
        self.open(output)?;
        self.wrapped(
            operand,
            Some(Some(oid::TEXT)),
            Parent::Operator(symbol),
            true,
            output,
        )?;
        output.push(" ")?;
        output.push(symbol)?;
        output.push(" similar_to_escape(")?;
        for (index, argument) in rest.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            self.write(argument, Some(oid::TEXT), output)?;
        }
        output.push(")")?;
        self.close(output)
    }

    /// `x [NOT] IN (list)` as `transformAExprIn` builds it: the items that
    /// are not columns as one `= ANY (ARRAY[...])` when there are several,
    /// and each other item as its own comparison, joined by `OR` (`AND`).
    pub(super) fn in_list(
        &self,
        operand: &Expr<'_>,
        list: &[&Expr<'_>],
        negated: bool,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let (array, count) = in_list_shape(list);
        let connective = if negated { BinaryOp::And } else { BinaryOp::Or };
        let comparison = if negated {
            BinaryOp::NotEq
        } else {
            BinaryOp::Eq
        };
        if count > 1 {
            self.open(output)?;
        }
        let mut first = true;
        let mut separator = |output: &mut Output<'_>| -> Result<(), SqlError> {
            if !first {
                output.push(if negated { " AND " } else { " OR " })?;
            }
            first = false;
            Ok(())
        };
        let parent = Parent::Connective(connective);
        if array {
            separator(output)?;
            let paren = count > 1 && self.pretty;
            if paren {
                output.push("(")?;
            }
            self.array_comparison(operand, list, negated, output)?;
            if paren {
                output.push(")")?;
            }
        }
        for item in list {
            if array && !in_list_var(item) {
                continue;
            }
            separator(output)?;
            let compared = Expr::Binary {
                source_position: SourcePosition::UNKNOWN,
                operator: comparison,
                left: operand,
                right: item,
            };
            if count > 1 {
                self.wrapped(&compared, Some(Some(oid::BOOL)), parent, false, output)?;
            } else {
                self.bare(&compared, output)?;
            }
        }
        if count > 1 {
            self.close(output)?;
        }
        Ok(())
    }

    /// `operand = ANY (ARRAY[items])` (`<> ALL` when negated), the items
    /// coerced to the operand's common type.
    fn array_comparison(
        &self,
        operand: &Expr<'_>,
        items: &[&Expr<'_>],
        negated: bool,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let mut element = self.type_of(operand)?.map(super::operator_type);
        for item in items.iter().filter(|item| !in_list_var(item)) {
            let Some(found) = self.type_of(item)? else {
                continue;
            };
            element = Some(match element {
                None => found,
                Some(current) => match (super::numeric_rank(current), super::numeric_rank(found)) {
                    (Some(have), Some(next)) if next > have => found,
                    _ => current,
                },
            });
        }
        let element = element.unwrap_or(oid::TEXT);
        if !self.pretty {
            output.push("(")?;
        }
        self.wrapped(
            operand,
            Some(Some(element)),
            Parent::ArrayComparison,
            true,
            output,
        )?;
        output.push(if negated {
            " <> ALL (ARRAY["
        } else {
            " = ANY (ARRAY["
        })?;
        let mut first = true;
        for item in items.iter().filter(|item| !in_list_var(item)) {
            if !first {
                output.push(", ")?;
            }
            first = false;
            self.write(item, Some(element), output)?;
        }
        output.push("])")?;
        if !self.pretty {
            output.push(")")?;
        }
        Ok(())
    }

    /// A comparison of two row constructors: pairwise `=` joined by `AND`,
    /// `<>` joined by `OR`, or a `RowCompareExpr` for an ordering.
    pub(super) fn row_comparison(
        &self,
        operator: BinaryOp,
        left: &[&Expr<'_>],
        right: &[&Expr<'_>],
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if left.len() != right.len() || left.is_empty() {
            return output.unsupported();
        }
        let connective = match operator {
            BinaryOp::Eq => Some(BinaryOp::And),
            BinaryOp::NotEq => Some(BinaryOp::Or),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => None,
            _ => return output.unsupported(),
        };
        let Some(connective) = connective else {
            // `get_rule_expr` always parenthesizes a `RowCompareExpr`.
            let symbol = operator.operator_name().unwrap_or_default();
            output.push("(ROW(")?;
            for (index, (item, other)) in left.iter().zip(right).enumerate() {
                if index != 0 {
                    output.push(", ")?;
                }
                let (expected, _) = self.binary_operands(operator, item, other)?;
                self.write(item, expected, output)?;
            }
            output.push(") ")?;
            output.push(symbol)?;
            output.push(" ROW(")?;
            for (index, (item, other)) in left.iter().zip(right).enumerate() {
                if index != 0 {
                    output.push(", ")?;
                }
                let (_, expected) = self.binary_operands(operator, item, other)?;
                self.write(other, expected, output)?;
            }
            return output.push("))");
        };
        if left.len() == 1 {
            return self.binary(operator, left[0], right[0], output);
        }
        self.open(output)?;
        for (index, (item, other)) in left.iter().zip(right).enumerate() {
            if index != 0 {
                output.push(if connective == BinaryOp::And {
                    " AND "
                } else {
                    " OR "
                })?;
            }
            let compared = Expr::Binary {
                source_position: SourcePosition::UNKNOWN,
                operator,
                left: item,
                right: other,
            };
            self.wrapped(
                &compared,
                Some(Some(oid::BOOL)),
                Parent::Connective(connective),
                index == 0,
                output,
            )?;
        }
        self.close(output)
    }

    /// `ROW(...) IS [NOT] DISTINCT FROM ROW(...)`: the fields' pairwise
    /// `IS DISTINCT FROM` joined by `OR`, negated for `IS NOT`.
    pub(super) fn rows_distinct(
        &self,
        left: &[&Expr<'_>],
        right: &[&Expr<'_>],
        negated: bool,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if left.len() != right.len() || left.is_empty() {
            return output.unsupported();
        }
        if negated {
            self.open(output)?;
            output.push("NOT ")?;
            if self.pretty && left.len() > 1 {
                output.push("(")?;
            }
        }
        let several = left.len() > 1;
        if several {
            self.open(output)?;
        }
        for (index, (item, other)) in left.iter().zip(right).enumerate() {
            if index != 0 {
                output.push(" OR ")?;
            }
            self.open(output)?;
            let (left_type, right_type) = self.operand_types(item, other)?;
            let parent = Parent::Operator("IS DISTINCT FROM");
            self.wrapped(item, Some(left_type), parent, true, output)?;
            output.push(" IS DISTINCT FROM ")?;
            self.wrapped(other, Some(right_type), parent, false, output)?;
            self.close(output)?;
        }
        if several {
            self.close(output)?;
        }
        if negated {
            if self.pretty && left.len() > 1 {
                output.push(")")?;
            }
            self.close(output)?;
        }
        Ok(())
    }

    /// `get_func_sql_syntax`: functions the grammar spells in SQL syntax.
    /// `false` when `expression` is not one of them.
    pub(super) fn sql_syntax_call(
        &self,
        expression: &Expr<'_>,
        output: &mut Output<'_>,
    ) -> Result<bool, SqlError> {
        let Expr::Call { name, args, .. } = expression else {
            return Ok(false);
        };
        let argument = |output: &mut Output<'_>, index: usize, expected: Option<i32>| {
            self.wrapped(
                args[index],
                Some(expected),
                Parent::SqlSyntax,
                false,
                output,
            )
        };
        let text = |index: usize| -> Result<Option<i32>, SqlError> {
            Ok(match self.type_of(args[index])? {
                Some(found) if text_like(found) => Some(oid::TEXT),
                Some(found) => Some(found),
                None => Some(oid::TEXT),
            })
        };
        match (*name, args.len()) {
            ("pg_catalog.extract", 2) => {
                let Expr::Str(field) = args[0] else {
                    return Ok(false);
                };
                output.push("EXTRACT(")?;
                output.push(field)?;
                output.push(" FROM ")?;
                self.write(args[1], None, output)?;
                output.push(")")?;
            }
            ("pg_catalog.timezone", 2) => {
                output.push("(")?;
                argument(output, 1, None)?;
                output.push(" AT TIME ZONE ")?;
                let zone = match self.type_of(args[0])? {
                    Some(oid::INTERVAL) => Some(oid::INTERVAL),
                    _ => Some(oid::TEXT),
                };
                argument(output, 0, zone)?;
                output.push(")")?;
            }
            ("pg_catalog.timezone", 1) => {
                output.push("(")?;
                argument(output, 0, None)?;
                output.push(" AT LOCAL)")?;
            }
            ("pg_catalog.overlay", 3 | 4) => {
                let kind = text(0)?;
                output.push("OVERLAY(")?;
                self.write(args[0], kind, output)?;
                output.push(" PLACING ")?;
                self.write(args[1], kind, output)?;
                output.push(" FROM ")?;
                self.write(args[2], Some(oid::INT4), output)?;
                if args.len() == 4 {
                    output.push(" FOR ")?;
                    self.write(args[3], Some(oid::INT4), output)?;
                }
                output.push(")")?;
            }
            ("pg_catalog.position", 2) => {
                let kind = text(0)?;
                output.push("POSITION((")?;
                self.write(args[1], kind, output)?;
                output.push(") IN (")?;
                self.write(args[0], kind, output)?;
                output.push("))")?;
            }
            ("pg_catalog.substring", 2 | 3) => {
                let kind = text(0)?;
                output.push("SUBSTRING(")?;
                self.write(args[0], kind, output)?;
                let pattern = args.len() == 3
                    && !matches!(self.type_of(args[1])?, Some(found) if super::numeric_rank(found) == Some(1));
                if pattern {
                    output.push(" SIMILAR ")?;
                    self.write(args[1], Some(oid::TEXT), output)?;
                    output.push(" ESCAPE ")?;
                    self.write(args[2], Some(oid::TEXT), output)?;
                } else {
                    output.push(" FROM ")?;
                    self.write(args[1], Some(oid::INT4), output)?;
                    if args.len() == 3 {
                        output.push(" FOR ")?;
                        self.write(args[2], Some(oid::INT4), output)?;
                    }
                }
                output.push(")")?;
            }
            ("pg_catalog.btrim" | "pg_catalog.ltrim" | "pg_catalog.rtrim", 1 | 2) => {
                let kind = text(0)?;
                output.push(match *name {
                    "pg_catalog.btrim" => "TRIM(BOTH",
                    "pg_catalog.ltrim" => "TRIM(LEADING",
                    _ => "TRIM(TRAILING",
                })?;
                if args.len() == 2 {
                    output.push(" ")?;
                    self.write(args[1], kind, output)?;
                }
                output.push(" FROM ")?;
                self.write(args[0], kind, output)?;
                output.push(")")?;
            }
            ("pg_catalog.normalize", 2) => {
                let Expr::Str(form) = args[1] else {
                    return Ok(false);
                };
                output.push("NORMALIZE(")?;
                self.write(args[0], Some(oid::TEXT), output)?;
                if !form.eq_ignore_ascii_case("nfc") || self.normalize_form_written(expression) {
                    output.push(", ")?;
                    output.push(super::upper_ascii(form).as_str())?;
                }
                output.push(")")?;
            }
            ("pg_catalog.is_normalized", 2) => {
                let Expr::Str(form) = args[1] else {
                    return Ok(false);
                };
                output.push("(")?;
                argument(output, 0, Some(oid::TEXT))?;
                output.push(" IS ")?;
                if !form.eq_ignore_ascii_case("nfc") {
                    output.push(super::upper_ascii(form).as_str())?;
                    output.push(" ")?;
                }
                output.push("NORMALIZED)")?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// PostgreSQL keeps a written default form; the parser does not record
    /// it, so the default `NFC` is printed only when it differs.
    fn normalize_form_written(&self, _expression: &Expr<'_>) -> bool {
        false
    }

    /// `(base)[subscripts]` (`printSubscripts`): a base other than a column
    /// or field selection is parenthesized.
    pub(super) fn subscript(
        &self,
        base: &Expr<'_>,
        lower: Option<&Expr<'_>>,
        upper: Option<Option<&Expr<'_>>>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let (base, outer) = self.subscript_chain(base);
        let paren = !matches!(base, Expr::Column { .. } | Expr::Field { .. });
        if paren {
            output.push("(")?;
        }
        self.write(base, None, output)?;
        if paren {
            output.push(")")?;
        }
        for (lower, upper) in outer.iter().rev() {
            self.subscript_part(*lower, *upper, output)?;
        }
        self.subscript_part(lower, upper, output)
    }

    /// The subscripts of a chain of subscripts applied to one base, from
    /// the innermost; PostgreSQL prints them as one reference.
    fn subscript_chain<'e>(&self, base: &'e Expr<'e>) -> (&'e Expr<'e>, SubscriptChain<'e>) {
        let mut chain = SubscriptChain::default();
        let mut current = base;
        loop {
            match current {
                Expr::Subscript { base, index } if chain.push((Some(index), None)) => {
                    current = base;
                }
                Expr::Slice { base, lower, upper } if chain.push((*lower, Some(*upper))) => {
                    current = base;
                }
                _ => return (current, chain),
            }
        }
    }

    fn subscript_part(
        &self,
        lower: Option<&Expr<'_>>,
        upper: Option<Option<&Expr<'_>>>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        output.push("[")?;
        if let Some(lower) = lower {
            self.write(lower, Some(oid::INT4), output)?;
        }
        if let Some(upper) = upper {
            output.push(":")?;
            if let Some(upper) = upper {
                self.write(upper, Some(oid::INT4), output)?;
            }
        }
        output.push("]")
    }

    /// `(base).field` (`FieldSelect`): the base is parenthesized unless it
    /// is a subscript or another field selection.
    pub(super) fn field(
        &self,
        base: &Expr<'_>,
        field: &str,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let paren = !matches!(base, Expr::Subscript { .. } | Expr::Field { .. });
        if paren {
            output.push("(")?;
        }
        if let Some(row) = self.whole_row(base) {
            push_identifier(output, row.printed)?;
        } else {
            self.write(base, None, output)?;
        }
        if paren {
            output.push(")")?;
        }
        output.push(".")?;
        push_identifier(output, field)
    }

    /// `(arg COLLATE "name")`.
    pub(super) fn collate(
        &self,
        operand: &Expr<'_>,
        collation: &crate::sql::ast::ParsedCollation<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        self.open(output)?;
        self.wrapped(operand, None, Parent::Collate, true, output)?;
        output.push(" COLLATE ")?;
        let resolved = match collation {
            crate::sql::ast::ParsedCollation::Builtin(collation) => Some(*collation),
            crate::sql::ast::ParsedCollation::Named(name) => {
                self.storage
                    .resolve_collation(name.schema, name.name, self.txid)
            }
        };
        let name = match resolved {
            Some(collation) => crate::sql::catalog::collation_name_by_oid(
                self.storage,
                self.txid,
                collation.oid(),
                self.arena,
            )?,
            None => None,
        };
        let Some(name) = name else {
            return output.unsupported();
        };
        output.push(name)?;
        self.close(output)
    }

    /// An ordered-set aggregate: its direct arguments, then `WITHIN GROUP`.
    pub(super) fn ordered_set_aggregate(
        &self,
        expression: &Expr<'_>,
        output: &mut Output<'_>,
    ) -> Result<bool, SqlError> {
        let Expr::Call {
            kind: CallKind::Routine,
            name,
            args,
            order_by,
            distinct: false,
            over: None,
            filter,
            ..
        } = expression
        else {
            return Ok(false);
        };
        let (direct, aggregated) = match *name {
            "percentile_cont" => {
                let interval = match order_by.first() {
                    Some(order) => self.type_of(order.expression)? == Some(oid::INTERVAL),
                    None => false,
                };
                let kind = if interval { oid::INTERVAL } else { oid::FLOAT8 };
                let direct = match args.first() {
                    Some(argument)
                        if super::element_of(self.type_of(argument)?.unwrap_or(0)).is_some() =>
                    {
                        FLOAT8_ARRAY
                    }
                    _ => oid::FLOAT8,
                };
                (Some(direct), Some(kind))
            }
            "percentile_disc" => {
                let direct = match args.first() {
                    Some(argument)
                        if super::element_of(self.type_of(argument)?.unwrap_or(0)).is_some() =>
                    {
                        FLOAT8_ARRAY
                    }
                    _ => oid::FLOAT8,
                };
                (Some(direct), None)
            }
            "mode" => (None, None),
            "rank" | "dense_rank" | "percent_rank" | "cume_dist" => (None, None),
            _ => return Ok(false),
        };
        if order_by.is_empty() {
            return Ok(false);
        }
        push_identifier(output, name)?;
        output.push("(")?;
        for (index, argument) in args.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            self.write(argument, direct, output)?;
        }
        output.push(") WITHIN GROUP (ORDER BY ")?;
        for (index, order) in order_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let expected = match aggregated {
                Some(kind) => Some(kind),
                None => self.type_of(order.expression)?,
            };
            super::query::sort_group_typed(self, order.expression, expected, output)?;
            super::query::order_direction(order, output)?;
        }
        output.push(")")?;
        self.call_suffix(*filter, None, output)?;
        Ok(true)
    }
}

/// A subscript chain's outer subscripts, innermost last.
#[derive(Default)]
struct SubscriptChain<'e> {
    parts: [(Option<&'e Expr<'e>>, Option<Option<&'e Expr<'e>>>); 6],
    count: usize,
}

impl<'e> SubscriptChain<'e> {
    fn push(&mut self, part: (Option<&'e Expr<'e>>, Option<Option<&'e Expr<'e>>>)) -> bool {
        if self.count == self.parts.len() {
            return false;
        }
        self.parts[self.count] = part;
        self.count += 1;
        true
    }

    fn iter(&self) -> core::slice::Iter<'_, (Option<&'e Expr<'e>>, Option<Option<&'e Expr<'e>>>)> {
        self.parts[..self.count].iter()
    }
}
