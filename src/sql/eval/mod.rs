//! Expression evaluation with PostgreSQL semantics: three-valued logic,
//! NULL propagation through operators, checked integer arithmetic
//! (overflow is an error, not a wrap), and division by zero as SQLSTATE
//! 22012 for integers and floats alike.

use crate::mem::arena::Arena;
use crate::stack_format;
use crate::util::StackStr;
use core::fmt::Write as _;

use super::ast::{BinaryOp, Expr, UnaryOp};
use super::numeric::Numeric;
use super::types::{ColType, Datum};

mod funcs;
mod operators;
pub use operators::compare_datums;
pub(crate) use operators::arithmetic;
use operators::{binary, coerce_unknown, logic, range_mismatch, unary};

#[derive(Debug)]
pub struct SqlError {
    /// Five-character SQLSTATE per PostgreSQL's errcodes table.
    pub sqlstate: &'static str,
    pub message: StackStr<192>,
}

#[macro_export]
macro_rules! sql_err {
    ($state:expr, $($arg:tt)*) => {
        $crate::sql::eval::SqlError {
            sqlstate: $state,
            message: $crate::stack_format!(192, $($arg)*),
        }
    };
}

pub mod sqlstate {
    pub const SYNTAX_ERROR: &str = "42601";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const NUMERIC_OUT_OF_RANGE: &str = "22003";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const TOO_MANY_CONNECTIONS: &str = "53300";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const UNDEFINED_FUNCTION: &str = "42883";
}

/// Resolves column references during evaluation. Statements without a FROM
/// clause use [`NoColumns`].
pub trait ColumnLookup<'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError>;

    /// The named table's row as record fields (name + type + value), or None
    /// for an outer-join null row. Used to build a `Datum::Record` for a
    /// whole-row reference; contexts without join rows reject it.
    fn whole_row_fields(
        &self,
        table: &str,
        _arena: &'a Arena,
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
        Err(sql_err!(
            "0A000",
            "whole-row reference to \"{}\" is not supported in this context",
            table
        ))
    }

    /// A whole-row reference (`t.*` as a value): Ok(true) when the row is
    /// present, Ok(false) when it is an outer-join null row. Contexts without
    /// join rows reject it.
    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        Err(sql_err!(
            "0A000",
            "whole-row reference to \"{}\" is not supported in this context",
            table
        ))
    }

    /// Static column type, if known — used to unify CASE branch types so a
    /// column reference contributes its declared type. Defaults to unknown.
    fn col_type(&self, _qualifier: Option<&str>, _name: &str) -> Option<ColType> {
        None
    }

    /// Whether a whole-row reference to `table` is a scalar (a
    /// set-returning-function scan's single output column) rather than a record.
    /// Defaults to false.
    fn whole_row_is_scalar(&self, _table: &str) -> bool {
        false
    }
}

/// A reference to a lookup is itself a lookup, so `&dyn ColumnLookup` can be
/// passed to the generic `eval`/`where_passes` helpers.
impl<'a, T: ColumnLookup<'a> + ?Sized> ColumnLookup<'a> for &T {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        (**self).lookup(qualifier, name)
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        (**self).whole_row_present(table)
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
        (**self).whole_row_fields(table, arena)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        (**self).col_type(qualifier, name)
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        (**self).whole_row_is_scalar(table)
    }
}

pub struct NoColumns;

impl<'a> ColumnLookup<'a> for NoColumns {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
}

/// No bound parameters (simple queries).
pub const NO_PARAMS: &[Datum<'static>] = &[];

/// Values injected into evaluation by the grouping/aggregation machinery
/// and by pre-evaluated subqueries, matched by AST equality (group keys)
/// or node identity (aggregates, subqueries).
#[derive(Clone, Copy)]
pub struct EvalHooks<'h, 'a> {
    /// (group-by expressions, this group's key values, active-column bitmask).
    /// The bitmask selects which `group_by` columns participate in the current
    /// grouping set (all bits set for a plain `GROUP BY`); it drives `GROUPING()`.
    pub group: Option<(&'h [&'h Expr<'h>], &'h [Datum<'a>], u64)>,
    /// (aggregate-call nodes by address, this group's results).
    pub aggs: Option<(&'h [*const Expr<'h>], &'h [Datum<'a>])>,
    /// (subquery nodes by address, their pre-evaluated results).
    pub subs: Option<&'h SubqueryValues<'h, 'a>>,
    /// (window-function call nodes by address, the current row's values).
    pub windows: Option<(&'h [*const Expr<'h>], &'h [Datum<'a>])>,
    /// Resolves catalog OIDs to reconstructed definition text for
    /// `pg_get_indexdef` (psql `\d`). A trait object so evaluation stays
    /// decoupled from `Storage`; `None` outside catalog-backed queries. Its
    /// generic method keeps `EvalHooks` variance unchanged.
    pub catalog: Option<&'h dyn CatalogAccess>,
    /// The current 1-based expansion index of a set-returning function
    /// (`_pg_expandarray`) in the projection; `None` outside such expansion.
    pub srf_index: Option<usize>,
}

/// Reconstructs catalog definition text (index / constraint DDL) that psql's
/// `\d` obtains through functions like `pg_get_indexdef`. Implemented over
/// `Storage`; abstract here so `eval` need not depend on the catalog.
pub trait CatalogAccess {
    /// The index definition for this OID: `col == 0` gives the whole
    /// `btree (col, ...)` form; `col > 0` gives the name of that 1-based indexed
    /// column. `None` if no such index is known.
    fn index_def<'a>(
        &self,
        oid: i32,
        col: usize,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError>;
    /// The `FOREIGN KEY (...) REFERENCES ...` definition of the constraint with
    /// this OID, or `None` if no such foreign-key constraint is known.
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena)
        -> Result<Option<&'a str>, SqlError>;
    /// The relation name for an OID, for rendering `oid::regclass`.
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError>;
    /// The OID of the relation named `name`, for `'relname'::regclass`.
    fn reloid(&self, name: &str) -> Option<i32>;
}

/// Pre-evaluated (uncorrelated) subquery results.
pub struct SubqueryValues<'h, 'a> {
    /// Scalar subqueries: (node address, value, type-witness datum — the
    /// result column's type even when the value is NULL, for describes).
    pub scalars: &'h [(*const Expr<'h>, Datum<'a>, Datum<'a>)],
    /// IN-subqueries: (node address, member list, saw a NULL member, a
    /// type-witness datum of the subquery's result column). The witness lets
    /// the operand be coerced to the column type even when the set is empty or
    /// all-NULL, matching PostgreSQL (which type-checks `x IN (...)` regardless
    /// of contents).
    pub lists: &'h [(*const Expr<'h>, &'a [Datum<'a>], bool, Datum<'a>)],
}

pub const NO_HOOKS: EvalHooks<'static, 'static> = EvalHooks {
    group: None,
    aggs: None,
    subs: None,
    windows: None, catalog: None, srf_index: None };

pub fn eval<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
) -> Result<Datum<'a>, SqlError> {
    eval_full(expression, arena, params, row, &NO_HOOKS)
}

/// Surfaces errors from every maximal constant subexpression, as
/// PostgreSQL's plan-time constant folding does: `SELECT 1/0` and
/// `... OR 0.0/0.0 > 1` error even when no row would reach them. Constant
/// subtrees are evaluated once here; per-row evaluation (with short-circuit)
/// handles the rest.
pub fn check_constant_errors<'a>(expression: &Expr<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    fold_check(expression, arena).map(|_| ())
}

