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
