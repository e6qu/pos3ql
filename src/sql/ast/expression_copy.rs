//! Copies expression identities when SQL grammar expands one occurrence into
//! several. Immutable strings and nested SELECT bodies keep their owners;
//! each copied subquery expression is an independent execution occurrence.

use super::{Expr, FrameBound, OrderBy, WindowSpec};
use crate::mem::arena::{Arena, ArenaFull};

fn expressions<'a>(
    values: &[&'a Expr<'a>],
    arena: &'a Arena,
    shared: &[&'a Expr<'a>],
) -> Result<&'a [&'a Expr<'a>], ArenaFull> {
    let copied = arena.alloc_slice_copy(values)?;
    for value in copied.iter_mut() {
        *value = duplicate_expression(value, arena, shared)?;
    }
    Ok(copied)
}

fn ordering<'a>(
    values: &[OrderBy<'a>],
    arena: &'a Arena,
    shared: &[&'a Expr<'a>],
) -> Result<&'a [OrderBy<'a>], ArenaFull> {
    let copied = arena.alloc_slice_copy(values)?;
    for value in copied.iter_mut() {
        value.expression = duplicate_expression(value.expression, arena, shared)?;
    }
    Ok(copied)
}

fn frame_bound<'a>(
    value: FrameBound<'a>,
    arena: &'a Arena,
    shared: &[&'a Expr<'a>],
) -> Result<FrameBound<'a>, ArenaFull> {
    Ok(match value {
        FrameBound::Preceding(expression) => {
            FrameBound::Preceding(duplicate_expression(expression, arena, shared)?)
        }
        FrameBound::Following(expression) => {
            FrameBound::Following(duplicate_expression(expression, arena, shared)?)
        }
        FrameBound::UnboundedPreceding
        | FrameBound::CurrentRow
        | FrameBound::UnboundedFollowing => value,
    })
}

pub(crate) fn duplicate_expression<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
    shared: &[&'a Expr<'a>],
) -> Result<&'a Expr<'a>, ArenaFull> {
    if let Some(node) = shared.iter().find(|node| core::ptr::eq(**node, expression)) {
        return Ok(*node);
    }
    let mut copied = *expression;
    match &mut copied {
        Expr::BoundLiteral { value, source, .. } => {
            *value = match *value {
                super::BoundLiteralValue::CheckedArrayInput(encoded) => {
                    super::BoundLiteralValue::CheckedArrayInput(arena.alloc_slice_copy(encoded)?)
                }
                super::BoundLiteralValue::CachedDefault(encoded) => {
                    super::BoundLiteralValue::CachedDefault(arena.alloc_slice_copy(encoded)?)
                }
                super::BoundLiteralValue::Folded(encoded) => {
                    super::BoundLiteralValue::Folded(arena.alloc_slice_copy(encoded)?)
                }
            };
            *source = duplicate_expression(source, arena, &[])?;
        }
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. }
        | Expr::RecordFieldIndex { base: operand, .. }
        | Expr::InSubquery { operand, .. }
        | Expr::QuantifiedSubquery { operand, .. } => {
            *operand = duplicate_expression(operand, arena, shared)?;
        }
        Expr::Binary { left, right, .. }
        | Expr::Subscript {
            base: left,
            index: right,
        }
        | Expr::AnyAll {
            operand: left,
            array: right,
            ..
        }
        | Expr::Match {
            operand: left,
            pattern: right,
            ..
        } => {
            *left = duplicate_expression(left, arena, shared)?;
            *right = duplicate_expression(right, arena, shared)?;
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            *args = expressions(args, arena, shared)?;
            *order_by = ordering(order_by, arena, shared)?;
            if let Some(window) = over {
                let mut window = **window;
                window.partition_by = expressions(window.partition_by, arena, shared)?;
                window.order_by = ordering(window.order_by, arena, shared)?;
                if let Some(frame) = &mut window.frame {
                    frame.start = frame_bound(frame.start, arena, shared)?;
                    frame.end = frame_bound(frame.end, arena, shared)?;
                }
                *over = Some(arena.alloc::<WindowSpec<'a>>(window)?);
            }
            *filter = filter
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
        }
        Expr::InList { operand, list, .. } => {
            *operand = duplicate_expression(operand, arena, shared)?;
            *list = expressions(list, arena, shared)?;
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            *operand = duplicate_expression(operand, arena, shared)?;
            *pattern = duplicate_expression(pattern, arena, shared)?;
            *escape = escape
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            *operand = operand
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
            let copied = arena.alloc_slice_copy(whens)?;
            for (condition, value) in copied.iter_mut() {
                *condition = duplicate_expression(condition, arena, shared)?;
                *value = duplicate_expression(value, arena, shared)?;
            }
            *whens = copied;
            *otherwise = otherwise
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
        }
        Expr::Array(values) => *values = expressions(values, arena, shared)?,
        Expr::Slice { base, lower, upper } => {
            *base = duplicate_expression(base, arena, shared)?;
            *lower = lower
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
            *upper = upper
                .map(|value| duplicate_expression(value, arena, shared))
                .transpose()?;
        }
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Column { .. }
        | Expr::RoutineParam { .. }
        | Expr::Param(_)
        | Expr::OuterAggregate { .. }
        | Expr::DefaultMarker
        | Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_)
        | Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::RecursiveState { .. } => {}
    }
    Ok(arena.alloc(copied)?)
}

