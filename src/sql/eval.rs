//! Expression evaluation with PostgreSQL semantics: three-valued logic,
//! NULL propagation through operators, checked integer arithmetic
//! (overflow is an error, not a wrap), and division by zero as SQLSTATE
//! 22012 for integers and floats alike.

use crate::mem::arena::Arena;
use crate::stack_format;
use crate::util::StackStr;
use core::fmt::Write as _;

use super::ast::{BinaryOp, Expr, UnaryOp};
use super::numeric::{self, Numeric};
use super::types::{ColType, Datum};

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

    /// Static column type, if known — used to unify CASE branch types so a
    /// column reference contributes its declared type. Defaults to unknown.
    fn col_type(&self, _qualifier: Option<&str>, _name: &str) -> Option<ColType> {
        None
    }
}

/// A reference to a lookup is itself a lookup, so `&dyn ColumnLookup` can be
/// passed to the generic `eval`/`where_passes` helpers.
impl<'a, T: ColumnLookup<'a> + ?Sized> ColumnLookup<'a> for &T {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        (**self).lookup(qualifier, name)
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        (**self).col_type(qualifier, name)
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
    /// (group-by expressions, this group's key values).
    pub group: Option<(&'h [&'h Expr<'h>], &'h [Datum<'a>])>,
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
    /// Scalar subqueries: (node address, value).
    pub scalars: &'h [(*const Expr<'h>, Datum<'a>)],
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
    expr: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
) -> Result<Datum<'a>, SqlError> {
    eval_full(expr, arena, params, row, &NO_HOOKS)
}

/// Surfaces errors from every maximal constant subexpression, as
/// PostgreSQL's plan-time constant folding does: `SELECT 1/0` and
/// `... OR 0.0/0.0 > 1` error even when no row would reach them. Constant
/// subtrees are evaluated once here; per-row evaluation (with short-circuit)
/// handles the rest.
pub fn check_constant_errors<'a>(expr: &Expr<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    fold_check(expr, arena).map(|_| ())
}