/// The simplification-aware core of [`check_constant_errors`], mirroring
/// PostgreSQL's `eval_const_expressions`: it folds constant subexpressions
/// (surfacing their errors) but simplifies `A AND FALSE`→`FALSE`,
/// `A OR TRUE`→`TRUE`, and constant `CASE` arms — so a constant error inside a
/// branch that simplification *drops* is not surfaced (PostgreSQL evaluates
/// `... WHERE FALSE AND (id > (-1 % 0))` to no rows, never folding `-1 % 0`).
/// Returns the folded boolean value when the expression provably reduces to
/// one, else `None`.
fn fold_check<'a>(expression: &Expr<'a>, arena: &'a Arena) -> Result<Option<bool>, SqlError> {
    use super::ast::BinaryOp;
    if expression.is_constant() {
        // A fully-constant subtree folds eagerly; its error surfaces here.
        return Ok(match eval(expression, arena, NO_PARAMS, &NoColumns)? {
            Datum::Bool(b) => Some(b),
            _ => None,
        });
    }
    match expression {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_)
        | Expr::NumericLit(_) | Expr::Str(_) | Expr::BitLit(_) | Expr::Column { .. }
        | Expr::WholeRow(_)
        | Expr::Param(_) | Expr::DefaultMarker => Ok(None),
        // Boolean connectives short-circuit like PostgreSQL's folding: a FALSE
        // (AND) / TRUE (OR) operand settles the result and drops the sibling,
        // so the sibling's constant errors are never surfaced.
        Expr::Binary { operator: BinaryOp::And, left, right } => {
            // FALSE settles AND; otherwise the result is known only when both
            // sides fold to TRUE (`TRUE AND TRUE` = TRUE).
            let l = fold_check(left, arena)?;
            if l == Some(false) {
                return Ok(Some(false));
            }
            let r = fold_check(right, arena)?;
            if r == Some(false) {
                return Ok(Some(false));
            }
            Ok(match (l, r) {
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Binary { operator: BinaryOp::Or, left, right } => {
            // TRUE settles OR; otherwise the result is known only when both
            // sides fold to FALSE (`FALSE OR FALSE` = FALSE) — so a constant
            // OR of dead predicates lets a CASE arm drop.
            let l = fold_check(left, arena)?;
            if l == Some(true) {
                return Ok(Some(true));
            }
            let r = fold_check(right, arena)?;
            if r == Some(true) {
                return Ok(Some(true));
            }
            Ok(match (l, r) {
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        // NOT propagates a folded boolean, so `NOT (x AND FALSE)` simplifies to
        // TRUE — which lets a CASE truncate exactly as PostgreSQL's plan-time
        // simplification does.
        Expr::Unary { operator: super::ast::UnaryOp::Not, operand } => {
            Ok(fold_check(operand, arena)?.map(|b| !b))
        }
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => {
            fold_check(operand, arena)?;
            Ok(None)
        }
        Expr::Binary { left, right, .. } => {
            fold_check(left, arena)?;
            fold_check(right, arena)?;
            Ok(None)
        }
        Expr::InList { operand, list, .. } => {
            fold_check(operand, arena)?;
            for e in *list {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Between { operand, low, high, .. } => {
            fold_check(operand, arena)?;
            fold_check(low, arena)?;
            fold_check(high, arena)?;
            Ok(None)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            fold_check(operand, arena)?;
            fold_check(pattern, arena)?;
            Ok(None)
        }
        Expr::Case { operand, whens, otherwise } => {
            if let Some(o) = operand {
                // Operand form (`CASE x WHEN v ...`): the WHENs are compared to
                // x, not boolean conditions, so no arm is dropped by folding.
                fold_check(o, arena)?;
                for (c, r) in *whens {
                    fold_check(c, arena)?;
                    fold_check(r, arena)?;
                }
            } else {
                // Searched form: a constant-FALSE WHEN drops its THEN; a
                // constant-TRUE WHEN makes the CASE that THEN and drops the
                // rest — matching PostgreSQL, so a division in a dead arm
                // (`WHEN 'a' LIKE 'b' THEN 2/0`) is never folded.
                for (c, r) in *whens {
                    match fold_check(c, arena)? {
                        Some(false) => continue,
                        Some(true) => {
                            fold_check(r, arena)?;
                            return Ok(None);
                        }
                        None => {
                            fold_check(r, arena)?;
                        }
                    }
                }
            }
            if let Some(e) = otherwise {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Call { args, .. } => {
            for a in *args {
                fold_check(a, arena)?;
            }
            Ok(None)
        }
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(None),
        Expr::Array(items) => {
            for e in *items {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Subscript { base, index } => {
            fold_check(base, arena)?;
            fold_check(index, arena)?;
            Ok(None)
        }
        Expr::Field { base, .. } => {
            fold_check(base, arena)?;
            Ok(None)
        }
        Expr::AnyAll { operand, array, .. } => {
            fold_check(operand, arena)?;
            fold_check(array, arena)?;
            Ok(None)
        }
    }
}

pub fn eval_full<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    // GROUPING(arg, ...): each argument contributes one bit (1 if that column
    // is NOT part of the current grouping set), most significant first.
    if let Expr::Call { name, args, .. } = expression
        && name.eq_ignore_ascii_case("grouping")
    {
        let Some((exprs, _, mask)) = hooks.group else {
            return Err(sql_err!(
                "42803",
                "GROUPING must be used with grouping sets or GROUP BY"
            ));
        };
        let mut result = 0i32;
        for arg in args.iter() {
            let idx = exprs.iter().position(|g| **g == **arg).ok_or_else(|| {
                sql_err!("42803", "arguments to GROUPING must be grouping expressions of the associated query level")
            })?;
            let grouped = mask & (1u64 << idx) != 0;
            result = (result << 1) | i32::from(!grouped);
        }
        return Ok(Datum::Int4(result));
    }
    // Group-key substitution: any expression equal to a GROUP BY key
    // evaluates to the group's value.
    if let Some((exprs, values, _mask)) = hooks.group {
        for (g, v) in exprs.iter().zip(values) {
            if **g == *expression {
                return Ok(*v);
            }
        }
    }
    match *expression {
        Expr::Null => Ok(Datum::Null),
        // A whole-row value: NULL for an outer-join null row, else a non-null
        // marker — consumable only by count() (type analysis rejects the rest).
        Expr::WholeRow(table) => match row.whole_row_fields(table, arena)? {
            // A function scan's whole row is its single scalar column.
            Some(fields) if row.whole_row_is_scalar(table) => {
                Ok(fields.first().map(|f| f.value).unwrap_or(Datum::Null))
            }
            Some(fields) => Ok(Datum::Record(fields)),
            None => Ok(Datum::Null), // outer-join null row
        },
        Expr::Bool(b) => Ok(Datum::Bool(b)),
        Expr::Int(v) => Ok(if let Ok(small) = i32::try_from(v) {
            Datum::Int4(small)
        } else {
            Datum::Int8(v)
        }),
        Expr::Float(v) => Ok(Datum::Float8(v)),
        Expr::NumericLit(s) => Ok(Datum::Numeric(Numeric::parse(s, arena)?)),
        Expr::Str(s) => Ok(Datum::Text(s)),
        Expr::BitLit(s) => Ok(Datum::Bit { bits: s, varying: false }),
        Expr::Column { qualifier, name } => match row.lookup(qualifier, name) {
            Ok(v) => Ok(v),
            // A bare name that is not a column but names a FROM item is a
            // whole-row reference (`SELECT t FROM t`, `row_to_json(r)`).
            Err(e) if qualifier.is_none() && e.sqlstate == sqlstate::UNDEFINED_COLUMN => {
                match row.whole_row_fields(name, arena) {
                    Ok(Some(fields)) if row.whole_row_is_scalar(name) => {
                        Ok(fields.first().map(|f| f.value).unwrap_or(Datum::Null))
                    }
                    Ok(Some(fields)) => Ok(Datum::Record(fields)),
                    Ok(None) => Ok(Datum::Null),
                    Err(_) => Err(e),
                }
            }
            Err(e) => Err(e),
        },
        Expr::Param(n) => params
            .get(n as usize - 1)
            .copied()
            .ok_or_else(|| sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "there is no parameter ${}",
                n
            )),
        Expr::Unary { operator, operand } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            unary(operator, v, arena)
        }
        Expr::Binary { operator: BinaryOp::And, left, right } => {
            // PostgreSQL simplifies `x AND FALSE` to FALSE and short-circuits a
            // scan qual in a cost order that is not fixed, so a FALSE operand
            // determines the result even when the *other* operand would error at
            // runtime. Match that: a definite FALSE on either side yields FALSE
            // and absorbs the sibling's runtime error. A constant erroring
            // operand still errors — `check_constant_errors` surfaces it before
            // we get here, so anything that reaches this point is per-row.
            eval_logic_short_circuit(BinaryOp::And, left, right, arena, params, row, hooks)
        }
        Expr::Binary { operator: BinaryOp::Or, left, right } => {
            // Dual of AND: a definite TRUE on either side yields TRUE and
            // absorbs the sibling's runtime error (PostgreSQL's `x OR TRUE`).
            eval_logic_short_circuit(BinaryOp::Or, left, right, arena, params, row, hooks)
        }
        Expr::Binary { operator, left, right } => {
            let l = eval_full(left, arena, params, row, hooks)?;
            let r = eval_full(right, arena, params, row, hooks)?;
            // `array || NULL` resolution depends on the NULL operand's static
            // type, which the datum has lost — resolve it here where the
            // expression is still available.
            if operator == BinaryOp::Concat
                && let Some(d) = array_null_concat(l, r, left, right, row, arena)?
            {
                return Ok(d);
            }
            // Track which side is an "unknown" literal (a string literal or a
            // parameter): only those coerce to the other operand's type, as
            // PostgreSQL does. A real text value never coerces to a number.
            binary(operator, l, r, is_unknown_literal(left), is_unknown_literal(right), arena)
        }
        Expr::Cast { operand, type_name, type_mod } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            // `oid::regclass` displays as the relation name (needs catalog
            // access); other reg-type casts fall through to the generic path.
            if type_name.eq_ignore_ascii_case("regclass")
                && let Some(cat) = hooks.catalog
            {
                match v {
                    // `oid::regclass` renders as the relation name.
                    Datum::Int4(_) | Datum::Int8(_) => {
                        let oid = if let Datum::Int8(x) = v { x as i32 } else if let Datum::Int4(x) = v { x } else { 0 };
                        if let Some(name) = cat.relname(oid, arena)? {
                            return Ok(Datum::Text(name));
                        }
                    }
                    // `'relname'::regclass` resolves to the relation's OID, so a
                    // catalog query's `attrelid = 'tbl'::regclass` compares OIDs
                    // (pgx and most tools introspect this way).
                    Datum::Text(name) => {
                        if let Some(oid) = cat.reloid(name) {
                            return Ok(Datum::Int4(oid));
                        }
                        return Err(sql_err!(
                            "42P01",
                            "relation \"{}\" does not exist",
                            name
                        ));
                    }
                    _ => {}
                }
            }
            // integer -> bit(n): the low n bits, right-aligned. This is
            // PostgreSQL's int-to-bit conversion, distinct from bit-string
            // length coercion (which left-aligns), so it is handled here where
            // the source type is known.
            if let Some(ct @ ColType::Bit { varying }) = ColType::from_sql_name(type_name)
                && matches!(v, Datum::Int4(_) | Datum::Int8(_))
            {
                let _ = ct;
                let n = if type_mod >= 4 { (type_mod - 4) as usize } else { 1 };
                let value = match v {
                    Datum::Int4(x) => x as u32 as u64,
                    Datum::Int8(x) => x as u64,
                    _ => unreachable!(),
                };
                return Ok(Datum::Bit { bits: int_to_bits(value, n, arena)?, varying });
            }
            let v = cast(v, type_name, arena)?;
            // `::numeric(p,s)` / `::varchar(n)`: enforce the modifier on the
            // cast result exactly as a column of that type would.
            if type_mod != -1
                && let Some(ct) = ColType::from_sql_name(type_name)
            {
                return super::exec::apply_cast_typmod(v, ct, type_mod, arena);
            }
            Ok(v)
        }
        Expr::IsNull { operand, negated } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            // A row `IS NULL` is true only when *every* field is null, and
            // `IS NOT NULL` only when every field is non-null — so a mixed row
            // is false for both (PostgreSQL's row null-test, not a plain
            // negation).
            if let Datum::Record(fields) = v {
                let result = if negated {
                    fields.iter().all(|f| !f.value.is_null())
                } else {
                    fields.iter().all(|f| f.value.is_null())
                };
                return Ok(Datum::Bool(result));
            }
            Ok(Datum::Bool(v.is_null() != negated))
        }
        Expr::Call { name, args, star, distinct, over, .. } => {
            // A window-function call resolves to this row's precomputed value.
            if over.is_some()
                && let Some((nodes, values)) = hooks.windows
            {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            if let Some((nodes, values)) = hooks.aggs {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            if distinct {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "DISTINCT is only supported inside aggregate functions"
                ));
            }
            call(name, args, star, arena, params, row, hooks)
        }
        Expr::InList { operand, list, negated } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            if v.is_null() {
                return Ok(Datum::Null);
            }
            // SQL semantics: x IN (..) with no match but a NULL member is
            // NULL, not false.
            let mut saw_null = false;
            for item in list {
                let member = eval_full(item, arena, params, row, hooks)?;
                if member.is_null() {
                    saw_null = true;
                    continue;
                }
                let l = coerce_unknown(v, &member)?;
                let r = coerce_unknown(member, &l)?;
                if compare_datums(&l, &r)?.is_eq() {
                    return Ok(Datum::Bool(!negated));
                }
            }
            Ok(if saw_null {
                Datum::Null
            } else {
                Datum::Bool(negated)
            })
        }
        Expr::Between { operand, low, high, negated } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let lo = eval_full(low, arena, params, row, hooks)?;
            let hi = eval_full(high, arena, params, row, hooks)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Datum::Null);
            }
            let a = coerce_unknown(v, &lo)?;
            let lo = coerce_unknown(lo, &a)?;
            let hi = coerce_unknown(hi, &a)?;
            let inside = compare_datums(&a, &lo)?.is_ge() && compare_datums(&a, &hi)?.is_le();
            Ok(Datum::Bool(inside != negated))
        }
        Expr::Like { operand, pattern, negated, case_insensitive } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let p = eval_full(pattern, arena, params, row, hooks)?;
            match (v, p) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::Text(s), Datum::Text(pat)) => {
                    let matched = like_match(s, pat, case_insensitive);
                    Ok(Datum::Bool(matched != negated))
                }
                (l, r) => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "LIKE requires text operands, got {:?} and {:?}",
                    l,
                    r
                )),
            }
        }
        Expr::Match { operand, pattern, negated, case_insensitive } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let p = eval_full(pattern, arena, params, row, hooks)?;
            match (v, p) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::Text(s), Datum::Text(pat)) => {
                    let matched = super::regex::regex_search(pat, s, case_insensitive)?;
                    Ok(Datum::Bool(matched != negated))
                }
                (l, r) => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "regex match requires text operands, got {:?} and {:?}",
                    l,
                    r
                )),
            }
        }
        Expr::Case { operand, whens, otherwise } => {
            let scrutinee = match operand {
                Some(operator) => Some(eval_full(operator, arena, params, row, hooks)?),
                None => None,
            };
            // PostgreSQL unifies all branch result types to one common type;
            // compute it so every row's value has the same type as the
            // column PostgreSQL would report.
            let unified = case_result_type(whens, &otherwise, row);
            let chosen = 'chosen: {
                for (cond, result) in whens {
                    let hit = match &scrutinee {
                        Some(s) => {
                            let c = eval_full(cond, arena, params, row, hooks)?;
                            if s.is_null() || c.is_null() {
                                false
                            } else {
                                let l = coerce_unknown(*s, &c)?;
                                let r = coerce_unknown(c, &l)?;
                                compare_datums(&l, &r)?.is_eq()
                            }
                        }
                        None => matches!(
                            eval_full(cond, arena, params, row, hooks)?,
                            Datum::Bool(true)
                        ),
                    };
                    if hit {
                        break 'chosen eval_full(result, arena, params, row, hooks)?;
                    }
                }
                match otherwise {
                    Some(e) => eval_full(e, arena, params, row, hooks)?,
                    None => Datum::Null,
                }
            };
            match unified {
                Some(t) if !chosen.is_null() => cast_to(chosen, t, arena),
                _ => Ok(chosen),
            }
        }
        Expr::DefaultMarker => Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "DEFAULT is only allowed in INSERT value lists"
        )),
        Expr::Subquery(_) | Expr::ArraySubquery(_) => {
            if let Some(subs) = hooks.subs {
                for (node, v, _) in subs.scalars {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "subqueries are not allowed in this context (or are correlated)"
            ))
        }
        Expr::InSubquery { operand, negated, .. } => {
            let Some(subs) = hooks.subs else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "subqueries are not allowed in this context"
                ));
            };
            let mut found: Option<(&[Datum], bool, Datum)> = None;
            for (node, list, saw_null, witness) in subs.lists {
                if core::ptr::eq(*node, expression as *const _) {
                    found = Some((list, *saw_null, *witness));
                    break;
                }
            }
            let Some((list, saw_null, witness)) = found else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "subqueries are not allowed in this context (or are correlated)"
                ));
            };
            // Coerce the operand to the subquery's column type first: PostgreSQL
            // type-checks `x IN (...)` regardless of the set's contents, so a
            // string literal that cannot become the column type errors even
            // against an empty or all-NULL set.
            let v = eval_full(operand, arena, params, row, hooks)?;
            let v = coerce_unknown(v, &witness)?;
            // A bit string is comparable only to another bit string; reject a
            // bit-vs-other membership test up front (PostgreSQL type-checks the
            // operand against the column type even over an empty set).
            if matches!(v, Datum::Bit { .. }) && !matches!(witness, Datum::Bit { .. } | Datum::Null) {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: bit = {}",
                    type_name_of(&witness)
                ));
            }
            // `x IN (subquery)` is `x = ANY (subquery)`. Over an empty set the
            // result is a constant FALSE (TRUE for NOT IN) regardless of x —
            // even a NULL x — so the empty case precedes the null short-circuit.
            if list.is_empty() {
                return Ok(Datum::Bool(negated));
            }
            if v.is_null() {
                return Ok(Datum::Null);
            }
            for member in list {
                if member.is_null() {
                    continue;
                }
                let l = coerce_unknown(v, member)?;
                let r = coerce_unknown(*member, &l)?;
                if compare_datums(&l, &r)?.is_eq() {
                    return Ok(Datum::Bool(!negated));
                }
            }
            Ok(if saw_null { Datum::Null } else { Datum::Bool(negated) })
        }
        Expr::Exists(_) => {
            // EXISTS results are pre-evaluated (uncorrelated) or evaluated per
            // outer row (correlated) and stored as a boolean scalar keyed by
            // node identity, alongside scalar subqueries.
            if let Some(subs) = hooks.subs {
                for (node, v, _) in subs.scalars {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXISTS is not allowed in this context"
            ))
        }
        Expr::Array(items) => {
            // Evaluate each element, unify to a common element type, build blob.
            let mut vals = [Datum::Null; 256];
            if items.len() > vals.len() {
                return Err(sql_err!("54000", "array constructor too large"));
            }
            let mut element: Option<super::types::ArrElem> = None;
            for (i, e) in items.iter().enumerate() {
                let v = eval_full(e, arena, params, row, hooks)?;
                if let Some(el) = super::types::ArrElem::from_datum(&v) {
                    element = Some(element.map_or(el, |acc| unify_arr_elem(acc, el)));
                }
                vals[i] = v;
            }
            let element = element.unwrap_or(super::types::ArrElem::Int4);
            // Coerce each element to the unified type.
            let ct = element.to_coltype();
            for v in vals.iter_mut().take(items.len()) {
                if !v.is_null() {
                    *v = cast_to(*v, ct, arena)?;
                }
            }
            Ok(Datum::Array { element, raw: super::array::build(&vals[..items.len()], arena)? })
        }
        Expr::Subscript { base, index } => {
            let b = eval_full(base, arena, params, row, hooks)?;
            let i = eval_full(index, arena, params, row, hooks)?;
            let index = match i {
                Datum::Int4(x) => x as i64,
                Datum::Int8(x) => x,
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("array subscript must be integer", &i)),
            };
            match b {
                Datum::Array { element, raw } => {
                    // PostgreSQL array subscripts are 1-based.
                    if index < 1 {
                        return Ok(Datum::Null);
                    }
                    Ok(super::array::get(raw, element, (index - 1) as usize).unwrap_or(Datum::Null))
                }
                Datum::Null => Ok(Datum::Null),
                _ => Err(type_mismatch("cannot subscript a non-array", &b)),
            }
        }
        Expr::Field { base, field } => {
            let b = eval_full(base, arena, params, row, hooks)?;
            match b {
                Datum::Null => Ok(Datum::Null),
                // A record: select the field by name (records carry lowercase
                // field names — `f1,f2,…` for ROW(), column names for a row).
                Datum::Record(fields) => match fields.iter().find(|f| f.name.eq_ignore_ascii_case(field)) {
                    Some(f) => Ok(f.value),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "could not identify column \"{}\" in record data type",
                        field
                    )),
                },
                // The `_pg_expandarray` result is encoded as the 2-element array
                // `[x, n]`; `.x`/`.f1` is the element and `.n`/`.f2` the ordinal.
                Datum::Array { element, raw } => {
                    let index = if field.eq_ignore_ascii_case("x") || field.eq_ignore_ascii_case("f1")
                    {
                        0
                    } else if field.eq_ignore_ascii_case("n") || field.eq_ignore_ascii_case("f2") {
                        1
                    } else {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "field \"{}\" not found",
                            field
                        ));
                    };
                    Ok(super::array::get(raw, element, index).unwrap_or(Datum::Null))
                }
                _ => Err(type_mismatch("field access on a non-composite value", &b)),
            }
        }
        Expr::AnyAll { operand, operator, array, all } => {
            let lhs = eval_full(operand, arena, params, row, hooks)?;
            let array = eval_full(array, arena, params, row, hooks)?;
            let (element, raw) = match array {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                // An unknown literal on the array side (`= ANY('{1,2}')`) is cast
                // to an array of the left operand's element type, as PostgreSQL
                // resolves it.
                Datum::Text(s) => {
                    let element =
                        super::types::ArrElem::from_datum(&lhs).unwrap_or(super::types::ArrElem::Text);
                    let raw = super::array::parse_literal(s, element, arena)?;
                    (element, raw)
                }
                _ => return Err(type_mismatch("ANY/ALL requires an array", &array)),
            };
            let n = super::array::len(raw);
            let mut saw_null = false;
            for i in 0..n {
                let el = super::array::get(raw, element, i).unwrap_or(Datum::Null);
                match binary(operator, lhs, el, false, false, arena)? {
                    Datum::Bool(true) if !all => return Ok(Datum::Bool(true)),
                    Datum::Bool(false) if all => return Ok(Datum::Bool(false)),
                    Datum::Null => saw_null = true,
                    _ => {}
                }
            }
            if saw_null {
                Ok(Datum::Null)
            } else {
                // ANY with no match is false; ALL with no counterexample is true.
                Ok(Datum::Bool(all))
            }
        }
    }
}

