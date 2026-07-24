//! Qualification planning: what a WHERE clause is rewritten to, in what order
//! its conjuncts run, and which of them can be pushed down.
//!
//! PostgreSQL's observable behaviour here is not only which rows come back but
//! *which errors do*: a qualification's conjuncts run in its cost order, so a
//! cheap filtering conjunct can exclude a row before a costlier one on the same
//! row would have errored. The canonicalization (`A AND FALSE` folded away,
//! common terms factored out of an OR) and the cost model that orders the
//! conjuncts both exist to reproduce that.

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::{eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError};
use crate::sql::types::Datum;
use crate::sql_err;

use super::{arena_full, QueryScope, ResolvedColumn, ScopeCols, MAX_JOIN_TABLES};

pub(super) const MAX_CONJUNCTS: usize = 32;

/// The set of table indices (as a bitmask) an expression references. `None` if
/// it contains a construct not analyzable for pushdown (subquery, aggregate, …).
pub(super) fn expr_tables(expression: &Expr, scope: &QueryScope) -> Option<u16> {
    use Expr::*;
    match expression {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | Param(_) => Some(0),
        Column { qualifier, name } => match scope.find_column(*qualifier, name).ok()? {
            ResolvedColumn::Table(t, _) => Some(1 << t),
            // Merged USING/NATURAL column: reads every contributing table.
            ResolvedColumn::Merged(m) => {
                let mc = &scope.merged[m];
                Some(mc.parts[..mc.n_parts].iter().fold(0u16, |mask, &(t, _)| mask | (1 << t)))
            }
        },
        Unary { operand, .. } | IsNull { operand, .. } | Cast { operand, .. } => {
            expr_tables(operand, scope)
        }
        Binary { left, right, .. } => Some(expr_tables(left, scope)? | expr_tables(right, scope)?),
        Between { operand, low, high, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(low, scope)? | expr_tables(high, scope)?)
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(pattern, scope)?)
        }
        InList { operand, list, .. } => {
            let mut m = expr_tables(operand, scope)?;
            for e in *list {
                m |= expr_tables(e, scope)?;
            }
            Some(m)
        }
        Call { args, over: None, .. } if !expression.is_aggregate() => {
            let mut m = 0;
            for a in *args {
                m |= expr_tables(a, scope)?;
            }
            Some(m)
        }
        _ => None,
    }
}

/// A cost-based execution order for a cross-join's tables (an identity order is
/// returned when reordering does not apply). PostgreSQL reorders joins by
/// selectivity; pos3ql's nested loop otherwise follows FROM order, so a table
/// with no predicate binding it to the already-joined tables (e.g. an
/// unconstrained table in the middle of the FROM list) multiplies the
/// intermediate product and turns a k-way join O(N^k). The greedy heuristic
/// picks, at each step, the remaining table that "unlocks" the most WHERE
/// conjuncts (its columns, together with the already-chosen tables, fully bind a
/// conjunct so pushdown can prune there), breaking ties by FROM order. This
/// keeps selective and equi-joined tables early and pushes unconstrained tables
/// last, without changing results (join order is free for inner/cross joins).
pub(super) fn join_order(scope: &QueryScope, where_clause: Option<&Expr>) -> [usize; MAX_JOIN_TABLES] {
    let mut order = core::array::from_fn(|i| i);
    let n = scope.n;
    if n < 3 {
        return order;
    }
    // Collect the WHERE conjuncts' table masks (only analyzable ones).
    let mut masks = [0u16; MAX_CONJUNCTS];
    let mut n_masks = 0;
    if let Some(w) = where_clause {
        let mut conjunct: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut nc = 0;
        let conjuncts: &[&Expr] =
            if flatten_and(w, &mut conjunct, &mut nc) { &conjunct[..nc] } else { core::slice::from_ref(&w) };
        for &c in conjuncts {
            if let Some(m) = expr_tables(c, scope)
                && n_masks < MAX_CONJUNCTS
            {
                masks[n_masks] = m;
                n_masks += 1;
            }
        }
    }
    let mut chosen_mask = 0u16;
    for slot in order.iter_mut().take(n) {
        // Among not-yet-chosen tables, pick the one unlocking the most conjuncts
        // (a conjunct is unlocked when the table is its last unbound one).
        let mut best = usize::MAX;
        let mut best_score = -1i32;
        for t in 0..n {
            if chosen_mask & (1 << t) != 0 {
                continue;
            }
            let after = chosen_mask | (1 << t);
            let mut score = 0i32;
            for &m in &masks[..n_masks] {
                if m & !chosen_mask == (1 << t) {
                    score += 1;
                }
                // Slight preference for being connected at all (bounds growth).
                if m & !after == 0 && m & (1 << t) != 0 {
                    score += 1;
                }
            }
            if score > best_score {
                best_score = score;
                best = t;
            }
        }
        *slot = best;
        chosen_mask |= 1 << best;
    }
    order
}