/// The simplification-aware core of [`check_constant_errors`], mirroring
/// PostgreSQL's `eval_const_expressions`: it folds constant subexpressions
/// (surfacing their errors) but simplifies `A AND FALSE`→`FALSE`,
/// `A OR TRUE`→`TRUE`, and constant `CASE` arms — so a constant error inside a
/// branch that simplification *drops* is not surfaced (PostgreSQL evaluates
/// `... WHERE FALSE AND (id > (-1 % 0))` to no rows, never folding `-1 % 0`).
/// Returns the folded boolean value when the expression provably reduces to
/// one, else `None`.
fn fold_check<'a>(expr: &Expr<'a>, arena: &'a Arena) -> Result<Option<bool>, SqlError> {
    use super::ast::BinaryOp;
    if expr.is_constant() {
        // A fully-constant subtree folds eagerly; its error surfaces here.
        return Ok(match eval(expr, arena, NO_PARAMS, &NoColumns)? {
            Datum::Bool(b) => Some(b),
            _ => None,
        });
    }
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_)
        | Expr::NumericLit(_) | Expr::Str(_) | Expr::Column { .. }
        | Expr::Param(_) | Expr::DefaultMarker => Ok(None),
        // Boolean connectives short-circuit like PostgreSQL's folding: a FALSE
        // (AND) / TRUE (OR) operand settles the result and drops the sibling,
        // so the sibling's constant errors are never surfaced.
        Expr::Binary { op: BinaryOp::And, left, right } => {
            if fold_check(left, arena)? == Some(false) {
                return Ok(Some(false));
            }
            if fold_check(right, arena)? == Some(false) {
                return Ok(Some(false));
            }
            Ok(None)
        }
        Expr::Binary { op: BinaryOp::Or, left, right } => {
            if fold_check(left, arena)? == Some(true) {
                return Ok(Some(true));
            }
            if fold_check(right, arena)? == Some(true) {
                return Ok(Some(true));
            }
            Ok(None)
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
    expr: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    // Group-key substitution: any expression equal to a GROUP BY key
    // evaluates to the group's value.
    if let Some((exprs, values)) = hooks.group {
        for (g, v) in exprs.iter().zip(values) {
            if **g == *expr {
                return Ok(*v);
            }
        }
    }
    match *expr {
        Expr::Null => Ok(Datum::Null),
        Expr::Bool(b) => Ok(Datum::Bool(b)),
        Expr::Int(v) => Ok(if let Ok(small) = i32::try_from(v) {
            Datum::Int4(small)
        } else {
            Datum::Int8(v)
        }),
        Expr::Float(v) => Ok(Datum::Float8(v)),
        Expr::NumericLit(s) => Ok(Datum::Numeric(Numeric::parse(s, arena)?)),
        Expr::Str(s) => Ok(Datum::Text(s)),
        Expr::Column { qualifier, name } => row.lookup(qualifier, name),
        Expr::Param(n) => params
            .get(n as usize - 1)
            .copied()
            .ok_or_else(|| sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "there is no parameter ${}",
                n
            )),
        Expr::Unary { op, operand } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            unary(op, v)
        }
        Expr::Binary { op: BinaryOp::And, left, right } => {
            // PostgreSQL simplifies `x AND FALSE` to FALSE and short-circuits a
            // scan qual in a cost order that is not fixed, so a FALSE operand
            // determines the result even when the *other* operand would error at
            // runtime. Match that: a definite FALSE on either side yields FALSE
            // and absorbs the sibling's runtime error. A constant erroring
            // operand still errors — `check_constant_errors` surfaces it before
            // we get here, so anything that reaches this point is per-row.
            eval_logic_short_circuit(BinaryOp::And, left, right, arena, params, row, hooks)
        }
        Expr::Binary { op: BinaryOp::Or, left, right } => {
            // Dual of AND: a definite TRUE on either side yields TRUE and
            // absorbs the sibling's runtime error (PostgreSQL's `x OR TRUE`).
            eval_logic_short_circuit(BinaryOp::Or, left, right, arena, params, row, hooks)
        }
        Expr::Binary { op, left, right } => {
            let l = eval_full(left, arena, params, row, hooks)?;
            let r = eval_full(right, arena, params, row, hooks)?;
            // Track which side is an "unknown" literal (a string literal or a
            // parameter): only those coerce to the other operand's type, as
            // PostgreSQL does. A real text value never coerces to a number.
            binary(op, l, r, is_unknown_literal(left), is_unknown_literal(right), arena)
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
            Ok(Datum::Bool(v.is_null() != negated))
        }
        Expr::Call { name, args, star, distinct, over, .. } => {
            // A window-function call resolves to this row's precomputed value.
            if over.is_some()
                && let Some((nodes, values)) = hooks.windows
            {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expr as *const _) {
                        return Ok(*v);
                    }
                }
            }
            if let Some((nodes, values)) = hooks.aggs {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expr as *const _) {
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
                Some(op) => Some(eval_full(op, arena, params, row, hooks)?),
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
                for (node, v) in subs.scalars {
                    if core::ptr::eq(*node, expr as *const _) {
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
                if core::ptr::eq(*node, expr as *const _) {
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
                for (node, v) in subs.scalars {
                    if core::ptr::eq(*node, expr as *const _) {
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
            let mut elem: Option<super::types::ArrElem> = None;
            for (i, e) in items.iter().enumerate() {
                let v = eval_full(e, arena, params, row, hooks)?;
                if let Some(el) = super::types::ArrElem::from_datum(&v) {
                    elem = Some(elem.map_or(el, |acc| unify_arr_elem(acc, el)));
                }
                vals[i] = v;
            }
            let elem = elem.unwrap_or(super::types::ArrElem::Int4);
            // Coerce each element to the unified type.
            let ct = elem.to_coltype();
            for v in vals.iter_mut().take(items.len()) {
                if !v.is_null() {
                    *v = cast_to(*v, ct, arena)?;
                }
            }
            Ok(Datum::Array { elem, raw: super::array::build(&vals[..items.len()], arena)? })
        }
        Expr::Subscript { base, index } => {
            let b = eval_full(base, arena, params, row, hooks)?;
            let i = eval_full(index, arena, params, row, hooks)?;
            let idx = match i {
                Datum::Int4(x) => x as i64,
                Datum::Int8(x) => x,
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("array subscript must be integer", &i)),
            };
            match b {
                Datum::Array { elem, raw } => {
                    // PostgreSQL array subscripts are 1-based.
                    if idx < 1 {
                        return Ok(Datum::Null);
                    }
                    Ok(super::array::get(raw, elem, (idx - 1) as usize).unwrap_or(Datum::Null))
                }
                Datum::Null => Ok(Datum::Null),
                _ => Err(type_mismatch("cannot subscript a non-array", &b)),
            }
        }
        Expr::Field { base, field } => {
            // The only composite we produce is the `_pg_expandarray` result,
            // encoded as the 2-element array `[x, n]`. `.x` is the element and
            // `.n` the 1-based ordinal.
            let b = eval_full(base, arena, params, row, hooks)?;
            match b {
                Datum::Null => Ok(Datum::Null),
                Datum::Array { elem, raw } => {
                    let idx = if field.eq_ignore_ascii_case("x") || field.eq_ignore_ascii_case("f1")
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
                    Ok(super::array::get(raw, elem, idx).unwrap_or(Datum::Null))
                }
                _ => Err(type_mismatch("field access on a non-composite value", &b)),
            }
        }
        Expr::AnyAll { operand, op, array, all } => {
            let lhs = eval_full(operand, arena, params, row, hooks)?;
            let arr = eval_full(array, arena, params, row, hooks)?;
            let (elem, raw) = match arr {
                Datum::Array { elem, raw } => (elem, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("ANY/ALL requires an array", &arr)),
            };
            let n = super::array::len(raw);
            let mut saw_null = false;
            for i in 0..n {
                let el = super::array::get(raw, elem, i).unwrap_or(Datum::Null);
                match binary(op, lhs, el, false, false, arena)? {
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
    match name {
        "count" | "sum" | "avg" | "min" | "max" | "bool_and" | "bool_or" | "every"
        | "string_agg" => Err(sql_err!(
            "42803",
            "aggregate functions are not allowed here"
        )),
        "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp" => {
            arity(0)?;
            Ok(Datum::Timestamptz(super::datetime::now_micros()))
        }
        "current_date" => {
            arity(0)?;
            Ok(Datum::Date(
                super::datetime::now_micros().div_euclid(86_400_000_000) as i32,
            ))
        }
        "version" => {
            arity(0)?;
            Ok(Datum::Text(concat!(
                "PostgreSQL 18.4 (pos3ql ",
                env!("CARGO_PKG_VERSION"),
                ") on aarch64-apple-darwin"
            )))
        }
        "current_database" | "current_catalog" => {
            arity(0)?;
            Ok(Datum::Text("postgres"))
        }
        "current_schema" => {
            arity(0)?;
            Ok(Datum::Text("public"))
        }
        // `current_schemas(bool)` returns the search-path schemas as a text[];
        // with `true` it prepends the implicit pg_catalog.
        "current_schemas" => {
            arity(1)?;
            let include_implicit =
                matches!(eval_full(args[0], arena, params, row, hooks)?, Datum::Bool(true));
            let elems: &[Datum] = if include_implicit {
                &[Datum::Text("pg_catalog"), Datum::Text("public")]
            } else {
                &[Datum::Text("public")]
            };
            Ok(Datum::Array {
                elem: super::types::ArrElem::Text,
                raw: super::array::build(elems, arena)?,
            })
        }
        "current_user" | "session_user" | "user" => {
            arity(0)?;
            Ok(Datum::Text("pos3ql"))
        }
        // Catalog helpers for psql introspection. Every user object lives in the
        // single visible schema owned by the connection role.
        "pg_get_userbyid" => {
            arity(1)?;
            Ok(Datum::Text("pos3ql"))
        }
        // A non-partitioned table is its own only ancestor/root; we have no
        // partitioning, so these return the argument unchanged.
        "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
            arity(1)?;
            eval_full(args[0], arena, params, row, hooks)
        }
        "array_length" | "cardinality" | "array_upper" => {
            let a = eval_full(args[0], arena, params, row, hooks)?;
            match a {
                Datum::Array { raw, .. } => {
                    let n = super::array::len(raw);
                    // array_length/array_upper of an empty array is NULL (PG).
                    if n == 0 && name != "cardinality" {
                        Ok(Datum::Null)
                    } else {
                        Ok(Datum::Int4(n as i32))
                    }
                }
                Datum::Null => Ok(Datum::Null),
                _ => Err(type_mismatch("array_length requires an array", &a)),
            }
        }
        "array_lower" => {
            // Lower bound is always 1 for our arrays.
            let a = eval_full(args[0], arena, params, row, hooks)?;
            match a {
                Datum::Array { raw, .. } if super::array::len(raw) > 0 => Ok(Datum::Int4(1)),
                Datum::Array { .. } | Datum::Null => Ok(Datum::Null),
                _ => Err(type_mismatch("array_lower requires an array", &a)),
            }
        }
        "array_position" => {
            // 1-based index of the first element equal to the target (NULL-safe),
            // or NULL if absent.
            arity(2)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let target = eval_full(args[1], arena, params, row, hooks)?;
            let (elem, raw) = match a {
                Datum::Array { elem, raw } => (elem, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("array_position requires an array", &a)),
            };
            for i in 0..super::array::len(raw) {
                let el = super::array::get(raw, elem, i).unwrap_or(Datum::Null);
                let hit = if target.is_null() {
                    el.is_null()
                } else if el.is_null() {
                    false
                } else {
                    compare_datums(&el, &target)?.is_eq()
                };
                if hit {
                    return Ok(Datum::Int4((i + 1) as i32));
                }
            }
            Ok(Datum::Null)
        }
        "jsonb_array_length" | "json_array_length" => {
            arity(1)?;
            let s = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch(name, &other)),
            };
            match super::json::parse(s, arena)? {
                super::json::Json::Array(items) => Ok(Datum::Int4(items.len() as i32)),
                _ => Err(sql_err!("22023", "cannot get array length of a scalar")),
            }
        }
        "pg_table_is_visible" | "pg_type_is_visible" | "pg_function_is_visible"
        | "has_table_privilege" | "has_column_privilege" | "has_schema_privilege"
        | "pg_relation_is_publishable" => {
            Ok(Datum::Bool(true))
        }
        // Set-returning `_pg_expandarray(arr)` yields, for the current expansion
        // index k, the composite `(x, n)` = (arr[k], k), encoded as `[x, n]`.
        "_pg_expandarray" => {
            arity(1)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let (elem, raw) = match a {
                Datum::Array { elem, raw } => (elem, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("_pg_expandarray requires an array", &a)),
            };
            let k = hooks.srf_index.unwrap_or(1);
            let x = super::array::get(raw, elem, k - 1).unwrap_or(Datum::Null);
            let comp = [x, Datum::Int4(k as i32)];
            Ok(Datum::Array {
                elem: super::types::ArrElem::Int4,
                raw: super::array::build(&comp, arena)?,
            })
        }
        "pg_get_indexdef" => {
            // `pg_get_indexdef(oid)` / `(oid, 0, _)` reconstruct the whole
            // `btree (cols)` definition; `(oid, n, _)` with n>0 returns the name
            // of the n-th (1-based) indexed column (used by JDBC getIndexInfo).
            let Some(cat) = hooks.catalog else {
                return Ok(Datum::Null);
            };
            let oid = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Int4(v) => v,
                Datum::Int8(v) => v as i32,
                _ => return Ok(Datum::Null),
            };
            let col = if args.len() >= 2 {
                match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Int4(v) => v.max(0) as usize,
                    Datum::Int8(v) => v.max(0) as usize,
                    _ => 0,
                }
            } else {
                0
            };
            Ok(cat.index_def(oid, col, arena)?.map(Datum::Text).unwrap_or(Datum::Null))
        }
        "pg_get_constraintdef" => {
            // psql `\d` calls this with a constraint OID; reconstruct a
            // foreign-key definition via the catalog resolver when present.
            let Some(cat) = hooks.catalog else {
                return Ok(Datum::Null);
            };
            let oid = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Int4(v) => v,
                Datum::Int8(v) => v as i32,
                _ => return Ok(Datum::Null),
            };
            Ok(cat.constraint_def(oid, arena)?.map(Datum::Text).unwrap_or(Datum::Null))
        }
        "pg_get_expr" | "pg_get_viewdef"
        | "pg_get_functiondef" | "col_description" | "obj_description"
        | "shobj_description" | "pg_get_statisticsobjdef_columns" => {
            // Definitions/comments we do not reconstruct render as empty/NULL,
            // as PostgreSQL does for an absent comment.
            Ok(Datum::Null)
        }
        "format_type" => {
            arity(2)?;
            // format_type(typoid, typmod): map the common base-type oids back to
            // their SQL spelling; unknown oids render as "???".
            let o = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Int4(v) => v,
                Datum::Int8(v) => v as i32,
                Datum::Null => return Ok(Datum::Null),
                _ => -1,
            };
            let name = super::exec::coltype_of_oid(o)
                .map(|t| t.name())
                .unwrap_or("???");
            Ok(Datum::Text(name))
        }
        "pg_encoding_to_char" => {
            arity(1)?;
            Ok(Datum::Text("UTF8"))
        }
        "array_to_string" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "array_to_string expects 2 or 3 arguments"
                ));
            }
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let delim = eval_full(args[1], arena, params, row, hooks)?;
            // Third argument is the string substituted for NULL elements; when
            // absent (or itself NULL) NULL elements are omitted entirely.
            let nullrep = if args.len() == 3 {
                match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Null => None,
                    Datum::Text(s) => Some(s),
                    other => return Err(type_mismatch("array_to_string null string", &other)),
                }
            } else {
                None
            };
            let (elem, raw) = match a {
                Datum::Null => return Ok(Datum::Null),
                Datum::Array { elem, raw } => (elem, raw),
                other => return Err(type_mismatch("array_to_string", &other)),
            };
            let delim = match delim {
                Datum::Null => return Ok(Datum::Null),
                Datum::Text(s) => s,
                other => return Err(type_mismatch("array_to_string delimiter", &other)),
            };
            let count = super::array::len(raw);
            // Renders the i-th element as text, or `None` to omit it (a NULL
            // element with no null-string replacement).
            let elem_text = |i: usize| -> Result<Option<&'a str>, SqlError> {
                match super::array::get(raw, elem, i) {
                    Some(Datum::Null) | None => Ok(nullrep),
                    Some(v) => match cast_to(v, ColType::Text, arena)? {
                        Datum::Text(s) => Ok(Some(s)),
                        Datum::Null => Ok(nullrep),
                        other => Err(type_mismatch("array_to_string element", &other)),
                    },
                }
            };
            // Pass 1: total byte length; pass 2: fill (elements re-rendered).
            let mut total = 0usize;
            let mut first = true;
            for i in 0..count {
                if let Some(s) = elem_text(i)? {
                    if !first {
                        total += delim.len();
                    }
                    total += s.len();
                    first = false;
                }
            }
            let out = arena.alloc_slice_with(total, |_| 0u8).map_err(|_| arena_full())?;
            let mut at = 0;
            let mut first = true;
            for i in 0..count {
                if let Some(s) = elem_text(i)? {
                    if !first {
                        out[at..at + delim.len()].copy_from_slice(delim.as_bytes());
                        at += delim.len();
                    }
                    out[at..at + s.len()].copy_from_slice(s.as_bytes());
                    at += s.len();
                    first = false;
                }
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "length" | "char_length" | "character_length" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Text(s) => Ok(Datum::Int4(s.chars().count() as i32)),
                other => Err(type_mismatch("length", &other)),
            }
        }
        "upper" | "lower" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Text(s) => {
                    let upper = name == "upper";
                    // Two passes: measure, then fill the arena slice.
                    let map_len = |c: char| -> usize {
                        if upper {
                            c.to_uppercase().map(char::len_utf8).sum()
                        } else {
                            c.to_lowercase().map(char::len_utf8).sum()
                        }
                    };
                    let out_len: usize = s.chars().map(map_len).sum();
                    let out = arena
                        .alloc_slice_with(out_len, |_| 0u8)
                        .map_err(|_| arena_full())?;
                    let mut at = 0;
                    for c in s.chars() {
                        if upper {
                            for u in c.to_uppercase() {
                                at += u.encode_utf8(&mut out[at..]).len();
                            }
                        } else {
                            for u in c.to_lowercase() {
                                at += u.encode_utf8(&mut out[at..]).len();
                            }
                        }
                    }
                    Ok(Datum::Text(unsafe {
                        core::str::from_utf8_unchecked(out)
                    }))
                }
                other => Err(type_mismatch(name, &other)),
            }
        }
        "abs" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Int4(v) => v
                    .checked_abs()
                    .map(Datum::Int4)
                    .ok_or_else(|| overflow("integer")),
                Datum::Int8(v) => v
                    .checked_abs()
                    .map(Datum::Int8)
                    .ok_or_else(|| overflow("bigint")),
                Datum::Float8(v) => Ok(Datum::Float8(v.abs())),
                Datum::Numeric(n) => Ok(Datum::Numeric(Numeric {
                    sign: match n.sign {
                        super::numeric::Sign::Neg => super::numeric::Sign::Pos,
                        other => other,
                    },
                    ..n
                })),
                other => Err(type_mismatch("abs", &other)),
            }
        }
        "coalesce" => {
            for arg in args {
                let v = eval_full(arg, arena, params, row, hooks)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Datum::Null)
        }
        "pg_typeof" => {
            arity(1)?;
            let v = eval_full(args[0], arena, params, row, hooks)?;
            Ok(Datum::Text(match v {
                Datum::Null => "unknown",
                Datum::Bool(_) => "boolean",
                Datum::Int4(_) => "integer",
                Datum::Int8(_) => "bigint",
                Datum::Float8(_) => "double precision",
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
                Datum::Numeric(_) => "numeric",
            }))
        }
        "trim" | "btrim" | "ltrim" | "rtrim" => {
            if star || !(1..=2).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let chars = if args.len() == 2 {
                match text_arg(name, args, 1, arena, params, row, hooks)? {
                    Some(c) => c,
                    None => return Ok(Datum::Null),
                }
            } else {
                " "
            };
            let mut out = s;
            if name != "rtrim" {
                out = out.trim_start_matches(|c| chars.contains(c));
            }
            if name != "ltrim" {
                out = out.trim_end_matches(|c| chars.contains(c));
            }
            Ok(Datum::Text(out))
        }
        "regexp_replace" => {
            // regexp_replace(source, pattern, replacement [, flags]).
            if !(3..=4).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let (Some(src), Some(pat), Some(rep)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
                text_arg(name, args, 2, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let mut global = false;
            let mut ci = false;
            if args.len() == 4 {
                let Some(flags) = text_arg(name, args, 3, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                for f in flags.chars() {
                    match f {
                        'g' => global = true,
                        'i' => ci = true,
                        'c' => ci = false,
                        _ => {
                            return Err(sql_err!(
                                "22023",
                                "invalid regular expression option: \"{}\"",
                                f
                            ))
                        }
                    }
                }
            }
            let mut out = StackStr::<8192>::new();
            let mut pos = 0usize;
            while let Some((s, e)) = super::regex::find(pat, src, pos, ci)? {
                if out.write_str(&src[pos..s]).is_err() {
                    return Err(sql_err!("54000", "regexp_replace result too large"));
                }
                expand_replacement(&mut out, rep, &src[s..e])?;
                if e == s {
                    // Empty match: emit one source char and advance past it so
                    // the scan makes progress (PostgreSQL inserts between chars).
                    match src[e..].chars().next() {
                        Some(c) => {
                            let _ = out.write_char(c);
                            pos = e + c.len_utf8();
                        }
                        None => {
                            pos = e;
                            break;
                        }
                    }
                } else {
                    pos = e;
                }
                if !global {
                    break;
                }
            }
            if out.write_str(&src[pos..]).is_err() {
                return Err(sql_err!("54000", "regexp_replace result too large"));
            }
            Ok(Datum::Text(arena.alloc_str(out.as_str()).map_err(|_| arena_full())?))
        }
        "regexp_count" | "regexp_instr" | "regexp_substr" => {
            // (source, pattern [, start [, flags]]). `start` is a 1-based
            // character position; `flags` may contain 'i' (case-insensitive).
            if !(2..=4).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let (Some(src), Some(pat)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let start_char = if args.len() >= 3 {
                match int_arg(name, args, 2, arena, params, row, hooks)? {
                    Some(v) => v.max(1),
                    None => return Ok(Datum::Null),
                }
            } else {
                1
            };
            let mut ci = false;
            if args.len() == 4 {
                let Some(flags) = text_arg(name, args, 3, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                ci = flags.contains('i');
            }
            let begin = char_index_to_byte(src, (start_char - 1) as usize);
            if name == "regexp_count" {
                let mut count = 0i32;
                let mut pos = begin;
                while let Some((s, e)) = super::regex::find(pat, src, pos, ci)? {
                    count += 1;
                    pos = if e == s {
                        match src[e..].chars().next() {
                            Some(c) => e + c.len_utf8(),
                            None => break,
                        }
                    } else {
                        e
                    };
                }
                return Ok(Datum::Int4(count));
            }
            match super::regex::find(pat, src, begin, ci)? {
                None if name == "regexp_instr" => Ok(Datum::Int4(0)),
                None => Ok(Datum::Null),
                Some((s, _)) if name == "regexp_instr" => {
                    Ok(Datum::Int4(byte_to_char_1based(src, s)))
                }
                Some((s, e)) => {
                    Ok(Datum::Text(arena.alloc_str(&src[s..e]).map_err(|_| arena_full())?))
                }
            }
        }
        "string_to_array" => {
            // (string, delimiter [, null_string]) -> text[]. A NULL delimiter
            // splits into individual characters; elements equal to null_string
            // become NULL.
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let delim = text_arg(name, args, 1, arena, params, row, hooks)?;
            let null_str = if args.len() == 3 {
                text_arg(name, args, 2, arena, params, row, hooks)?
            } else {
                None
            };
            let mut items: [Datum; 1024] = [Datum::Null; 1024];
            let mut n = 0usize;
            // Collect the split pieces (slices of `s`, so they share its
            // lifetime); a piece equal to null_string becomes NULL.
            let mut pieces: [&str; 1024] = [""; 1024];
            match delim {
                Some("") => {
                    pieces[0] = s;
                    n = 1;
                }
                Some(d) if !s.is_empty() => {
                    for piece in s.split(d) {
                        if n >= pieces.len() {
                            return Err(sql_err!("54000", "string_to_array result too large"));
                        }
                        pieces[n] = piece;
                        n += 1;
                    }
                }
                Some(_) => {} // empty input yields an empty array
                None => {
                    for (i, c) in s.char_indices() {
                        if n >= pieces.len() {
                            return Err(sql_err!("54000", "string_to_array result too large"));
                        }
                        pieces[n] = &s[i..i + c.len_utf8()];
                        n += 1;
                    }
                }
            }
            for (k, &piece) in pieces[..n].iter().enumerate() {
                items[k] = if null_str == Some(piece) {
                    Datum::Null
                } else {
                    Datum::Text(piece)
                };
            }
            Ok(Datum::Array {
                elem: super::types::ArrElem::Text,
                raw: super::array::build(&items[..n], arena)?,
            })
        }
        "overlay" => {
            // overlay(s placing r from n [for l]): replace l characters of s
            // starting at 1-based position n with r (l defaults to length(r)).
            if !(3..=4).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let (Some(s), Some(r)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let Some(n) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let l = if args.len() == 4 {
                match int_arg(name, args, 3, arena, params, row, hooks)? {
                    Some(v) => v,
                    None => return Ok(Datum::Null),
                }
            } else {
                r.chars().count() as i64
            };
            // Prefix = first (n-1) chars of s; suffix = s from char (n-1+l).
            let prefix_chars = (n - 1).max(0) as usize;
            let skip_to = (n - 1 + l).max(0) as usize;
            let prefix_end = s.char_indices().nth(prefix_chars).map_or(s.len(), |(b, _)| b);
            let suffix_start = s.char_indices().nth(skip_to).map_or(s.len(), |(b, _)| b);
            let suffix_start = suffix_start.max(prefix_end);
            let total = prefix_end + r.len() + (s.len() - suffix_start);
            alloc_text(arena, &[&s[..prefix_end], r, &s[suffix_start..]], total)
        }
        "substr" | "substring" => {
            if star || !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let Some(from) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let count = if args.len() == 3 {
                match int_arg(name, args, 2, arena, params, row, hooks)? {
                    Some(c) => {
                        if c < 0 {
                            return Err(sql_err!("22011", "negative substring length not allowed"));
                        }
                        Some(c)
                    }
                    None => return Ok(Datum::Null),
                }
            } else {
                None
            };
            // 1-based window of character indices [max(from,1), from+count).
            let lo = from.max(1);
            let hi = count.map(|c| from.saturating_add(c));
            let mut start: Option<usize> = None;
            let mut end = s.len();
            for (k, (byte, _ch)) in (1_i64..).zip(s.char_indices()) {
                if start.is_none() && k >= lo {
                    start = Some(byte);
                }
                if hi == Some(k) || hi.is_some_and(|h| k > h) {
                    end = byte;
                    break;
                }
            }
            let start = start.unwrap_or(s.len());
            let end = end.max(start);
            Ok(Datum::Text(&s[start..end]))
        }
        "replace" => {
            arity(3)?;
            let (Some(s), Some(from), Some(to)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
                text_arg(name, args, 2, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            if from.is_empty() {
                return Ok(Datum::Text(s));
            }
            let n = s.matches(from).count();
            let out_len = s.len() + n * to.len().saturating_sub(from.len())
                - n * from.len().saturating_sub(to.len());
            let out = arena.alloc_slice_with(out_len, |_| 0u8).map_err(|_| arena_full())?;
            let mut at = 0;
            let mut rest = s;
            while let Some(pos) = rest.find(from) {
                out[at..at + pos].copy_from_slice(&rest.as_bytes()[..pos]);
                at += pos;
                out[at..at + to.len()].copy_from_slice(to.as_bytes());
                at += to.len();
                rest = &rest[pos + from.len()..];
            }
            out[at..at + rest.len()].copy_from_slice(rest.as_bytes());
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "repeat" => {
            arity(2)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let n = n.max(0) as usize;
            let out_len = s.len().checked_mul(n).ok_or_else(|| overflow("text"))?;
            let out = arena.alloc_slice_with(out_len, |_| 0u8).map_err(|_| arena_full())?;
            for i in 0..n {
                out[i * s.len()..(i + 1) * s.len()].copy_from_slice(s.as_bytes());
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "reverse" => {
            arity(1)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let out = arena.alloc_slice_with(s.len(), |_| 0u8).map_err(|_| arena_full())?;
            let mut at = s.len();
            for c in s.chars() {
                at -= c.len_utf8();
                c.encode_utf8(&mut out[at..]);
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "left" | "right" => {
            arity(2)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let total = s.chars().count() as i64;
            // Negative n means "all but the last/first |n| characters".
            let take = if name == "left" {
                if n < 0 { (total + n).max(0) } else { n.min(total) }
            } else if n < 0 {
                (total + n).max(0)
            } else {
                n.min(total)
            };
            let out = if name == "left" {
                let end: usize = s
                    .char_indices()
                    .nth(take as usize)
                    .map(|(b, _)| b)
                    .unwrap_or(s.len());
                &s[..end]
            } else {
                let start: usize = s
                    .char_indices()
                    .nth((total - take) as usize)
                    .map(|(b, _)| b)
                    .unwrap_or(s.len());
                &s[start..]
            };
            Ok(Datum::Text(out))
        }
        "strpos" => {
            arity(2)?;
            let (Some(s), Some(sub)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let pos = match s.find(sub) {
                Some(byte) => s[..byte].chars().count() as i32 + 1,
                None => 0,
            };
            Ok(Datum::Int4(pos))
        }
        "concat" => {
            // Concatenates every argument's text form, skipping NULLs.
            let mut total = 0usize;
            let mut parts: [&str; 32] = [""; 32];
            if args.len() > 32 || star {
                return Err(arity_err(name, args.len()));
            }
            let mut np = 0;
            for a in args {
                let v = eval_full(a, arena, params, row, hooks)?;
                if v.is_null() {
                    continue;
                }
                let t = datum_to_text(v, arena)?;
                parts[np] = t;
                total += t.len();
                np += 1;
            }
            alloc_text(arena, &parts[..np], total)
        }
        "concat_ws" => {
            if star || args.is_empty() {
                return Err(arity_err(name, args.len()));
            }
            let sep = match text_arg(name, args, 0, arena, params, row, hooks)? {
                Some(s) => s,
                None => return Ok(Datum::Null),
            };
            let mut parts: [&str; 64] = [""; 64];
            let mut np = 0;
            let mut total = 0usize;
            for a in &args[1..] {
                let v = eval_full(a, arena, params, row, hooks)?;
                if v.is_null() {
                    continue;
                }
                if np > 0 {
                    parts[np] = sep;
                    total += sep.len();
                    np += 1;
                }
                let t = datum_to_text(v, arena)?;
                parts[np] = t;
                total += t.len();
                np += 1;
            }
            alloc_text(arena, &parts[..np], total)
        }
        "initcap" => {
            arity(1)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            // Upper-case the first letter of each word (runs of alphanumerics),
            // lower-casing the rest — PostgreSQL's rule.
            let out_len: usize = s
                .chars()
                .map(|c| c.to_uppercase().map(char::len_utf8).sum::<usize>().max(c.len_utf8()))
                .sum::<usize>()
                .max(s.len());
            let out = arena.alloc_slice_with(out_len, |_| 0u8).map_err(|_| arena_full())?;
            let mut at = 0;
            let mut prev_alnum = false;
            for c in s.chars() {
                let mapped: &mut dyn Iterator<Item = char> = if c.is_alphanumeric() && !prev_alnum {
                    &mut c.to_uppercase()
                } else {
                    &mut c.to_lowercase()
                };
                for m in mapped {
                    at += m.encode_utf8(&mut out[at..]).len();
                }
                prev_alnum = c.is_alphanumeric();
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(&out[..at]) }))
        }
        "ascii" => {
            arity(1)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            Ok(match s.chars().next() {
                Some(c) => Datum::Int4(c as i32),
                None => Datum::Int4(0),
            })
        }
        "chr" => {
            arity(1)?;
            let Some(n) = int_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            if n == 0 {
                return Err(sql_err!("54000", "null character not permitted"));
            }
            let c = u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| sql_err!("22023", "requested character not valid for encoding"))?;
            let out = arena.alloc_slice_with(c.len_utf8(), |_| 0u8).map_err(|_| arena_full())?;
            c.encode_utf8(out);
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "octet_length" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Text(s) => Ok(Datum::Int4(s.len() as i32)),
                Datum::Bytea(b) => Ok(Datum::Int4(b.len() as i32)),
                other => Err(type_mismatch(name, &other)),
            }
        }
        "lpad" | "rpad" => {
            if star || !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let Some(len) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let fill = if args.len() == 3 {
                match text_arg(name, args, 2, arena, params, row, hooks)? {
                    Some(f) => f,
                    None => return Ok(Datum::Null),
                }
            } else {
                " "
            };
            let len = len.max(0) as usize;
            let s_len = s.chars().count();
            // Longer than the target: truncate to the first `len` characters.
            if s_len >= len {
                let end = s.char_indices().nth(len).map(|(b, _)| b).unwrap_or(s.len());
                return Ok(Datum::Text(&s[..end]));
            }
            if fill.is_empty() {
                return Ok(Datum::Text(s));
            }
            let pad_count = len - s_len;
            // Padding is `fill` repeated, cut to `pad_count` characters.
            let pad_len: usize = fill.chars().cycle().take(pad_count).map(char::len_utf8).sum();
            let total = pad_len + s.len();
            let buf = arena.alloc_slice_with(total, |_| 0u8).map_err(|_| arena_full())?;
            let mut at = 0;
            let write_pad = |buf: &mut [u8], at: &mut usize| {
                for c in fill.chars().cycle().take(pad_count) {
                    *at += c.encode_utf8(&mut buf[*at..]).len();
                }
            };
            if name == "lpad" {
                write_pad(buf, &mut at);
                buf[at..at + s.len()].copy_from_slice(s.as_bytes());
            } else {
                buf[at..at + s.len()].copy_from_slice(s.as_bytes());
                at += s.len();
                write_pad(buf, &mut at);
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(buf) }))
        }
        "split_part" => {
            arity(3)?;
            let (Some(s), Some(delim)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let Some(n) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            if n == 0 {
                return Err(sql_err!("22023", "field position must not be zero"));
            }
            if delim.is_empty() {
                return Ok(Datum::Text(if n == 1 || n == -1 { s } else { "" }));
            }
            let part = if n > 0 {
                s.split(delim).nth((n - 1) as usize).unwrap_or("")
            } else {
                let total = s.split(delim).count() as i64;
                let idx = total + n; // n is negative
                if idx < 0 {
                    ""
                } else {
                    s.split(delim).nth(idx as usize).unwrap_or("")
                }
            };
            Ok(Datum::Text(part))
        }
        "translate" => {
            arity(3)?;
            let (Some(s), Some(from), Some(to)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
                text_arg(name, args, 2, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            // Each character of `s` that appears in `from` is replaced by the
            // char at the same index in `to`, or removed if `to` is shorter.
            let out_cap: usize = s.chars().map(|c| c.len_utf8()).sum();
            let buf = arena.alloc_slice_with(out_cap.max(1), |_| 0u8).map_err(|_| arena_full())?;
            let mut at = 0;
            for c in s.chars() {
                match from.chars().position(|f| f == c) {
                    Some(i) => {
                        if let Some(r) = to.chars().nth(i) {
                            at += r.encode_utf8(&mut buf[at..]).len();
                        }
                        // else: removed.
                    }
                    None => {
                        at += c.encode_utf8(&mut buf[at..]).len();
                    }
                }
            }
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(&buf[..at]) }))
        }
        "greatest" | "least" => {
            if star || args.is_empty() {
                return Err(arity_err(name, args.len()));
            }
            // PostgreSQL types the result as the common type of all arguments,
            // so `least(1, 2.5)` is numeric even when the int wins. The rank
            // comes from each argument's *static* type (so a NULL float8 still
            // makes the result float8, as PostgreSQL does), falling back to the
            // runtime value for expressions whose static type is unknown.
            let rank = |d: &Datum| match d {
                Datum::Int4(_) => 1,
                Datum::Int8(_) => 2,
                Datum::Numeric(_) => 3,
                Datum::Float8(_) => 4,
                _ => 0,
            };
            let static_rank = |t: ColType| match t {
                ColType::Int4 => 1,
                ColType::Int8 => 2,
                ColType::Numeric => 3,
                ColType::Float8 => 4,
                _ => 0,
            };
            let mut best: Option<Datum> = None;
            let mut widest = 0u8;
            for a in args {
                if let Some(t) = static_type(a, row) {
                    widest = widest.max(static_rank(t));
                }
                let v = eval_full(a, arena, params, row, hooks)?;
                widest = widest.max(rank(&v));
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(cur) => {
                        let ord = compare_datums(&cur, &v)?;
                        let take_v = if name == "greatest" { ord.is_lt() } else { ord.is_gt() };
                        if take_v { v } else { cur }
                    }
                });
            }
            let best = best.unwrap_or(Datum::Null);
            Ok(match (widest, best) {
                (4, d) => Datum::Float8(match d {
                    Datum::Int4(x) => x as f64,
                    Datum::Int8(x) => x as f64,
                    Datum::Numeric(n) => n.to_f64(),
                    Datum::Float8(f) => f,
                    other => return Ok(other),
                }),
                (3, d) => match d {
                    Datum::Int4(x) => Datum::Numeric(Numeric::from_i64(x as i64, arena)?),
                    Datum::Int8(x) => Datum::Numeric(Numeric::from_i64(x, arena)?),
                    other => other,
                },
                (2, Datum::Int4(x)) => Datum::Int8(x as i64),
                (_, d) => d,
            })
        }
        "nullif" => {
            arity(2)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let b = eval_full(args[1], arena, params, row, hooks)?;
            if !a.is_null() && !b.is_null() && compare_datums(&a, &b)?.is_eq() {
                Ok(Datum::Null)
            } else {
                Ok(a)
            }
        }
        "floor" | "ceil" | "ceiling" | "trunc" | "round" => {
            use super::numeric::RoundMode;
            let mode = match name {
                "floor" => RoundMode::Floor,
                "ceil" | "ceiling" => RoundMode::Ceil,
                "trunc" => RoundMode::Trunc,
                _ => RoundMode::HalfAwayZero,
            };
            // round(x, n) / trunc(x, n) adjust a numeric to n fractional digits
            // (round: half away from zero; trunc: toward zero).
            if (name == "round" || name == "trunc") && args.len() == 2 {
                let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let v = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Numeric(v) => v,
                    Datum::Int4(x) => Numeric::from_i64(x as i64, arena)?,
                    Datum::Int8(x) => Numeric::from_i64(x, arena)?,
                    other => return Err(type_mismatch(name, &other)),
                };
                let result = if n >= 0 {
                    v.round_scale(n as usize, mode, arena)?
                } else {
                    // A negative scale rounds to the left of the point: round
                    // v / 10^|n| to an integer, then scale back up.
                    let pow = Numeric::parse(stack_format!(24, "1e{}", -n).as_str(), arena)?;
                    let scaled = numeric::div(&v, &pow, arena)?.round_scale(0, mode, arena)?;
                    numeric::mul(&scaled, &pow, arena)?
                };
                return Ok(Datum::Numeric(result));
            }
            if star || args.len() != 1 {
                return Err(arity_err(name, args.len()));
            }
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                // For an integer, floor/ceil/round/trunc are the identity; as in
                // PostgreSQL the result type is double precision.
                Datum::Int4(v) => Ok(Datum::Float8(v as f64)),
                Datum::Int8(v) => Ok(Datum::Float8(v as f64)),
                Datum::Float8(v) => Ok(Datum::Float8(match mode {
                    RoundMode::Floor => v.floor(),
                    RoundMode::Ceil => v.ceil(),
                    RoundMode::Trunc => v.trunc(),
                    RoundMode::HalfAwayZero => v.round_ties_even(),
                })),
                Datum::Numeric(v) => Ok(Datum::Numeric(v.round_scale(0, mode, arena)?)),
                other => Err(type_mismatch(name, &other)),
            }
        }
        "sign" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Int4(v) => Ok(Datum::Float8(v.signum() as f64)),
                Datum::Int8(v) => Ok(Datum::Float8(v.signum() as f64)),
                Datum::Float8(v) => Ok(Datum::Float8(if v > 0.0 {
                    1.0
                } else if v < 0.0 {
                    -1.0
                } else {
                    0.0
                })),
                Datum::Numeric(n) => {
                    let s = if n.is_zero() {
                        "0"
                    } else if n.sign == super::numeric::Sign::Neg {
                        "-1"
                    } else {
                        "1"
                    };
                    Ok(Datum::Numeric(Numeric::parse(s, arena)?))
                }
                other => Err(type_mismatch(name, &other)),
            }
        }
        "sqrt" | "exp" | "ln" => {
            arity(1)?;
            // A numeric argument keeps the numeric domain (arbitrary precision);
            // int/float arguments follow PostgreSQL and return double precision.
            let d = eval_full(args[0], arena, params, row, hooks)?;
            if d.is_null() {
                return Ok(Datum::Null);
            }
            if let Datum::Numeric(n) = d {
                if name == "sqrt" && n.sign == super::numeric::Sign::Neg && !n.is_zero() {
                    return Err(sql_err!("2201F", "cannot take square root of a negative number"));
                }
                if name == "ln" && (n.sign == super::numeric::Sign::Neg || n.is_zero()) {
                    return Err(sql_err!("2201E", "cannot take logarithm of a non-positive number"));
                }
                return Ok(Datum::Numeric(match name {
                    "sqrt" => numeric::sqrt(&n, arena)?,
                    "exp" => numeric::exp(&n, arena)?,
                    _ => numeric::ln(&n, arena)?,
                }));
            }
            let x = datum_f64(name, d)?;
            if name == "sqrt" && x < 0.0 {
                return Err(sql_err!("2201F", "cannot take square root of a negative number"));
            }
            if name == "ln" && x <= 0.0 {
                return Err(sql_err!("2201E", "cannot take logarithm of a non-positive number"));
            }
            Ok(Datum::Float8(match name {
                "sqrt" => x.sqrt(),
                "exp" => x.exp(),
                _ => x.ln(),
            }))
        }
        "log" | "log10" => {
            // log(x)/log10(x) are base-10; log(b, x) is base-b. A numeric
            // argument stays numeric (arbitrary precision); int/float go double.
            let two_arg = name == "log" && args.len() == 2;
            if !two_arg && args.len() != 1 {
                return Err(arity_err(name, args.len()));
            }
            if two_arg {
                let db = eval_full(args[0], arena, params, row, hooks)?;
                let dv = eval_full(args[1], arena, params, row, hooks)?;
                if db.is_null() || dv.is_null() {
                    return Ok(Datum::Null);
                }
                if matches!(db, Datum::Numeric(_)) || matches!(dv, Datum::Numeric(_)) {
                    let b = datum_numeric(name, db, arena)?;
                    let v = datum_numeric(name, dv, arena)?;
                    log_domain_check(&v)?;
                    log_domain_check(&b)?;
                    return Ok(Datum::Numeric(numeric::logb(&b, &v, arena)?));
                }
                let (b, v) = (datum_f64(name, db)?, datum_f64(name, dv)?);
                return Ok(Datum::Float8(v.log(b)));
            }
            let d = eval_full(args[0], arena, params, row, hooks)?;
            if d.is_null() {
                return Ok(Datum::Null);
            }
            if let Datum::Numeric(n) = d {
                log_domain_check(&n)?;
                return Ok(Datum::Numeric(numeric::log10(&n, arena)?));
            }
            Ok(Datum::Float8(datum_f64(name, d)?.log10()))
        }
        "power" | "pow" => {
            arity(2)?;
            let da = eval_full(args[0], arena, params, row, hooks)?;
            let db = eval_full(args[1], arena, params, row, hooks)?;
            if da.is_null() || db.is_null() {
                return Ok(Datum::Null);
            }
            // A numeric argument keeps the numeric domain, but a float argument
            // wins (double precision is preferred), so both go to the f64 path.
            let any_numeric = matches!(da, Datum::Numeric(_)) || matches!(db, Datum::Numeric(_));
            let any_float = matches!(da, Datum::Float8(_)) || matches!(db, Datum::Float8(_));
            if any_numeric && !any_float {
                let a = datum_numeric(name, da, arena)?;
                let b = datum_numeric(name, db, arena)?;
                return Ok(Datum::Numeric(numeric::pow(&a, &b, arena)?));
            }
            let (a, bb) = (datum_f64(name, da)?, datum_f64(name, db)?);
            // PostgreSQL rejects the cases whose real result is undefined,
            // rather than returning NaN/Inf as libm's powf would.
            if a < 0.0 && bb.fract() != 0.0 {
                return Err(sql_err!(
                    "2201F",
                    "a negative number raised to a non-integer power yields a complex result"
                ));
            }
            if a == 0.0 && bb < 0.0 {
                return Err(sql_err!("2201F", "zero raised to a negative power is undefined"));
            }
            Ok(Datum::Float8(a.powf(bb)))
        }
        "mod" => {
            arity(2)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let b = eval_full(args[1], arena, params, row, hooks)?;
            if a.is_null() || b.is_null() {
                return Ok(Datum::Null);
            }
            // A numeric operand keeps the numeric domain (matching the `%`
            // operator); mixed integer widths pick the wider integer type.
            if matches!(a, Datum::Numeric(_)) || matches!(b, Datum::Numeric(_)) {
                let x = datum_numeric(name, a, arena)?;
                let y = datum_numeric(name, b, arena)?;
                return Ok(Datum::Numeric(numeric::rem(&x, &y, arena)?));
            }
            let (x, y, wide) = match (a, b) {
                (Datum::Int4(x), Datum::Int4(y)) => (x as i64, y as i64, false),
                (Datum::Int4(x), Datum::Int8(y)) => (x as i64, y, true),
                (Datum::Int8(x), Datum::Int4(y)) => (x, y as i64, true),
                (Datum::Int8(x), Datum::Int8(y)) => (x, y, true),
                (other, _) => return Err(type_mismatch(name, &other)),
            };
            if y == 0 {
                return Err(sql_err!("22012", "division by zero"));
            }
            let r = x % y;
            Ok(if wide { Datum::Int8(r) } else { Datum::Int4(r as i32) })
        }
        "gcd" | "lcm" => {
            arity(2)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let b = eval_full(args[1], arena, params, row, hooks)?;
            if a.is_null() || b.is_null() {
                return Ok(Datum::Null);
            }
            let (x, y, wide) = match (a, b) {
                (Datum::Int4(x), Datum::Int4(y)) => (x as i64, y as i64, false),
                (Datum::Int4(x), Datum::Int8(y)) => (x as i64, y, true),
                (Datum::Int8(x), Datum::Int4(y)) => (x, y as i64, true),
                (Datum::Int8(x), Datum::Int8(y)) => (x, y, true),
                (other, _) => return Err(type_mismatch(name, &other)),
            };
            let range = || sql_err!("22003", "{} result is out of range", name);
            let (gx, gy) = (x.unsigned_abs(), y.unsigned_abs());
            let mut g = gx;
            let mut h = gy;
            while h != 0 {
                let t = g % h;
                g = h;
                h = t;
            }
            let out: i64 = if name == "gcd" {
                i64::try_from(g).map_err(|_| range())?
            } else {
                // lcm is 0 when the gcd is 0 (both inputs 0); otherwise |a/gcd*b|.
                match gx.checked_div(g) {
                    None => 0,
                    Some(q) => {
                        let l = q.checked_mul(gy).ok_or_else(range)?;
                        i64::try_from(l).map_err(|_| range())?
                    }
                }
            };
            Ok(if wide {
                Datum::Int8(out)
            } else {
                Datum::Int4(i32::try_from(out).map_err(|_| range())?)
            })
        }
        "width_bucket" => {
            // 4-arg form: which of `count` equal-width buckets over [low, high]
            // the operand falls in (0 below, count+1 at/above). Numeric args use
            // exact numeric arithmetic; a float argument uses double precision.
            arity(4)?;
            let op = eval_full(args[0], arena, params, row, hooks)?;
            let lo = eval_full(args[1], arena, params, row, hooks)?;
            let hi = eval_full(args[2], arena, params, row, hooks)?;
            let Some(cnt) = int_arg(name, args, 3, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            if op.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Datum::Null);
            }
            if cnt <= 0 {
                return Err(sql_err!("2201G", "count must be greater than zero"));
            }
            let any_float = matches!(op, Datum::Float8(_))
                || matches!(lo, Datum::Float8(_))
                || matches!(hi, Datum::Float8(_));
            if any_float {
                let (o, l, h) = (datum_f64(name, op)?, datum_f64(name, lo)?, datum_f64(name, hi)?);
                if l == h {
                    return Err(sql_err!("22004", "lower and upper bounds cannot be equal"));
                }
                let b = width_bucket_f64(o, l, h, cnt);
                return Ok(Datum::Int4(b));
            }
            let (o, l, h) = (
                datum_numeric(name, op, arena)?,
                datum_numeric(name, lo, arena)?,
                datum_numeric(name, hi, arena)?,
            );
            Ok(Datum::Int4(width_bucket_numeric(&o, &l, &h, cnt, arena)?))
        }
        "format" => {
            if args.is_empty() {
                return Err(arity_err(name, 0));
            }
            let Some(fmt) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let mut out = StackStr::<4096>::new();
            let mut argi = 1usize;
            let bytes = fmt.as_bytes();
            let mut i = 0usize;
            while i < bytes.len() {
                if bytes[i] != b'%' {
                    let _ = out.write_char(bytes[i] as char);
                    i += 1;
                    continue;
                }
                i += 1;
                let Some(&spec) = bytes.get(i) else {
                    return Err(sql_err!("22023", "unterminated format specifier"));
                };
                i += 1;
                if spec == b'%' {
                    let _ = out.write_char('%');
                    continue;
                }
                if argi >= args.len() {
                    return Err(sql_err!("22023", "too few arguments for format()"));
                }
                let v = eval_full(args[argi], arena, params, row, hooks)?;
                argi += 1;
                match spec {
                    b's' => format_append_str(&mut out, v, arena)?,
                    b'I' => format_append_ident(&mut out, v)?,
                    b'L' => format_append_literal(&mut out, v, arena)?,
                    other => {
                        return Err(sql_err!(
                            "22023",
                            "unrecognized format() type specifier \"{}\"",
                            other as char
                        ))
                    }
                }
            }
            Ok(Datum::Text(arena.alloc_str(out.as_str()).map_err(|_| arena_full())?))
        }
        "div" => {
            // Integer quotient trunc(y/x) in the numeric domain (integer args
            // are promoted to numeric, as PostgreSQL's `div(numeric,numeric)`).
            arity(2)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let b = eval_full(args[1], arena, params, row, hooks)?;
            if a.is_null() || b.is_null() {
                return Ok(Datum::Null);
            }
            let (x, y) = (datum_numeric(name, a, arena)?, datum_numeric(name, b, arena)?);
            Ok(Datum::Numeric(numeric::trunc_div(&x, &y, arena)?))
        }
        "scale" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Numeric(n) => Ok(Datum::Int4(n.dscale as i32)),
                Datum::Int4(_) | Datum::Int8(_) => Ok(Datum::Int4(0)),
                other => Err(type_mismatch(name, &other)),
            }
        }
        "min_scale" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Numeric(n) => Ok(Datum::Int4(n.min_scale() as i32)),
                Datum::Int4(_) | Datum::Int8(_) => Ok(Datum::Int4(0)),
                other => Err(type_mismatch(name, &other)),
            }
        }
        "trim_scale" => {
            arity(1)?;
            match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Numeric(n) => Ok(Datum::Numeric(n.round_scale(
                    n.min_scale() as usize,
                    super::numeric::RoundMode::Trunc,
                    arena,
                )?)),
                d @ (Datum::Int4(_) | Datum::Int8(_)) => Ok(d),
                other => Err(type_mismatch(name, &other)),
            }
        }
        "to_char" => {
            arity(2)?;
            let v = eval_full(args[0], arena, params, row, hooks)?;
            let f = eval_full(args[1], arena, params, row, hooks)?;
            if v.is_null() || f.is_null() {
                return Ok(Datum::Null);
            }
            let Datum::Text(fmt) = f else {
                return Err(type_mismatch(name, &f));
            };
            let n = datum_numeric(name, v, arena)?;
            Ok(Datum::Text(super::to_char::number(&n, fmt, arena)?))
        }
        "to_number" => {
            arity(2)?;
            let (Some(s), Some(fmt)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            Ok(Datum::Numeric(super::to_char::to_number(s, fmt, arena)?))
        }
        "to_date" | "to_timestamp" => {
            arity(2)?;
            let (Some(s), Some(fmt)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            if name == "to_date" {
                Ok(Datum::Date(super::datetime::to_date(s, fmt)?))
            } else {
                Ok(Datum::Timestamptz(super::datetime::to_timestamp(s, fmt)?))
            }
        }
        "make_date" | "make_time" | "make_timestamp" => {
            let want = if name == "make_timestamp" { 6 } else { 3 };
            arity(want)?;
            // The seconds field is a double; every other field is an integer.
            let sec_idx = if name == "make_date" { usize::MAX } else { want - 1 };
            let mut ints = [0i64; 6];
            for (i, slot) in ints[..want].iter_mut().enumerate() {
                if i == sec_idx {
                    continue;
                }
                match int_arg(name, args, i, arena, params, row, hooks)? {
                    Some(v) => *slot = v,
                    None => return Ok(Datum::Null),
                }
            }
            let sec = if sec_idx == usize::MAX {
                0.0
            } else {
                match num_f64(name, args, sec_idx, arena, params, row, hooks)? {
                    Some(v) => v,
                    None => return Ok(Datum::Null),
                }
            };
            match name {
                "make_date" => {
                    Ok(Datum::Date(super::datetime::make_date(ints[0], ints[1], ints[2])?))
                }
                "make_time" => {
                    Ok(Datum::Time(super::datetime::make_time(ints[0], ints[1], sec)?))
                }
                _ => Ok(Datum::Timestamp(super::datetime::make_timestamp(
                    ints[0], ints[1], ints[2], ints[3], ints[4], sec,
                )?)),
            }
        }
        "to_hex" => {
            arity(1)?;
            let s = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Null => return Ok(Datum::Null),
                Datum::Int4(v) => stack_format!(16, "{:x}", v as u32),
                Datum::Int8(v) => stack_format!(16, "{:x}", v as u64),
                other => return Err(type_mismatch(name, &other)),
            };
            Ok(Datum::Text(arena.alloc_str(s.as_str()).map_err(|_| arena_full())?))
        }
        "bit_length" => {
            arity(1)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            Ok(Datum::Int4((s.len() as i64 * 8) as i32))
        }
        "md5" => {
            arity(1)?;
            let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let d = super::md5::digest(s.as_bytes());
            let mut hexbuf = [0u8; 32];
            super::md5::hex(&d, &mut hexbuf);
            let out = arena.alloc_slice_with(32, |i| hexbuf[i]).map_err(|_| arena_full())?;
            Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
        }
        "starts_with" => {
            arity(2)?;
            let (Some(s), Some(p)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            Ok(Datum::Bool(s.starts_with(p)))
        }
        "cbrt" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "sinh" | "cosh"
        | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians" => {
            arity(1)?;
            let Some(x) = num_f64(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            Ok(Datum::Float8(match name {
                "cbrt" => x.cbrt(),
                "sin" => x.sin(),
                "cos" => x.cos(),
                "tan" => x.tan(),
                "cot" => 1.0 / x.tan(),
                "asin" => x.asin(),
                "acos" => x.acos(),
                "atan" => x.atan(),
                "sinh" => x.sinh(),
                "cosh" => x.cosh(),
                "tanh" => x.tanh(),
                "asinh" => x.asinh(),
                "acosh" => x.acosh(),
                "atanh" => x.atanh(),
                "degrees" => x.to_degrees(),
                _ => x.to_radians(),
            }))
        }
        "atan2" => {
            arity(2)?;
            let (Some(a), Some(bb)) = (
                num_f64(name, args, 0, arena, params, row, hooks)?,
                num_f64(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            Ok(Datum::Float8(a.atan2(bb)))
        }
        "pi" => {
            arity(0)?;
            Ok(Datum::Float8(core::f64::consts::PI))
        }
        "factorial" => {
            arity(1)?;
            let Some(n) = int_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            if n < 0 {
                return Err(sql_err!("22003", "factorial of a negative number is undefined"));
            }
            // n! as an exact numeric; a too-large product exhausts the arena and
            // errors loudly, matching PostgreSQL's numeric overflow.
            let mut acc = Numeric::from_i64(1, arena)?;
            let mut k = 2i64;
            while k <= n {
                acc = super::numeric::mul(&acc, &Numeric::from_i64(k, arena)?, arena)?;
                k += 1;
            }
            Ok(Datum::Numeric(acc))
        }
        "extract" | "date_part" => {
            arity(2)?;
            let Some(field) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let (days, in_day) = match eval_full(args[1], arena, params, row, hooks)? {
                Datum::Null => return Ok(Datum::Null),
                Datum::Date(d) => (d as i64, 0i64),
                Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                    (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000))
                }
                other => return Err(type_mismatch(name, &other)),
            };
            use super::datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS, PG_EPOCH_SECS};
            let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
            let (secs, frac) = (in_day / 1_000_000, in_day % 1_000_000);
            let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
            let eq = |k: &str| field.eq_ignore_ascii_case(k);
            let dow0 = day_of_week(days) as i64;
            // Integer-valued fields.
            let int_val: Option<i64> = if eq("year") || eq("years") {
                Some(y)
            } else if eq("month") || eq("months") {
                Some(m as i64)
            } else if eq("day") || eq("days") {
                Some(d as i64)
            } else if eq("hour") || eq("hours") {
                Some(h)
            } else if eq("minute") || eq("minutes") {
                Some(mi)
            } else if eq("dow") {
                Some(dow0)
            } else if eq("isodow") {
                Some(if dow0 == 0 { 7 } else { dow0 })
            } else if eq("doy") {
                Some(days_from_civil(y, m, d) - days_from_civil(y, 1, 1) + 1)
            } else if eq("quarter") {
                Some((m as i64 - 1) / 3 + 1)
            } else if eq("decade") {
                Some(y.div_euclid(10))
            } else if eq("century") {
                Some(if y > 0 { (y - 1) / 100 + 1 } else { y / 100 - 1 })
            } else if eq("millennium") {
                Some(if y > 0 { (y - 1) / 1000 + 1 } else { y / 1000 - 1 })
            } else if eq("microseconds") {
                Some(s * 1_000_000 + frac)
            } else if eq("week") {
                // ISO week: the week that contains this row's Thursday.
                let isodow = if dow0 == 0 { 7 } else { dow0 };
                let thursday = days + (4 - isodow);
                let (ty, tm, td) = civil_from_days(thursday + PG_EPOCH_DAYS);
                Some((days_from_civil(ty, tm, td) - days_from_civil(ty, 1, 1)) / 7 + 1)
            } else {
                None
            };
            if let Some(iv) = int_val {
                return Ok(if name == "extract" {
                    Datum::Numeric(Numeric::from_i64(iv, arena)?)
                } else {
                    Datum::Float8(iv as f64)
                });
            }
            // Fractional fields, scaled to microseconds.
            let micros_val: i64 = if eq("second") || eq("seconds") {
                s * 1_000_000 + frac
            } else if eq("epoch") {
                (days * 86_400_000_000 + in_day) + PG_EPOCH_SECS * 1_000_000
            } else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "unit \"{}\" not recognized for {}()",
                    field,
                    name
                ));
            };
            if name == "extract" {
                let neg = micros_val < 0;
                let a = micros_val.unsigned_abs();
                let text = stack_format!(
                    40,
                    "{}{}.{:06}",
                    if neg { "-" } else { "" },
                    a / 1_000_000,
                    a % 1_000_000
                );
                Ok(Datum::Numeric(Numeric::parse(text.as_str(), arena)?))
            } else {
                Ok(Datum::Float8(micros_val as f64 / 1_000_000.0))
            }
        }
        "date_trunc" => {
            arity(2)?;
            let Some(field) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            let (is_tz, t) = match eval_full(args[1], arena, params, row, hooks)? {
                Datum::Null => return Ok(Datum::Null),
                Datum::Timestamp(t) => (false, t),
                Datum::Timestamptz(t) => (true, t),
                Datum::Date(d) => (false, d as i64 * 86_400_000_000),
                other => return Err(type_mismatch(name, &other)),
            };
            use super::datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS};
            let (days, in_day) = (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000));
            let (y, m, _d) = civil_from_days(days + PG_EPOCH_DAYS);
            let (secs, _frac) = (in_day / 1_000_000, in_day % 1_000_000);
            let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
            let eq = |k: &str| field.eq_ignore_ascii_case(k);
            // (new day count since epoch, seconds within the day).
            let (new_days, sod): (i64, i64) = if eq("year") || eq("years") {
                (days_from_civil(y, 1, 1) - PG_EPOCH_DAYS, 0)
            } else if eq("quarter") {
                (days_from_civil(y, ((m - 1) / 3) * 3 + 1, 1) - PG_EPOCH_DAYS, 0)
            } else if eq("month") || eq("months") {
                (days_from_civil(y, m, 1) - PG_EPOCH_DAYS, 0)
            } else if eq("week") {
                let dow0 = day_of_week(days) as i64;
                let isodow = if dow0 == 0 { 7 } else { dow0 };
                (days - (isodow - 1), 0)
            } else if eq("day") || eq("days") {
                (days, 0)
            } else if eq("hour") || eq("hours") {
                (days, h * 3600)
            } else if eq("minute") || eq("minutes") {
                (days, h * 3600 + mi * 60)
            } else if eq("second") || eq("seconds") {
                (days, h * 3600 + mi * 60 + s)
            } else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "unit \"{}\" not recognized for date_trunc()",
                    field
                ));
            };
            let micros = new_days * 86_400_000_000 + sod * 1_000_000;
            Ok(if is_tz {
                Datum::Timestamptz(micros)
            } else {
                Datum::Timestamp(micros)
            })
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
fn width_bucket_f64(op: f64, lo: f64, hi: f64, count: i64) -> i32 {
    let c = count as f64;
    let bucket = if lo < hi {
        if op < lo {
            0
        } else if op >= hi {
            count + 1
        } else {
            ((op - lo) / (hi - lo) * c).floor() as i64 + 1
        }
    } else if op > lo {
        0
    } else if op <= hi {
        count + 1
    } else {
        ((lo - op) / (lo - hi) * c).floor() as i64 + 1
    };
    bucket as i32
}

