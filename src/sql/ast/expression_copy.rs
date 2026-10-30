//! Copies expression identities when SQL grammar expands one occurrence into
//! several. Immutable strings and nested SELECT bodies keep their owners;
//! each copied subquery expression is an independent execution occurrence.

use super::{Expr, FrameBound, OrderBy, WindowSpec};
use crate::mem::arena::{Arena, ArenaFull};

fn expressions<'a>(
    values: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], ArenaFull> {
    let copied = arena.alloc_slice_copy(values)?;
    for value in copied.iter_mut() {
        *value = duplicate_expression(value, arena)?;
    }
    Ok(copied)
}

fn ordering<'a>(values: &[OrderBy<'a>], arena: &'a Arena) -> Result<&'a [OrderBy<'a>], ArenaFull> {
    let copied = arena.alloc_slice_copy(values)?;
    for value in copied.iter_mut() {
        value.expression = duplicate_expression(value.expression, arena)?;
    }
    Ok(copied)
}

fn frame_bound<'a>(value: FrameBound<'a>, arena: &'a Arena) -> Result<FrameBound<'a>, ArenaFull> {
    Ok(match value {
        FrameBound::Preceding(expression) => {
            FrameBound::Preceding(duplicate_expression(expression, arena)?)
        }
        FrameBound::Following(expression) => {
            FrameBound::Following(duplicate_expression(expression, arena)?)
        }
        FrameBound::UnboundedPreceding
        | FrameBound::CurrentRow
        | FrameBound::UnboundedFollowing => value,
    })
}

pub(crate) fn duplicate_expression<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, ArenaFull> {
    let mut copied = *expression;
    match &mut copied {
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. }
        | Expr::RecordFieldIndex { base: operand, .. }
        | Expr::InSubquery { operand, .. }
        | Expr::QuantifiedSubquery { operand, .. } => {
            *operand = duplicate_expression(operand, arena)?;
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
            *left = duplicate_expression(left, arena)?;
            *right = duplicate_expression(right, arena)?;
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            *args = expressions(args, arena)?;
            *order_by = ordering(order_by, arena)?;
            if let Some(window) = over {
                let mut window = **window;
                window.partition_by = expressions(window.partition_by, arena)?;
                window.order_by = ordering(window.order_by, arena)?;
                if let Some(frame) = &mut window.frame {
                    frame.start = frame_bound(frame.start, arena)?;
                    frame.end = frame_bound(frame.end, arena)?;
                }
                *over = Some(arena.alloc::<WindowSpec<'a>>(window)?);
            }
            *filter = filter
                .map(|value| duplicate_expression(value, arena))
                .transpose()?;
        }
        Expr::InList { operand, list, .. } => {
            *operand = duplicate_expression(operand, arena)?;
            *list = expressions(list, arena)?;
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            *operand = duplicate_expression(operand, arena)?;
            *pattern = duplicate_expression(pattern, arena)?;
            *escape = escape
                .map(|value| duplicate_expression(value, arena))
                .transpose()?;
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            *operand = operand
                .map(|value| duplicate_expression(value, arena))
                .transpose()?;
            let copied = arena.alloc_slice_copy(whens)?;
            for (condition, value) in copied.iter_mut() {
                *condition = duplicate_expression(condition, arena)?;
                *value = duplicate_expression(value, arena)?;
            }
            *whens = copied;
            *otherwise = otherwise
                .map(|value| duplicate_expression(value, arena))
                .transpose()?;
        }
        Expr::Array(values) => *values = expressions(values, arena)?,
        Expr::Slice { base, lower, upper } => {
            *base = duplicate_expression(base, arena)?;
            *lower = lower
                .map(|value| duplicate_expression(value, arena))
                .transpose()?;
            *upper = upper
                .map(|value| duplicate_expression(value, arena))
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
