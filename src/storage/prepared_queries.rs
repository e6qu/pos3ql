//! Saved query declarations retain their recorded identities while still private.

use super::catalog_sources::{CatalogDdlOperation, CatalogObjectSlot};
use super::*;

impl Storage {
    pub(crate) fn restore_prepared_view(
        &self,
        identity: ViewCatalogIdentity,
        schema: SqlName,
        name: SqlName,
        definition: ViewDefinition,
        transaction: u32,
    ) -> Result<ViewPublication, SqlError> {
        validate_stored_search_path(&definition.query.creation_path)?;
        let slot = identity.slot();
        let rule_slot = identity.return_rule_slot();
        if slot >= self.view_count() || rule_slot >= self.rule_count() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared view position is out of range"
            ));
        }
        let selected = self.view(slot);
        if selected.ddl_state != CatalogDdlState::Absent {
            if selected.database != current_database()
                || !selected.visible_to(transaction)
                || selected.schema_for(transaction) != schema
                || selected.name_for(transaction) != name
                || self.view_catalog_identity(slot) != identity
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared view selects another lifetime"
                ));
            }
            let target = self.prepare_view_edit(slot, transaction)?;
            return self
                .replace_view_definition(target, definition, transaction)
                .map(ViewPublication::Altered);
        }
        self.require_schema_create(schema.as_str(), transaction)?;
        let ownership = self.initial_ownership(transaction)?;
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            type_contracts::CatalogPublicationInput::ViewDefinition,
            None,
        );
        if owners.catalog.views[slot].ddl_state != CatalogDdlState::Absent
            || owners.catalog.rules[rule_slot].ddl_state != CatalogDdlState::Absent
            || owners.catalog.views.iter().any(|view| {
                view.database == current_database()
                    && view.ddl_state != CatalogDdlState::Absent
                    && (view.created_at == identity.created_at()
                        || view.ddl_state.pending_drop_txid() != Some(transaction)
                            && view.schema_for(transaction) == schema
                            && view.name_for(transaction) == name)
            })
            || owners.catalog.rules.iter().any(|rule| {
                rule.database == current_database()
                    && rule.ddl_state != CatalogDdlState::Absent
                    && rule.created_at == identity.return_rule_created_at()
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared view or return-rule identity is unavailable"
            ));
        }
        self.require_row_type_name(
            owners.types.as_deref().expect("view recovery owns types"),
            schema,
            name,
            transaction,
            None,
            Some(&owners.catalog),
        )?;
        owners.write_query_image(
            self,
            self.committed_dependency_image(StoredQueryDependencyOwner::Rule(rule_slot as u16)),
            definition.query.dependencies.view(),
            transaction,
        )?;
        Self::clear_object_acl_entries_locked(
            owners
                .permissions
                .as_mut()
                .expect("view recovery owns permissions"),
            AccessObject {
                class: AccessClass::View,
                slot: slot as u16,
            },
        );
        owners.catalog.rules[rule_slot] = RuleDef {
            database: current_database(),
            created_at: identity.return_rule_created_at(),
            version: identity.return_rule_created_at(),
            ddl_version: identity.return_rule_created_at(),
            pending_parent_drop_txid: None,
            definition: RuleDefinition::view_return(slot, &definition.query),
            pending: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        owners.catalog.views[slot] = ViewDef {
            database: current_database(),
            created_at: identity.created_at(),
            schema,
            name,
            persistence: definition.persistence,
            return_rule: rule_slot as u16,
            options: definition.options,
            columns: definition.columns,
            ownership,
            version: identity.created_at(),
            ddl_version: identity.created_at(),
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        drop(owners);
        self.catalog_sequence
            .observe(identity.created_at().max(identity.return_rule_created_at()));
        Ok(ViewPublication::Created(CatalogDdlUndo {
            source: self.catalog_object_identity(
                CatalogObjectSlot::View(slot),
                current_database(),
                identity.created_at(),
            ),
            transaction,
            operation: CatalogDdlOperation::Create,
            version: identity.created_at(),
            prior_version: 0,
        }))
    }

    pub(crate) fn restore_prepared_rule(
        &self,
        slot: usize,
        created_at: u64,
        definition: RuleDefinition,
        dependencies: StoredQueryDependencyView<'_>,
        transaction: u32,
    ) -> Result<RulePublication, SqlError> {
        CatalogIdentity::require(created_at)?;
        validate_stored_search_path(&definition.creation_path)?;
        if slot >= self.rule_count() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared rule position is out of range"
            ));
        }
        let target = self.query_rule_target(definition.target, transaction)?;
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            type_contracts::CatalogPublicationInput::QueryDefinition(dependencies),
            target.table(),
        );
        owners.validate_rule_target(target, transaction)?;
        let selected = owners.catalog.rules[slot];
        let occupied = selected.ddl_state != CatalogDdlState::Absent;
        if occupied
            && (selected.database != current_database()
                || selected.created_at != created_at
                || !selected.visible_to(transaction)
                || selected.definition_for(transaction).target != definition.target
                || selected
                    .pending
                    .is_some_and(|pending| pending.txid != transaction))
            || owners
                .catalog
                .rules
                .iter()
                .enumerate()
                .any(|(other, rule)| {
                    other != slot
                        && rule.database == current_database()
                        && rule.ddl_state != CatalogDdlState::Absent
                        && (rule.created_at == created_at
                            || rule.ddl_state.pending_drop_txid() != Some(transaction)
                                && rule.definition_for(transaction).target == definition.target
                                && rule.definition_for(transaction).name == definition.name)
                })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared rule selects another lifetime"
            ));
        }
        if occupied {
            if selected.definition_for(transaction).is_view_return() != definition.is_view_return()
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared rule changes return-rule identity"
                ));
            }
            let version = self.catalog_sequence.next()?;
            let prior = selected.pending;
            let dependency_slot = owners.allocate_query_image(
                self,
                StoredQueryDependencyOwner::Rule(slot as u16),
                prior.map(|value| value.dependency_slot),
                dependencies,
                transaction,
            )?;
            owners.catalog.rules[slot].pending = Some(PendingRuleDefinition {
                txid: transaction,
                version,
                definition,
                dependency_slot,
            });
            Ok(RulePublication::Altered(RuleAlterUndo {
                target: catalog_sources::CatalogEditTarget {
                    source: self.catalog_object_identity(
                        CatalogObjectSlot::Rule(slot),
                        selected.database,
                        selected.created_at,
                    ),
                    transaction,
                    version,
                },
                prior,
            }))
        } else {
            owners.write_query_image(
                self,
                self.committed_dependency_image(StoredQueryDependencyOwner::Rule(slot as u16)),
                dependencies,
                transaction,
            )?;
            owners.catalog.rules[slot] = RuleDef {
                database: current_database(),
                created_at,
                version: created_at,
                ddl_version: created_at,
                pending_parent_drop_txid: None,
                definition,
                pending: None,
                ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
            };
            drop(owners);
            self.catalog_sequence.observe(created_at);
            Ok(RulePublication::Created(CatalogDdlUndo {
                source: self.catalog_object_identity(
                    CatalogObjectSlot::Rule(slot),
                    current_database(),
                    created_at,
                ),
                transaction,
                operation: CatalogDdlOperation::Create,
                version: created_at,
                prior_version: 0,
            }))
        }
    }

    pub(crate) fn restore_prepared_trigger(
        &mut self,
        identity: CatalogPosition,
        spec: TriggerSpec,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let created_at = bounded_catalog_generation(
            identity.created_at(),
            MAX_TRIGGER_OID_GENERATION,
            "trigger",
        )?;
        if !spec.is_valid() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid prepared trigger definition"
            ));
        }
        let function = u16::try_from(spec.function).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine slot exceeds trigger capacity"
            )
        })?;
        let mut catalog = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        let slot = identity.slot();
        if catalog
            .definitions
            .get(slot)
            .is_none_or(|trigger| trigger.ddl_state != CatalogDdlState::Absent)
            || catalog.definitions.iter().any(|trigger| {
                trigger.database == current_database()
                    && trigger.ddl_state != CatalogDdlState::Absent
                    && (trigger.created_at == created_at
                        || trigger.ddl_state.pending_drop_txid() != Some(transaction)
                            && trigger.target == spec.target
                            && trigger.name_to(transaction) == spec.name)
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared trigger position or lifetime is unavailable"
            ));
        }
        catalog.definitions[slot] = TriggerDef {
            database: current_database(),
            created_at,
            name: spec.name,
            target: spec.target,
            kind: spec.kind,
            function,
            timing: spec.timing,
            level: spec.level,
            events: spec.events,
            update_columns: spec.update_columns,
            transition_tables: spec.transition_tables,
            when: spec.when,
            arguments: spec.arguments,
            enabled: TriggerEnabled::Origin,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        self.catalog_sequence.observe(created_at);
        Ok(slot)
    }

    pub(crate) fn restore_prepared_partition_trigger(
        &mut self,
        expected: Option<PartitionTriggerImage>,
        image: Option<PartitionTriggerImage>,
        transaction: u32,
    ) -> Result<(usize, PartitionTriggerState), SqlError> {
        let selected = image.or(expected).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "empty prepared inherited trigger change"
            )
        })?;
        if expected == image
            || selected.trigger.slot() >= self.trigger_capacity()
            || selected.table.slot() >= self.table_count()
            || expected.is_some_and(|old| {
                old.trigger.slot() != selected.trigger.slot()
                    || old.table.slot() != selected.table.slot()
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid prepared inherited trigger change"
            ));
        }
        if let Some(image) = image {
            let trigger = self.trigger_to(image.trigger.slot(), transaction);
            if trigger.database != current_database()
                || !trigger.visible_to(transaction)
                || trigger.created_at != image.trigger.created_at()
                || !self.table_slot_visible_to(image.table.slot(), transaction)
                || self.table_position(image.table.slot()) != image.table
                || !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row)
                || !matches!(trigger.target, TriggerTarget::Table(parent) if self.partition_descends_from(image.table.slot(), usize::from(parent), transaction))
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared inherited trigger selects another source"
                ));
            }
        }
        let slot =
            self.partition_trigger_state_slot(selected.trigger.slot(), selected.table.slot());
        let mut catalog = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        let prior = catalog.partition_states[slot];
        if prior
            .pending
            .is_some_and(|pending| pending.txid != transaction)
            || prior.image_to(transaction) != expected
            || (prior.committed.is_some() || prior.pending.is_some())
                && prior.database != current_database()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared inherited trigger selects another lifetime"
            ));
        }
        catalog.partition_states[slot] = PartitionTriggerState {
            database: current_database(),
            committed: prior.committed,
            pending: Some(partition_triggers::PendingPartitionTriggerState {
                txid: transaction,
                image,
            }),
        };
        drop(catalog);
        if let Some(image) = image {
            self.catalog_sequence.observe(image.creation.created_at());
        }
        Ok((slot, prior))
    }

    pub(crate) fn restore_prepared_policy(
        &self,
        identity: CatalogPosition,
        spec: PolicySpec<'_>,
        dependencies: &StoredQueryDependencies,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let created_at = CatalogIdentity::require(identity.created_at())?.created_at();
        for role in spec.roles {
            if *role != PUBLIC_ROLE {
                self.role_reference_ready(usize::from(*role), transaction)?;
            }
        }
        Self::validate_policy_role_image(self.policy_roles_per_image, spec.roles)?;
        if spec.roles.is_empty()
            || matches!(spec.command, PolicyCommandKind::Insert) && spec.definition.using.is_some()
            || matches!(
                spec.command,
                PolicyCommandKind::Select | PolicyCommandKind::Delete
            ) && spec.definition.with_check.is_some()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid prepared policy definition"
            ));
        }
        let table = u16::try_from(spec.table).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table slot exceeds policy capacity"
            )
        })?;
        let target = self.query_table_target(
            spec.table,
            transaction,
            type_contracts::QueryTargetUse::Read,
        )?;
        let mut owners = self.catalog_dependency_owners(
            &self.policy_catalog,
            type_contracts::CatalogPublicationInput::QueryDefinition(dependencies.view()),
            Some(target),
        );
        owners.validate_target(transaction, type_contracts::QueryTargetUse::Read)?;
        let slot = identity.slot();
        if owners
            .catalog
            .definitions
            .get(slot)
            .is_none_or(|policy| policy.ddl_state != CatalogDdlState::Absent)
            || owners.catalog.definitions.iter().any(|policy| {
                policy.database == current_database()
                    && policy.ddl_state != CatalogDdlState::Absent
                    && (policy.created_at == created_at
                        || policy.ddl_state.pending_drop_txid() != Some(transaction)
                            && policy.table == table
                            && policy.name == spec.name)
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared policy position or lifetime is unavailable"
            ));
        }
        owners.write_query_image(
            self,
            self.committed_dependency_image(StoredQueryDependencyOwner::Policy(slot as u16)),
            dependencies.view(),
            transaction,
        )?;
        let role_count = Self::write_policy_role_image(
            &mut owners.catalog,
            self.policy_roles_per_image,
            slot,
            spec.roles,
        )
        .expect("policy roles were validated before publication");
        owners.catalog.definitions[slot] = PolicyDef {
            database: current_database(),
            created_at,
            name: spec.name,
            table,
            command: spec.command,
            permissive: spec.permissive,
            definition: spec.definition,
            role_count,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        self.catalog_sequence.observe(created_at);
        Ok(slot)
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{
        ColType, ColumnSet, DependencyClass, MAX_COLUMNS, ResolvedRelation,
        StoredDependencyIdentity, StoredQueryDependency, TableDefBuilder,
    };

    #[test]
    fn stored_relation_sources_reject_serialized_slots_and_unselected_definitions() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def("bound_items", &[("id", ColType::Int4, false)]))
            .unwrap();
        let mut dependency = StoredQueryDependency {
            class: DependencyClass::Table,
            slot: u16::MAX,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: ColumnSet::EMPTY,
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("bound_items").unwrap(),
            referenced_schema: SqlName::EMPTY,
            referenced_name: SqlName::parse("bound_items").unwrap(),
        };
        crate::mem::guard::forbid_alloc(|| {
            let error = storage
                .resolve_stored_query_relation(&dependency, 0)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
            assert!(error.message.as_str().contains("unresolved"));
            dependency.slot = table as u16;
            assert!(
                matches!(storage.resolve_stored_query_relation(&dependency, 0).unwrap(), ResolvedRelation::Table(slot) if slot == table)
            );
            let (target, selected) = storage.select_table_definition(table, 7);
            let mut replacement = TableDefBuilder::from_definition(&selected);
            replacement.name = SqlName::parse("private_items").unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(replacement.columns[0].name);
            let change = storage
                .write_table_def(target, replacement, &mapping, false)
                .unwrap();
            assert!(
                storage
                    .resolve_stored_query_relation(&dependency, 0)
                    .is_ok()
            );
            assert!(
                storage
                    .resolve_stored_query_relation(&dependency, 7)
                    .is_err()
            );
            let pending = StoredQueryDependency {
                name: replacement.name,
                ..dependency
            };
            assert!(storage.resolve_stored_query_relation(&pending, 7).is_ok());
            assert!(storage.resolve_stored_query_relation(&pending, 0).is_err());
            storage.rollback_table_def(change);
            assert!(
                storage
                    .resolve_stored_query_relation(&dependency, 7)
                    .is_ok()
            );
            assert!(storage.resolve_stored_query_relation(&pending, 7).is_err());
            storage.commit_drop(table);
            let reused = storage
                .create_table(make_def(
                    "replacement_items",
                    &[("id", ColType::Int4, false)],
                ))
                .unwrap();
            assert_eq!(table, reused);
            assert!(
                storage
                    .resolve_stored_query_relation(&dependency, 0)
                    .is_err()
            );
        });
    }
}
