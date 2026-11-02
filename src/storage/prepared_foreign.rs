//! Exact foreign catalog images remain private until prepared resolution.

use super::*;
use foreign::{PreparedForeignChange, PreparedForeignImage};

macro_rules! prepared_image {
    ($method:ident, $catalog:ident, $definition:ty, $validate:expr) => {
        pub(crate) fn $method(
            &self,
            slot: usize,
            created_at: u64,
            owner: Option<u16>,
            definition: Option<$definition>,
            transaction: u32,
            namespace: &[foreign::PreparedForeignNamespace],
        ) -> Result<PreparedForeignChange<$definition>, SqlError> {
            if let Some(definition) = definition {
                if let Some(owner) = owner {
                    self.role_reference_ready(usize::from(owner), transaction)?;
                }
                ($validate)(self, definition, transaction)?;
            }
            let change = self.foreign_catalog().$catalog(
                slot,
                PreparedForeignImage {
                    database: current_database(),
                    created_at,
                    owner,
                    definition,
                    transaction,
                },
                namespace,
            )?;
            self.catalog_sequence.observe(created_at);
            Ok(change)
        }
    };
}

impl Storage {
    pub(crate) fn prepare_foreign_namespace(
        &self,
        capacity: usize,
    ) -> Result<crate::mem::lease_pool::MutableSlice<foreign::PreparedForeignNamespace>, SqlError>
    {
        self.metadata_pool
            .lease(capacity)
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
    }

    prepared_image!(
        stage_prepared_foreign_wrapper,
        stage_prepared_wrapper,
        foreign::ForeignDataWrapperDefinition,
        |_storage: &Storage,
         _definition: foreign::ForeignDataWrapperDefinition,
         _transaction: u32|
         -> Result<(), SqlError> { Ok(()) }
    );
    prepared_image!(
        stage_prepared_foreign_server,
        stage_prepared_server,
        foreign::ForeignServerDefinition,
        |_storage: &Storage,
         _definition: foreign::ForeignServerDefinition,
         _transaction: u32|
         -> Result<(), SqlError> { Ok(()) }
    );
    prepared_image!(
        stage_prepared_foreign_mapping,
        stage_prepared_mapping,
        foreign::UserMappingDefinition,
        |storage: &Storage,
         definition: foreign::UserMappingDefinition,
         transaction: u32|
         -> Result<(), SqlError> {
            if let foreign::ForeignMappingUser::Role(role) = definition.user {
                storage.role_reference_ready(usize::from(role), transaction)?;
            }
            Ok(())
        }
    );
    prepared_image!(
        stage_prepared_foreign_table,
        stage_prepared_table,
        foreign::ForeignTableDefinition,
        |storage: &Storage,
         definition: foreign::ForeignTableDefinition,
         transaction: u32|
         -> Result<(), SqlError> {
            if !storage
                .tables
                .get(usize::from(definition.table))
                .is_some_and(|table| {
                    table.database() == current_database() && table.visible_to(transaction)
                })
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared foreign table relation is unavailable"
                ));
            }
            Ok(())
        }
    );
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    #[test]
    fn prepared_foreign_images_validate_roles_and_physical_relations_before_mutation() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let wrapper = foreign::ForeignDataWrapperDefinition {
            name: SqlName::parse("missing_owner").unwrap(),
            handler: foreign::ForeignDataHandler::None,
            validator: foreign::ForeignDataValidator::None,
            options: foreign::ForeignOptions::EMPTY,
        };
        crate::mem::guard::forbid_alloc(|| {
            assert!(
                storage
                    .stage_prepared_foreign_wrapper(0, 41, Some(u16::MAX), Some(wrapper), 7, &[])
                    .is_err()
            );
            assert_eq!(
                storage.foreign_wrapper_entry(0).ddl_state,
                CatalogDdlState::Absent
            );
            let mapping = foreign::UserMappingDefinition {
                server: 0,
                user: foreign::ForeignMappingUser::Role(u16::MAX),
                options: foreign::ForeignOptions::EMPTY,
            };
            assert!(
                storage
                    .stage_prepared_foreign_mapping(0, 42, None, Some(mapping), 7, &[])
                    .is_err()
            );
            assert_eq!(
                storage.foreign_mapping_entry(0).ddl_state,
                CatalogDdlState::Absent
            );
            let table = foreign::ForeignTableDefinition {
                table: u16::MAX,
                server: 0,
                options: foreign::ForeignOptions::EMPTY,
                column_options: foreign::ForeignColumnOptions::EMPTY,
            };
            assert!(
                storage
                    .stage_prepared_foreign_table(0, 43, None, Some(table), 7, &[])
                    .is_err()
            );
            assert_eq!(
                storage.foreign_table_entry(0).ddl_state,
                CatalogDdlState::Absent
            );
        });
    }
}