/// A top-down replacement for [`rewrite_expression`]: `Some` replaces the
/// node and its whole subtree, `None` keeps the node and visits its children.
pub(crate) type ExpressionReplacement<'r, 'a, E> =
    dyn FnMut(&'a Expr<'a>) -> Result<Option<&'a Expr<'a>>, E> + 'r;

/// Rebuilds `expression` with `replace` applied to every node, top down.
/// Subquery bodies are leaves here: `replace` answers for the nodes that hold
/// them. Unchanged subtrees keep their nodes, so identities that locate
/// positions and execution sites survive.
pub(crate) fn rewrite_expression<'a, E: From<ArenaFull>>(
    expression: &'a Expr<'a>,
    arena: &'a Arena,
    replace: &mut ExpressionReplacement<'_, 'a, E>,
) -> Result<&'a Expr<'a>, E> {
    if let Some(replacement) = replace(expression)? {
        return Ok(replacement);
    }
    let mut changed = false;
    let changed = &mut changed;
    fn child<'a, E: From<ArenaFull>>(
        value: &'a Expr<'a>,
        arena: &'a Arena,
        replace: &mut ExpressionReplacement<'_, 'a, E>,
        changed: &mut bool,
    ) -> Result<&'a Expr<'a>, E> {
        let rewritten = rewrite_expression(value, arena, replace)?;
        *changed |= !core::ptr::eq(rewritten, value);
        Ok(rewritten)
    }
    let mut copied = *expression;
    match &mut copied {
        Expr::BoundLiteral { .. } => {}
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. }
        | Expr::RecordFieldIndex { base: operand, .. }
        | Expr::InSubquery { operand, .. }
        | Expr::QuantifiedSubquery { operand, .. } => {
            *operand = child(operand, arena, replace, changed)?
        }
        Expr::Binary { left, right, .. }
        | Expr::Subscript {
            base: left,
            index: right,
        }
        | Expr::AnyAll {
            operand: left,
            array: right,
            ..
        }
        | Expr::Match {
            operand: left,
            pattern: right,
            ..
        } => {
            *left = child(left, arena, replace, changed)?;
            *right = child(right, arena, replace, changed)?;
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            *args = rewrite_expressions(args, arena, replace, changed)?;
            *order_by = rewrite_ordering(order_by, arena, replace, changed)?;
            if let Some(window) = over {
                let before = core::mem::replace(changed, false);
                let mut copy = **window;
                copy.partition_by =
                    rewrite_expressions(window.partition_by, arena, replace, changed)?;
                copy.order_by = rewrite_ordering(window.order_by, arena, replace, changed)?;
                if let Some(frame) = &mut copy.frame {
                    for bound in [&mut frame.start, &mut frame.end] {
                        if let FrameBound::Preceding(value) | FrameBound::Following(value) = bound {
                            *value = child(value, arena, replace, changed)?;
                        }
                    }
                }
                if *changed {
                    *over = Some(arena.alloc::<WindowSpec<'a>>(copy)?);
                }
                *changed |= before;
            }
            *filter = filter
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
        }
        Expr::InList { operand, list, .. } => {
            *operand = child(operand, arena, replace, changed)?;
            *list = rewrite_expressions(list, arena, replace, changed)?;
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            *operand = child(operand, arena, replace, changed)?;
            *pattern = child(pattern, arena, replace, changed)?;
            *escape = escape
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            *operand = operand
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
            let mut rewritten = None;
            for (index, (condition, value)) in whens.iter().enumerate() {
                let pair = (
                    child(condition, arena, replace, changed)?,
                    child(value, arena, replace, changed)?,
                );
                if (!core::ptr::eq(pair.0, *condition) || !core::ptr::eq(pair.1, *value))
                    && rewritten.is_none()
                {
                    rewritten = Some(arena.alloc_slice_copy(whens)?);
                }
                if let Some(copy) = rewritten.as_deref_mut() {
                    copy[index] = pair;
                }
            }
            if let Some(copy) = rewritten {
                *whens = copy;
            }
            *otherwise = otherwise
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
        }
        Expr::Array(values) => *values = rewrite_expressions(values, arena, replace, changed)?,
        Expr::Slice { base, lower, upper } => {
            *base = child(base, arena, replace, changed)?;
            *lower = lower
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
            *upper = upper
                .map(|value| child(value, arena, replace, changed))
                .transpose()?;
        }
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Column { .. }
        | Expr::RoutineParam { .. }
        | Expr::Param(_)
        | Expr::OuterAggregate { .. }
        | Expr::DefaultMarker
        | Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_)
        | Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::RecursiveState { .. } => {}
    }
    if !*changed {
        return Ok(expression);
    }
    Ok(arena.alloc(copied)?)
}

