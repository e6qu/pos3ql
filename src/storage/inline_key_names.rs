//! Inline key names retain stable column numbers through renames and compaction.

use super::{DefinitionBacking, MAX_COLUMNS, Storage, TableDefinition};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

impl Storage {
    pub(crate) fn validate_inline_key_names<B: DefinitionBacking>(
        definition: &TableDefinition<B>,
    ) -> Result<(), SqlError> {
        let invalid = || {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored inline key names do not match the table's active keys"
            )
        };
        if definition.n_inline_key_names > MAX_COLUMNS
            || definition.n_inline_key_names > definition.inline_key_names.as_ref().len()
            || definition.n_inline_key_names
                != definition
                    .columns()
                    .iter()
                    .filter(|column| column.primary || column.unique)
                    .count()
        {
            return Err(invalid());
        }
        for (position, key) in definition.inline_key_names().iter().enumerate() {
            if key.name.as_str().is_empty()
                || !definition
                    .attribute_column(key.attribute)
                    .is_some_and(|column| {
                        let column = &definition.columns.as_ref()[column];
                        column.primary || column.unique
                    })
                || definition.inline_key_names()[..position]
                    .iter()
                    .any(|previous| {
                        previous.attribute == key.attribute || previous.name == key.name
                    })
                || definition
                    .uniques()
                    .iter()
                    .any(|constraint| constraint.name == key.name)
                || definition
                    .checks()
                    .iter()
                    .any(|constraint| constraint.name == key.name)
                || definition
                    .fkeys()
                    .iter()
                    .any(|constraint| constraint.name == key.name)
                || definition
                    .exclusions()
                    .iter()
                    .any(|constraint| constraint.name == key.name)
                || definition.not_null_names.as_ref()[..definition.n_columns]
                    .contains(&Some(key.name))
                || definition
                    .partition
                    .detached_bound
                    .is_some_and(|constraint| constraint.name == key.name)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::ColType;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{InlineKeyName, SqlName, TableDefBuilder, UniqueKey};

    fn named_keys() -> TableDefBuilder {
        let mut definition = make_def(
            "original",
            &[
                ("first", ColType::Int4, false),
                ("second", ColType::Int4, false),
            ],
        );
        definition.columns[0].unique = true;
        definition.columns[1].primary = true;
        definition
            .attributes
            .initialize(definition.n_columns)
            .unwrap();
        definition
            .set_column_key_name(0, SqlName::parse("original_first_key").unwrap())
            .unwrap();
        definition
            .set_column_key_name(1, SqlName::parse("original_pkey").unwrap())
            .unwrap();
        definition
    }

    #[test]
    fn inline_key_names_follow_stable_attributes_through_rename_compaction_and_transfer() {
        crate::mem::guard::forbid_alloc(|| {
            let original = named_keys();
            Storage::validate_inline_key_names(&original).unwrap();
            let retained = original.column_key_name(1).unwrap();
            let mut changed = original.clone();
            changed.name = SqlName::parse("renamed").unwrap();
            changed.columns[0] = original.columns[1];
            changed.columns[0].name = SqlName::parse("renamed_second").unwrap();
            changed.n_columns = 1;
            let mut mapping = [None; MAX_COLUMNS];
            mapping[1] = Some(changed.columns[0].name);
            changed.attributes = crate::storage::column_attributes::ColumnAttributes::rewrite(
                &original,
                changed.columns(),
                &mapping,
            )
            .unwrap();
            changed.retain_active_inline_key_names().unwrap();
            assert_eq!(
                changed.inline_key_names(),
                &[InlineKeyName {
                    attribute: 2,
                    name: retained
                }]
            );
            assert_eq!(changed.column_key_name(0), Some(retained));
            Storage::validate_inline_key_names(&changed).unwrap();
            assert_eq!(changed.take_column_key_name(0), Some(retained));
            changed.columns[0].primary = false;
            let mut named = UniqueKey::EMPTY;
            named.name = retained;
            named.n_cols = 1;
            changed.uniques[0] = named;
            changed.n_uniques = 1;
            assert_eq!(changed.n_inline_key_names, 0);
            Storage::validate_inline_key_names(&changed).unwrap();
        });
    }

    #[test]
    fn inline_key_name_validation_rejects_missing_duplicate_and_non_key_attributes() {
        crate::mem::guard::forbid_alloc(|| {
            for fault in 0..6 {
                let mut definition = named_keys();
                match fault {
                    0 => definition.n_inline_key_names = 1,
                    1 => definition.inline_key_names[0].name = SqlName::EMPTY,
                    2 => definition.inline_key_names[1].attribute = 1,
                    3 => definition.inline_key_names[1].name = definition.inline_key_names[0].name,
                    4 => definition.inline_key_names[0].attribute = 3,
                    _ => definition.n_inline_key_names = MAX_COLUMNS + 1,
                }
                assert_eq!(
                    Storage::validate_inline_key_names(&definition)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
        });
    }

    #[test]
    fn invalid_inline_key_names_do_not_reserve_catalog_identities() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut definition = named_keys();
            definition.inline_key_names[1].name = definition.inline_key_names[0].name;
            let sequence = storage.catalog_identity_sequence();
            assert_eq!(
                storage
                    .reserve_table_catalog_identities(&mut definition)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), sequence);
        });
    }
}
