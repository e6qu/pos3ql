//! Assignment through composite fields and array elements (`UPDATE t SET
//! p.x = 1`, `a[2].y = 'q'`, `n.p.x = 3`), with PostgreSQL's FieldStore and
//! subscripting semantics: a NULL composite first becomes a row of NULL
//! fields, the other fields keep their values, and an element assignment
//! follows `array_set_element`.

use crate::mem::arena::Arena;
use crate::sql::array_assignment::AssignmentSubscript;
use crate::sql::eval::{CatalogAccess, SqlError, sqlstate};
use crate::sql::types::{ColType, Datum, oid};
use crate::sql_err;

/// One step of an assignment target after its column.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step<'s> {
    Field(&'s str),
    Subscript(AssignmentSubscript),
}

/// `base<steps> := value`. `base_type` is the base's type OID, which a NULL
/// base needs; each step's value takes the type of the field or element it
/// replaces.
pub(crate) fn assign<'a>(
    base: Datum<'a>,
    base_type: Option<i32>,
    steps: &[Step<'_>],
    value: Datum<'a>,
    catalog: Option<&dyn CatalogAccess>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let Some(step) = steps.first() else {
        return coerce(value, base_type, arena);
    };
    match *step {
        Step::Field(name) => {
            let base = match base {
                Datum::CompositeText {
                    slot,
                    physical_fields,
                    text,
                } => catalog
                    .ok_or_else(|| not_composite(name))?
                    .materialize_composite(slot, physical_fields, text, arena)?,
                base => base,
            };
            // The row is rebuilt as a record: the target's assignment
            // coercion converts it, checking every field's domain.
            let fields = match base {
                Datum::Record(fields) | Datum::Composite { fields, .. } => fields,
                Datum::Null => base_type
                    .and_then(|type_oid| {
                        catalog.map(|catalog| catalog.null_composite_fields(type_oid, arena))
                    })
                    .transpose()?
                    .flatten()
                    .ok_or_else(|| not_composite(name))?,
                _ => return Err(not_composite(name)),
            };
            let index = fields
                .iter()
                .position(|field| field.name == name)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "cannot assign to field \"{}\" because there is no such column",
                        name
                    )
                })?;
            let updated = assign(
                fields[index].value,
                Some(fields[index].type_oid),
                &steps[1..],
                value,
                catalog,
                arena,
            )?;
            let output = arena
                .alloc_slice_copy(fields)
                .map_err(|_| crate::sql::eval::arena_full())?;
            output[index].value = updated;
            Ok(Datum::Record(output))
        }
        Step::Subscript(_) => {
            let count = steps
                .iter()
                .take_while(|step| matches!(step, Step::Subscript(_)))
                .count();
            let mut subscripts = [AssignmentSubscript::Element(0); 6];
            if count > subscripts.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "number of array dimensions ({}) exceeds the maximum allowed (6)",
                    count
                ));
            }
            for (slot, step) in subscripts.iter_mut().zip(&steps[..count]) {
                let Step::Subscript(subscript) = *step else {
                    unreachable!("counted subscript steps")
                };
                *slot = subscript;
            }
            let subscripts = &subscripts[..count];
            let element_type = base_type.and_then(element_type_oid);
            let mut updated = if subscripts
                .iter()
                .any(|subscript| matches!(subscript, AssignmentSubscript::Slice { .. }))
            {
                // A slice takes an array of the target's type.
                coerce(value, base_type, arena)?
            } else {
                let mut indexes = [0i32; 6];
                for (index, subscript) in indexes.iter_mut().zip(subscripts) {
                    if let AssignmentSubscript::Element(element) = subscript {
                        *index = *element;
                    }
                }
                assign(
                    element_at(base, &indexes[..count]),
                    element_type,
                    &steps[count..],
                    value,
                    catalog,
                    arena,
                )?
            };
            // An array stores a named composite member in its durable form.
            if let Some((crate::storage::NamedTypeKind::Composite, slot, false)) =
                element_type.and_then(crate::storage::NamedTypeKind::from_oid)
                && matches!(updated, Datum::Record(_) | Datum::Composite { .. })
            {
                updated = catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "named composite catalog access is unavailable"
                        )
                    })?
                    .composite_array_element(updated, slot, arena)?;
            }
            crate::sql::array_assignment::assign(base, subscripts, updated, arena)
        }
    }
}

fn not_composite(field: &str) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "cannot assign to field \"{}\" because its type is not a composite type",
        field
    )
}

/// The element `indexes` name, or NULL when the array lacks it.
fn element_at<'a>(base: Datum<'a>, indexes: &[i32]) -> Datum<'a> {
    let Datum::Array { element, raw } = base else {
        return Datum::Null;
    };
    let Some(shape) = crate::sql::array::shape(raw) else {
        return Datum::Null;
    };
    if shape.dimension_count() != indexes.len() {
        return Datum::Null;
    }
    let mut flat = 0usize;
    for (dimension, index) in indexes.iter().enumerate() {
        let (Some(lower), Some(upper), Some(length)) = (
            shape.lower_bound(dimension),
            shape.upper_bound(dimension),
            shape.dimension(dimension),
        ) else {
            return Datum::Null;
        };
        if *index < lower || *index > upper {
            return Datum::Null;
        }
        flat = flat * length + (i64::from(*index) - i64::from(lower)) as usize;
    }
    crate::sql::array::get(raw, element, flat).unwrap_or(Datum::Null)
}

/// The element type of an array type.
pub(crate) fn element_type_oid(array_oid: i32) -> Option<i32> {
    use crate::storage::NamedTypeKind;
    if let Some(ColType::Array(element)) = ColType::from_oid(array_oid) {
        return Some(element.element_oid());
    }
    if let Some((kind, creation, true)) = NamedTypeKind::from_oid(array_oid) {
        return Some(match kind {
            NamedTypeKind::Domain => oid::domain_oid(creation),
            NamedTypeKind::Enum => oid::enum_oid(creation),
            NamedTypeKind::Composite => oid::composite_oid(creation),
        });
    }
    None
}

/// The assigned value in the target's type, as assignment coercion converts
/// it. A user-defined target takes the value as its own column coercion
/// converts it.
fn coerce<'a>(
    value: Datum<'a>,
    target: Option<i32>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match target.and_then(ColType::from_oid) {
        Some(ctype) if !value.is_null() => crate::sql::eval::cast_to(value, ctype, arena),
        _ => Ok(value),
    }
}