/// `width_bucket` with exact numeric arithmetic (matching PostgreSQL's numeric
/// form), using an integer quotient so bucket boundaries land exactly.
fn width_bucket_numeric(
    op: &Numeric,
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
        (compare(op, lo) == Ordering::Less, compare(op, hi) != Ordering::Less)
    } else {
        (compare(op, lo) == Ordering::Greater, compare(op, hi) != Ordering::Greater)
    };
    if below {
        return Ok(0);
    }
    if at_or_above {
        return Ok((count + 1) as i32);
    }
    // floor((|op-lo| * count) / |hi-lo|) + 1
    let (num_a, den) = if ascending {
        (sub(op, lo, arena)?, sub(hi, lo, arena)?)
    } else {
        (sub(lo, op, arena)?, sub(lo, hi, arena)?)
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
    whole: &str,
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
                let _ = out.write_str(whole);
                i += 2;
            }
            Some(b'\\') => {
                let _ = out.write_char('\\');
                i += 2;
            }
            Some(d) if d.is_ascii_digit() => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "capture-group backreferences in regexp_replace are not supported"
                ));
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
fn datum_to_text<'a>(v: Datum<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    match v {
        Datum::Text(s) => Ok(s),
        other => {
            let s = stack_format!(64, "{}", other);
            arena.alloc_str(s.as_str()).map_err(|_| arena_full())
        }
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
        Expr::Unary { op: UnaryOp::Neg, operand } => static_type(operand, row),
        Expr::Unary { op: UnaryOp::Not, .. } | Expr::IsNull { .. }
        | Expr::InList { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::Match { .. } => Some(ColType::Bool),
        Expr::Binary { op, left, right } => match op {
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

fn unary<'a>(op: UnaryOp, v: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    match (op, v) {
        (_, Datum::Null) => Ok(Datum::Null),
        (UnaryOp::Neg, Datum::Int4(x)) => x
            .checked_neg()
            .map(Datum::Int4)
            .ok_or_else(|| overflow("integer")),
        (UnaryOp::Neg, Datum::Int8(x)) => x
            .checked_neg()
            .map(Datum::Int8)
            .ok_or_else(|| overflow("bigint")),
        (UnaryOp::Neg, Datum::Float8(x)) => Ok(Datum::Float8(-x)),
        (UnaryOp::Neg, Datum::Numeric(n)) => Ok(Datum::Numeric(Numeric {
            // Negating zero stays positive (no negative zero).
            sign: if n.is_zero() {
                super::numeric::Sign::Pos
            } else {
                match n.sign {
                    super::numeric::Sign::Pos => super::numeric::Sign::Neg,
                    super::numeric::Sign::Neg => super::numeric::Sign::Pos,
                    super::numeric::Sign::NaN => super::numeric::Sign::NaN,
                }
            },
            ..n
        })),
        (UnaryOp::Not, Datum::Bool(b)) => Ok(Datum::Bool(!b)),
        (UnaryOp::BitNot, Datum::Int4(x)) => Ok(Datum::Int4(!x)),
        (UnaryOp::BitNot, Datum::Int8(x)) => Ok(Datum::Int8(!x)),
        (UnaryOp::Neg, other) => Err(type_mismatch("-", &other)),
        (UnaryOp::Not, other) => Err(type_mismatch("NOT", &other)),
        (UnaryOp::BitNot, other) => Err(type_mismatch("~", &other)),
    }
}