/// Evaluates one WHERE conjunct to a filter decision (NULL and FALSE both
/// exclude the row; a non-boolean is a type error).
pub(super) fn conjunct_passes<'a>(
    e: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    match eval_full(e, arena, params, row, hooks)? {
        Datum::Bool(true) => Ok(true),
        Datum::Bool(false) | Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean"
        )),
    }
}

/// Flattens a top-level `AND` chain into `out`, returning the count, or `None`
/// if it would overflow (caller then evaluates the predicate whole).
pub(super) fn flatten_and<'e, 'a>(e: &'e Expr<'a>, out: &mut [&'e Expr<'a>], n: &mut usize) -> bool {
    if let Expr::Binary { operator: crate::sql::ast::BinaryOp::And, left, right } = e {
        return flatten_and(left, out, n) && flatten_and(right, out, n);
    }
    if *n == out.len() {
        return false;
    }
    out[*n] = e;
    *n += 1;
    true
}

/// Whether an expression cannot raise a runtime error (so evaluating it early
/// as a WHERE filter is always safe). Conservative: any arithmetic (which can
/// divide by zero or overflow), cast, function call, CASE, or subquery counts
/// as potentially-erroring.
pub(super) fn is_error_safe(e: &Expr) -> bool {
    use crate::sql::ast::{BinaryOp::*, UnaryOp};
    // A constant subexpression cannot raise a *runtime* error: PostgreSQL folds
    // it at plan time and `check_constant_errors` surfaces any error eagerly
    // there, so by the time a row is filtered it is known good. This lets a
    // constant-false conjunct (e.g. `-2.25 <> -2.25`, whose unary minus would
    // otherwise mark it unsafe) filter the row before an erroring sibling runs.
    if e.is_constant() {
        return true;
    }
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
        | Expr::Str(_) | Expr::Column { .. } | Expr::Param(_) | Expr::DefaultMarker => true,
        Expr::Binary { operator, left, right } => match operator {
            Add | Sub | Mul | Div | Mod => false,
            _ => is_error_safe(left) && is_error_safe(right),
        },
        Expr::Unary { operator, operand } => matches!(operator, UnaryOp::Not) && is_error_safe(operand),
        Expr::IsNull { operand, .. } => is_error_safe(operand),
        Expr::InList { operand, list, .. } => {
            is_error_safe(operand) && list.iter().all(|e| is_error_safe(e))
        }
        Expr::Between { operand, low, high, .. } => {
            is_error_safe(operand) && is_error_safe(low) && is_error_safe(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => is_error_safe(operand) && is_error_safe(pattern),
        _ => false,
    }
}

/// Evaluates a WHERE or HAVING predicate against a row — NULL and FALSE both
/// filter it out, a non-boolean is a type error — short-circuiting a top-level
/// AND chain left-to-right. The conjuncts are already in PostgreSQL's cost
/// order — the scan reorders them once via [`reorder_qual`] before iterating
/// rows — so a cheap filtering conjunct runs before a costlier erroring one
/// (and a cheap erroring conjunct before a costlier filtering one),
/// reproducing PostgreSQL's error timing without re-sorting per row.
pub(super) fn where_passes<'e, 'a>(
    predicate: &'e Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let mut conjuncts: [&'e Expr<'a>; MAX_CONJUNCTS] = [predicate; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(predicate, &mut conjuncts, &mut n) || n <= 1 {
        return conjunct_passes(predicate, arena, params, row, hooks);
    }
    for &c in &conjuncts[..n] {
        if !conjunct_passes(c, arena, params, row, hooks)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Folds `col IS NOT NULL` to TRUE and `col IS NULL` to FALSE for a column with
/// a NOT NULL constraint, as PostgreSQL does using the constraint — so
/// `WHERE x/0 = 1 OR id IS NOT NULL` (id NOT NULL) drops the erroring branch.
/// Rewrites only the boolean spine (AND/OR/NOT/IS NULL); other nodes pass
/// through, since an IS NULL test appears as a boolean operand.
pub(super) fn fold_null<'a>(
    e: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use crate::sql::ast::{BinaryOp, UnaryOp};
    match e {
        Expr::IsNull { operand: Expr::Column { qualifier, name }, negated }
            if scope
                .find_column(*qualifier, name)
                .ok()
                .and_then(|entry| match entry {
                    ResolvedColumn::Table(t, c) => {
                        scope.defs[t].map(|d| d.columns()[c].not_null)
                    }
                    // A merged USING/NATURAL column can be null even over
                    // NOT NULL parts (outer-join null rows) — never fold.
                    ResolvedColumn::Merged(_) => None,
                })
                .unwrap_or(false) =>
        {
            Ok(&*arena.alloc(Expr::Bool(*negated)).map_err(|_| arena_full())?)
        }
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let (l, r) = (fold_null(left, scope, arena)?, fold_null(right, scope, arena)?);
            if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?)
            }
        }
        Expr::Unary { operator: UnaryOp::Not, operand } => {
            let o = fold_null(operand, scope, arena)?;
            if core::ptr::eq(o, *operand) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Unary { operator: UnaryOp::Not, operand: o })
                    .map_err(|_| arena_full())?)
            }
        }
        _ => Ok(e),
    }
}

