//! Partition keys are resolved against the definition that owns the values.

use super::{
    DefinitionBacking, MAX_PARTITION_KEYS, PartitionBound, PartitionScheme, TableDefinition,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;

pub(crate) struct PartitionRow<'row, 'value, B: DefinitionBacking> {
    definition: &'row TableDefinition<B>,
    values: &'row [Datum<'value>],
}

#[derive(Clone, Copy)]
pub(crate) struct PartitionKeyRow<'row, 'value> {
    scheme: PartitionScheme,
    values: &'row [Datum<'value>],
}

impl<B: DefinitionBacking> Copy for PartitionRow<'_, '_, B> {}

impl<B: DefinitionBacking> Clone for PartitionRow<'_, '_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'row, 'value, B: DefinitionBacking> PartitionRow<'row, 'value, B> {
    pub(crate) fn new(
        definition: &'row TableDefinition<B>,
        values: &'row [Datum<'value>],
    ) -> Result<Self, SqlError> {
        if values.len() != definition.n_columns {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "partition row width does not match its definition"
            ));
        }
        Ok(Self { definition, values })
    }

    pub(crate) fn keys(
        self,
        parent: &TableDefinition<impl DefinitionBacking>,
        mut scheme: PartitionScheme,
    ) -> Result<PartitionKeyRow<'row, 'value>, SqlError> {
        if usize::from(scheme.n_keys) > MAX_PARTITION_KEYS {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "partition key count exceeds its stored capacity"
            ));
        }
        for key in &mut scheme.keys[..usize::from(scheme.n_keys)] {
            let column = parent.columns().get(usize::from(*key)).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "partition key is outside its table definition"
                )
            })?;
            let index = self
                .definition
                .column_index(column.name.as_str())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "partition row is missing column \"{}\"",
                        column.name.as_str()
                    )
                })?;
            let actual = &self.definition.columns()[index];
            if column.ctype != actual.ctype
                || column.type_mod != actual.type_mod
                || column.collation != actual.collation
                || column.user_type != actual.user_type
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "partition row has a different type for column \"{}\"",
                    column.name.as_str()
                ));
            }
            *key = index as u16;
        }
        Ok(PartitionKeyRow {
            scheme,
            values: self.values,
        })
    }
}

impl PartitionKeyRow<'_, '_> {
    /// PostgreSQL's `ExecBuildSlotPartitionKeyDescription` detail, naming the
    /// keys by `parent`'s columns.
    pub(crate) fn describe(
        self,
        parent: &TableDefinition<impl DefinitionBacking>,
    ) -> crate::util::StackStr<1024> {
        use core::fmt::Write;
        let mut detail =
            crate::util::StackStr::<1024>::from_str("Partition key of the failing row contains (");
        let keys = &self.scheme.keys[..usize::from(self.scheme.n_keys)];
        let names = parent
            .partition
            .scheme
            .expect("a routing parent is partitioned")
            .keys;
        for (index, &name) in names[..keys.len()].iter().enumerate() {
            let _ = detail.write_str(if index == 0 { "" } else { ", " });
            let _ = detail.write_str(parent.columns()[usize::from(name)].name.as_str());
        }
        let _ = detail.write_str(") = (");
        for (index, &key) in keys.iter().enumerate() {
            let _ = detail.write_str(if index == 0 { "" } else { ", " });
            match self.values[usize::from(key)] {
                Datum::Null => {
                    let _ = detail.write_str("null");
                }
                value => {
                    let _ = write!(detail, "{value}");
                }
            }
        }
        let _ = detail.write_str(").");
        detail
    }

    pub(crate) fn all_known(self, known: &[bool]) -> bool {
        self.scheme.keys[..usize::from(self.scheme.n_keys)]
            .iter()
            .all(|key| known.get(usize::from(*key)).copied().unwrap_or(false))
    }

    pub(crate) fn matches(self, bound: PartitionBound) -> Result<bool, SqlError> {
        super::partition_bound_matches(
            self.scheme.strategy,
            self.scheme.keys,
            self.scheme.n_keys,
            bound,
            self.values,
        )
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::ColType;
    use crate::storage::{
        ColumnMeta, MAX_PARTITION_LIST_VALUES, OwnedDatum, PartitionStrategy, SqlName,
        TableDefBuilder,
    };

    #[test]
    fn partition_key_rows_match_and_prune_using_the_values_own_column_order() {
        let mut parent = TableDefBuilder::empty();
        parent.n_columns = 2;
        parent.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            ..ColumnMeta::EMPTY
        };
        parent.columns[1] = ColumnMeta {
            name: SqlName::parse("label").unwrap(),
            ctype: ColType::Text,
            ..ColumnMeta::EMPTY
        };
        let mut child = TableDefBuilder::empty();
        child.n_columns = 2;
        child.columns[0] = parent.columns[1];
        child.columns[1] = parent.columns[0];
        let mut list = [OwnedDatum::Null; MAX_PARTITION_LIST_VALUES];
        list[0] = OwnedDatum::Int4(7);
        let bound = PartitionBound::List {
            values: list,
            n_values: 1,
        };
        let scheme = PartitionScheme {
            strategy: PartitionStrategy::List,
            keys: [0; MAX_PARTITION_KEYS],
            n_keys: 1,
        };
        crate::mem::guard::forbid_alloc(|| {
            let values = [Datum::Text("kept"), Datum::Int4(7)];
            let row = PartitionRow::new(&child, &values).unwrap();
            let keys = row.keys(&parent, scheme).unwrap();
            assert!(keys.matches(bound).unwrap());
            assert!(!keys.all_known(&[true, false]));
            assert!(keys.all_known(&[false, true]));
            let other = [Datum::Text("kept"), Datum::Int4(8)];
            assert!(
                !PartitionRow::new(&child, &other)
                    .unwrap()
                    .keys(&parent, scheme)
                    .unwrap()
                    .matches(bound)
                    .unwrap()
            );
            let error = PartitionRow::new(&child, &values[..1]).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            let mut invalid = scheme;
            invalid.keys[0] = 2;
            let error = row.keys(&parent, invalid).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            child.columns[1].name = SqlName::parse("other").unwrap();
            let error = PartitionRow::new(&child, &values)
                .unwrap()
                .keys(&parent, scheme)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
        });
    }
}
