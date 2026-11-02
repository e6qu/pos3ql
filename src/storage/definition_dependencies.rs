//! New catalog dependencies cannot escape an admitted metadata publication.

use super::{
    RoutineResult, Storage, TableDefBuilder, TableTypeMembership, TypeCatalog, current_database,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

impl Storage {
    pub(super) fn require_type_dependency_access(
        &self,
        catalog: &TypeCatalog,
        result: RoutineResult,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let Some(name) = result.user_type else {
            return Ok(());
        };
        let source = match name.binding {
            super::type_references::TypeBinding::Captured(source) => source,
            super::type_references::TypeBinding::Polymorphic(_) => return Ok(()),
            super::type_references::TypeBinding::Stored(_)
            | super::type_references::TypeBinding::Declaration => {
                catalog
                    .resolve_type_reference(name, current_database(), transaction)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "new dependency names a missing type source"
                        )
                    })?
                    .identity
            }
        };
        catalog.require_definition_publication_access(self, source, transaction)
    }

    /// The caller retains the type catalog from binding through installation.
    /// Publication seals its complete dependency scan under that same owner.
    pub(super) fn require_definition_dependency_access(
        &self,
        catalog: &TypeCatalog,
        definition: &TableDefBuilder,
        transaction: u32,
        own_table: Option<usize>,
    ) -> Result<(), SqlError> {
        for column in definition.columns() {
            self.require_type_dependency_access(
                catalog,
                RoutineResult {
                    ctype: column.ctype,
                    user_type: column.user_type,
                },
                transaction,
            )?;
        }
        if let TableTypeMembership::Composite(reference) = definition.type_membership {
            let source = catalog
                .resolve_source_reference(reference, current_database(), transaction)?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "new typed-table dependency names a missing type source"
                    )
                })?;
            catalog.require_definition_publication_access(self, source.identity, transaction)?;
        }
        if definition.n_fkeys == 0 {
            return Ok(());
        }
        for (slot, table) in self.tables.iter().enumerate() {
            if Some(slot) == own_table {
                continue;
            }
            let state = table.definition.read();
            if state.identity.database != current_database()
                || !state.identity.existence.visible_to(transaction)
            {
                continue;
            }
            let (schema, name) = state
                .pending
                .filter(|head| head.transaction == transaction)
                .map_or((state.committed().schema, state.committed().name), |head| {
                    (head.schema, head.name)
                });
            if definition
                .fkeys()
                .iter()
                .any(|key| key.parent_schema == schema && key.parent == name)
                && let Some(owner) = state
                    .publication_owner()
                    .filter(|owner| *owner != transaction)
            {
                return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
            }
        }
        Ok(())
    }
}