/// [`rewrite_expression`] over a list; the original list survives unchanged.
pub(crate) fn rewrite_expressions<'a, E: From<ArenaFull>>(
    values: &'a [&'a Expr<'a>],
    arena: &'a Arena,
    replace: &mut ExpressionReplacement<'_, 'a, E>,
    changed: &mut bool,
) -> Result<&'a [&'a Expr<'a>], E> {
    let mut rewritten: Option<&'a mut [&'a Expr<'a>]> = None;
    for (index, value) in values.iter().enumerate() {
        let new = rewrite_expression(value, arena, replace)?;
        if !core::ptr::eq(new, *value) && rewritten.is_none() {
            rewritten = Some(arena.alloc_slice_copy(values)?);
        }
        if let Some(copy) = rewritten.as_deref_mut() {
            copy[index] = new;
        }
    }
    Ok(match rewritten {
        Some(copy) => {
            *changed = true;
            copy
        }
        None => values,
    })
}

/// [`rewrite_expressions`] for ORDER BY keys.
pub(crate) fn rewrite_ordering<'a, E: From<ArenaFull>>(
    values: &'a [OrderBy<'a>],
    arena: &'a Arena,
    replace: &mut ExpressionReplacement<'_, 'a, E>,
    changed: &mut bool,
) -> Result<&'a [OrderBy<'a>], E> {
    let mut rewritten: Option<&'a mut [OrderBy<'a>]> = None;
    for (index, value) in values.iter().enumerate() {
        let new = rewrite_expression(value.expression, arena, replace)?;
        if !core::ptr::eq(new, value.expression) && rewritten.is_none() {
            rewritten = Some(arena.alloc_slice_copy(values)?);
        }
        if let Some(copy) = rewritten.as_deref_mut() {
            copy[index].expression = new;
        }
    }
    Ok(match rewritten {
        Some(copy) => {
            *changed = true;
            copy
        }
        None => values,
    })
}