/// A string literal or a parameter is PostgreSQL's "unknown" type, which
/// coerces to whatever it is compared/combined with. A real typed value
/// (column, function result, cast) does not.
fn is_unknown_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Str(_) | Expr::Param(_))
}

#[allow(clippy::too_many_arguments)]
fn binary<'a>(
    op: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    match op {
        And | Or => logic(op, l, r),
        Concat => concat(l, r, arena),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => compare(op, l, r, l_unknown, r_unknown),
        Add | Sub | Mul | Div | Mod => arithmetic(op, l, r, l_unknown, r_unknown, arena),
        JsonGet | JsonGetText => json_get(l, r, op == JsonGetText, arena),
        BitAnd | BitOr | BitXor | Shl | Shr => bitwise(op, l, r),
        Pow => {
            // PostgreSQL `^` stays numeric when an operand is numeric (and none
            // is float8); otherwise it is double-precision exponentiation.
            if l.is_null() || r.is_null() {
                return Ok(Datum::Null);
            }
            let any_numeric = matches!(l, Datum::Numeric(_)) || matches!(r, Datum::Numeric(_));
            let any_float = matches!(l, Datum::Float8(_)) || matches!(r, Datum::Float8(_));
            if any_numeric && !any_float {
                let a = datum_numeric("^", l, arena)?;
                let b = datum_numeric("^", r, arena)?;
                return Ok(Datum::Numeric(numeric::pow(&a, &b, arena)?));
            }
            let (a, b) = (datum_f64("^", l)?, datum_f64("^", r)?);
            Ok(Datum::Float8(a.powf(b)))
        }
    }
}