/// The wider of two array element types (for `ARRAY[...]` type unification).
fn unify_arr_elem(a: super::types::ArrElem, b: super::types::ArrElem) -> super::types::ArrElem {
    use super::types::ArrElem::*;
    match (a, b) {
        (x, y) if x == y => x,
        (Float8, _) | (_, Float8) => Float8,
        (Numeric, _) | (_, Numeric) => Numeric,
        (Int8, _) | (_, Int8) => Int8,
        (Text, _) | (_, Text) => Text,
        _ => a,
    }
}

/// Translates a SQL `SIMILAR TO` pattern into a POSIX regular expression
/// anchored to the whole string. `%`/`_` become `.*`/`.`; the SIMILAR TO
/// metacharacters (`| * + ? ( ) [ ] { }`) pass through; characters that are
/// literal in SIMILAR TO but special in POSIX (`. ^ $`) are escaped; `\` escapes
/// the next character. Inside a `[...]` bracket expression everything is literal.
fn similar_to_posix(pattern: &str, buffer: &mut crate::util::StackStr<256>) -> Result<(), SqlError> {
    use core::fmt::Write as _;
    let _ = buffer.write_char('^');
    let mut chars = pattern.chars();
    let mut in_bracket = false;
    while let Some(c) = chars.next() {
        if in_bracket {
            let _ = buffer.write_char(c);
            if c == ']' {
                in_bracket = false;
            }
            continue;
        }
        match c {
            '\\' => {
                let _ = buffer.write_char('\\');
                if let Some(next) = chars.next() {
                    let _ = buffer.write_char(next);
                }
            }
            '%' => {
                let _ = buffer.write_str(".*");
            }
            '_' => {
                let _ = buffer.write_char('.');
            }
            '.' | '^' | '$' => {
                let _ = buffer.write_char('\\');
                let _ = buffer.write_char(c);
            }
            '[' => {
                let _ = buffer.write_char('[');
                in_bracket = true;
            }
            _ => {
                let _ = buffer.write_char(c);
            }
        }
    }
    let _ = buffer.write_char('$');
    if buffer.is_truncated() {
        return Err(sql_err!("22026", "SIMILAR TO pattern is too long"));
    }
    Ok(())
}

/// SQL LIKE: `%` matches any run (including empty), `_` exactly one
/// character, `\` escapes the next pattern character. Iterative
/// two-pointer match with backtracking to the last `%`; allocation-free.
pub fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    fn next_char(s: &str, at: usize) -> Option<(char, usize)> {
        s[at..].chars().next().map(|c| (c, at + c.len_utf8()))
    }
    let eq = |a: char, b: char| {
        if case_insensitive {
            a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };

    let mut t = 0usize;
    let mut p = 0usize;
    let mut star: Option<(usize, usize)> = None; // (pattern pos after %, text pos)

    loop {
        if let Some((pc, p_next)) = next_char(pattern, p) {
            match pc {
                '%' => {
                    star = Some((p_next, t));
                    p = p_next;
                    continue;
                }
                '_' => {
                    if let Some((_, t_next)) = next_char(text, t) {
                        t = t_next;
                        p = p_next;
                        continue;
                    }
                }
                '\\' => {
                    let (want, after) = match next_char(pattern, p_next) {
                        Some((c, n)) => (c, n),
                        None => ('\\', p_next), // trailing backslash: literal
                    };
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, want) {
                            t = t_next;
                            p = after;
                            continue;
                        }
                }
                _ => {
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, pc) {
                            t = t_next;
                            p = p_next;
                            continue;
                        }
                }
            }
        } else if t >= text.len() {
            return true;
        }
        // Mismatch (or pattern exhausted with text left): backtrack.
        match star {
            Some((star_p, star_t)) => match next_char(text, star_t) {
                Some((_, nt)) => {
                    star = Some((star_p, nt));
                    t = nt;
                    p = star_p;
                }
                None => return false,
            },
            None => return false,
        }
    }
}

fn call<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let arity = |n: usize| -> Result<(), SqlError> {
        if args.len() != n || star {
            Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ))
        } else {
            Ok(())
        }
    };
    if let Some(result) = funcs::bytea::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::math::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::string::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::datetime::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::json::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::array::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::range::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::regex::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::system::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::conditional::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::misc::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    match name {
        "count" | "sum" | "avg" | "min" | "max" | "bool_and" | "bool_or" | "every"
        | "string_agg" => Err(sql_err!(
            "42803",
            "aggregate functions are not allowed here"
        )),
        // Set-returning functions: during expansion `hooks.srf_index` (1-based)
        // selects which element/value this output row carries.
        "unnest" => {
            arity(1)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let (element, raw) = match a {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("unnest requires an array", &a)),
            };
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            Ok(super::array::get(raw, element, k - 1).unwrap_or(Datum::Null))
        }
        "generate_series" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?
                as i64;
            let start = eval_full(args[0], arena, params, row, hooks)?;
            let stop = eval_full(args[1], arena, params, row, hooks)?;
            let step = if args.len() == 3 {
                eval_full(args[2], arena, params, row, hooks)?
            } else {
                Datum::Int4(1)
            };
            if let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) {
                let v = s + (k - 1) * st;
                // Past the end of this series (a shorter SRF paired with a longer
                // one runs out): NULL, matching PostgreSQL's lockstep expansion.
                if st == 0 || (st > 0 && v > e) || (st < 0 && v < e) {
                    return Ok(Datum::Null);
                }
                // int4 unless an argument is int8 or the value overflows int4.
                let wide = matches!(start, Datum::Int8(_)) || matches!(step, Datum::Int8(_));
                return Ok(if !wide && i32::try_from(v).is_ok() {
                    Datum::Int4(v as i32)
                } else {
                    Datum::Int8(v)
                });
            }
            // Temporal series: date/timestamp[tz] start with an interval step.
            let Some((base, kind)) = timestamp_series_start(&start) else {
                if start.is_null() {
                    return Ok(Datum::Null);
                }
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "generate_series is supported for integer and timestamp arguments"
                ));
            };
            let stop_micros = timestamp_series_start(&cast_to(stop, kind.coltype(), arena)?).map(|(m, _)| m);
            // The step is an interval — coerce a bare string literal, as
            // PostgreSQL's function resolution does.
            let Datum::Interval(step_iv) = cast_to(step, ColType::Interval, arena)? else {
                return Ok(Datum::Null);
            };
            // Iterative addition — calendar month/day arithmetic does not
            // distribute over multiplication, so the k-th value is `start`
            // stepped k-1 times (matching PostgreSQL).
            let mut v = base;
            for _ in 1..k {
                v = super::datetime::add_interval(v, step_iv);
            }
            // Past the end of this series (lockstep with a longer SRF): NULL.
            let positive = interval_is_positive(step_iv);
            match stop_micros {
                Some(stop) if (positive && v > stop) || (!positive && v < stop) => Ok(Datum::Null),
                Some(_) => Ok(kind.datum(v)),
                None => Ok(Datum::Null),
            }
        }
        // Set-returning `regexp_matches(string, pattern [, flags])`: for the
        // current expansion index k, the capture groups of the k-th match as a
        // text[] (or the whole match when the pattern has no groups). NULLs
        // (arguments or non-participating groups) follow PostgreSQL.
        "regexp_matches" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            let (Some(string), Some(pattern)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let flags = if args.len() == 3 {
                text_arg(name, args, 2, arena, params, row, hooks)?.unwrap_or("")
            } else {
                ""
            };
            let (global, ci) = regexp_flags(flags)?;
            let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
            let mut from = 0usize;
            let mut count = 0usize;
            loop {
                let Some(((mstart, mend), ng)) =
                    super::regex::find_captures(pattern, string, from, ci, &mut spans)?
                else {
                    return Ok(Datum::Null);
                };
                count += 1;
                if count == k {
                    // No capture groups: the whole match is the single element.
                    let mut elems = [Datum::Null; super::regex::MAX_GROUPS];
                    let n = if ng == 0 {
                        elems[0] = Datum::Text(&string[mstart..mend]);
                        1
                    } else {
                        for (i, span) in spans[..ng].iter().enumerate() {
                            elems[i] = if span.0 < 0 {
                                Datum::Null
                            } else {
                                Datum::Text(&string[span.0 as usize..span.1 as usize])
                            };
                        }
                        ng
                    };
                    return Ok(Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(&elems[..n], arena)?,
                    });
                }
                if !global {
                    return Ok(Datum::Null);
                }
                from = if mend > mstart { mend } else { mend + 1 };
                if from > string.len() {
                    return Ok(Datum::Null);
                }
            }
        }
        // Set-returning `_pg_expandarray(array)` yields, for the current expansion
        // index k, the composite `(x, n)` = (array[k], k), encoded as `[x, n]`.
        "_pg_expandarray" => {
            arity(1)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let (element, raw) = match a {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("_pg_expandarray requires an array", &a)),
            };
            let k = hooks.srf_index.unwrap_or(1);
            let x = super::array::get(raw, element, k - 1).unwrap_or(Datum::Null);
            let comp = [x, Datum::Int4(k as i32)];
            Ok(Datum::Array {
                element: super::types::ArrElem::Int4,
                raw: super::array::build(&comp, arena)?,
            })
        }
        // Set-returning `jsonb_object_keys(obj)` / `json_object_keys(obj)`
        // yield each key of the object as one text row.
        // Set-returning `regexp_split_to_table(source, pattern [, flags])`:
        // the k-th split piece for the current expansion index.
        "regexp_split_to_table" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let (Some(src), Some(pat)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let case_insensitive = if args.len() == 3 {
                let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                regexp_flags(flags)?.1
            } else {
                false
            };
            let pieces = regex_split_pub(src, pat, case_insensitive, arena)?;
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            Ok(pieces.get(k - 1).copied().unwrap_or(Datum::Null))
        }
        // Set-returning `generate_subscripts(array, dim)`: the k-th 1-based index
        // of the array along `dim` (only dimension 1 exists here).
        "generate_subscripts" => {
            arity(2)?;
            let raw = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Array { raw, .. } => raw,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("generate_subscripts requires an array", &other)),
            };
            let dim = match eval_full(args[1], arena, params, row, hooks)? {
                Datum::Int4(v) => v as i64,
                Datum::Int8(v) => v,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("generate_subscripts dim must be an integer", &other)),
            };
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            if dim == 1 && k <= super::array::len(raw) {
                Ok(Datum::Int4(k as i32))
            } else {
                Ok(Datum::Null)
            }
        }
        "jsonb_object_keys" | "json_object_keys" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("object_keys requires an object", &other)),
            };
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            let kind = super::json::kind_of(text);
            if kind != super::json::Kind::Object {
                return Err(super::json::object_keys_error(name, kind));
            }
            if jsonb {
                // jsonb keys: sorted, deduplicated (the normalized parse order).
                let super::json::Json::Object(members) = super::json::parse(text, arena)? else {
                    return Err(super::json::object_keys_error(name, kind));
                };
                return Ok(members.get(k - 1).map(|(key, _)| Datum::Text(key)).unwrap_or(Datum::Null));
            }
            // json keys: original source order, duplicates kept.
            let members = super::json::object_members_source(text, arena)?;
            Ok(members.get(k - 1).map(|(key, _)| Datum::Text(key)).unwrap_or(Datum::Null))
        }
        // Set-returning `jsonb_array_elements` / `json_array_elements` yield each
        // array element as a json/jsonb row; the `_text` variants yield text.
        "jsonb_array_elements" | "json_array_elements" | "jsonb_array_elements_text"
        | "json_array_elements_text" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let as_text = name.ends_with("_text");
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("array_elements requires an array", &other)),
            };
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            let kind = super::json::kind_of(text);
            if kind != super::json::Kind::Array {
                return Err(super::json::array_elements_error(name, jsonb, kind));
            }
            if jsonb {
                // jsonb elements: normalized (re-rendered) json values.
                let super::json::Json::Array(items) = super::json::parse(text, arena)? else {
                    return Err(super::json::array_elements_error(name, jsonb, kind));
                };
                let Some(element) = items.get(k - 1) else {
                    return Ok(Datum::Null);
                };
                if as_text {
                    return Ok(match *element {
                        super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                        super::json::Json::Null => Datum::Null,
                        _ => Datum::Text(json_to_text(element, arena)?),
                    });
                }
                return Ok(Datum::Json { text: json_to_text(element, arena)?, jsonb });
            }
            // json elements: verbatim source text (interior whitespace kept).
            let items = super::json::array_elements_source(text, arena)?;
            let Some(element) = items.get(k - 1) else {
                return Ok(Datum::Null);
            };
            if as_text {
                // The text form of a json element: a string's decoded value,
                // anything else its verbatim json (NULL for a json null).
                let parsed = super::json::parse(element, arena)?;
                return Ok(match parsed {
                    super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                    super::json::Json::Null => Datum::Null,
                    _ => Datum::Text(element),
                });
            }
            Ok(Datum::Json { text: element, jsonb })
        }
        // Set-returning `json_each` / `jsonb_each[_text]` yield, for the current
        // expansion index k, the composite `(key, value)` of the k-th object
        // member as a record (`SELECT * FROM json_each(...)` expands it to two
        // columns; a bare `SELECT json_each(...)` shows the record).
        "json_each" | "jsonb_each" | "json_each_text" | "jsonb_each_text" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let as_text = name.ends_with("_text");
            let value_oid = if as_text {
                super::types::oid::TEXT
            } else if jsonb {
                super::types::oid::JSONB
            } else {
                super::types::oid::JSON
            };
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(sql_err!("22023", "cannot deconstruct a scalar")),
            };
            let pairs = json_each_pairs(text, jsonb, as_text, arena)?;
            let k = hooks
                .srf_index
                .ok_or_else(|| sql_err!("0A000", "set-returning function called where not allowed"))?;
            let Some((key, value)) = pairs.get(k - 1) else {
                return Ok(Datum::Null);
            };
            let fields = arena
                .alloc_slice_copy(&[
                    super::types::RecordField {
                        name: "key",
                        type_oid: super::types::oid::TEXT,
                        value: Datum::Text(key),
                    },
                    super::types::RecordField {
                        name: "value",
                        type_oid: value_oid,
                        value: *value,
                    },
                ])
                .map_err(|_| arena_full())?;
            Ok(Datum::Record(fields))
        }
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {}() does not exist",
            name
        )),
    }
}