/// Reorders a WHERE predicate's top-level AND conjuncts by PostgreSQL's
/// `order_qual_clauses` cost (cheapest first, stably), returning a rebuilt
/// left-deep AND. Done once per scan (not per row), so it can afford the
/// type-aware `qual_cost`. Constants and non-AND predicates pass through
/// unchanged.
pub(super) fn reorder_qual<'a>(
    pred: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let mut conjunct: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(pred, &mut conjunct, &mut n) || n == 0 {
        return Ok(pred);
    }
    // PostgreSQL rewrites `x BETWEEN a AND b` at parse time into `x >= a AND
    // x <= b` — two *independent* top-level conjuncts that order separately
    // (each is one comparison, cheaper than a compound clause).
    let mut expanded: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut m = 0usize;
    for &c in &conjunct[..n] {
        if let Expr::Between { operand, low, high, negated: false } = c {
            if m + 2 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = arena
                .alloc(Expr::Binary { operator: crate::sql::ast::BinaryOp::GtEq, left: operand, right: low })
                .map_err(|_| arena_full())?;
            expanded[m + 1] = arena
                .alloc(Expr::Binary { operator: crate::sql::ast::BinaryOp::LtEq, left: operand, right: high })
                .map_err(|_| arena_full())?;
            m += 2;
        } else {
            if m + 1 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = c;
            m += 1;
        }
    }
    let conjunct = expanded;
    let n = m;
    if n <= 1 {
        return Ok(conjunct[0]);
    }
    // PostgreSQL routes top-level *equality* conjuncts through its
    // equivalence-class machinery, which re-appends them to the qual list
    // AFTER every other conjunct; only then does `order_qual_clauses` run its
    // stable per-tuple-cost insertion sort (verified against the PostgreSQL 18
    // source and pinned empirically — `(a%a)=a AND (…OR…)` evaluates the OR
    // first on an exact cost tie, while `0 <> (…) AND (…OR…)` keeps written
    // order). The same calibrated cost model drives projection postponement.
    let is_equality = |c: &Expr| -> bool {
        matches!(c, Expr::Binary { operator: crate::sql::ast::BinaryOp::Eq, .. })
            || matches!(c, Expr::InList { list, negated: false, .. } if list.len() == 1)
    };
    let mut order = [0usize; MAX_CONJUNCTS];
    let mut at = 0usize;
    for (i, c) in conjunct[..n].iter().enumerate() {
        if !is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    for (i, c) in conjunct[..n].iter().enumerate() {
        if is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    let mut cost = [0u32; MAX_CONJUNCTS];
    for (i, c) in conjunct[..n].iter().enumerate() {
        cost[i] = postpone_cost(c, scope, arena);
    }
    for i in 1..n {
        let mut j = i;
        while j > 0 && cost[order[j - 1]] > cost[order[j]] {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
    // Rebuild a left-deep AND in cost order.
    let mut acc = conjunct[order[0]];
    for &i in &order[1..n] {
        acc = arena
            .alloc(Expr::Binary { operator: crate::sql::ast::BinaryOp::And, left: acc, right: conjunct[i] })
            .map_err(|_| arena_full())?;
    }
    Ok(acc)
}



/// PostgreSQL's `find_duplicate_ors` canonicalization: AND terms common to
/// every arm of an OR are factored out in front — `(A AND B) OR (A AND C)`
/// becomes `A AND (B OR C)`, and when an arm consists *only* of common terms
/// the whole OR collapses to them (`(A AND B) OR A` ≡ `A`), so the dropped
/// arms' other conjuncts are never evaluated.
pub(super) fn factor_common_or_terms<'a>(
    e: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use crate::sql::ast::BinaryOp;
    const MAX_PARTS: usize = 16;
    fn flatten_or<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::Or, left, right } = x {
            return flatten_or(left, out, n) && flatten_or(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    fn and_terms<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::And, left, right } = x {
            return and_terms(left, out, n) && and_terms(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    let dummy = e;
    let mut arms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_arms = 0;
    if !flatten_or(e, &mut arms, &mut n_arms) || n_arms < 2 {
        return Ok(e);
    }
    // Terms of the first arm that appear in every other arm.
    let mut common: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_common = 0;
    if !and_terms(arms[0], &mut common, &mut n_common) {
        return Ok(e);
    }
    let mut kept = 0usize;
    'term: for i in 0..n_common {
        for arm in &arms[1..n_arms] {
            let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
            let mut nt = 0;
            if !and_terms(arm, &mut terms, &mut nt) {
                return Ok(e);
            }
            if !terms[..nt].iter().any(|t| **t == *common[i]) {
                continue 'term;
            }
        }
        common[kept] = common[i];
        kept += 1;
    }
    if kept == 0 {
        return Ok(e);
    }
    // Residue of each arm (its terms minus the common ones). An empty residue
    // means that arm is implied by the common terms: the OR collapses.
    let mut residues: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_res = 0;
    for arm in &arms[..n_arms] {
        let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
        let mut nt = 0;
        let _ = and_terms(arm, &mut terms, &mut nt);
        let mut residue: Option<&Expr> = None;
        for &t in &terms[..nt] {
            if common[..kept].iter().any(|c| **c == *t) {
                continue;
            }
            residue = Some(match residue {
                None => t,
                Some(acc) => arena
                    .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: t })
                    .map_err(|_| arena_full())?,
            });
        }
        match residue {
            None => {
                // This arm is exactly the common terms: OR collapses to them.
                let mut acc = common[0];
                for &c in &common[1..kept] {
                    acc = arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
                        .map_err(|_| arena_full())?;
                }
                return Ok(acc);
            }
            Some(x) => {
                residues[n_res] = x;
                n_res += 1;
            }
        }
    }
    // AND(common..., OR(residues...)).
    let mut or_acc = residues[0];
    for &x in &residues[1..n_res] {
        or_acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::Or, left: or_acc, right: x })
            .map_err(|_| arena_full())?;
    }
    let mut acc = common[0];
    for &c in &common[1..kept] {
        acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
            .map_err(|_| arena_full())?;
    }
    Ok(&*arena
        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: or_acc })
        .map_err(|_| arena_full())?)
}

