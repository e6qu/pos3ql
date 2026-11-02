//! Restored prepared object definitions reserve their final private owners.

use super::*;
use crate::sql::txn::DdlUndo;

impl Storage {
    pub(crate) fn restore_prepared_access_method(
        &self,
        identity: CatalogPosition,
        definition: AccessMethodDefinition,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        CatalogIdentity::require(identity.created_at())?;
        if matches!(
            definition.name.as_str(),
            "heap" | "btree" | "hash" | "gist" | "gin" | "brin" | "spgist"
        ) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared access method shadows a builtin"
            ));
        }
        let mut catalog = self
            .operator_catalog
            .lock()
            .expect("operator catalog lock poisoned");
        let slot = identity.slot();
        if slot >= catalog.access_methods.len()
            || catalog.access_methods[slot].ddl_state != CatalogDdlState::Absent
            || catalog.access_methods.iter().any(|entry| {
                entry.database == current_database()
                    && entry.ddl_state != CatalogDdlState::Absent
                    && (entry.created_at == identity.created_at()
                        || entry.definition.name == definition.name
                            && entry.ddl_state.visible_to(transaction))
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared access method identity is unavailable"
            ));
        }
        catalog.access_methods[slot] = AccessMethodDef {
            database: current_database(),
            created_at: identity.created_at(),
            definition,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        self.catalog_sequence.observe(identity.created_at());
        Ok(slot)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "prepared extension image preserves all durable identity fields"
    )]
    pub(crate) fn restore_prepared_extension(
        &self,
        name: SqlName,
        namespace: usize,
        relocatable: bool,
        version: ExtensionVersion,
        owner: usize,
        created_at: u64,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        CatalogIdentity::require(created_at)?;
        if namespace >= self.schema_count()
            || !self
                .schema_def_for(namespace, transaction)
                .visible_to(transaction)
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared extension namespace is unavailable"
            ));
        }
        self.role_reference_ready(owner, transaction)?;
        let mut catalog = self.extension_catalog();
        if let Some(slot) = catalog.definitions.iter().position(|extension| {
            extension.database == current_database()
                && extension.visible_to(transaction)
                && extension.name == name
        }) {
            let selected = catalog.definitions[slot];
            if selected.created_at != created_at
                || selected.pending_owner_other_than(transaction).is_some()
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared extension selects another lifetime"
                ));
            }
            catalog.definitions[slot].pending = Some(PendingExtensionDefinition {
                txid: transaction,
                namespace: namespace as u16,
                relocatable,
                version,
            });
            // Existing extension ownership has its own SetObjectOwner record.
            return Ok(DdlUndo::ExtensionAltered {
                slot: slot as u32,
                prior: selected.pending,
            });
        }
        if catalog.definitions.iter().any(|extension| {
            extension.database == current_database()
                && extension.ddl_state != CatalogDdlState::Absent
                && (extension.created_at == created_at
                    || extension.name == name
                        && extension.pending_owner_other_than(transaction).is_some())
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared extension creation identity is duplicated"
            ));
        }
        let slot = catalog
            .definitions
            .iter()
            .position(|extension| extension.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "prepared extension catalog capacity exhausted"
                )
            })?;
        catalog.definitions[slot] = ExtensionDef {
            database: current_database(),
            created_at,
            name,
            namespace: namespace as u16,
            relocatable,
            version,
            ownership: Ownership {
                owner: owner as u16,
                pending: None,
            },
            pending: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        self.catalog_sequence.observe(created_at);
        Ok(DdlUndo::ExtensionCreated(slot as u32))
    }

    pub(crate) fn stage_prepared_extension_config(
        &self,
        extension: usize,
        relation: ExtensionConfigRelation,
        condition: ExtensionConfigCondition,
        exists: bool,
        ordinal: u16,
        transaction: u32,
    ) -> Result<(usize, Option<PendingExtensionConfig>), SqlError> {
        self.change_extension_config_with_ordinal(
            extension,
            relation,
            condition,
            exists,
            Some(ordinal),
            transaction,
        )
    }
}