fn arity_err(name: &str, got: usize) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) with {} arguments does not exist",
        name,
        got
    )
}

/// Evaluates `args[i]` and requires text (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
fn text_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<&'a str>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Text(s) => Ok(Some(s)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and requires a bytea (None = SQL NULL). An unknown text
/// literal is parsed as a bytea, as PostgreSQL's coercion does.
#[allow(clippy::too_many_arguments)]
fn bytea_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<&'a [u8]>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Bytea(b) => Ok(Some(b)),
        Datum::Text(s) => Ok(Some(parse_bytea(s, arena)?)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and requires an integer (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
fn int_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<i64>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Int4(v) => Ok(Some(v as i64)),
        Datum::Int8(v) => Ok(Some(v)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and converts a numeric value to f64 (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
fn num_f64<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<f64>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Int4(v) => Ok(Some(v as f64)),
        Datum::Int8(v) => Ok(Some(v as f64)),
        Datum::Float8(v) => Ok(Some(v)),
        Datum::Numeric(n) => Ok(Some(n.to_f64())),
        other => Err(type_mismatch(name, &other)),
    }
}

/// f64 view of an already-evaluated numeric-category datum.
fn datum_f64(name: &str, d: Datum<'_>) -> Result<f64, SqlError> {
    match d {
        Datum::Int4(v) => Ok(v as f64),
        Datum::Int8(v) => Ok(v as f64),
        Datum::Float8(v) => Ok(v),
        Datum::Numeric(n) => Ok(n.to_f64()),
        other => Err(type_mismatch(name, &other)),
    }
}

/// `width_bucket` for double-precision bounds; `count` buckets over [low,high]
/// (or reversed when high < low), 0 below and count+1 at/above the range.
fn width_bucket_f64(operator: f64, lo: f64, hi: f64, count: i64) -> i32 {
    let c = count as f64;
    let bucket = if lo < hi {
        if operator < lo {
            0
        } else if operator >= hi {
            count + 1
        } else {
            ((operator - lo) / (hi - lo) * c).floor() as i64 + 1
        }
    } else if operator > lo {
        0
    } else if operator <= hi {
        count + 1
    } else {
        ((lo - operator) / (lo - hi) * c).floor() as i64 + 1
    };
    bucket as i32
}

/// `width_bucket` with exact numeric arithmetic (matching PostgreSQL's numeric
/// form), using an integer quotient so bucket boundaries land exactly.
fn width_bucket_numeric(
    operator: &Numeric,
    lo: &Numeric,
    hi: &Numeric,
    count: i64,
    arena: &Arena,
) -> Result<i32, SqlError> {
    use super::numeric::{compare, mul, sub, trunc_div};
    use core::cmp::Ordering;
    if compare(lo, hi) == Ordering::Equal {
        return Err(sql_err!("22004", "lower and upper bounds cannot be equal"));
    }
    let cnt = Numeric::from_i64(count, arena)?;
    let ascending = compare(lo, hi) == Ordering::Less;
    let (below, at_or_above) = if ascending {
        (compare(operator, lo) == Ordering::Less, compare(operator, hi) != Ordering::Less)
    } else {
        (compare(operator, lo) == Ordering::Greater, compare(operator, hi) != Ordering::Greater)
    };
    if below {
        return Ok(0);
    }
    if at_or_above {
        return Ok((count + 1) as i32);
    }
    // floor((|operator-lo| * count) / |hi-lo|) + 1
    let (num_a, den) = if ascending {
        (sub(operator, lo, arena)?, sub(hi, lo, arena)?)
    } else {
        (sub(lo, operator, arena)?, sub(lo, hi, arena)?)
    };
    let q = trunc_div(&mul(&num_a, &cnt, arena)?, &den, arena)?;
    Ok((q.to_i64()? + 1) as i32)
}

/// `format()` `%s`: the argument's text (NULL renders as empty).
fn format_append_str<'a>(
    out: &mut StackStr<4096>,
    v: Datum<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if !v.is_null() {
        let _ = out.write_str(datum_to_text(v, arena)?);
    }
    Ok(())
}

/// `format()` `%I`: a SQL identifier, double-quoted only when it is not a bare
/// lowercase identifier.
fn format_append_ident(out: &mut StackStr<4096>, v: Datum<'_>) -> Result<(), SqlError> {
    if v.is_null() {
        return Err(sql_err!("22004", "null value cannot be formatted as SQL identifier"));
    }
    let s = match v {
        Datum::Text(s) => s,
        other => return Err(type_mismatch("format", &other)),
    };
    let bare = !s.is_empty()
        && s.bytes().enumerate().all(|(i, c)| {
            c == b'_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit())
        });
    if bare {
        let _ = out.write_str(s);
    } else {
        let _ = out.write_char('"');
        for c in s.chars() {
            if c == '"' {
                let _ = out.write_char('"');
            }
            let _ = out.write_char(c);
        }
        let _ = out.write_char('"');
    }
    Ok(())
}

/// `format()` `%L`: a SQL literal — `NULL` for null, otherwise single-quoted
/// with embedded quotes doubled.
fn format_append_literal<'a>(
    out: &mut StackStr<4096>,
    v: Datum<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if v.is_null() {
        let _ = out.write_str("NULL");
        return Ok(());
    }
    let s = datum_to_text(v, arena)?;
    let _ = out.write_char('\'');
    for c in s.chars() {
        if c == '\'' {
            let _ = out.write_char('\'');
        }
        let _ = out.write_char(c);
    }
    let _ = out.write_char('\'');
    Ok(())
}

/// Byte offset of the 0-based character index `n` in `s` (clamped to the end).
fn char_index_to_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(b, _)| b)
}

/// 1-based character position of byte offset `b` in `s`.
fn byte_to_char_1based(s: &str, b: usize) -> i32 {
    s[..b].chars().count() as i32 + 1
}

/// Expands a `regexp_replace` replacement string into `out`: `\&` is the whole
/// match, `\\` a literal backslash, `\` + other the literal character.
/// Capture-group backreferences (`\1`..`\9`) are rejected loudly — this engine
/// does not track capture positions.
fn expand_replacement(
    out: &mut StackStr<8192>,
    rep: &str,
    src: &str,
    match_start: usize,
    match_end: usize,
    spans: &[(i64, i64)],
) -> Result<(), SqlError> {
    let bytes = rep.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            // Copy a whole UTF-8 char.
            let c = rep[i..].chars().next().unwrap();
            let _ = out.write_char(c);
            i += c.len_utf8();
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'&') => {
                let _ = out.write_str(&src[match_start..match_end]);
                i += 2;
            }
            Some(b'\\') => {
                let _ = out.write_char('\\');
                i += 2;
            }
            // `\1`..`\9`: the n-th capturing group's text. A group that did not
            // participate in the match — or a number beyond the pattern's group
            // count — contributes nothing (verified against PostgreSQL 18.4).
            Some(d) if d.is_ascii_digit() => {
                let n = (d - b'0') as usize;
                if n == 0 {
                    // `\0` is not a backreference: PostgreSQL keeps it literally.
                    let _ = out.write_str("\\0");
                } else if n <= spans.len() {
                    let (gs, ge) = spans[n - 1];
                    if gs >= 0 {
                        let _ = out.write_str(&src[gs as usize..ge as usize]);
                    }
                }
                i += 2;
            }
            Some(&c) => {
                let _ = out.write_char(c as char);
                i += 2;
            }
            None => {
                let _ = out.write_char('\\');
                i += 1;
            }
        }
    }
    Ok(())
}

/// Rejects a non-positive logarithm argument the way PostgreSQL does.
fn log_domain_check(n: &Numeric) -> Result<(), SqlError> {
    if n.is_zero() {
        return Err(sql_err!("2201E", "cannot take logarithm of zero"));
    }
    if n.sign == super::numeric::Sign::Neg {
        return Err(sql_err!("2201E", "cannot take logarithm of a negative number"));
    }
    Ok(())
}

/// Numeric view of an already-evaluated integer/numeric datum.
fn datum_numeric<'a>(name: &str, d: Datum<'a>, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    match d {
        Datum::Numeric(n) => Ok(n),
        Datum::Int4(v) => Numeric::from_i64(v as i64, arena),
        Datum::Int8(v) => Numeric::from_i64(v, arena),
        Datum::Float8(v) => Numeric::parse(stack_format!(64, "{}", v).as_str(), arena),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Text form of a datum for concat-family functions.
/// A date/time value as microseconds for OVERLAPS comparison, or None for NULL
/// or a non-temporal value.
fn overlaps_micros(d: &Datum) -> Option<i64> {
    match d {
        Datum::Date(days) => Some(*days as i64 * 86_400_000_000),
        Datum::Timestamp(v) | Datum::Timestamptz(v) | Datum::Time(v) => Some(*v),
        _ => None,
    }
}

/// The end microseconds of an OVERLAPS pair whose start is `start`: either the
/// end value directly, or `start` advanced by an interval end.
fn overlaps_end_micros(start: &Datum, end: &Datum) -> Option<i64> {
    if let Datum::Interval(iv) = end {
        return overlaps_micros(start).map(|s| super::datetime::add_interval(s, *iv));
    }
    overlaps_micros(end)
}

/// Whether an identifier must be double-quoted to round-trip: it is not a bare
/// `[a-z_][a-z0-9_]*` token, or it is a keyword that would otherwise be
/// reinterpreted. "Keyword" here is PostgreSQL's own test, every category but
/// plain `unreserved` — `insert` is a keyword yet needs no quotes, while
/// `between` and `all` do.
fn ident_needs_quotes(s: &str) -> bool {
    let mut chars = s.chars();
    let valid = match chars.next() {
        Some(c) if c == '_' || c.is_ascii_lowercase() => {
            chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
        }
        _ => false,
    };
    !valid || super::parser::keyword_needs_quotes(s)
}

/// Double-quotes an identifier into the arena, doubling embedded quotes. The
/// buffer lives here rather than on the huge `call()` frame.
fn quote_ident_str<'a>(s: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    use core::fmt::Write as _;
    let mut out = crate::util::StackStr::<8192>::new();
    let _ = out.write_char('"');
    for c in s.chars() {
        if c == '"' {
            let _ = out.write_str("\"\"");
        } else {
            let _ = out.write_char(c);
        }
    }
    let _ = out.write_char('"');
    if out.is_truncated() {
        return Err(sql_err!("54000", "identifier too long to quote"));
    }
    arena.alloc_str(out.as_str()).map_err(|_| arena_full())
}

/// Wraps `text` as a SQL string literal into the arena, doubling single quotes
/// and backslashes (and prefixing `E` when a backslash is present).
fn quote_literal_str<'a>(text: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    use core::fmt::Write as _;
    let mut out = crate::util::StackStr::<16384>::new();
    if text.contains('\\') {
        let _ = out.write_char('E');
    }
    let _ = out.write_char('\'');
    for c in text.chars() {
        match c {
            '\'' => {
                let _ = out.write_str("''");
            }
            '\\' => {
                let _ = out.write_str("\\\\");
            }
            _ => {
                let _ = out.write_char(c);
            }
        }
    }
    let _ = out.write_char('\'');
    if out.is_truncated() {
        return Err(sql_err!("54000", "literal too long to quote"));
    }
    arena.alloc_str(out.as_str()).map_err(|_| arena_full())
}

/// Splits a possibly-qualified identifier (`schema.table`, `"Weird Name".col`)
/// into its parts, folding unquoted parts to lowercase and unescaping quoted
/// ones. Returns the part count.
fn parse_qualified_ident<'a>(
    input: &str,
    out: &mut [Datum<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let bad = || sql_err!("42601", "string is not a valid identifier: \"{}\"", input);
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(bad());
        }
        if n == out.len() {
            return Err(sql_err!("54000", "identifier has too many parts"));
        }
        let part = if bytes[i] == b'"' {
            // Quoted part: read to the closing quote, collapsing `""` to `"`.
            i += 1;
            let mut buffer = crate::util::StackStr::<1024>::new();
            use core::fmt::Write as _;
            loop {
                match bytes.get(i) {
                    None => return Err(bad()),
                    Some(b'"') if bytes.get(i + 1) == Some(&b'"') => {
                        let _ = buffer.write_char('"');
                        i += 2;
                    }
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        let _ = buffer.write_char(c as char);
                        i += 1;
                    }
                }
            }
            arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?
        } else {
            // Unquoted part: letters/digits/underscore, folded to lowercase.
            let start = i;
            while i < bytes.len()
                && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric() || bytes[i] >= 0x80)
            {
                i += 1;
            }
            if i == start {
                return Err(bad());
            }
            let raw = &input[start..i];
            let lower = arena.alloc_slice_with(raw.len(), |k| raw.as_bytes()[k].to_ascii_lowercase())
                .map_err(|_| arena_full())?;
            unsafe { core::str::from_utf8_unchecked(lower) }
        };
        out[n] = Datum::Text(part);
        n += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'.') => i += 1,
            None => return Ok(n),
            _ => return Err(bad()),
        }
    }
}

fn datum_to_text<'a>(v: Datum<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    match v {
        Datum::Text(s) => Ok(s),
        other => arena.alloc_str_display(other).map_err(|_| arena_full()),
    }
}

/// Concatenates text pieces into a fresh arena string of total length `total`.
fn alloc_text<'a>(arena: &'a Arena, parts: &[&str], total: usize) -> Result<Datum<'a>, SqlError> {
    let out = arena.alloc_slice_with(total, |_| 0u8).map_err(|_| arena_full())?;
    let mut at = 0;
    for p in parts {
        out[at..at + p.len()].copy_from_slice(p.as_bytes());
        at += p.len();
    }
    Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
}

/// The common type of all CASE branch results (+ ELSE), by PostgreSQL's
/// numeric-tower preference. Returns None when the branches are all
/// unknown or of a single non-unifiable class (leave values as-is).
fn case_result_type<'a>(
    whens: &[(&Expr<'a>, &Expr<'a>)],
    otherwise: &Option<&Expr<'a>>,
    row: &impl ColumnLookup<'a>,
) -> Option<ColType> {
    let mut acc: Option<ColType> = None;
    let mut mixed = false;
    let mut consider = |e: &Expr<'a>| {
        if let Some(t) = static_type(e, row) {
            acc = Some(match acc {
                None => t,
                Some(prev) => match unify_types(prev, t) {
                    Some(u) => u,
                    None => {
                        mixed = true;
                        prev
                    }
                },
            });
        }
    };
    for (_, result) in whens {
        consider(result);
    }
    if let Some(e) = otherwise {
        consider(e);
    }
    if mixed {
        None
    } else {
        acc
    }
}

