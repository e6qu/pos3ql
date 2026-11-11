//! Assignment to array elements and slices (`UPDATE t SET a[2] = 9`,
//! `a[1:2] = '{1,2}'`), with PostgreSQL's `array_set_element` and
//! `array_set_slice` semantics: a one-dimensional array grows with NULL
//! fill, other arrays stay within their bounds, and an empty or NULL array
//! takes the shape the subscripts describe.

use crate::mem::arena::Arena;
use crate::sql::array::{self, Shape};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{ArrElem, Datum};
use crate::sql_err;

/// PostgreSQL's MAXDIM.
const MAX_DIMENSIONS: usize = 6;

/// One written subscript of an assignment target. A list holding any slice
/// is a slice throughout: a plain `[i]` then means `[1:i]`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum AssignmentSubscript {
    Element(i32),
    Slice {
        lower: Option<i32>,
        upper: Option<i32>,
    },
}

fn subscript_error(message: &'static str) -> SqlError {
    sql_err!(sqlstate::ARRAY_SUBSCRIPT_ERROR, "{}", message)
}

/// The existing array as items and shape; NULL and empty arrays have none.
struct Current<'a> {
    element: Option<ArrElem>,
    items: &'a [Datum<'a>],
    shape: Shape,
}

fn current<'a>(base: Datum<'a>, arena: &'a Arena) -> Result<Current<'a>, SqlError> {
    match base {
        Datum::Array { element, raw } => {
            let shape = array::shape(raw).expect("array datum invariant");
            let items = array::alloc_items(arena, shape.element_count())?;
            for (index, item) in items.iter_mut().enumerate() {
                *item = array::get(raw, element, index).expect("array element in range");
            }
            Ok(Current {
                element: Some(element),
                items,
                shape,
            })
        }
        Datum::Null => Ok(Current {
            element: None,
            items: &[],
            shape: Shape::empty(),
        }),
        _ => Err(crate::sql::eval::not_subscriptable(&base)),
    }
}

/// Converts `value` to the array's element type, as PostgreSQL's assignment
/// coercion does, and returns the element type to build with.
fn element_value<'a>(
    element: Option<ArrElem>,
    value: Datum<'a>,
    arena: &'a Arena,
) -> Result<(ArrElem, Datum<'a>), SqlError> {
    match element {
        Some(element) if !value.is_null() => Ok((
            element,
            crate::sql::eval::cast_to(value, element.to_coltype(), arena)?,
        )),
        Some(element) => Ok((element, Datum::Null)),
        None => Ok((ArrElem::from_datum(&value).unwrap_or(ArrElem::Text), value)),
    }
}

/// Builds the array of `shape` whose element at each position comes from
/// `fill(multi-index)`.
fn build<'a>(
    element: ArrElem,
    shape: Shape,
    arena: &'a Arena,
    mut fill: impl FnMut(&[i32]) -> Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    let count = shape.element_count();
    let items = array::alloc_items(arena, count)?;
    let dimensions = shape.dimension_count();
    let mut index = [0i32; MAX_DIMENSIONS];
    for (dimension, slot) in index.iter_mut().enumerate().take(dimensions) {
        *slot = shape.lower_bound(dimension).expect("dimension in range");
    }
    for item in items.iter_mut() {
        *item = fill(&index[..dimensions]);
        // Row-major: the last subscript varies fastest.
        for dimension in (0..dimensions).rev() {
            if index[dimension] < shape.upper_bound(dimension).expect("dimension in range") {
                index[dimension] += 1;
                break;
            }
            index[dimension] = shape.lower_bound(dimension).expect("dimension in range");
        }
    }
    Ok(Datum::Array {
        element,
        raw: array::build_shaped(items, shape, arena)?,
    })
}

/// The existing element at `index`, or NULL outside the current bounds.
fn existing<'a>(current: &Current<'a>, index: &[i32]) -> Datum<'a> {
    let shape = current.shape;
    if index.len() != shape.dimension_count() {
        return Datum::Null;
    }
    let mut offset = 0usize;
    for (dimension, &position) in index.iter().enumerate() {
        let lower = shape.lower_bound(dimension).expect("dimension in range");
        let upper = shape.upper_bound(dimension).expect("dimension in range");
        if position < lower || position > upper {
            return Datum::Null;
        }
        offset = offset * shape.dimension(dimension).expect("dimension in range")
            + (position - lower) as usize;
    }
    current.items[offset]
}

fn shape_of(lower: &[i32], upper: &[i32]) -> Result<Shape, SqlError> {
    let mut dimensions = [0usize; MAX_DIMENSIONS];
    for (dimension, (lower, upper)) in lower.iter().zip(upper).enumerate() {
        dimensions[dimension] = usize::try_from(i64::from(*upper) - i64::from(*lower) + 1)
            .map_err(|_| subscript_error("array size exceeds the maximum allowed"))?;
    }
    Shape::new(&dimensions[..lower.len()], lower)
}