/// The plan-time boolean value of a condition, when PostgreSQL's
/// `eval_const_expressions` can decide it: a constant subtree, or an AND/OR
/// settled by one constant side. `None` = not decidable at plan time.
pub(super) fn plan_time_bool(e: &Expr, arena: &Arena) -> Option<bool> {
    use crate::sql::ast::BinaryOp;
    if e.is_constant() {
        return match crate::sql::eval::eval(e, arena, crate::sql::eval::NO_PARAMS, &crate::sql::eval::NoColumns) {
            Ok(Datum::Bool(b)) => Some(b),
            _ => None,
        };
    }
    match e {
        Expr::Binary { operator: BinaryOp::And, left, right } => {
            if plan_time_bool(left, arena) == Some(false)
                || plan_time_bool(right, arena) == Some(false)
            {
                Some(false)
            } else {
                None
            }
        }
        Expr::Binary { operator: BinaryOp::Or, left, right } => {
            if plan_time_bool(left, arena) == Some(true)
                || plan_time_bool(right, arena) == Some(true)
            {
                Some(true)
            } else {
                None
            }
        }
        Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand } => {
            plan_time_bool(operand, arena).map(|b| !b)
        }
        _ => None,
    }
}

/// PostgreSQL's plan-time boolean simplification applied to a qual: an AND arm
/// folding TRUE (or an OR arm folding FALSE) is dropped, and a decided
/// connective collapses to its constant. This exposes a nested AND to the
/// top-level conjunct ordering — `(a AND b) OR const-false` orders `a`/`b` by
/// cost just as PostgreSQL does after simplifying the OR away.
pub(super) fn simplify_qual<'a>(e: &'a Expr<'a>, arena: &'a Arena) -> Result<&'a Expr<'a>, SqlError> {
    use crate::sql::ast::BinaryOp;
    if let Some(b) = plan_time_bool(e, arena) {
        return Ok(if b { &Expr::Bool(true) } else { &Expr::Bool(false) });
    }
    match e {
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let keep_true = matches!(operator, BinaryOp::And);
            let l = simplify_qual(left, arena)?;
            let r = simplify_qual(right, arena)?;
            // The decided-connective cases returned above, so at most one side
            // is the droppable constant here.
            if *l == Expr::Bool(keep_true) {
                return Ok(r);
            }
            if *r == Expr::Bool(keep_true) {
                return Ok(l);
            }
            let rebuilt: &Expr = if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                e
            } else {
                arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?
            };
            if matches!(operator, BinaryOp::Or) {
                return factor_common_or_terms(rebuilt, arena);
            }
            Ok(rebuilt)
        }
        // NOT pushes through the connectives (De Morgan), exposing the pieces
        // to top-level conjunct ordering exactly as PostgreSQL's
        // `canonicalize_qual` does: `NOT (x OR y IS NOT NULL)` becomes
        // `NOT x AND y IS NULL`, so the cheap null test can filter first.
        Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand } => {
            let negated: &Expr = match *operand {
                Expr::Binary { operator: BinaryOp::Or, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Binary { operator: BinaryOp::And, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::Or, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Unary { operator: crate::sql::ast::UnaryOp::Not, operand: inner } => inner,
                Expr::IsNull { operand: inner, negated } => arena
                    .alloc(Expr::IsNull { operand: inner, negated: !negated })
                    .map_err(|_| arena_full())?,
                _ => return Ok(e),
            };
            simplify_qual(negated, arena)
        }
        _ => Ok(e),
    }
}