/// Numeric-tower unification (int4 < int8 < numeric < float8); same type
/// unifies to itself; text unifies with text. Otherwise None.
fn unify_types(a: ColType, b: ColType) -> Option<ColType> {
    use ColType::*;
    if a == b {
        return Some(a);
    }
    let rank = |t: ColType| match t {
        Int4 => Some(1),
        Int8 => Some(2),
        Numeric => Some(3),
        Float8 => Some(4),
        _ => None,
    };
    match (rank(a), rank(b)) {
        (Some(ra), Some(rb)) => Some(if ra >= rb { a } else { b }),
        _ => None,
    }
}

/// Best-effort static type of an expression for CASE unification.
fn static_type<'a>(e: &Expr<'a>, row: &impl ColumnLookup<'a>) -> Option<ColType> {
    match e {
        Expr::Null | Expr::Param(_) => None,
        Expr::Bool(_) => Some(ColType::Bool),
        Expr::Int(v) => Some(if i32::try_from(*v).is_ok() { ColType::Int4 } else { ColType::Int8 }),
        Expr::Float(_) => Some(ColType::Float8),
        Expr::NumericLit(_) => Some(ColType::Numeric),
        Expr::Str(_) => Some(ColType::Text),
        Expr::Column { qualifier, name } => row.col_type(*qualifier, name),
        Expr::Cast { type_name, .. } => ColType::from_sql_name(type_name),
        Expr::Unary { operator: UnaryOp::Neg, operand } => static_type(operand, row),
        Expr::Unary { operator: UnaryOp::Not, .. } | Expr::IsNull { .. }
        | Expr::InList { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::Match { .. } => Some(ColType::Bool),
        Expr::Binary { operator, left, right } => match operator {
            BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::LtEq
            | BinaryOp::Gt | BinaryOp::GtEq | BinaryOp::And | BinaryOp::Or => Some(ColType::Bool),
            BinaryOp::Concat => Some(ColType::Text),
            _ => {
                let l = static_type(left, row)?;
                let r = static_type(right, row)?;
                unify_types(l, r)
            }
        },
        Expr::Case { whens, otherwise, .. } => case_result_type(whens, otherwise, row),
        _ => None,
    }
}

/// A string literal or a parameter is PostgreSQL's "unknown" type, which
/// coerces to whatever it is compared/combined with. A real typed value
/// (column, function result, cast) does not.
fn is_unknown_literal(expression: &Expr) -> bool {
    matches!(expression, Expr::Str(_) | Expr::Param(_))
}

#[allow(clippy::too_many_arguments)]
/// Bitwise combine of two `bit_and`/`bit_or`/`bit_xor` aggregate inputs, over
/// integers or bit strings, reusing the operator machinery (bit strings of
/// differing lengths error, as in PostgreSQL).
pub fn bit_aggregate<'a>(
    operator: BinaryOp,
    a: Datum<'a>,
    b: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    binary(operator, a, b, false, false, arena)
}

/// `substring(str FROM posix_pattern)`: the first match, or its first capture
/// group when the pattern has one (PostgreSQL semantics). NULL if no match.
fn regex_substring<'a>(s: &'a str, pattern: &str) -> Result<Datum<'a>, SqlError> {
    let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
    match super::regex::find_captures(pattern, s, 0, false, &mut spans)? {
        None => Ok(Datum::Null),
        Some(((mstart, mend), ng)) => {
            if ng >= 1 {
                let (gs, ge) = spans[0];
                if gs < 0 {
                    Ok(Datum::Null)
                } else {
                    Ok(Datum::Text(&s[gs as usize..ge as usize]))
                }
            } else {
                Ok(Datum::Text(&s[mstart..mend]))
            }
        }
    }
}

/// `substring(str FROM sql_pattern FOR escape)`: the SQL-regular-expression
/// form. The pattern uses `SIMILAR TO` syntax; an `<escape>"` pair delimits the
/// portion to return (else the whole match is returned). NULL if no match.
fn sql_regex_substring<'a>(
    s: &'a str,
    pattern: &str,
    escape: &str,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let escape_char = escape.chars().next();
    // Translate to a POSIX regex, turning the `<escape>"` markers into a single
    // capture group and every SIMILAR TO metacharacter per `similar_to_posix`.
    let mut posix = crate::util::StackStr::<512>::new();
    use core::fmt::Write as _;
    let _ = posix.write_char('^');
    let mut chars = pattern.chars().peekable();
    let mut captures = 0u8;
    let mut in_bracket = false;
    while let Some(c) = chars.next() {
        if Some(c) == escape_char {
            match chars.next() {
                Some('"') => {
                    // Group boundary: first opens the capture, second closes it.
                    let _ = posix.write_char(if captures == 0 { '(' } else { ')' });
                    captures += 1;
                }
                Some(other) => {
                    // An escaped metacharacter is a literal.
                    if "\\^$.[]|()*+?{}".contains(other) {
                        let _ = posix.write_char('\\');
                    }
                    let _ = posix.write_char(other);
                }
                None => return Err(sql_err!("22025", "invalid escape string")),
            }
            continue;
        }
        if in_bracket {
            let _ = posix.write_char(c);
            if c == ']' {
                in_bracket = false;
            }
            continue;
        }
        match c {
            '%' => {
                // Outside the `#"..."#` capture, `%` is lazy so the captured run
                // is maximal; inside it, greedy so the capture itself is maximal
                // — matching PostgreSQL's SQL-regex substring extraction.
                let _ = posix.write_str(if captures == 1 { ".*" } else { ".*?" });
            }
            '_' => {
                let _ = posix.write_char('.');
            }
            '[' => {
                in_bracket = true;
                let _ = posix.write_char('[');
            }
            '.' | '^' | '$' | '\\' => {
                let _ = posix.write_char('\\');
                let _ = posix.write_char(c);
            }
            _ => {
                let _ = posix.write_char(c);
            }
        }
    }
    let _ = posix.write_char('$');
    if posix.is_truncated() {
        return Err(sql_err!("54000", "substring pattern too long"));
    }
    let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
    match super::regex::find_captures(posix.as_str(), s, 0, false, &mut spans)? {
        None => Ok(Datum::Null),
        Some(((mstart, mend), ng)) => {
            let (from, to) = if ng >= 1 && spans[0].0 >= 0 {
                (spans[0].0 as usize, spans[0].1 as usize)
            } else {
                (mstart, mend)
            };
            Ok(Datum::Text(arena.alloc_str(&s[from..to]).map_err(|_| arena_full())?))
        }
    }
}

/// The result kind of a temporal `generate_series` / `date_bin`: a plain
/// timestamp, or a timestamptz (which a `date` argument resolves to, matching
/// PostgreSQL's preference for the timestamptz overload).
#[derive(Clone, Copy)]
pub enum SeriesKind {
    Timestamp,
    Timestamptz,
}

impl SeriesKind {
    pub fn datum<'a>(self, micros: i64) -> Datum<'a> {
        match self {
            SeriesKind::Timestamp => Datum::Timestamp(micros),
            SeriesKind::Timestamptz => Datum::Timestamptz(micros),
        }
    }

    pub fn coltype(self) -> ColType {
        match self {
            SeriesKind::Timestamp => ColType::Timestamp,
            SeriesKind::Timestamptz => ColType::Timestamptz,
        }
    }
}

/// Whether a `generate_series` interval step advances toward larger timestamps.
/// Uses PostgreSQL's canonical interval ordering (30-day months, 24-hour days).
fn interval_is_positive(step: super::types::Interval) -> bool {
    let canonical = step.months as i128 * 2_592_000_000_000
        + step.days as i128 * 86_400_000_000
        + step.micros as i128;
    canonical > 0
}

/// The number of values a temporal `generate_series(base, stop, step)` yields,
/// iterating by calendar addition. A zero step errors; a runaway series is a
/// loud error rather than an unbounded loop.
pub fn timestamp_series_count(
    base: i64,
    stop: i64,
    step: super::types::Interval,
) -> Result<usize, SqlError> {
    if step.months == 0 && step.days == 0 && step.micros == 0 {
        return Err(sql_err!("22023", "step size cannot equal zero"));
    }
    let positive = interval_is_positive(step);
    let mut v = base;
    let mut n = 0usize;
    while if positive { v <= stop } else { v >= stop } {
        n += 1;
        // A generous backstop against a pathologically large series; real limits
        // come from the row arena when the values are materialized.
        if n > 100_000_000 {
            return Err(sql_err!("54000", "generate_series produces too many rows"));
        }
        v = super::datetime::add_interval(v, step);
    }
    Ok(n)
}

/// The base micros and result kind of a temporal `generate_series` start value,
/// or None when it is not a date/timestamp. A `date` becomes UTC-midnight
/// timestamptz.
pub fn timestamp_series_start(d: &Datum) -> Option<(i64, SeriesKind)> {
    match d {
        Datum::Timestamp(v) => Some((*v, SeriesKind::Timestamp)),
        Datum::Timestamptz(v) => Some((*v, SeriesKind::Timestamptz)),
        Datum::Date(days) => Some((*days as i64 * 86_400_000_000, SeriesKind::Timestamptz)),
        _ => None,
    }
}

/// Splits `src` on every match of `pattern` into an arena slice of text pieces,
/// for callers outside this module (`regexp_split_to_table` in the FROM clause).
pub fn regex_split_pub<'a>(
    src: &'a str,
    pattern: &str,
    case_insensitive: bool,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    let mut pieces = [Datum::Null; 1024];
    let n = regex_split(src, pattern, case_insensitive, &mut pieces)?;
    Ok(&*arena.alloc_slice_copy(&pieces[..n]).map_err(|_| arena_full())?)
}

/// Splits `src` on every match of `pattern`, writing the pieces into `out` and
/// returning the count. An empty pattern splits into individual characters.
fn regex_split<'a>(
    src: &'a str,
    pattern: &str,
    case_insensitive: bool,
    out: &mut [Datum<'a>],
) -> Result<usize, SqlError> {
    let mut n = 0usize;
    let mut push = |piece: &'a str, n: &mut usize| -> Result<(), SqlError> {
        if *n == out.len() {
            return Err(sql_err!("54000", "too many split pieces"));
        }
        out[*n] = Datum::Text(piece);
        *n += 1;
        Ok(())
    };
    if pattern.is_empty() {
        for (i, ch) in src.char_indices() {
            push(&src[i..i + ch.len_utf8()], &mut n)?;
        }
        return Ok(n);
    }
    let mut last = 0usize;
    let mut pos = 0usize;
    while pos <= src.len() {
        let Some((start, end)) = super::regex::find(pattern, src, pos, case_insensitive)? else {
            break;
        };
        if end == start {
            // A zero-width match: advance one character so the scan progresses.
            let step = src[pos..].chars().next().map_or(1, char::len_utf8);
            if pos + step > src.len() {
                break;
            }
            pos += step;
            continue;
        }
        push(&src[last..start], &mut n)?;
        last = end;
        pos = end;
    }
    push(&src[last..], &mut n)?;
    Ok(n)
}

/// Parses PostgreSQL regex flags into `(global, case_insensitive)`; an unknown
/// flag is a loud error.
pub fn regexp_flags(flags: &str) -> Result<(bool, bool), SqlError> {
    let mut global = false;
    let mut case_insensitive = false;
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => case_insensitive = true,
            'c' => case_insensitive = false,
            _ => return Err(sql_err!("22023", "invalid regular expression option: \"{}\"", f)),
        }
    }
    Ok((global, case_insensitive))
}

/// `json -> key/index` and `json ->> key/index`. A missing member yields NULL;
/// `->>` unwraps a JSON string to plain text.
fn json_get<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    as_text: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (text, jsonb) = match l {
        Datum::Json { text, jsonb } => (text, jsonb),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("-> requires json/jsonb", &other)),
    };
    if r.is_null() {
        return Ok(Datum::Null);
    }
    let tree = super::json::parse(text, arena)?;
    let child = match r {
        Datum::Text(k) => tree.get_field(k),
        Datum::Int4(i) => tree.get_index(i as i64),
        Datum::Int8(i) => tree.get_index(i),
        other => return Err(type_mismatch("-> key must be text or integer", &other)),
    };
    let Some(child) = child else {
        return Ok(Datum::Null);
    };
    if as_text {
        // ->> renders a JSON string as its unescaped text; other values as
        // their canonical JSON.
        if let super::json::Json::Str(s) = child {
            return Ok(Datum::Text(super::json::decode_string(s, arena)?));
        }
        let mut buffer = crate::util::StackStr::<8192>::new();
        let _ = core::fmt::Write::write_fmt(&mut buffer, format_args!("{}", super::json::JsonWrite(&child)));
        return Ok(Datum::Text(arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?));
    }
    let mut buffer = crate::util::StackStr::<8192>::new();
    let _ = core::fmt::Write::write_fmt(&mut buffer, format_args!("{}", super::json::JsonWrite(&child)));
    Ok(Datum::Json { text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?, jsonb })
}

/// Renders a `Json` value to canonical jsonb text in the arena.
/// Renders a parsed JSON node back to its canonical text, for callers outside
/// this module (set-returning-function materialization in the query layer).
pub fn json_to_text_pub<'a>(
    v: &super::json::Json<'a>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    json_to_text(v, arena)
}

/// Decodes every element of an array blob into `items` starting at `start`,
/// coercing each to `to` (PostgreSQL promotes the element type when array
/// functions mix numeric widths). Returns the new count; errors on overflow.
fn load_array<'a>(
    raw: &'a [u8],
    from: super::types::ArrElem,
    to: super::types::ArrElem,
    items: &mut [Datum<'a>],
    start: usize,
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let mut n = start;
    let to_coltype = to.to_coltype();
    for i in 0..super::array::len(raw) {
        if n == items.len() {
            return Err(sql_err!("54000", "array value too large"));
        }
        let el = super::array::get(raw, from, i).unwrap_or(Datum::Null);
        items[n] = if el.is_null() || from == to { el } else { cast_to(el, to_coltype, arena)? };
        n += 1;
    }
    Ok(n)
}

fn json_to_text<'a>(v: &super::json::Json<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    // Render straight into the arena at exact length — a jsonb value can be
    // larger than any fixed scratch buffer, and truncating it would corrupt it.
    arena.alloc_str_display(super::json::JsonWrite(v)).map_err(|_| arena_full())
}

/// Expands a `(record).*` base to its fields for a projection. The runtime
/// field count matches the static shape (`exec::record_shape`) for every
/// supported record source, so describe and data-row column counts agree.
/// A null or non-composite value is rejected loudly (a `(t).*` over an
/// outer-join null row is the one shape whose width is not carried at runtime).
pub fn record_star_expand<'a>(
    base: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [super::types::RecordField<'a>], SqlError> {
    match eval_full(base, arena, params, row, hooks)? {
        Datum::Record(fields) => Ok(fields),
        other => Err(type_mismatch("record expansion of a non-composite value", &other)),
    }
}