/// `base[subscripts] := value`.
pub(crate) fn assign<'a>(
    base: Datum<'a>,
    subscripts: &[AssignmentSubscript],
    value: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if subscripts.len() > MAX_DIMENSIONS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "number of array dimensions ({}) exceeds the maximum allowed ({})",
            subscripts.len(),
            MAX_DIMENSIONS
        ));
    }
    let current = current(base, arena)?;
    if subscripts
        .iter()
        .any(|subscript| matches!(subscript, AssignmentSubscript::Slice { .. }))
    {
        return assign_slice(current, subscripts, value, arena);
    }
    let positions = subscripts.iter().map(|subscript| match subscript {
        AssignmentSubscript::Element(position) => *position,
        AssignmentSubscript::Slice { .. } => unreachable!("checked above"),
    });
    let mut target = [0i32; MAX_DIMENSIONS];
    for (slot, position) in target.iter_mut().zip(positions) {
        *slot = position;
    }
    let target = &target[..subscripts.len()];
    let (element, value) = element_value(current.element, value, arena)?;
    let dimensions = current.shape.dimension_count();
    let mut lower = [0i32; MAX_DIMENSIONS];
    let mut upper = [0i32; MAX_DIMENSIONS];
    // A NULL or empty array takes a one-element shape at the subscripts.
    if dimensions == 0 {
        lower[..target.len()].copy_from_slice(target);
        upper[..target.len()].copy_from_slice(target);
    } else {
        if subscripts.len() != dimensions {
            return Err(subscript_error("wrong number of array subscripts"));
        }
        let shape = current.shape;
        for dimension in 0..dimensions {
            lower[dimension] = shape.lower_bound(dimension).expect("dimension in range");
            upper[dimension] = shape.upper_bound(dimension).expect("dimension in range");
        }
        if dimensions == 1 {
            // Only a one-dimensional array grows, filling gaps with NULL.
            lower[0] = lower[0].min(target[0]);
            upper[0] = upper[0].max(target[0]);
        } else if (0..dimensions).any(|dimension| {
            target[dimension] < lower[dimension] || target[dimension] > upper[dimension]
        }) {
            return Err(subscript_error("array subscript out of range"));
        }
    }
    let rank = target.len();
    let (lower, upper) = (&lower[..rank], &upper[..rank]);
    let shape = shape_of(lower, upper)?;
    build(element, shape, arena, |index| {
        if index == target {
            value
        } else {
            existing(&current, index)
        }
    })
}

fn assign_slice<'a>(
    current: Current<'a>,
    subscripts: &[AssignmentSubscript],
    value: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (source_element, source) = match value {
        // PostgreSQL leaves the array unchanged for a NULL source.
        Datum::Null => {
            return Ok(match current.element {
                Some(element) => build(element, current.shape, arena, |index| {
                    existing(&current, index)
                })?,
                None => Datum::Null,
            });
        }
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(text) => {
            let element = current.element.unwrap_or(ArrElem::Text);
            (element, array::parse_literal(text, element, arena)?)
        }
        other => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "source for a slice assignment must be an array, not type {}",
                crate::sql::eval::type_name_of_pub(&other)
            ));
        }
    };
    let element = current.element.unwrap_or(source_element);
    let dimensions = current.shape.dimension_count();
    let written = subscripts.len();
    let mut lower = [0i32; MAX_DIMENSIONS];
    let mut upper = [0i32; MAX_DIMENSIONS];
    let rank = if dimensions == 0 { written } else { dimensions };
    if dimensions != 0 && written > dimensions {
        return Err(subscript_error("wrong number of array subscripts"));
    }
    for dimension in 0..rank {
        let (given_lower, given_upper) = match subscripts.get(dimension) {
            Some(AssignmentSubscript::Element(position)) => (Some(1), Some(*position)),
            Some(AssignmentSubscript::Slice { lower, upper }) => (*lower, *upper),
            None => (None, None),
        };
        let (current_lower, current_upper) = if dimensions == 0 {
            (None, None)
        } else {
            (
                current.shape.lower_bound(dimension),
                current.shape.upper_bound(dimension),
            )
        };
        let (Some(slice_lower), Some(slice_upper)) =
            (given_lower.or(current_lower), given_upper.or(current_upper))
        else {
            crate::sql::eval::stash_diagnostic(
                Some(crate::util::StackStr::from_str(
                    "When assigning to a slice of an empty array value, slice boundaries must be fully specified.",
                )),
                None,
            );
            return Err(subscript_error(
                "array slice subscript must provide both boundaries",
            ));
        };
        if slice_upper < slice_lower {
            return Err(subscript_error(
                "upper bound cannot be less than lower bound",
            ));
        }
        lower[dimension] = slice_lower;
        upper[dimension] = slice_upper;
    }
    let mut new_lower = lower;
    let mut new_upper = upper;
    if dimensions == 1 {
        new_lower[0] = lower[0].min(current.shape.lower_bound(0).expect("one dimension"));
        new_upper[0] = upper[0].max(current.shape.upper_bound(0).expect("one dimension"));
    } else if dimensions > 1 {
        for dimension in 0..dimensions {
            let current_lower = current.shape.lower_bound(dimension).expect("in range");
            let current_upper = current.shape.upper_bound(dimension).expect("in range");
            if lower[dimension] < current_lower || upper[dimension] > current_upper {
                return Err(subscript_error("array subscript out of range"));
            }
            new_lower[dimension] = current_lower;
            new_upper[dimension] = current_upper;
        }
    }
    let slice_shape = shape_of(&lower[..rank], &upper[..rank])?;
    let source_shape = array::shape(source).expect("array datum invariant");
    if source_shape.element_count() < slice_shape.element_count() {
        return Err(subscript_error("source array too small"));
    }
    // The slice takes the source's leading elements, converted to the
    // array's element type.
    let replacement = array::alloc_items(arena, slice_shape.element_count())?;
    for (index, item) in replacement.iter_mut().enumerate() {
        let source_item = array::get(source, source_element, index).expect("source checked above");
        *item = if source_element == element || source_item.is_null() {
            source_item
        } else {
            crate::sql::eval::cast_to(source_item, element.to_coltype(), arena)?
        };
    }
    let shape = shape_of(&new_lower[..rank], &new_upper[..rank])?;
    let mut next = 0usize;
    build(element, shape, arena, |index| {
        let inside = index.iter().enumerate().all(|(dimension, position)| {
            *position >= lower[dimension] && *position <= upper[dimension]
        });
        if inside {
            next += 1;
            replacement[next - 1]
        } else {
            existing(&current, index)
        }
    })
}