/// Evaluation cost of a select-list expression in half-operator units,
/// approximating PostgreSQL's `cost_qual_eval`: each operator or function
/// application costs 2, an implicit numeric-family coercion costs 2, an IN
/// list costs 1 per element (PostgreSQL charges half an operator per element),
/// and AND/OR/IS NULL cost nothing. Used for the sort/limit projection
/// postponement decision (threshold empirically pinned against PostgreSQL 18.4:
/// items costing more than 10 operators are projected above the Sort + Limit).
pub(super) fn postpone_cost(e: &Expr, scope: &QueryScope, arena: &Arena) -> u32 {
    use Expr::*;
    // PostgreSQL costs the *plan-time-folded* expression: a fully-constant
    // subtree has become a Const by then and costs nothing.
    if e.is_constant() {
        return 0;
    }
    let oid_of = |x: &Expr| -> Option<i32> {
        crate::sql::exec::infer_type_res(x, &ScopeCols(scope)).ok().map(|t| t.0)
    };
    // Numeric-family promotion rank: the lower-ranked operand is the one
    // PostgreSQL casts (int → numeric → float8).
    let rank = |o: i32| -> Option<u32> {
        use crate::sql::types::oid;
        Some(match o {
            oid::INT2 => 0,
            oid::INT4 => 1,
            oid::INT8 => 2,
            oid::NUMERIC => 3,
            oid::FLOAT4 => 4,
            oid::FLOAT8 => 5,
            _ => return None,
        })
    };
    // One implicit cast (2 half-ops) when a numeric-family pair mixes types —
    // free when the coerced side is a constant, which PostgreSQL folds into a
    // pre-cast Const at plan time.
    let coercion = |l: &Expr, r: &Expr| -> u32 {
        match (oid_of(l).and_then(rank), oid_of(r).and_then(rank)) {
            (Some(a), Some(b)) if a != b => {
                let coerced = if a < b { l } else { r };
                if coerced.is_constant() { 0 } else { 2 }
            }
            _ => 0,
        }
    };
    match e {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | BitLit(_) | Param(_)
        | DefaultMarker | Column { .. } | WholeRow(_) | SchemaColumn { .. } => 0,
        Unary { operator: crate::sql::ast::UnaryOp::Not, operand } => postpone_cost(operand, scope, arena),
        Unary { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        IsNull { operand, .. } => postpone_cost(operand, scope, arena),
        Cast { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        Binary { operator: crate::sql::ast::BinaryOp::And | crate::sql::ast::BinaryOp::Or, left, right } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena)
        }
        Binary { left, right, .. } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena) + 2 + coercion(left, right)
        }
        Between { operand, low, high, .. } => {
            postpone_cost(operand, scope, arena)
                + postpone_cost(low, scope, arena)
                + postpone_cost(high, scope, arena)
                + 4
                + coercion(operand, low)
                + coercion(operand, high)
        }
        InList { operand, list, .. } => {
            // PostgreSQL rewrites a one-element IN to plain `=` (one operator);
            // longer lists cost half an operator per element (= ANY(array)).
            let applications = if list.len() <= 1 { 2 } else { list.len() as u32 };
            postpone_cost(operand, scope, arena)
                + list.iter().map(|x| postpone_cost(x, scope, arena)).sum::<u32>()
                + applications
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            postpone_cost(operand, scope, arena) + postpone_cost(pattern, scope, arena) + 2
        }
        Call { name, args, .. } => {
            // GREATEST/LEAST/COALESCE unify their arguments' types, and
            // PostgreSQL charges one operator for each argument it has to cast
            // (a constant is pre-cast at plan time, as in the CASE arm below).
            // GREATEST and LEAST are a MinMaxExpr, which costs one operator of
            // its own; COALESCE is a CoalesceExpr, which like CASE costs
            // nothing beyond its casts.
            let unifying = name.eq_ignore_ascii_case("greatest")
                || name.eq_ignore_ascii_case("least")
                || name.eq_ignore_ascii_case("coalesce");
            let node = if name.eq_ignore_ascii_case("coalesce") { 0 } else { 2 };
            let mut c = args.iter().map(|a| postpone_cost(a, scope, arena)).sum::<u32>() + node;
            if unifying {
                let unified = oid_of(e).and_then(rank);
                for a in *args {
                    if a.is_constant() {
                        continue;
                    }
                    match (oid_of(a).and_then(rank), unified) {
                        (Some(x), Some(y)) if x != y => c += 2,
                        _ => {}
                    }
                }
            }
            c
        }
        Case { operand, whens, otherwise, .. } => {
            let mut c = operand.map_or(0, |o| postpone_cost(o, scope, arena));
            // Non-constant branch results whose type differs from the CASE's
            // unified result type carry an implicit cast, which PostgreSQL
            // counts (a constant result is pre-cast at plan time).
            let case_rank = oid_of(e).and_then(rank);
            let result_cast = |result: &Expr| -> u32 {
                if result.is_constant() {
                    return 0;
                }
                match (oid_of(result).and_then(rank), case_rank) {
                    (Some(a), Some(b)) if a != b => 2,
                    _ => 0,
                }
            };
            for (cond, result) in whens.iter() {
                // PostgreSQL's plan-time simplification drops a WHEN whose
                // condition folds to constant FALSE, and truncates the CASE at
                // one folding to constant TRUE.
                match plan_time_bool(cond, arena) {
                    Some(false) => continue,
                    Some(true) => {
                        c += postpone_cost(result, scope, arena) + result_cast(result);
                        return c;
                    }
                    None => {}
                }
                c += postpone_cost(cond, scope, arena) + postpone_cost(result, scope, arena);
                c += result_cast(result);
                // The simple form compares the operand per WHEN.
                if operand.is_some() {
                    c += 2;
                }
            }
            if let Some(o) = otherwise {
                c += postpone_cost(o, scope, arena) + result_cast(o);
            }
            c
        }
        Array(items) => items.iter().map(|x| postpone_cost(x, scope, arena)).sum(),
        Subscript { base, index } => postpone_cost(base, scope, arena) + postpone_cost(index, scope, arena),
        Field { base, .. } => postpone_cost(base, scope, arena),
        AnyAll { operand, array, .. } => {
            let elements = if let Array(items) = array { items.len() as u32 } else { 20 };
            postpone_cost(operand, scope, arena) + postpone_cost(array, scope, arena) + elements
        }
        // Subqueries carry a subplan's cost in PostgreSQL and are postponed.
        Subquery(_) | Exists(_) | ArraySubquery(_) | InSubquery { .. } => 1000,
    }
}