/// The `(key, value)` members a `json_each` / `jsonb_each` family call yields
/// for the object `text`. `jsonb` selects normalized (sorted, deduplicated,
/// re-rendered) members over the `json` variants' source-order/verbatim members;
/// `as_text` makes each value the `_text` form (a decoded string, else the
/// value's json text). Errors match PostgreSQL's `cannot deconstruct ...`.
pub fn json_each_pairs<'a>(
    text: &'a str,
    jsonb: bool,
    as_text: bool,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, Datum<'a>)], SqlError> {
    match super::json::kind_of(text) {
        super::json::Kind::Object => {}
        super::json::Kind::Array => {
            return Err(sql_err!("22023", "cannot deconstruct an array as an object"));
        }
        super::json::Kind::Scalar => {
            return Err(sql_err!("22023", "cannot deconstruct a scalar"));
        }
    }
    if jsonb {
        let super::json::Json::Object(members) = super::json::parse(text, arena)? else {
            return Err(sql_err!("22023", "cannot deconstruct an array as an object"));
        };
        let out = arena
            .alloc_slice_with(members.len(), |_| ("", Datum::Null))
            .map_err(|_| arena_full())?;
        for (slot, (key, value)) in out.iter_mut().zip(members.iter()) {
            let datum = if as_text {
                match *value {
                    super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                    super::json::Json::Null => Datum::Null,
                    _ => Datum::Text(json_to_text(value, arena)?),
                }
            } else {
                Datum::Json { text: json_to_text(value, arena)?, jsonb: true }
            };
            *slot = (*key, datum);
        }
        return Ok(&*out);
    }
    // json: source order, duplicates kept, values verbatim.
    let members = super::json::object_members_source(text, arena)?;
    let out = arena
        .alloc_slice_with(members.len(), |_| ("", Datum::Null))
        .map_err(|_| arena_full())?;
    for (slot, (key, value)) in out.iter_mut().zip(members.iter()) {
        let datum = if as_text {
            match super::json::parse(value, arena)? {
                super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                super::json::Json::Null => Datum::Null,
                _ => Datum::Text(value),
            }
        } else {
            Datum::Json { text: value, jsonb: false }
        };
        *slot = (*key, datum);
    }
    Ok(&*out)
}

/// `jsonb || jsonb`: merge two objects (right key wins), concatenate two
/// arrays, else concatenate as arrays wrapping any non-array operand.
fn jsonb_concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    use super::json::Json;
    let text_of = |d: Datum<'a>| -> Result<Option<&'a str>, SqlError> {
        match d {
            Datum::Json { text, .. } => Ok(Some(text)),
            // An unknown text literal (`'{"b":2}'`) coerces to jsonb.
            Datum::Text(s) => Ok(Some(s)),
            Datum::Null => Ok(None),
            other => Err(type_mismatch("|| requires jsonb", &other)),
        }
    };
    let (Some(lt), Some(rt)) = (text_of(l)?, text_of(r)?) else {
        return Ok(Datum::Null);
    };
    let lj = super::json::parse(lt, arena)?;
    let rj = super::json::parse(rt, arena)?;
    let merged = match (&lj, &rj) {
        (Json::Object(a), Json::Object(b)) => {
            // Concatenate then re-sort/dedup (last wins) by re-serializing an
            // object literal through the parser.
            let mut buffer = crate::util::StackStr::<32768>::new();
            let _ = core::fmt::Write::write_str(&mut buffer, "{");
            let mut first = true;
            for (k, v) in a.iter().chain(b.iter()) {
                if !first {
                    let _ = core::fmt::Write::write_str(&mut buffer, ",");
                }
                first = false;
                let _ = super::json::write_json_raw_string(k, &mut buffer);
                let _ = core::fmt::Write::write_str(&mut buffer, ":");
                let _ = core::fmt::Write::write_fmt(
                    &mut buffer,
                    format_args!("{}", super::json::JsonWrite(v)),
                );
            }
            let _ = core::fmt::Write::write_str(&mut buffer, "}");
            let owned = arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?;
            return Ok(Datum::Json { text: json_to_text(&super::json::parse(owned, arena)?, arena)?, jsonb: true });
        }
        (Json::Array(a), Json::Array(b)) => {
            let items = arena
                .alloc_slice_with(a.len() + b.len(), |_| Json::Null)
                .map_err(|_| arena_full())?;
            items[..a.len()].copy_from_slice(a);
            items[a.len()..].copy_from_slice(b);
            Json::Array(items)
        }
        // Non-array || anything (or vice-versa): each non-array becomes a
        // one-element array, then concatenate.
        _ => {
            let as_items = |j: &Json<'a>| -> &'a [Json<'a>] {
                match j {
                    Json::Array(items) => items,
                    _ => core::slice::from_ref(arena.alloc(*j).expect("arena")),
                }
            };
            let (ai, bi) = (as_items(&lj), as_items(&rj));
            let items = arena
                .alloc_slice_with(ai.len() + bi.len(), |_| Json::Null)
                .map_err(|_| arena_full())?;
            items[..ai.len()].copy_from_slice(ai);
            items[ai.len()..].copy_from_slice(bi);
            Json::Array(items)
        }
    };
    Ok(Datum::Json { text: json_to_text(&merged, arena)?, jsonb: true })
}

/// `json #> path` / `#>>`: extract the value at a `text[]` path.
/// Extracts a JSON path (`text[]`, or an unknown `'{a,b}'` literal) into its
/// string parts, for `jsonb_set` / `jsonb_insert` / `#-`.
fn json_path_parts<'a>(r: Datum<'a>, arena: &'a Arena) -> Result<&'a [&'a str], SqlError> {
    let (element, raw) = match r {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(lit) => (
            super::types::ArrElem::Text,
            super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
        ),
        other => return Err(type_mismatch("path must be a text array", &other)),
    };
    let n = super::array::len(raw);
    if n > 64 {
        return Err(sql_err!("54000", "JSON path too long"));
    }
    let mut buffer = [""; 64];
    for (i, slot) in buffer[..n].iter_mut().enumerate() {
        *slot = match super::array::get(raw, element, i) {
            Some(Datum::Text(s)) => s,
            _ => return Err(sql_err!("22023", "path element is not text")),
        };
    }
    Ok(&*arena.alloc_slice_copy(&buffer[..n]).map_err(|_| arena_full())?)
}

/// `jsonb - text`/`text[]`/`integer`: delete a key, several keys, or an element.
fn jsonb_delete<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let Datum::Json { text, .. } = l else {
        return Err(type_mismatch("- requires jsonb", &l));
    };
    let root = super::json::parse(text, arena)?;
    let result = match r {
        Datum::Null => return Ok(Datum::Null),
        Datum::Text(key) => super::json::delete_key(root, key, arena)?,
        Datum::Int4(i) => super::json::delete_index(root, i as i64, arena)?,
        Datum::Int8(i) => super::json::delete_index(root, i, arena)?,
        Datum::Array { element, raw } => {
            // `jsonb - text[]`: delete each named key.
            let mut node = root;
            for i in 0..super::array::len(raw) {
                if let Some(Datum::Text(key)) = super::array::get(raw, element, i) {
                    node = super::json::delete_key(node, key, arena)?;
                }
            }
            node
        }
        other => return Err(type_mismatch("- requires text, text[], or integer", &other)),
    };
    Ok(Datum::Json { text: json_to_text(&result, arena)?, jsonb: true })
}

/// `jsonb #- text[]`: delete the value at a path.
fn jsonb_delete_path<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let text = match l {
        Datum::Json { text, .. } => text,
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#- requires jsonb", &other)),
    };
    if r.is_null() {
        return Ok(Datum::Null);
    }
    let root = super::json::parse(text, arena)?;
    let path = json_path_parts(r, arena)?;
    let result = super::json::delete_path(root, path, arena)?;
    Ok(Datum::Json { text: json_to_text(&result, arena)?, jsonb: true })
}

/// Parses a json/jsonb argument (or unknown text literal) into a tree.
fn json_tree_arg<'a>(d: Datum<'a>, arena: &'a Arena) -> Result<super::json::Json<'a>, SqlError> {
    match d {
        Datum::Json { text, .. } => super::json::parse(text, arena),
        Datum::Text(s) => super::json::parse(s, arena),
        other => Err(type_mismatch("argument is not jsonb", &other)),
    }
}

fn json_path<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    as_text: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (text, jsonb) = match l {
        Datum::Json { text, jsonb } => (text, jsonb),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#> requires json/jsonb", &other)),
    };
    // The path is a `text[]`; an unknown literal (`'{a,b}'`) arrives as text
    // and is parsed as a text-array literal, as PostgreSQL coerces it.
    let (element, raw) = match r {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(lit) => (
            super::types::ArrElem::Text,
            super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
        ),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#> path must be a text array", &other)),
    };
    let mut node = super::json::parse(text, arena)?;
    for i in 0..super::array::len(raw) {
        let step = super::array::get(raw, element, i).unwrap_or(Datum::Null);
        let Datum::Text(key) = step else {
            return Ok(Datum::Null);
        };
        let next = match &node {
            super::json::Json::Object(_) => node.get_field(key),
            super::json::Json::Array(_) => key.parse::<i64>().ok().and_then(|n| node.get_index(n)),
            _ => None,
        };
        let Some(next) = next else {
            return Ok(Datum::Null);
        };
        node = next;
    }
    if as_text {
        if let super::json::Json::Str(str_value) = node {
            return Ok(Datum::Text(super::json::decode_string(str_value, arena)?));
        }
        if matches!(node, super::json::Json::Null) {
            return Ok(Datum::Null);
        }
        return Ok(Datum::Text(json_to_text(&node, arena)?));
    }
    Ok(Datum::Json { text: json_to_text(&node, arena)?, jsonb })
}

/// `jsonb ? key` / `?|` / `?&`: key/element existence tests.
fn json_exists<'a>(
    operator: super::ast::BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use super::ast::BinaryOp::{JsonExistsAll, JsonExistsAny};
    use super::json::Json;
    let text = match l {
        Datum::Json { text, .. } => text,
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("? requires jsonb", &other)),
    };
    let node = super::json::parse(text, arena)?;
    // Does a single string key exist (object key, or array string element)?
    let has = |key: &str| -> bool {
        match &node {
            Json::Object(members) => members.iter().any(|(k, _)| *k == key),
            Json::Array(items) => items.iter().any(|it| matches!(it, Json::Str(s) if *s == key)),
            _ => false,
        }
    };
    match operator {
        super::ast::BinaryOp::JsonExists => {
            let Datum::Text(key) = r else {
                if r.is_null() {
                    return Ok(Datum::Null);
                }
                return Err(type_mismatch("? key must be text", &r));
            };
            Ok(Datum::Bool(has(key)))
        }
        JsonExistsAny | JsonExistsAll => {
            let (element, raw) = match r {
                Datum::Array { element, raw } => (element, raw),
                Datum::Text(lit) => (
                    super::types::ArrElem::Text,
                    super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
                ),
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("?|/?& require a text array", &other)),
            };
            let n = super::array::len(raw);
            let all = operator == JsonExistsAll;
            let mut result = all;
            for i in 0..n {
                let key = super::array::get(raw, element, i).unwrap_or(Datum::Null);
                let present = matches!(key, Datum::Text(k) if has(k));
                if all {
                    result = result && present;
                } else if present {
                    result = true;
                    break;
                }
            }
            Ok(Datum::Bool(result))
        }
        _ => unreachable!("json_exists only handles ?, ?|, ?&"),
    }
}

/// Evaluates `left AND right` / `left OR right` with PostgreSQL's short-circuit
/// semantics. The *absorbing* value is FALSE for AND, TRUE for OR. PostgreSQL
/// simplifies `x AND FALSE` / `x OR TRUE` at plan time — dropping `x` even when
/// it would error, and even when the settling value is nested (`A AND (FALSE
/// AND c)` drops `A`) — but is otherwise strict left-to-right: `(1/a=1) AND
/// (b>0)` errors on the division, it does not swallow it because `b>0` is not
/// statically FALSE. `fold_check` decides statically (surfacing a constant
/// operand's own error left-first, exactly as plan-time folding does).
fn eval_logic_short_circuit<'a>(
    operator: BinaryOp,
    left: &Expr<'a>,
    right: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let absorbing = matches!(operator, BinaryOp::Or);
    // Left first: a statically-determined left settles the result (absorbing) or
    // hands offset to the right (non-absorbing), matching plan-time folding order.
    match fold_check(left, arena)? {
        Some(b) if b == absorbing => return Ok(Datum::Bool(absorbing)),
        Some(_) => return eval_full(right, arena, params, row, hooks),
        None => {}
    }
    // Left is runtime; if the right statically folds to the absorbing value it
    // settles the result and drops the (possibly-erroring) left.
    match fold_check(right, arena)? {
        Some(b) if b == absorbing => return Ok(Datum::Bool(absorbing)),
        Some(_) => return eval_full(left, arena, params, row, hooks),
        None => {}
    }
    let l = eval_full(left, arena, params, row, hooks)?;
    if matches!(l, Datum::Bool(b) if b == absorbing) {
        return Ok(Datum::Bool(absorbing));
    }
    let r = eval_full(right, arena, params, row, hooks)?;
    logic(operator, l, r)
}

/// Resolves `array || NULL` / `NULL || array`, which PostgreSQL decides from
/// the NULL operand's static type: an untyped NULL or a NULL of the array type
/// is the identity (returns the array), a NULL of the element type appends a
/// NULL element, and any other type is an undefined operator. Returns `None`
/// when this is not an array-with-NULL concatenation (fall through to `concat`).
fn array_null_concat<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    left: &Expr<'a>,
    right: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
    arena: &'a Arena,
) -> Result<Option<Datum<'a>>, SqlError> {
    let (array, element, null_expr) = match (l, r) {
        (Datum::Array { element, .. }, Datum::Null) => (l, element, right),
        (Datum::Null, Datum::Array { element, .. }) => (r, element, left),
        _ => return Ok(None),
    };
    match static_type(null_expr, row) {
        // Untyped NULL or a NULL of the array type: identity.
        None | Some(ColType::Array(_)) => Ok(Some(array)),
        // NULL of the element type: append/prepend a NULL element.
        Some(t) if super::types::ArrElem::from_coltype(t) == Some(element) => {
            Ok(Some(array_concat(l, r, arena)?))
        }
        Some(t) => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {}[] || {}",
            element.to_coltype().name(),
            t.name()
        )),
    }
}

