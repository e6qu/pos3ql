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

/// Rebuilds `expression` with each node that `replace` answers replaced, and
/// every other node kept. `replace` sees a node before its children; a node
/// it replaces is not descended into. Unchanged subtrees keep their identity,
/// so an expression nothing replaced is returned as is. Nested SELECT bodies
/// are leaves here: `replace` decides what a subquery node becomes.
pub(crate) fn map_expression<'a, E: From<ArenaFull>>(
    expression: &'a Expr<'a>,
    arena: &'a Arena,
    replace: &mut dyn FnMut(&'a Expr<'a>) -> Result<Option<&'a Expr<'a>>, E>,
) -> Result<&'a Expr<'a>, E> {
    if let Some(replaced) = replace(expression)? {
        return Ok(replaced);
    }
    let mut changed = false;
    let child = |value: &'a Expr<'a>,
                 changed: &mut bool,
                 replace: &mut dyn FnMut(&'a Expr<'a>) -> Result<Option<&'a Expr<'a>>, E>|
     -> Result<&'a Expr<'a>, E> {
        let mapped = map_expression(value, arena, replace)?;
        *changed |= !core::ptr::eq(mapped, value);
        Ok(mapped)
    };
    let list = |values: &'a [&'a Expr<'a>],
                changed: &mut bool,
                replace: &mut dyn FnMut(&'a Expr<'a>) -> Result<Option<&'a Expr<'a>>, E>|
     -> Result<&'a [&'a Expr<'a>], E> {
        let mut copied: Option<&'a mut [&'a Expr<'a>]> = None;
        for (index, value) in values.iter().enumerate() {
            let mapped = map_expression(value, arena, replace)?;
            if !core::ptr::eq(mapped, *value) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => arena.alloc_slice_copy(values)?,
                };
                slice[index] = mapped;
                copied = Some(slice);
            }
        }
        Ok(match copied {
            Some(slice) => {
                *changed = true;
                slice
            }
            None => values,
        })
    };
    let ordering_list =
        |values: &'a [OrderBy<'a>],
         changed: &mut bool,
         replace: &mut dyn FnMut(&'a Expr<'a>) -> Result<Option<&'a Expr<'a>>, E>|
         -> Result<&'a [OrderBy<'a>], E> {
            let mut copied: Option<&'a mut [OrderBy<'a>]> = None;
            for (index, value) in values.iter().enumerate() {
                let mapped = map_expression(value.expression, arena, replace)?;
                if !core::ptr::eq(mapped, value.expression) {
                    let slice = match copied.take() {
                        Some(slice) => slice,
                        None => arena.alloc_slice_copy(values)?,
                    };
                    slice[index].expression = mapped;
                    copied = Some(slice);
                }
            }
            Ok(match copied {
                Some(slice) => {
                    *changed = true;
                    slice
                }
                None => values,
            })
        };
    let mut copied = *expression;
    match &mut copied {
        Expr::BoundLiteral { .. }
        | Expr::Null
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
            *operand = child(operand, &mut changed, replace)?;
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
            *left = child(left, &mut changed, replace)?;
            *right = child(right, &mut changed, replace)?;
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            *args = list(args, &mut changed, replace)?;
            *order_by = ordering_list(order_by, &mut changed, replace)?;
            if let Some(window) = over {
                let mut window_changed = false;
                let mut mapped = **window;
                mapped.partition_by = list(window.partition_by, &mut window_changed, replace)?;
                mapped.order_by = ordering_list(window.order_by, &mut window_changed, replace)?;
                if let Some(frame) = &mut mapped.frame {
                    for bound in [&mut frame.start, &mut frame.end] {
                        if let FrameBound::Preceding(value) | FrameBound::Following(value) = bound {
                            *value = child(value, &mut window_changed, replace)?;
                        }
                    }
                }
                if window_changed {
                    changed = true;
                    *over = Some(arena.alloc::<WindowSpec<'a>>(mapped)?);
                }
            }
            if let Some(value) = filter {
                *value = child(value, &mut changed, replace)?;
            }
        }
        Expr::InList {
            operand,
            list: values,
            ..
        } => {
            *operand = child(operand, &mut changed, replace)?;
            *values = list(values, &mut changed, replace)?;
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            *operand = child(operand, &mut changed, replace)?;
            *pattern = child(pattern, &mut changed, replace)?;
            if let Some(value) = escape {
                *value = child(value, &mut changed, replace)?;
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(value) = operand {
                *value = child(value, &mut changed, replace)?;
            }
            let mut copied_whens: Option<&'a mut [(&'a Expr<'a>, &'a Expr<'a>)]> = None;
            for (index, (condition, result)) in whens.iter().enumerate() {
                let mut pair_changed = false;
                let mapped_condition = child(condition, &mut pair_changed, replace)?;
                let mapped_result = child(result, &mut pair_changed, replace)?;
                if pair_changed {
                    let slice = match copied_whens.take() {
                        Some(slice) => slice,
                        None => arena.alloc_slice_copy(whens)?,
                    };
                    slice[index] = (mapped_condition, mapped_result);
                    copied_whens = Some(slice);
                }
            }
            if let Some(slice) = copied_whens {
                changed = true;
                *whens = slice;
            }
            if let Some(value) = otherwise {
                *value = child(value, &mut changed, replace)?;
            }
        }
        Expr::Array(values) => *values = list(values, &mut changed, replace)?,
        Expr::Slice { base, lower, upper } => {
            *base = child(base, &mut changed, replace)?;
            if let Some(value) = lower {
                *value = child(value, &mut changed, replace)?;
            }
            if let Some(value) = upper {
                *value = child(value, &mut changed, replace)?;
            }
        }
    }
    if changed {
        Ok(arena.alloc(copied)?)
    } else {
        Ok(expression)
    }
}