/// Integer bitwise operators (`& | # << >>`). Both operands must be integers.
fn bitwise<'a>(op: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    let int = |d: &Datum| -> Result<i64, SqlError> {
        match d {
            Datum::Int4(v) => Ok(i64::from(*v)),
            Datum::Int8(v) => Ok(*v),
            other => Err(type_mismatch("bitwise operator requires integers", other)),
        }
    };
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (int(&l)?, int(&r)?);
    let v = match op {
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        Shl => a << (b & 63),
        Shr => a >> (b & 63),
        _ => unreachable!("bitwise only"),
    };
    // Result width follows the wider operand (int8 if either is int8).
    if matches!(l, Datum::Int8(_)) || matches!(r, Datum::Int8(_)) {
        Ok(Datum::Int8(v))
    } else {
        Ok(Datum::Int4(v as i32))
    }
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
            return Ok(Datum::Text(s));
        }
        let mut buf = crate::util::StackStr::<8192>::new();
        let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("{}", super::json::JsonWrite(&child)));
        return Ok(Datum::Text(arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?));
    }
    let mut buf = crate::util::StackStr::<8192>::new();
    let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("{}", super::json::JsonWrite(&child)));
    Ok(Datum::Json { text: arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?, jsonb })
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
    op: BinaryOp,
    left: &Expr<'a>,
    right: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let absorbing = matches!(op, BinaryOp::Or);
    // Left first: a statically-determined left settles the result (absorbing) or
    // hands off to the right (non-absorbing), matching plan-time folding order.
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
    logic(op, l, r)
}

