//! Route once, then run leaf work with the definition that owns its bytes.

use super::{MAX_COLUMNS, SqlError, Storage, TableDef};
use crate::mem::arena::Arena;
use crate::sql::types::Datum;
use crate::storage::RelationColumnMap;

pub(super) fn with_partition_insert_row<'arena, R>(
    storage: &mut Storage,
    logical: usize,
    definition: &TableDef,
    values: &mut [Datum<'arena>; MAX_COLUMNS],
    txid: u32,
    arena: &'arena Arena,
    operation: impl FnOnce(
        &mut Storage,
        usize,
        &TableDef,
        &mut [Datum<'arena>; MAX_COLUMNS],
        Option<RelationColumnMap<'arena>>,
    ) -> R,
) -> Result<R, SqlError> {
    let target =
        storage.partition_target(logical, definition, &values[..definition.n_columns], txid)?;
    if target == logical {
        clear_generated(definition, values);
        return Ok(operation(storage, target, definition, values, None));
    }
    storage.lock_table(
        txid,
        target,
        crate::sql::ast::TableLockMode::RowExclusive,
        false,
    )?;
    let definitions = storage.table_definition_images();
    let physical_definition = definitions.definition(storage, target, txid)?;
    if RelationColumnMap::same_order(definition, physical_definition) {
        clear_generated(physical_definition, values);
        return Ok(operation(
            storage,
            target,
            physical_definition,
            values,
            None,
        ));
    }
    let mapping = RelationColumnMap::new(definition, physical_definition, arena)?;
    let mut physical = [Datum::Null; MAX_COLUMNS];
    mapping.overlay(values, &mut physical);
    clear_generated(physical_definition, &mut physical);
    let result = operation(
        storage,
        target,
        physical_definition,
        &mut physical,
        Some(mapping),
    );
    mapping.project(&physical, values);
    Ok(result)
}

pub(super) fn logical_insert_values<'row, 'arena: 'row>(
    mapping: Option<RelationColumnMap<'arena>>,
    width: usize,
    physical: &'row [Datum<'arena>],
    arena: &'arena Arena,
) -> Result<&'row [Datum<'arena>], SqlError> {
    let Some(mapping) = mapping else {
        return Ok(&physical[..width]);
    };
    let logical = arena
        .alloc_slice_with(width, |_| Datum::Null)
        .map_err(|_| super::arena_full())?;
    mapping.project(physical, logical);
    Ok(logical)
}

pub(super) fn logical_insert_row_bytes<'arena>(
    mapping: Option<RelationColumnMap<'arena>>,
    width: usize,
    schema: &[crate::sql::types::ColType],
    bytes: &'arena [u8],
    arena: &'arena Arena,
) -> Result<&'arena [u8], SqlError> {
    if mapping.is_none() {
        return Ok(bytes);
    }
    let mut physical = [Datum::Null; MAX_COLUMNS];
    crate::storage::rowenc::decode(bytes, schema, &mut physical)?;
    let logical = logical_insert_values(mapping, width, &physical[..schema.len()], arena)?;
    let encoded = arena
        .alloc_slice_with(crate::storage::rowenc::encoded_len(logical), |_| 0u8)
        .map_err(|_| super::arena_full())?;
    crate::storage::rowenc::encode(logical, encoded);
    Ok(encoded)
}

// BEFORE INSERT sees the input row; only the destination table's generation
// expressions run after its triggers have supplied their final base values.
fn clear_generated(definition: &TableDef, values: &mut [Datum]) {
    for (column, value) in definition.columns().iter().zip(values) {
        if column.default.is_generated() {
            *value = Datum::Null;
        }
    }
}