fn concat<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    // `||` on arrays concatenates: array||array, and array||element or
    // element||array append/prepend the element. An *unknown literal* on the
    // scalar side is resolved as array||array (PostgreSQL casts it to the array
    // type), so it is parsed as an array literal and errors if malformed —
    // matching `ARRAY['a','b'] || 'c'` (error) vs `|| 'c'::text` (append).
    let arr_elem = match (&l, &r) {
        (Datum::Array { element, .. }, _) | (_, Datum::Array { element, .. }) => Some(*element),
        _ => None,
    };
    if let Some(element) = arr_elem {
        let coerce = |d: Datum<'a>, unknown: bool| -> Result<Datum<'a>, SqlError> {
            match d {
                Datum::Text(s) if unknown => {
                    Ok(Datum::Array { element, raw: super::array::parse_literal(s, element, arena)? })
                }
                other => Ok(other),
            }
        };
        return array_concat(coerce(l, l_unknown)?, coerce(r, r_unknown)?, arena);
    }
    let left = cast_to_text(l, arena)?;
    let right = cast_to_text(r, arena)?;
    let bytes = arena
        .alloc_slice_with(left.len() + right.len(), |i| {
            if i < left.len() {
                left.as_bytes()[i]
            } else {
                right.as_bytes()[i - left.len()]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Text(unsafe {
        core::str::from_utf8_unchecked(bytes)
    }))
}

/// Concatenates two operands where at least one is an array, following
/// PostgreSQL's `array || array`, `array || element`, and `element || array`.
fn array_concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let element = match (&l, &r) {
        (Datum::Array { element, .. }, _) | (_, Datum::Array { element, .. }) => *element,
        _ => unreachable!("caller ensures one side is an array"),
    };
    let mut items = [Datum::Null; 4096];
    let mut n = 0usize;
    for side in [l, r] {
        match side {
            Datum::Array { raw, element: e } => {
                for i in 0..super::array::len(raw) {
                    if n >= items.len() {
                        return Err(sql_err!("54000", "array size exceeds the maximum allowed"));
                    }
                    items[n] = super::array::get(raw, e, i).ok_or_else(|| {
                        sql_err!("XX000", "corrupt array element")
                    })?;
                    n += 1;
                }
            }
            scalar => {
                if n >= items.len() {
                    return Err(sql_err!("54000", "array size exceeds the maximum allowed"));
                }
                items[n] = scalar;
                n += 1;
            }
        }
    }
    Ok(Datum::Array { element, raw: super::array::build(&items[..n], arena)? })
}

pub fn cast<'a>(v: Datum<'a>, type_name: &str, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let Some(target) = ColType::from_sql_name(type_name) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            type_name
        ));
    };
    cast_to(v, target, arena)
}

pub fn cast_to<'a>(
    v: Datum<'a>,
    target: ColType,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let out = match target {
        ColType::Bool => match v {
            Datum::Bool(_) => v,
            Datum::Int4(x) => Datum::Bool(x != 0),
            Datum::Text(s) => Datum::Bool(parse_bool(s)?),
            _ => return Err(cast_unsupported(&v, "boolean")),
        },
        ColType::Int4 => {
            if let Datum::Bit { bits, .. } = v {
                // bit -> integer: the bits are the low bits of the result
                // (two's complement), so a full 32-bit string round-trips.
                Datum::Int4(bits_to_uint(bits, 32, "integer")? as u32 as i32)
            } else {
                let x = to_i64_for_cast(&v, "integer")?;
                Datum::Int4(i32::try_from(x).map_err(|_| overflow("integer"))?)
            }
        }
        ColType::Int8 => {
            if let Datum::Bit { bits, .. } = v {
                Datum::Int8(bits_to_uint(bits, 64, "bigint")? as i64)
            } else {
                Datum::Int8(to_i64_for_cast(&v, "bigint")?)
            }
        }
        // real/float4 collapse to float8 storage: full precision is retained so
        // text output stays shortest round-trip (true 4-byte float4 rounding
        // would need a dedicated Datum to render correctly).
        ColType::Float8 | ColType::Float4 => match v {
            Datum::Int4(x) => Datum::Float8(f64::from(x)),
            Datum::Int8(x) => Datum::Float8(x as f64),
            Datum::Float8(_) => v,
            Datum::Numeric(n) => Datum::Float8(n.to_f64()),
            Datum::Text(s) => Datum::Float8(s.trim().parse().map_err(|_| bad_text(s, "double precision"))?),
            _ => return Err(cast_unsupported(&v, "double precision")),
        },
        ColType::Text | ColType::Varchar | ColType::Bpchar => Datum::Text(cast_to_text(v, arena)?),
        ColType::Int2 => {
            let x = to_i64_for_cast(&v, "smallint")?;
            if !(-32768..=32767).contains(&x) {
                return Err(overflow("smallint"));
            }
            Datum::Int4(x as i32)
        }
        ColType::Date => match v {
            Datum::Date(_) => v,
            Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                Datum::Date(t.div_euclid(86_400_000_000) as i32)
            }
            Datum::Text(s) => Datum::Date(super::datetime::parse_date(s)?),
            _ => return Err(cast_unsupported(&v, "date")),
        },
        ColType::Timestamp => match v {
            Datum::Timestamp(_) => v,
            Datum::Timestamptz(t) => Datum::Timestamp(t),
            Datum::Date(d) => Datum::Timestamp(d as i64 * 86_400_000_000),
            Datum::Text(s) => Datum::Timestamp(super::datetime::parse_timestamp(s, false)?),
            _ => return Err(cast_unsupported(&v, "timestamp")),
        },
        ColType::Timestamptz => match v {
            Datum::Timestamptz(_) => v,
            Datum::Timestamp(t) => Datum::Timestamptz(t),
            Datum::Date(d) => Datum::Timestamptz(d as i64 * 86_400_000_000),
            Datum::Text(s) => Datum::Timestamptz(super::datetime::parse_timestamp(s, true)?),
            _ => return Err(cast_unsupported(&v, "timestamp with time zone")),
        },
        ColType::Time => match v {
            Datum::Time(_) => v,
            // The time-of-day portion of a timestamp (microseconds past midnight).
            Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                Datum::Time(t.rem_euclid(86_400_000_000))
            }
            Datum::Text(s) => Datum::Time(super::datetime::parse_time(s)?),
            _ => return Err(cast_unsupported(&v, "time without time zone")),
        },
        ColType::Interval => match v {
            Datum::Interval(_) => v,
            Datum::Text(s) => Datum::Interval(super::datetime::parse_interval(s)?),
            _ => return Err(cast_unsupported(&v, "interval")),
        },
        ColType::Json => match v {
            Datum::Json { text, .. } => {
                super::json::validate(text, arena)?;
                Datum::Json { text, jsonb: false }
            }
            Datum::Text(s) => {
                super::json::validate(s, arena)?;
                Datum::Json { text: s, jsonb: false }
            }
            _ => return Err(cast_unsupported(&v, "json")),
        },
        ColType::Jsonb => match v {
            Datum::Json { jsonb: true, .. } => v,
            Datum::Json { text, jsonb: false } | Datum::Text(text) => {
                let tree = super::json::parse(text, arena)?;
                let mut buffer = crate::util::StackStr::<8192>::new();
                let _ = core::fmt::Write::write_fmt(&mut buffer, format_args!("{}", super::json::JsonWrite(&tree)));
                if buffer.is_truncated() {
                    return Err(sql_err!("54000", "jsonb value exceeds the supported size"));
                }
                Datum::Json { text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?, jsonb: true }
            }
            _ => return Err(cast_unsupported(&v, "jsonb")),
        },
        ColType::Array(element) => match v {
            Datum::Array { element: e, .. } if e == element => v,
            // A different element type: re-encode each element cast to it.
            Datum::Array { element: e, raw } => {
                let mut items = [Datum::Null; 1024];
                let n = load_array(raw, e, element, &mut items, 0, arena)?;
                Datum::Array { element, raw: super::array::build(&items[..n], arena)? }
            }
            Datum::Text(s) => Datum::Array { element, raw: super::array::parse_literal(s, element, arena)? },
            _ => return Err(cast_unsupported(&v, "array")),
        },
        ColType::Uuid => match v {
            Datum::Uuid(_) => v,
            Datum::Text(s) => Datum::Uuid(parse_uuid(s)?),
            _ => return Err(cast_unsupported(&v, "uuid")),
        },
        ColType::Bytea => match v {
            Datum::Bytea(_) => v,
            Datum::Text(s) => Datum::Bytea(parse_bytea(s, arena)?),
            _ => return Err(cast_unsupported(&v, "bytea")),
        },
        ColType::Numeric => match v {
            Datum::Numeric(_) => v,
            Datum::Int4(x) => Datum::Numeric(Numeric::from_i64(i64::from(x), arena)?),
            Datum::Int8(x) => Datum::Numeric(Numeric::from_i64(x, arena)?),
            Datum::Float8(x) => {
                // float8 -> numeric via the shortest round-trip decimal.
                let text = crate::stack_format!(64, "{}", x);
                Datum::Numeric(Numeric::parse(text.as_str(), arena)?)
            }
            Datum::Text(s) => Datum::Numeric(Numeric::parse(s, arena)?),
            _ => return Err(cast_unsupported(&v, "numeric")),
        },
        ColType::Range(kind) => match v {
            Datum::Range { kind: k, .. } if k == kind => v,
            Datum::Text(s) => {
                let p = super::range::parse(s, kind)?;
                Datum::Range { text: super::range::canonical(&p, kind, arena)?, kind }
            }
            _ => return Err(cast_unsupported(&v, kind.name())),
        },
        ColType::Bit { varying } => match v {
            Datum::Bit { bits, .. } => Datum::Bit { bits, varying },
            Datum::Text(s) => Datum::Bit { bits: validate_bits(s)?, varying },
            // int -> bit yields the two's-complement bits at the type's full
            // width; `apply_cast_typmod` then keeps the low N bits for bit(N).
            Datum::Int4(x) => Datum::Bit { bits: int_to_bits(x as u32 as u64, 32, arena)?, varying },
            Datum::Int8(x) => Datum::Bit { bits: int_to_bits(x as u64, 64, arena)?, varying },
            _ => return Err(cast_unsupported(&v, "bit")),
        },
        ColType::Multirange(kind) => match v {
            Datum::Multirange { kind: k, .. } if k == kind => v,
            // A range promotes to a one-element multirange (empty range → {}).
            Datum::Range { text, kind: k } if k == kind => {
                Datum::Multirange { text: super::range::multirange_from_range(text, kind, arena)?, kind }
            }
            Datum::Text(s) => {
                Datum::Multirange { text: super::range::parse_multirange(s, kind, arena)?, kind }
            }
            _ => return Err(cast_unsupported(&v, kind.multirange_name())),
        },
    };
    Ok(out)
}

/// Validates that every character of a bit-string literal is `0` or `1`,
/// returning it unchanged.
fn validate_bits(s: &str) -> Result<&str, SqlError> {
    for c in s.bytes() {
        if c != b'0' && c != b'1' {
            return Err(sql_err!(
                "22P02",
                "\"{}\" is not a valid binary digit",
                (c as char)
            ));
        }
    }
    Ok(s)
}

/// Interprets a `'0'`/`'1'` bit string as an unsigned integer (most significant
/// bit first). Bit strings wider than `max_bits` overflow the target loudly.
fn bits_to_uint(bits: &str, max_bits: usize, target: &'static str) -> Result<u64, SqlError> {
    if bits.len() > max_bits {
        return Err(overflow(target));
    }
    let mut value = 0u64;
    for c in bits.bytes() {
        value = (value << 1) | u64::from(c == b'1');
    }
    Ok(value)
}

/// Renders `value` as a `width`-bit `'0'`/`'1'` string, most significant bit
/// first (right-aligned: the low bits occupy the rightmost positions, higher
/// positions zero-fill). Supports widths beyond 64 bits.
pub fn int_to_bits(value: u64, width: usize, arena: &Arena) -> Result<&str, SqlError> {
    let out = arena
        .alloc_slice_with(width, |i| {
            let shift = width - 1 - i;
            if shift < 64 && (value >> shift) & 1 != 0 { b'1' } else { b'0' }
        })
        .map_err(|_| arena_full())?;
    Ok(unsafe { core::str::from_utf8_unchecked(out) })
}

/// Fits a bit string to a declared length `n`: fixed `bit(n)` zero-pads or
/// truncates on the right to exactly `n`; `varbit(n)` only truncates when
/// longer. (PostgreSQL adjusts bit-string length on the right.)
pub fn fit_bits<'a>(
    bits: &'a str,
    n: usize,
    varying: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let len = bits.len();
    if len == n || (varying && len < n) {
        return Ok(Datum::Bit { bits, varying });
    }
    if len > n {
        let out = arena.alloc_str(&bits[..n]).map_err(|_| arena_full())?;
        return Ok(Datum::Bit { bits: out, varying });
    }
    // Fixed bit(n) shorter than n: zero-pad on the right.
    let out = arena
        .alloc_slice_with(n, |i| if i < len { bits.as_bytes()[i] } else { b'0' })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit { bits: unsafe { core::str::from_utf8_unchecked(out) }, varying })
}

fn parse_uuid(s: &str) -> Result<[u8; 16], SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type uuid: \"{}\"",
            s
        )
    };
    let mut out = [0u8; 16];
    let mut nibbles = 0usize;
    for c in s.trim().chars() {
        if c == '-' {
            continue;
        }
        let d = c.to_digit(16).ok_or_else(bad)? as u8;
        if nibbles >= 32 {
            return Err(bad());
        }
        if nibbles.is_multiple_of(2) {
            out[nibbles / 2] = d << 4;
        } else {
            out[nibbles / 2] |= d;
        }
        nibbles += 1;
    }
    if nibbles != 32 {
        return Err(bad());
    }
    Ok(out)
}

/// `\x` hex form (PostgreSQL's default bytea output).
fn parse_bytea<'a>(s: &str, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    // The hex form `\xNN…`; otherwise PostgreSQL's escape form (printable bytes
    // verbatim, `\\` for backslash, `\ooo` octal for the rest).
    if let Some(hex) = s.strip_prefix("\\x") {
        let bad = || {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type bytea"
            )
        };
        // Whitespace between hex digits is permitted.
        let out = arena.alloc_slice_with(hex.len() / 2 + 1, |_| 0u8).map_err(|_| arena_full())?;
        let mut n = 0usize;
        let mut high: Option<u8> = None;
        for &c in hex.as_bytes() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                continue;
            }
            let v = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(bad()),
            };
            match high {
                None => high = Some(v),
                Some(h) => {
                    out[n] = (h << 4) | v;
                    n += 1;
                    high = None;
                }
            }
        }
        if high.is_some() {
            return Err(bad());
        }
        return Ok(&out[..n]);
    }
    super::encoding::escape_decode(s, arena)
}

/// Cast-to-text semantics (`true`/`false`), unlike wire output (`t`/`f`).
fn cast_to_text<'a>(v: Datum<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    match v {
        Datum::Text(s) => Ok(s),
        Datum::Bool(b) => Ok(if b { "true" } else { "false" }),
        Datum::Bytea(b) => {
            // 2 + 2 bytes per input byte, straight into the arena.
            let out = arena
                .alloc_slice_with(2 + b.len() * 2, |_| 0u8)
                .map_err(|_| arena_full())?;
            out[0] = b'\\';
            out[1] = b'x';
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for (i, byte) in b.iter().enumerate() {
                out[2 + i * 2] = HEX[(byte >> 4) as usize];
                out[3 + i * 2] = HEX[(byte & 0xf) as usize];
            }
            Ok(unsafe { core::str::from_utf8_unchecked(out) })
        }
        other => arena.alloc_str_display(other).map_err(|_| arena_full()),
    }
}