/// SQL three-valued AND/OR.
fn logic<'a>(op: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    let as_bool = |d: &Datum| -> Result<Option<bool>, SqlError> {
        match d {
            Datum::Null => Ok(None),
            Datum::Bool(b) => Ok(Some(*b)),
            other => Err(type_mismatch("boolean operator", other)),
        }
    };
    let (a, b) = (as_bool(&l)?, as_bool(&r)?);
    let out = match (op, a, b) {
        (BinaryOp::And, Some(false), _) | (BinaryOp::And, _, Some(false)) => Some(false),
        (BinaryOp::And, Some(true), Some(true)) => Some(true),
        (BinaryOp::And, _, _) => None,
        (BinaryOp::Or, Some(true), _) | (BinaryOp::Or, _, Some(true)) => Some(true),
        (BinaryOp::Or, Some(false), Some(false)) => Some(false),
        (BinaryOp::Or, _, _) => None,
        _ => unreachable!(),
    };
    Ok(out.map_or(Datum::Null, Datum::Bool))
}

fn concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    // `||` on arrays concatenates: array||array, and array||element or
    // element||array append/prepend the element.
    if matches!(l, Datum::Array { .. }) || matches!(r, Datum::Array { .. }) {
        return array_concat(l, r, arena);
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
    let elem = match (&l, &r) {
        (Datum::Array { elem, .. }, _) | (_, Datum::Array { elem, .. }) => *elem,
        _ => unreachable!("caller ensures one side is an array"),
    };
    let mut items = [Datum::Null; 4096];
    let mut n = 0usize;
    for side in [l, r] {
        match side {
            Datum::Array { raw, elem: e } => {
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
    Ok(Datum::Array { elem, raw: super::array::build(&items[..n], arena)? })
}

/// Total order used by comparisons and ORDER BY. NULL handling differs
/// between the two, so NULL never reaches here.
/// Exact comparison between a Numeric and an integer, allocation-free.
fn compare_numeric_int(l: &Datum, r: &Datum) -> Result<core::cmp::Ordering, SqlError> {
    let mut buf = [0u8; 20];
    match (l, r) {
        (Datum::Numeric(n), other) => {
            let iv = as_i64(other).expect("integer side");
            let t = Numeric::from_i64_stack(iv, &mut buf);
            Ok(numeric::compare(n, &t))
        }
        (other, Datum::Numeric(n)) => {
            let iv = as_i64(other).expect("integer side");
            let t = Numeric::from_i64_stack(iv, &mut buf);
            Ok(numeric::compare(&t, n))
        }
        _ => unreachable!("compare_numeric_int only for numeric/int pairs"),
    }
}

pub fn compare_datums(l: &Datum, r: &Datum) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    let ord = match (l, r) {
        (Datum::Bool(a), Datum::Bool(b)) => a.cmp(b),
        (Datum::Text(a), Datum::Text(b)) => a.cmp(b),
        (Datum::Date(a), Datum::Date(b)) => a.cmp(b),
        (Datum::Timestamp(a), Datum::Timestamp(b))
        | (Datum::Timestamptz(a), Datum::Timestamptz(b))
        | (Datum::Timestamp(a), Datum::Timestamptz(b))
        | (Datum::Timestamptz(a), Datum::Timestamp(b)) => a.cmp(b),
        (Datum::Time(a), Datum::Time(b)) => a.cmp(b),
        (Datum::Json { text: a, .. }, Datum::Json { text: b, .. }) => a.cmp(b),
        (Datum::Array { elem, raw: ra }, Datum::Array { raw: rb, .. }) => {
            // Element-wise, then by length (PostgreSQL array ordering).
            let (na, nb) = (super::array::len(ra), super::array::len(rb));
            for i in 0..na.min(nb) {
                let x = super::array::get(ra, *elem, i).unwrap_or(Datum::Null);
                let y = super::array::get(rb, *elem, i).unwrap_or(Datum::Null);
                let c = compare_datums(&x, &y)?;
                if !c.is_eq() {
                    return Ok(c);
                }
            }
            na.cmp(&nb)
        }
        (Datum::Date(a), Datum::Timestamp(b) | Datum::Timestamptz(b)) => {
            (i64::from(*a) * 86_400_000_000).cmp(b)
        }
        (Datum::Timestamp(a) | Datum::Timestamptz(a), Datum::Date(b)) => {
            a.cmp(&(i64::from(*b) * 86_400_000_000))
        }
        (Datum::Uuid(a), Datum::Uuid(b)) => a.cmp(b),
        (Datum::Bytea(a), Datum::Bytea(b)) => a.cmp(b),
        (Datum::Numeric(a), Datum::Numeric(b)) => numeric::compare(a, b),
        // Numeric vs integer: compare exactly via numeric.
        (Datum::Numeric(_), Datum::Int4(_) | Datum::Int8(_))
        | (Datum::Int4(_) | Datum::Int8(_), Datum::Numeric(_)) => {
            // Fall through to the float comparison below only if exactness is
            // not required; integers convert to numeric exactly.
            return compare_numeric_int(l, r);
        }
        _ => {
            if let (Some(a), Some(b)) = (as_i64(l), as_i64(r)) {
                a.cmp(&b)
            } else if let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) {
                // PostgreSQL float comparison treats NaN as largest.
                return Ok(a.partial_cmp(&b).unwrap_or_else(|| {
                    match (a.is_nan(), b.is_nan()) {
                        (true, false) => Ordering::Greater,
                        (false, true) => Ordering::Less,
                        _ => Ordering::Equal,
                    }
                }));
            } else {
                // PostgreSQL reports incompatible comparisons as
                // "operator does not exist" (42883), not a datatype mismatch.
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} = {}",
                    type_name_of(l),
                    type_name_of(r)
                ));
            }
        }
    };
    Ok(ord)
}

