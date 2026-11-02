//! Exact foreign catalog images remain private until prepared resolution.

use super::*;
use foreign::{PreparedForeignChange, PreparedForeignImage};

macro_rules! prepared_image {
    ($method:ident, $catalog:ident, $definition:ty) => {
        pub(crate) fn $method(
            &self,
            slot: usize,
            created_at: u64,
            owner: Option<u16>,
            definition: Option<$definition>,
            transaction: u32,
        ) -> Result<PreparedForeignChange<$definition>, SqlError> {
            let change = self.foreign_catalog().$catalog(
                slot,
                PreparedForeignImage {
                    database: current_database(),
                    created_at,
                    owner,
                    definition,
                    transaction,
                },
            )?;
            self.catalog_sequence.observe(created_at);
            Ok(change)
        }
    };
}

impl Storage {
    prepared_image!(
        stage_prepared_foreign_wrapper,
        stage_prepared_wrapper,
        foreign::ForeignDataWrapperDefinition
    );
    prepared_image!(
        stage_prepared_foreign_server,
        stage_prepared_server,
        foreign::ForeignServerDefinition
    );
    prepared_image!(
        stage_prepared_foreign_mapping,
        stage_prepared_mapping,
        foreign::UserMappingDefinition
    );
    prepared_image!(
        stage_prepared_foreign_table,
        stage_prepared_table,
        foreign::ForeignTableDefinition
    );
}
