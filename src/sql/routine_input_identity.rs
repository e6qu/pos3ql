//! Exact PostgreSQL function-cache specialization, independent of caller plans.

use super::{
    ast::Collation,
    eval::{SqlError, sqlstate},
};
use crate::{
    mem::arena::Arena,
    sql_err,
    storage::{RoutineDef, RoutineKind, Storage},
};

pub(super) fn canonical<'a>(
    storage: &Storage,
    routine: &RoutineDef,
    input_collation: Collation,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let mut length = 0usize;
    encode(storage, routine, input_collation, txid, &mut |bytes| {
        length = length
            .checked_add(bytes.len())
            .ok_or_else(super::eval::arena_full)?;
        Ok(())
    })?;
    let bytes = arena
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| super::eval::arena_full())?;
    let mut offset = 0;
    encode(storage, routine, input_collation, txid, &mut |part| {
        let end = offset + part.len();
        bytes[offset..end].copy_from_slice(part);
        offset = end;
        Ok(())
    })?;
    Ok(bytes)
}

fn encode(
    storage: &Storage,
    routine: &RoutineDef,
    input_collation: Collation,
    txid: u32,
    emit: &mut impl FnMut(&[u8]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let type_oid = |ctype, user_type| {
        storage
            .routine_type_oid(ctype, user_type, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SQL routine specialization type is unavailable"
                )
            })
    };
    emit(&input_collation.oid().to_le_bytes())?;
    emit(&(routine.argument_count as u32).to_le_bytes())?;
    // RoutineCallContract binds polymorphic and RECORD inputs before admission.
    for argument in routine.arguments() {
        emit(&type_oid(argument.ctype, argument.user_type)?.to_le_bytes())?;
    }
    let composite = match routine.kind {
        RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
            if let super::types::ColType::Composite(Some(creation)) = result.ctype {
                let target = crate::storage::TypeDefinitionTarget::by_creation(
                    storage,
                    crate::storage::NamedTypeKind::Composite,
                    creation,
                )?;
                Some(
                    storage
                        .read_composite_definition(target, txid)?
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "SQL routine result type is unavailable"
                            )
                        })?,
                )
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(composite) = composite {
        emit(&[1])?;
        emit(&super::types::oid::composite_oid(composite.creation()).to_le_bytes())?;
        emit(&(composite.n_fields as u32).to_le_bytes())?;
        for field in composite.fields() {
            emit(&(field.name.as_str().len() as u32).to_le_bytes())?;
            emit(field.name.as_str().as_bytes())?;
            emit(&type_oid(field.ctype, field.user_type)?.to_le_bytes())?;
            emit(&field.type_mod.to_le_bytes())?;
            emit(&field.collation.oid().to_le_bytes())?;
            emit(&[u8::from(field.dropped)])?;
        }
    } else {
        emit(&[u8::from(routine.result_column_count != 0)])?;
        if routine.result_column_count != 0 {
            emit(&super::types::oid::RECORD.to_le_bytes())?;
        }
        emit(&(routine.result_column_count as u32).to_le_bytes())?;
        for field in &routine.result_columns[..routine.result_column_count] {
            emit(&(field.name.as_str().len() as u32).to_le_bytes())?;
            emit(field.name.as_str().as_bytes())?;
            emit(&type_oid(field.ctype, field.user_type)?.to_le_bytes())?;
        }
    }
    Ok(())
}