/// PostgreSQL's unknown-literal rule, approximated: a text value meeting a
/// typed value in a comparison or arithmetic context converts to the typed
/// side (text parameters and quoted literals are "unknown", not text).
fn coerce_unknown<'a>(v: Datum<'a>, other: &Datum) -> Result<Datum<'a>, SqlError> {
    let Datum::Text(s) = v else {
        return Ok(v);
    };
    Ok(match other {
        Datum::Int4(_) => Datum::Int4(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "integer"))?,
        ),
        Datum::Int8(_) => Datum::Int8(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "bigint"))?,
        ),
        Datum::Float8(_) => Datum::Float8(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "double precision"))?,
        ),
        Datum::Bool(_) => Datum::Bool(parse_bool(s)?),
        Datum::Date(_) => Datum::Date(super::datetime::parse_date(s)?),
        Datum::Timestamp(_) => Datum::Timestamp(super::datetime::parse_timestamp(s, false)?),
        Datum::Timestamptz(_) => {
            Datum::Timestamptz(super::datetime::parse_timestamp(s, true)?)
        }
        Datum::Uuid(_) => Datum::Uuid(parse_uuid(s)?),
        _ => v,
    })
}

fn compare<'a>(
    op: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let l = if l_unknown { coerce_unknown(l, &r)? } else { l };
    let r = if r_unknown { coerce_unknown(r, &l)? } else { r };
    let ord = compare_datums(&l, &r)?;
    let out = match op {
        BinaryOp::Eq => ord.is_eq(),
        BinaryOp::NotEq => ord.is_ne(),
        BinaryOp::Lt => ord.is_lt(),
        BinaryOp::LtEq => ord.is_le(),
        BinaryOp::Gt => ord.is_gt(),
        BinaryOp::GtEq => ord.is_ge(),
        _ => unreachable!(),
    };
    Ok(Datum::Bool(out))
}

