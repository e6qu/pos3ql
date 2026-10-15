//! Column names connect a logical relation to a child's stored row layout.

use super::{ColumnSet, TableDef};
use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;

#[derive(Clone, Copy)]
pub(crate) struct RelationColumnMap<'arena> {
    columns: &'arena [u16],
}

impl<'arena> RelationColumnMap<'arena> {
    pub(crate) fn same_order(logical: &TableDef, physical: &TableDef) -> bool {
        logical.n_columns == physical.n_columns
            && logical
                .columns()
                .iter()
                .zip(physical.columns())
                .all(|(left, right)| {
                    left.name == right.name
                        && left.ctype == right.ctype
                        && left.type_mod == right.type_mod
                        && left.collation == right.collation
                        && left.user_type == right.user_type
                })
    }

    pub(crate) fn new(
        logical: &TableDef,
        physical: &TableDef,
        arena: &'arena Arena,
    ) -> Result<Self, SqlError> {
        let columns = arena
            .alloc_slice_with(logical.n_columns, |_| 0u16)
            .map_err(|_| arena_full())?;
        for (column, expected) in logical.columns().iter().enumerate() {
            let target = physical_column(expected, physical)?;
            columns[column] = target as u16;
        }
        Ok(Self { columns })
    }

    pub(crate) fn physical_column(self, logical: usize) -> Result<usize, SqlError> {
        self.columns
            .get(logical)
            .copied()
            .map(usize::from)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "logical column is outside its retained mapping"
                )
            })
    }

    pub(crate) fn project<'values>(
        self,
        physical: &[Datum<'values>],
        logical: &mut [Datum<'values>],
    ) {
        for (column, &source) in self.columns.iter().enumerate() {
            logical[column] = physical[usize::from(source)];
        }
    }

    pub(crate) fn overlay<'values>(
        self,
        logical: &[Datum<'values>],
        physical: &mut [Datum<'values>],
    ) {
        for (column, &target) in self.columns.iter().enumerate() {
            physical[usize::from(target)] = logical[column];
        }
    }

    pub(crate) fn column_set(self, logical: ColumnSet) -> ColumnSet {
        let mut physical = ColumnSet::EMPTY;
        for (column, &target) in self.columns.iter().enumerate() {
            if logical.contains(column) {
                physical.insert(usize::from(target));
            }
        }
        physical
    }
}

fn physical_column(expected: &super::ColumnMeta, physical: &TableDef) -> Result<usize, SqlError> {
    let target = physical
        .column_index(expected.name.as_str())
        .ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "selected relation is missing column \"{}\"",
                expected.name.as_str()
            )
        })?;
    let actual = &physical.columns()[target];
    if expected.ctype != actual.ctype
        || expected.type_mod != actual.type_mod
        || expected.collation != actual.collation
        || expected.user_type != actual.user_type
    {
        return Err(sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "selected relation has a different type for column \"{}\"",
            expected.name.as_str()
        ));
    }
    Ok(target)
}

/// A stack-owned decoder projects stored bytes into a constraint's logical
/// column order. No row value or metadata guard outlives the byte callback.
pub(crate) struct RelationRowDecoder {
    schema: [crate::sql::types::ColType; super::MAX_COLUMNS],
    columns: [u16; super::MAX_COLUMNS],
    logical_width: usize,
    physical_width: usize,
    same_order: bool,
}

impl RelationRowDecoder {
    pub(crate) fn new(logical: &TableDef, physical: &TableDef) -> Result<Self, SqlError> {
        let mut decoder = Self {
            schema: [crate::sql::types::ColType::Bool; super::MAX_COLUMNS],
            columns: [0; super::MAX_COLUMNS],
            logical_width: logical.n_columns,
            physical_width: physical.n_columns,
            same_order: logical.n_columns == physical.n_columns,
        };
        physical.schema(&mut decoder.schema);
        for (column, expected) in logical.columns().iter().enumerate() {
            let position = physical_column(expected, physical)?;
            decoder.columns[column] = position as u16;
            decoder.same_order &= column == position;
        }
        Ok(decoder)
    }

    pub(crate) fn decode<'bytes>(
        &self,
        bytes: &'bytes [u8],
        output: &mut [Datum<'bytes>],
    ) -> Result<(), SqlError> {
        if output.len() < self.logical_width {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "logical row decoder output is too short"
            ));
        }
        let schema = &self.schema[..self.physical_width];
        if self.same_order {
            super::rowenc::decode(bytes, schema, output)?;
        } else {
            let mut physical = [Datum::Null; super::MAX_COLUMNS];
            super::rowenc::decode(bytes, schema, &mut physical)?;
            for (column, &source) in self.columns[..self.logical_width].iter().enumerate() {
                output[column] = physical[usize::from(source)];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::sql::types::ColType;
    use crate::storage::{ColumnMeta, SqlName};

    #[test]
    fn relation_column_maps_keep_extra_values_and_reject_missing_or_changed_columns() {
        let mut logical = TableDef::empty();
        logical.n_columns = 2;
        logical.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            ..ColumnMeta::EMPTY
        };
        logical.columns[1] = ColumnMeta {
            name: SqlName::parse("label").unwrap(),
            ctype: ColType::Text,
            ..ColumnMeta::EMPTY
        };
        let mut physical = TableDef::empty();
        physical.n_columns = 3;
        physical.columns[0] = ColumnMeta {
            name: SqlName::parse("extra").unwrap(),
            ctype: ColType::Bool,
            ..ColumnMeta::EMPTY
        };
        physical.columns[1] = logical.columns[1];
        physical.columns[2] = logical.columns[0];
        let mut budget = Budget::new(129);
        let arena = Arena::new(&mut budget, "column_map", 128).unwrap();
        let short = Arena::new(&mut budget, "short_column_map", 1).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mapping = RelationColumnMap::new(&logical, &physical, &arena).unwrap();
            let mut stored = [Datum::Bool(true), Datum::Text("original"), Datum::Int4(7)];
            let mut selected = [Datum::Null; 2];
            mapping.project(&stored, &mut selected);
            let decoder = RelationRowDecoder::new(&logical, &physical).unwrap();
            let mut encoded = [0u8; 128];
            let width = super::super::rowenc::encoded_len(&stored);
            super::super::rowenc::encode(&stored, &mut encoded[..width]);
            let mut decoded = [Datum::Null; 2];
            decoder.decode(&encoded[..width], &mut decoded).unwrap();
            assert_eq!(decoded, selected);
            let error = decoder
                .decode(&encoded[..width], &mut decoded[..1])
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert_eq!(selected, [Datum::Int4(7), Datum::Text("original")]);
            mapping.overlay(&[Datum::Int4(8), Datum::Text("changed")], &mut stored);
            assert_eq!(
                stored,
                [Datum::Bool(true), Datum::Text("changed"), Datum::Int4(8)]
            );
            let mut updated = ColumnSet::EMPTY;
            updated.insert(0);
            let columns = mapping.column_set(updated);
            assert!(!columns.contains(0));
            assert!(!columns.contains(1));
            assert!(columns.contains(2));
            let error = RelationColumnMap::new(&logical, &physical, &short)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            physical.columns[2].name = SqlName::parse("other").unwrap();
            let error = RelationColumnMap::new(&logical, &physical, &arena)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            physical.columns[2] = logical.columns[0];
            physical.columns[2].ctype = ColType::Int8;
            let error = RelationColumnMap::new(&logical, &physical, &arena)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            physical.columns[2] = logical.columns[0];
            assert!(RelationColumnMap::new(&logical, &physical, &arena).is_ok());
        });
    }
}
