//! Outer-join reduction, as PostgreSQL's planner does it
//! (`reduce_outer_joins`): a condition above an outer join that cannot be
//! true when the nullable side's columns are all NULL removes every
//! null-extended row, so the join runs, and `EXPLAIN` prints it, as the
//! narrower join. `a LEFT JOIN b ON a.id = b.id WHERE b.x = 1` is an inner
//! join; a `FULL` join becomes `LEFT`, `RIGHT` or inner by which sides such a
//! condition reads.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/prep/prepjointree.c

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, CallKind, Expr, FromClause, Join, JoinKind, Select, UnaryOp};
use crate::sql::eval::{ColumnLookup, SqlError};
use crate::storage::Storage;

use super::{QueryScope, ResolvedColumn, arena_full};

/// `select` with each outer join its conditions reduce narrowed. The FROM
/// list is left-deep: join `i` joins the items before it (`0..=i`) with
/// item `i + 1`.
pub(crate) fn reduce_outer_joins<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<&'a Select<'a>, SqlError> {
    let Some(from) = select.from.as_ref() else {
        return Ok(select);
    };
    if from.joins.len() >= 63
        || !from
            .joins
            .iter()
            .any(|join| matches!(join.kind, JoinKind::Left | JoinKind::Right | JoinKind::Full))
    {
        return Ok(select);
    }
    // Names that do not resolve fail the statement's own planning right
    // after this, with the error PostgreSQL reports; here they only leave
    // the joins as written.
    let Ok(scope) = QueryScope::resolve_schema(storage, from, txid, arena, outer) else {
        return Ok(select);
    };
    if scope.n != from.joins.len() + 1 {
        return Ok(select);
    }
    let relation = |qualifier: Option<&str>, name: &str| match scope.find_column(qualifier, name) {
        Ok(ResolvedColumn::Table(table, _)) if table < 64 => Some(table),
        _ => None,
    };
    // A call's strictness is known once bound; an unbound call of a
    // built-in name is strict when every overload of its arity is.
    let strict_call = |name: &str, bound: Option<bool>, arguments: usize| {
        bound.unwrap_or_else(|| {
            !name.contains('.')
                && crate::sql::catalog::builtin_overloads_strict(storage, txid, name, arguments)
        })
    };
    let relation = &relation;
    let nonnullable = |condition: &Expr<'_>| {
        nonnullable(
            condition,
            true,
            &|qualifier, name| relation(qualifier, name),
            &strict_call,
        )
    };
    let mut kinds = [JoinKind::Inner; 63];
    let mut changed = false;
    let mut upper = select.where_clause.map_or(0, nonnullable);
    for (index, join) in from.joins.iter().enumerate().rev() {
        let right = 1u64 << (index + 1);
        let left = right - 1;
        let merged = join.using.is_some() || join.natural;
        let kind = match join.kind {
            JoinKind::Left if upper & right != 0 => JoinKind::Inner,
            // A merged `USING` column reads the right side of a right join
            // and both sides of a full join, so those keep their kind.
            JoinKind::Right if !merged && upper & left != 0 => JoinKind::Inner,
            JoinKind::Full if !merged => match (upper & left != 0, upper & right != 0) {
                (true, true) => JoinKind::Inner,
                (true, false) => JoinKind::Left,
                (false, true) => JoinKind::Right,
                (false, false) => JoinKind::Full,
            },
            kind => kind,
        };
        changed |= kind != join.kind;
        kinds[index] = kind;
        let own = join.on.map_or(0, nonnullable);
        // The items before an inner join, or before the nullable side's
        // partner of a right join, also lose rows the join's own condition
        // rejects; a left join's condition only narrows its right item, and
        // nothing passes through a full join.
        upper = match kind {
            JoinKind::Inner | JoinKind::Cross | JoinKind::Comma | JoinKind::Right => upper | own,
            JoinKind::Left => upper,
            JoinKind::Full => 0,
        };
    }
    if !changed {
        return Ok(select);
    }
    let joins = arena
        .alloc_slice_with(from.joins.len(), |index| Join {
            kind: kinds[index],
            ..from.joins[index]
        })
        .map_err(|_| arena_full())?;
    let reduced = arena
        .alloc(Select {
            from: Some(FromClause {
                base: from.base,
                joins,
            }),
            ..*select
        })
        .map_err(|_| arena_full())?;
    Ok(reduced)
}

/// The FROM items (bit per item) whose all-NULL columns make `expression`
/// NULL or, at the top level of a condition, false: PostgreSQL's
/// `find_nonnullable_rels`.
fn nonnullable(
    expression: &Expr<'_>,
    top: bool,
    relation: &dyn Fn(Option<&str>, &str) -> Option<usize>,
    strict_call: &dyn Fn(&str, Option<bool>, usize) -> bool,
) -> u64 {
    let strict = |expression: &Expr<'_>| nonnullable(expression, false, relation, strict_call);
    let same = |expression: &Expr<'_>| nonnullable(expression, top, relation, strict_call);
    match expression {
        Expr::Column { qualifier, name } => {
            relation(*qualifier, name).map_or(0, |table| 1 << table)
        }
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } => {
            // Below the top level one NULL arm can still yield false.
            let (left, right) = (same(left), same(right));
            if top { left | right } else { left & right }
        }
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
            ..
        } => same(left) & same(right),
        Expr::Binary {
            operator:
                BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::Pow
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq,
            left,
            right,
            ..
        } => strict(left) | strict(right),
        Expr::Unary {
            operator:
                UnaryOp::Not
                | UnaryOp::Neg
                | UnaryOp::AbsoluteValue
                | UnaryOp::SquareRoot
                | UnaryOp::CubeRoot,
            operand,
            ..
        } => strict(operand),
        Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => strict(operand),
        Expr::IsNull {
            operand,
            negated: true,
        } if top => strict(operand),
        Expr::InList { operand, .. } if top => strict(operand),
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::Match {
            operand, pattern, ..
        } => strict(operand) | strict(pattern),
        Expr::Call {
            kind: CallKind::Routine,
            name,
            bound_strict,
            args,
            over: None,
            filter: None,
            star: false,
            distinct: false,
            order_by: [],
            ..
        } if strict_call(name, *bound_strict, args.len()) => args
            .iter()
            .fold(0, |rels, argument| rels | strict(argument)),
        _ => 0,
    }
}