#[allow(clippy::too_many_arguments)]
fn arithmetic<'a>(
    op: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let l = if l_unknown { coerce_unknown(l, &r)? } else { l };
    let r = if r_unknown { coerce_unknown(r, &l)? } else { r };
    // Date arithmetic (PostgreSQL): `date + int` / `date - int` -> date;
    // `date - date` -> int (days). Handled before the generic integer path,
    // which would otherwise coerce a date to a bare day count.
    // Interval arithmetic: date/timestamp ± interval -> timestamp; interval
    // ± interval -> interval. Months add calendar months (day clamped).
    match (op, l, r) {
        (BinaryOp::Add | BinaryOp::Sub, Datum::Interval(a), Datum::Interval(b)) => {
            let s: i32 = if op == BinaryOp::Sub { -1 } else { 1 };
            return Ok(Datum::Interval(super::types::Interval {
                months: a.months + s * b.months,
                days: a.days + s * b.days,
                micros: a.micros + s as i64 * b.micros,
            }));
        }
        (BinaryOp::Add | BinaryOp::Sub, dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_)), Datum::Interval(iv))
        | (BinaryOp::Add, Datum::Interval(iv), dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_))) => {
            let base = match dt {
                Datum::Timestamp(t) | Datum::Timestamptz(t) => t,
                Datum::Date(d) => d as i64 * 86_400_000_000,
                _ => unreachable!(),
            };
            let signed = if op == BinaryOp::Sub {
                super::types::Interval { months: -iv.months, days: -iv.days, micros: -iv.micros }
            } else {
                iv
            };
            let out = super::datetime::add_interval(base, signed);
            // date ± interval yields timestamp in PostgreSQL; timestamptz stays tz.
            return Ok(match dt {
                Datum::Timestamptz(_) => Datum::Timestamptz(out),
                _ => Datum::Timestamp(out),
            });
        }
        _ => {}
    }
    match (op, l, r) {
        (BinaryOp::Sub, Datum::Date(a), Datum::Date(b)) => {
            return Ok(Datum::Int4(a - b));
        }
        // timestamp - timestamp -> interval (days + time, no month folding).
        (BinaryOp::Sub, Datum::Timestamp(a), Datum::Timestamp(b))
        | (BinaryOp::Sub, Datum::Timestamptz(a), Datum::Timestamptz(b)) => {
            let diff = a - b;
            return Ok(Datum::Interval(super::types::Interval {
                months: 0,
                days: (diff / 86_400_000_000) as i32,
                micros: diff % 86_400_000_000,
            }));
        }
        (BinaryOp::Add | BinaryOp::Sub, Datum::Date(a), _) if as_i64(&r).is_some() => {
            let days = as_i64(&r).expect("checked");
            return date_shift(a, days, op == BinaryOp::Sub);
        }
        // `int + date` is commutative with `date + int`; `int - date` is not
        // defined in PostgreSQL, so only Add is accepted here.
        (BinaryOp::Add, _, Datum::Date(b)) if as_i64(&l).is_some() => {
            let days = as_i64(&l).expect("checked");
            return date_shift(b, days, false);
        }
        _ => {}
    }
    // PostgreSQL numeric-promotion: int op int -> int; if either side is
    // numeric (and neither is float8) -> numeric; if either is float8 ->
    // float8.
    let either_numeric = matches!(l, Datum::Numeric(_)) || matches!(r, Datum::Numeric(_));
    let either_float = matches!(l, Datum::Float8(_)) || matches!(r, Datum::Float8(_));
    // Integer op integer stays integral.
    if let (Some(a), Some(b)) = (as_i64(&l), as_i64(&r)) {
        let out = match op {
            BinaryOp::Add => a.checked_add(b),
            BinaryOp::Sub => a.checked_sub(b),
            BinaryOp::Mul => a.checked_mul(b),
            BinaryOp::Div => {
                if b == 0 {
                    return Err(division_by_zero());
                }
                a.checked_div(b)
            }
            BinaryOp::Mod => {
                if b == 0 {
                    return Err(division_by_zero());
                }
                a.checked_rem(b)
            }
            _ => unreachable!(),
        };
        let v = out.ok_or_else(|| overflow("bigint"))?;
        return narrow_int(v, &l, &r);
    }
    if either_numeric && !either_float {
        let a = to_numeric(&l, arena)?;
        let b = to_numeric(&r, arena)?;
        let out = match op {
            BinaryOp::Add => numeric::add(&a, &b, arena)?,
            BinaryOp::Sub => numeric::sub(&a, &b, arena)?,
            BinaryOp::Mul => numeric::mul(&a, &b, arena)?,
            BinaryOp::Div => numeric::div(&a, &b, arena)?,
            BinaryOp::Mod => numeric::rem(&a, &b, arena)?,
            _ => unreachable!(),
        };
        return Ok(Datum::Numeric(out));
    }
    // PostgreSQL defines no modulo operator for double precision, so `%` with
    // a float8 operand is undefined even though `+`/`-`/`*`/`/` are not.
    if op == BinaryOp::Mod && either_float {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {} % {}",
            type_name_of(&l),
            type_name_of(&r)
        ));
    }
    if let (Some(a), Some(b)) = (as_f64(&l), as_f64(&r)) {
        let out = match op {
            BinaryOp::Add => a + b,
            BinaryOp::Sub => a - b,
            BinaryOp::Mul => a * b,
            BinaryOp::Div => {
                if b == 0.0 {
                    return Err(division_by_zero());
                }
                a / b
            }
            BinaryOp::Mod => {
                if b == 0.0 {
                    return Err(division_by_zero());
                }
                a % b
            }
            _ => unreachable!(),
        };
        return Ok(Datum::Float8(out));
    }
    // No arithmetic operator is defined for this operand pair (e.g. int - date,
    // text + int). PostgreSQL reports this as "operator does not exist" (42883).
    let sym = match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        _ => "?",
    };
    Err(sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {} {}",
        type_name_of(&l),
        sym,
        type_name_of(&r)
    ))
}

/// Shift a date (days since the PostgreSQL epoch) by `days`, subtracting when
/// `sub` is set. Out-of-range results error like PostgreSQL (22008).
fn date_shift<'a>(date: i32, days: i64, sub: bool) -> Result<Datum<'a>, SqlError> {
    let delta = if sub { -days } else { days };
    let shifted = i64::from(date)
        .checked_add(delta)
        .and_then(|v| i32::try_from(v).ok());
    match shifted {
        Some(d) => Ok(Datum::Date(d)),
        None => Err(sql_err!("22008", "date out of range")),
    }
}

/// int4 op int4 yields int4 (with range check), as in PostgreSQL.
fn narrow_int<'a>(v: i64, l: &Datum, r: &Datum) -> Result<Datum<'a>, SqlError> {
    let both_int4 = matches!(l, Datum::Int4(_)) && matches!(r, Datum::Int4(_));
    if both_int4 {
        return match i32::try_from(v) {
            Ok(small) => Ok(Datum::Int4(small)),
            Err(_) => Err(overflow("integer")),
        };
    }
    Ok(Datum::Int8(v))
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
            let x = to_i64_for_cast(&v, "integer")?;
            Datum::Int4(i32::try_from(x).map_err(|_| overflow("integer"))?)
        }
        ColType::Int8 => Datum::Int8(to_i64_for_cast(&v, "bigint")?),
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
                let mut buf = crate::util::StackStr::<8192>::new();
                let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("{}", super::json::JsonWrite(&tree)));
                if buf.is_truncated() {
                    return Err(sql_err!("54000", "jsonb value exceeds the supported size"));
                }
                Datum::Json { text: arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?, jsonb: true }
            }
            _ => return Err(cast_unsupported(&v, "jsonb")),
        },
        ColType::Array(elem) => match v {
            Datum::Array { elem: e, .. } if e == elem => v,
            Datum::Text(s) => Datum::Array { elem, raw: super::array::parse_literal(s, elem, arena)? },
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
    };
    Ok(out)
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
    let bad = || {
        sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type bytea (use \\x hex)"
        )
    };
    let t = s.trim();
    let hex = t.strip_prefix("\\x").ok_or_else(bad)?;
    if hex.len() % 2 != 0 {
        return Err(bad());
    }
    let out = arena
        .alloc_slice_with(hex.len() / 2, |_| 0u8)
        .map_err(|_| arena_full())?;
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(&*out)
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
        other => {
            let s = stack_format!(40, "{}", other);
            arena.alloc_str(s.as_str()).map_err(|_| arena_full())
        }
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
fn parse_int_literal(s: &str) -> Option<i64> {
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
    let mut buf = [0u8; 80];
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
        if n >= buf.len() {
            return None;
        }
        buf[n] = c;
        n += 1;
    }
    let cleaned = core::str::from_utf8(&buf[..n]).ok()?;
    let v = i64::from_str_radix(cleaned, radix).ok()?;
    Some(if neg { -v } else { v })
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

fn type_mismatch(op: &str, d: &Datum) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "operator {} does not accept {:?}",
        op,
        d
    )
}

fn cast_unsupported(from: &Datum, to: &'static str) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "cannot cast {:?} to {}",
        from,
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

fn arena_full() -> SqlError {
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
        let SelectItem::Expr { expr, .. } = s.items[0] else { panic!() };
        eval(expr, arena, NO_PARAMS, &NoColumns)
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