fn to_i64_for_cast(v: &Datum, target: &'static str) -> Result<i64, SqlError> {
    if let Datum::Numeric(n) = v {
        return n.to_i64().map_err(|_| overflow(target));
    }
    match v {
        Datum::Int4(x) => Ok(i64::from(*x)),
        Datum::Int8(x) => Ok(*x),
        Datum::Bool(b) => Ok(i64::from(*b)),
        Datum::Float8(x) => {
            // PostgreSQL rounds half away from zero.
            let rounded = x.round();
            if rounded >= i64::MIN as f64 && rounded <= i64::MAX as f64 {
                Ok(rounded as i64)
            } else {
                Err(overflow(target))
            }
        }
        Datum::Text(s) => parse_int_literal(s).ok_or_else(|| bad_text(s, target)),
        Datum::Null => unreachable!("null handled by caller"),
        other => Err(cast_unsupported(other, target)),
    }
}

/// Parses an integer the way PostgreSQL's integer input does: optional sign, an
/// optional `0x`/`0o`/`0b` base prefix, and `_` digit separators (only between
/// digits). Returns None for anything malformed or out of `i64` range.
pub(crate) fn parse_int_literal(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) = if let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16, r)
    } else if let Some(r) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        (8, r)
    } else if let Some(r) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        (2, r)
    } else {
        (10, rest)
    };
    let db = digits.as_bytes();
    if db.is_empty() || db[0] == b'_' || db[db.len() - 1] == b'_' {
        return None;
    }
    let mut buffer = [0u8; 80];
    let mut n = 0;
    let mut prev_underscore = false;
    for &c in db {
        if c == b'_' {
            if prev_underscore {
                return None; // `__` is not allowed
            }
            prev_underscore = true;
            continue;
        }
        prev_underscore = false;
        if n >= buffer.len() {
            return None;
        }
        buffer[n] = c;
        n += 1;
    }
    let cleaned = core::str::from_utf8(&buffer[..n]).ok()?;
    let v = i64::from_str_radix(cleaned, radix).ok()?;
    Some(if neg { -v } else { v })
}

/// Converts a temporal datum to microseconds from the PostgreSQL epoch, as the
/// symbolic-age functions need. A date is taken at midnight.
fn timestamp_micros(name: &str, d: Datum) -> Result<i64, SqlError> {
    match d {
        Datum::Timestamp(t) | Datum::Timestamptz(t) => Ok(t),
        Datum::Date(day) => Ok(i64::from(day) * 86_400_000_000),
        other => Err(type_mismatch(name, &other)),
    }
}

/// A numeric scaling factor for `interval * n` / `interval / n` (integer,
/// double, or numeric). Text and other types are not factors.
fn num_factor(d: &Datum) -> Option<f64> {
    match d {
        Datum::Int4(x) => Some(f64::from(*x)),
        Datum::Int8(x) => Some(*x as f64),
        Datum::Float8(x) => Some(*x),
        Datum::Numeric(n) => Some(n.to_f64()),
        _ => None,
    }
}

fn parse_bool(s: &str) -> Result<bool, SqlError> {
    // Accepted spellings per PostgreSQL's boolean input, case-insensitive.
    let t = s.trim();
    if ["t", "true", "yes", "on", "1"].iter().any(|w| t.eq_ignore_ascii_case(w)) {
        Ok(true)
    } else if ["f", "false", "no", "off", "0"].iter().any(|w| t.eq_ignore_ascii_case(w)) {
        Ok(false)
    } else {
        Err(bad_text(s, "boolean"))
    }
}

/// Promotes an integer or numeric datum to Numeric (arena-allocated).
fn to_numeric<'a>(d: &Datum, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    match d {
        Datum::Numeric(n) => Ok(Numeric {
            sign: n.sign,
            weight: n.weight,
            dscale: n.dscale,
            // Re-alloc digit bytes into this arena scope.
            digits: arena.alloc_slice_copy(n.digits).map_err(|_| overflow("numeric"))?,
        }),
        Datum::Int4(x) => Numeric::from_i64(*x as i64, arena),
        Datum::Int8(x) => Numeric::from_i64(*x, arena),
        other => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "cannot use {:?} as numeric",
            other
        )),
    }
}

/// PostgreSQL type name for a datum, for operator-error messages.
/// PostgreSQL's `interval_cmp_value`: the canonical microsecond magnitude used
/// to order intervals, counting a month as 30 days and a day as 24 hours. i128
/// keeps the full range exact.
fn interval_cmp_value(interval: super::types::Interval) -> i128 {
    i128::from(interval.months) * 30 * 86_400_000_000
        + i128::from(interval.days) * 86_400_000_000
        + i128::from(interval.micros)
}

/// `EXTRACT` / `date_part` on an interval, decomposing its `(months, days,
/// micros)` components exactly as PostgreSQL's `interval2tm` does (truncating
/// division toward zero, so negative intervals split the same way). Hours are
/// not rolled into days, and the year-scaled fields (decade/century/millennium)
/// use plain division, not the AD/BC-adjusted timestamp rule. `numeric_result`
/// selects `EXTRACT` (numeric) over `date_part` (double precision).
fn interval_extract<'a>(
    numeric_result: bool,
    field: &str,
    interval: super::types::Interval,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use super::numeric::Numeric;
    let eq = |k: &str| field.eq_ignore_ascii_case(k);
    let months = i64::from(interval.months);
    let days = i64::from(interval.days);
    let micros = interval.micros;
    let year = months / 12;
    let hour = micros / 3_600_000_000;
    let after_hour = micros % 3_600_000_000;
    let minute = after_hour / 60_000_000;
    let sub_minute = after_hour % 60_000_000; // whole seconds + fractional micros
    let int_val: Option<i64> = if eq("year") || eq("years") {
        Some(year)
    } else if eq("month") || eq("months") {
        Some(months % 12)
    } else if eq("day") || eq("days") {
        Some(days)
    } else if eq("hour") || eq("hours") {
        Some(hour)
    } else if eq("minute") || eq("minutes") {
        Some(minute)
    } else if eq("microseconds") {
        Some(sub_minute)
    } else if eq("decade") || eq("decades") {
        Some(year / 10)
    } else if eq("century") || eq("centuries") {
        Some(year / 100)
    } else if eq("millennium") || eq("millennia") {
        Some(year / 1000)
    } else if eq("quarter") {
        Some((months % 12) / 3 + 1)
    } else {
        None
    };
    if let Some(v) = int_val {
        return Ok(if numeric_result {
            Datum::Numeric(Numeric::from_i64(v, arena)?)
        } else {
            Datum::Float8(v as f64)
        });
    }
    // Fractional fields carried in microseconds, with PostgreSQL's per-unit
    // display scale (seconds/epoch → 6 fractional digits, milliseconds → 3).
    // `epoch` scales whole years by 365.25 days and residual months by 30 days
    // (PostgreSQL's DAYS_PER_YEAR / DAYS_PER_MONTH); i128 keeps it exact.
    let (value_micros, divisor, decimals): (i128, i128, usize) = if eq("second") || eq("seconds") {
        (i128::from(sub_minute), 1_000_000, 6)
    } else if eq("milliseconds") {
        (i128::from(sub_minute), 1_000, 3)
    } else if eq("epoch") {
        let epoch = (i128::from(months) / 12) * 31_557_600_000_000
            + (i128::from(months) % 12) * 2_592_000_000_000
            + i128::from(days) * 86_400_000_000
            + i128::from(micros);
        (epoch, 1_000_000, 6)
    } else {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "unit \"{}\" not supported for type interval",
            field
        ));
    };
    if numeric_result {
        let neg = value_micros < 0;
        let magnitude = value_micros.unsigned_abs();
        let text = stack_format!(
            48,
            "{}{}.{:0width$}",
            if neg { "-" } else { "" },
            magnitude / divisor as u128,
            magnitude % divisor as u128,
            width = decimals
        );
        Ok(Datum::Numeric(Numeric::parse(text.as_str(), arena)?))
    } else {
        Ok(Datum::Float8(value_micros as f64 / divisor as f64))
    }
}

fn type_name_of(d: &Datum) -> &'static str {
    match d {
        Datum::Null => "unknown",
        Datum::Bool(_) => "boolean",
        Datum::Int4(_) => "integer",
        Datum::Int8(_) => "bigint",
        Datum::Float8(_) => "double precision",
        Datum::Numeric(_) => "numeric",
        Datum::Text(_) => "text",
        Datum::Date(_) => "date",
        Datum::Timestamp(_) => "timestamp without time zone",
        Datum::Timestamptz(_) => "timestamp with time zone",
        Datum::Time(_) => "time without time zone",
        Datum::Interval(_) => "interval",
        Datum::Json { jsonb: false, .. } => "json",
        Datum::Json { jsonb: true, .. } => "jsonb",
        Datum::Array { .. } => "array",
        Datum::Uuid(_) => "uuid",
        Datum::Bytea(_) => "bytea",
        Datum::Range { kind, .. } => kind.name(),
        Datum::Bit { varying: false, .. } => "bit",
        Datum::Bit { varying: true, .. } => "bit varying",
        Datum::Multirange { kind, .. } => kind.multirange_name(),
        Datum::Record(_) => "record",
    }
}

fn as_i64(d: &Datum) -> Option<i64> {
    match d {
        Datum::Int4(x) => Some(i64::from(*x)),
        Datum::Int8(x) => Some(*x),
        _ => None,
    }
}

fn as_f64(d: &Datum) -> Option<f64> {
    if let Datum::Numeric(n) = d {
        return Some(n.to_f64());
    }
    match d {
        Datum::Int4(x) => Some(f64::from(*x)),
        Datum::Int8(x) => Some(*x as f64),
        Datum::Float8(x) => Some(*x),
        _ => None,
    }
}

fn overflow(what: &'static str) -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "{} out of range", what)
}

fn division_by_zero() -> SqlError {
    sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero")
}

/// [`type_mismatch`] for callers outside this module (table-function args).
pub fn type_mismatch_pub(operator: &str, d: &Datum) -> SqlError {
    type_mismatch(operator, d)
}

fn type_mismatch(operator: &str, d: &Datum) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "operator {} does not accept {}",
        operator,
        type_name_of(d)
    )
}

fn cast_unsupported(from: &Datum, to: &'static str) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "cannot cast {} to {}",
        type_name_of(from),
        to
    )
}

fn bad_text(s: &str, target: &'static str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type {}: \"{}\"",
        target,
        s
    )
}

pub(crate) fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "statement too large for SQL arena"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::sql::parser::Parser;
    use crate::sql::ast::{SelectItem, Stmt};

    fn eval_one<'a>(arena: &'a Arena, text: &'a str) -> Result<Datum<'a>, SqlError> {
        let mut p = Parser::new(text, arena).unwrap();
        let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
            panic!()
        };
        let SelectItem::Expr { expression, .. } = s.items[0] else { panic!() };
        eval(expression, arena, NO_PARAMS, &NoColumns)
    }

    fn with_arena(f: impl FnOnce(&Arena)) {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 18).unwrap();
        f(&arena);
    }

    #[test]
    fn arithmetic_matches_postgres() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT 1 + 2 * 3").unwrap(), Datum::Int4(7));
            assert_eq!(eval_one(a, "SELECT 7 / 2").unwrap(), Datum::Int4(3));
            assert_eq!(eval_one(a, "SELECT 7 % 2").unwrap(), Datum::Int4(1));
            // Decimal literals are NUMERIC (as in PostgreSQL), so 7.0/2 is
            // numeric 3.5000000000000000, not float8.
            assert_eq!(
                eval_one(a, "SELECT 7.0 / 2").unwrap().to_string(),
                "3.5000000000000000"
            );
            assert_eq!(eval_one(a, "SELECT 7.0::float8 / 2").unwrap(), Datum::Float8(3.5));
            assert_eq!(eval_one(a, "SELECT -(-5)").unwrap(), Datum::Int4(5));
            // int4 + int4 overflows like PostgreSQL (no silent widening);
            // int8 arithmetic carries the value.
            assert_eq!(
                eval_one(a, "SELECT 2147483647 + 1").unwrap_err().sqlstate,
                "22003"
            );
            assert_eq!(
                eval_one(a, "SELECT 2147483647::bigint + 1").unwrap(),
                Datum::Int8(2147483648)
            );
        });
    }

    #[test]
    fn division_by_zero_is_22012() {
        with_arena(|a| {
            for q in ["SELECT 1/0", "SELECT 1.0/0", "SELECT 1%0"] {
                let err = eval_one(a, q).unwrap_err();
                assert_eq!(err.sqlstate, "22012", "{q}");
            }
        });
    }

    #[test]
    fn int8_overflow_is_22003() {
        with_arena(|a| {
            let err = eval_one(a, "SELECT 9223372036854775807 + 1").unwrap_err();
            assert_eq!(err.sqlstate, "22003");
        });
    }

    #[test]
    fn three_valued_logic() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT NULL AND FALSE").unwrap(), Datum::Bool(false));
            assert_eq!(eval_one(a, "SELECT NULL AND TRUE").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT NULL OR TRUE").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT NULL OR FALSE").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT NOT NULL::bool").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT 1 = NULL").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT NULL IS NULL").unwrap(), Datum::Bool(true));
        });
    }

    #[test]
    fn comparisons_and_concat() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT 1 < 2").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT 2.5 >= 2").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT 'abc' < 'abd'").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT 'a' || 'b' || 'c'").unwrap(), Datum::Text("abc"));
            assert_eq!(eval_one(a, "SELECT 'n=' || 42").unwrap(), Datum::Text("n=42"));
            assert_eq!(eval_one(a, "SELECT 'x' || NULL").unwrap(), Datum::Null);
        });
    }

    #[test]
    fn casts() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT '42'::int").unwrap(), Datum::Int4(42));
            assert_eq!(eval_one(a, "SELECT 42::bigint").unwrap(), Datum::Int8(42));
            assert_eq!(eval_one(a, "SELECT 2.7::int").unwrap(), Datum::Int4(3));
            assert_eq!(eval_one(a, "SELECT true::text").unwrap(), Datum::Text("true"));
            assert_eq!(eval_one(a, "SELECT 'on'::bool").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT '2.5'::float8").unwrap(), Datum::Float8(2.5));
            let err = eval_one(a, "SELECT 'zap'::int").unwrap_err();
            assert_eq!(err.sqlstate, "22P02");
            let err = eval_one(a, "SELECT 1::geometry").unwrap_err();
            assert_eq!(err.sqlstate, "42704");
        });
    }
}
