//! Catalog edits identify the object whose saved plans must be invalidated.

use crate::sql::{
    eval::{SqlError, sqlstate},
    txn::DdlUndo,
};
use crate::sql_err;

use super::{AccessClass, AccessObject, NamedTypeKind, Storage, TypeCreation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectedPlanTarget {
    Relation {
        created_at: u64,
    },
    Routine {
        oid: i32,
    },
    Type {
        kind: NamedTypeKind,
        creation: TypeCreation,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct TruncateTarget {
    pub slot: u16,
    pub created_at: u64,
}

impl Storage {
    pub(crate) fn capture_truncate_targets(
        &self,
        tables: &[usize],
        transaction: u32,
    ) -> Result<crate::mem::lease_pool::SharedSlice<TruncateTarget>, SqlError> {
        let full = |error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error);
        let mut targets = self
            .metadata_pool
            .lease::<TruncateTarget>(tables.len())
            .map_err(full)?;
        for &slot in tables {
            targets
                .push_reserved(TruncateTarget {
                    slot: u16::try_from(slot).expect("validated physical table slot"),
                    created_at: self.table_def(slot, transaction).identity().created_at,
                })
                .map_err(full)?;
        }
        Ok(targets.freeze())
    }

    pub(crate) fn operator_family_plan_change(&self, change: &DdlUndo, transaction: u32) -> bool {
        match change {
            DdlUndo::OperatorFamilyCreated(slot) | DdlUndo::OperatorFamilyDropped(slot) => self
                .operator_family_for(*slot as usize, transaction)
                .operators
                .iter()
                .any(|member| member.used),
            DdlUndo::OperatorFamilyAltered { slot, prior } => {
                let family = self.operator_family(*slot as usize);
                let previous = prior
                    .as_ref()
                    .map_or(&family.definition, |prior| &prior.definition);
                let current = family.definition_for(transaction);
                let mut left = previous.operators.iter().filter(|member| member.used);
                let right = current.operators.iter().filter(|member| member.used);
                left.clone().count() != right.clone().count()
                    || left.any(|member| !right.clone().any(|other| other == member))
            }
            _ => false,
        }
    }

    /// Resolve before publication or undo can release a dropped definition.
    pub(crate) fn selected_plan_target(
        &self,
        change: &DdlUndo,
        transaction: u32,
    ) -> Result<Option<SelectedPlanTarget>, SqlError> {
        let table = |slot| SelectedPlanTarget::Relation {
            created_at: self.table_def(slot, transaction).identity().created_at,
        };
        let selected = match change {
            DdlUndo::Created(slot) => Some(table(*slot as usize)),
            DdlUndo::Dropped(change) => Some(table(change.slot())),
            DdlUndo::TableAltered(change) => Some(table(change.table())),
            DdlUndo::ViewChanged(change) => Some(SelectedPlanTarget::Relation {
                created_at: self.view(change.slot()).created_at,
            }),
            DdlUndo::ViewDropped(change) => Some(SelectedPlanTarget::Relation {
                created_at: self.view(change.slot()).created_at,
            }),
            DdlUndo::MatviewChanged(change) => Some(table(usize::from(
                self.matview(change.slot()).backing_table,
            ))),
            DdlUndo::MatviewPopulationChanged(change) => Some(table(usize::from(
                self.matview(change.slot()).backing_table,
            ))),
            DdlUndo::MatviewQueryChanged(change) => Some(table(usize::from(
                self.matview(change.slot()).backing_table,
            ))),
            DdlUndo::RuleChanged { publication, .. } => self.selected_access_target(
                self.rule(publication.slot())
                    .definition_for(transaction)
                    .target
                    .access_object(),
                transaction,
            )?,
            DdlUndo::RuleAltered(change) => self.selected_access_target(
                self.rule(change.slot())
                    .definition_for(transaction)
                    .target
                    .access_object(),
                transaction,
            )?,
            DdlUndo::RuleDropped(change) => self.selected_access_target(
                self.rule(change.slot())
                    .definition_for(transaction)
                    .target
                    .access_object(),
                transaction,
            )?,
            DdlUndo::RoutineChanged(change) => Some(SelectedPlanTarget::Routine {
                oid: super::routine_oid(&self.routine_for(change.slot(), transaction)),
            }),
            DdlUndo::IndexCreated(slot) | DdlUndo::IndexDropped(slot) => {
                let binding = self.index(*slot as usize).table_binding.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "index plan invalidation has no retained table identity"
                    )
                })?;
                Some(SelectedPlanTarget::Relation {
                    created_at: binding.source.created_at(),
                })
            }
            DdlUndo::PolicyCreated(slot)
            | DdlUndo::PolicyDropped(slot)
            | DdlUndo::PolicyAltered { slot, .. } => {
                Some(table(usize::from(self.policy(*slot as usize).table)))
            }
            DdlUndo::TriggerCreated(slot)
            | DdlUndo::TriggerDropped(slot)
            | DdlUndo::TriggerAltered { slot, .. } => self.selected_access_target(
                self.trigger(*slot as usize).target.access_object(),
                transaction,
            )?,
            DdlUndo::PartitionTriggerAltered { slot, prior } => {
                let state = self.partition_trigger_state(*slot as usize);
                let image = state
                    .image_to(transaction)
                    .or_else(|| prior.image_to(transaction))
                    .or(prior.committed)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "inherited trigger change has no retained table identity"
                        )
                    })?;
                Some(SelectedPlanTarget::Relation {
                    created_at: image.table.created_at(),
                })
            }
            DdlUndo::StatisticsCreated(slot)
            | DdlUndo::StatisticsDropped(slot)
            | DdlUndo::StatisticsAltered { slot, .. }
            | DdlUndo::StatisticsKeysAltered { slot, .. } => Some(table(usize::from(
                self.extended_statistics(*slot as usize).table,
            ))),
            DdlUndo::SequenceChanged(change) => Some(SelectedPlanTarget::Relation {
                created_at: self.sequence(change.slot()).created_at,
            }),
            DdlUndo::ForeignTableCreated(slot)
            | DdlUndo::ForeignTableDropped(slot)
            | DdlUndo::ForeignTableAltered { slot, .. } => {
                let table_slot = self
                    .foreign_catalog()
                    .entry_table(*slot as usize)
                    .definition_for(transaction)
                    .table;
                Some(table(usize::from(table_slot)))
            }
            DdlUndo::DomainCreated(change) | DdlUndo::DomainDropped(change) => {
                Some(SelectedPlanTarget::Type {
                    kind: NamedTypeKind::Domain,
                    creation: self.domain(change.slot()).creation(),
                })
            }
            DdlUndo::DomainAltered(change) => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Domain,
                creation: self.domain(change.slot()).creation(),
            }),
            DdlUndo::EnumCreated(change) | DdlUndo::EnumDropped(change) => {
                Some(SelectedPlanTarget::Type {
                    kind: NamedTypeKind::Enum,
                    creation: self.enum_for(change.slot(), transaction).creation(),
                })
            }
            DdlUndo::EnumAltered(change) => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Enum,
                creation: self.enum_for(change.slot(), transaction).creation(),
            }),
            DdlUndo::CompositeCreated(change) | DdlUndo::CompositeDropped(change) => {
                Some(SelectedPlanTarget::Type {
                    kind: NamedTypeKind::Composite,
                    creation: self.composite_for(change.slot(), transaction).creation(),
                })
            }
            DdlUndo::CompositeAltered(change) => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Composite,
                creation: self.composite_for(change.slot(), transaction).creation(),
            }),
            DdlUndo::ObjectOwnerChanged { object, .. } => {
                self.selected_access_target(*object, transaction)?
            }
            DdlUndo::ObjectAclChanged { prior, .. } => {
                self.selected_access_target(prior.object(), transaction)?
            }
            DdlUndo::ColumnAclChanged { slot, .. } => self.selected_access_target(
                self.column_acl_entry(*slot as usize).target.relation,
                transaction,
            )?,
            DdlUndo::IndexRenamed { .. } | DdlUndo::IndexAltered { .. } => None,
            _ => None,
        };
        Ok(selected)
    }

    fn selected_access_target(
        &self,
        object: AccessObject,
        transaction: u32,
    ) -> Result<Option<SelectedPlanTarget>, SqlError> {
        let slot = usize::from(object.slot);
        Ok(match object.class {
            AccessClass::Table => Some(SelectedPlanTarget::Relation {
                created_at: self.table_def(slot, transaction).identity().created_at,
            }),
            AccessClass::View => Some(SelectedPlanTarget::Relation {
                created_at: self.view(slot).created_at,
            }),
            AccessClass::MaterializedView => Some(SelectedPlanTarget::Relation {
                created_at: self
                    .table_def(usize::from(self.matview(slot).backing_table), transaction)
                    .identity()
                    .created_at,
            }),
            AccessClass::Sequence => Some(SelectedPlanTarget::Relation {
                created_at: self.sequence(slot).created_at,
            }),
            AccessClass::Routine => Some(SelectedPlanTarget::Routine {
                oid: super::routine_oid(&self.routine_for(slot, transaction)),
            }),
            AccessClass::Domain => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Domain,
                creation: self.domain(slot).creation(),
            }),
            AccessClass::Enum => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Enum,
                creation: self.enum_for(slot, transaction).creation(),
            }),
            AccessClass::Composite => Some(SelectedPlanTarget::Type {
                kind: NamedTypeKind::Composite,
                creation: self.composite_for(slot, transaction).creation(),
            }),
            AccessClass::Index => {
                let binding = self.index(slot).table_binding.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "index privilege invalidation has no retained table identity"
                    )
                })?;
                Some(SelectedPlanTarget::Relation {
                    created_at: binding.source.created_at(),
                })
            }
            AccessClass::Statistics => Some(SelectedPlanTarget::Relation {
                created_at: self
                    .table_def(
                        usize::from(self.extended_statistics(slot).table),
                        transaction,
                    )
                    .identity()
                    .created_at,
            }),
            AccessClass::Trigger => {
                return self.selected_access_target(
                    self.trigger(slot).target.access_object(),
                    transaction,
                );
            }
            _ => None,
        })
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::storage::{
        ColType, IndexDef, SqlName,
        tests::{make_def, test_budget, test_config},
    };

    #[test]
    fn inherited_trigger_changes_select_the_retained_partition_image() {
        use crate::storage::{
            CatalogIdentity, CatalogPosition, DatabaseOid, PartitionTriggerImage,
            PartitionTriggerState, TriggerEnabled,
            partition_triggers::PendingPartitionTriggerState,
        };
        let mut config = test_config();
        config.max_tables = 2;
        config.max_triggers = 1;
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let slot = storage.partition_trigger_state_slot(0, 1);
        let image = PartitionTriggerImage {
            creation: CatalogIdentity::require(71).unwrap(),
            trigger: CatalogPosition::new(0, 73).unwrap(),
            table: CatalogPosition::new(1, 79).unwrap(),
            enabled: TriggerEnabled::Origin,
        };
        crate::mem::guard::forbid_alloc(|| {
            for (committed, pending, prior) in [
                (None, Some(image), PartitionTriggerState::EMPTY),
                (
                    Some(image),
                    None,
                    PartitionTriggerState {
                        committed: Some(image),
                        ..PartitionTriggerState::EMPTY
                    },
                ),
                (
                    None,
                    None,
                    PartitionTriggerState {
                        pending: Some(PendingPartitionTriggerState {
                            txid: 1,
                            image: Some(image),
                        }),
                        ..PartitionTriggerState::EMPTY
                    },
                ),
            ] {
                storage.trigger_catalog.lock().unwrap().partition_states[slot] =
                    PartitionTriggerState {
                        database: DatabaseOid::POSTGRES,
                        committed,
                        pending: Some(PendingPartitionTriggerState {
                            txid: 1,
                            image: pending,
                        }),
                    };
                assert_eq!(
                    storage
                        .selected_plan_target(
                            &DdlUndo::PartitionTriggerAltered {
                                slot: slot as u32,
                                prior,
                            },
                            1,
                        )
                        .unwrap(),
                    Some(SelectedPlanTarget::Relation { created_at: 79 })
                );
            }
        });
    }

    #[test]
    fn truncate_events_retain_identities_and_exact_startup_budget_through_rewind() {
        use crate::sql::txn::TxnState;
        let mut config = test_config();
        config.table_metadata_leases = 64;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "truncate_identity",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let target = SelectedPlanTarget::Relation {
            created_at: storage.table_def(table, 0).identity().created_at,
        };
        let mut transaction_budget =
            crate::mem::Budget::new(TxnState::budget_bytes_with_large_objects(1, 0, 2));
        let mut transaction =
            TxnState::new_with_large_objects(&mut transaction_budget, 1, 0, 2).unwrap();
        assert_eq!(transaction_budget.remaining(), 0);
        transaction.txid = 7;
        let mut held = crate::mem::FixedVec::new(
            &mut budget,
            "test shared metadata pressure",
            config.table_metadata_leases,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            loop {
                match storage.metadata_pool.lease::<u8>(64) {
                    Ok(lease) => assert!(held.push(lease).is_ok()),
                    Err(error) => {
                        assert_eq!(error.available_leases, 0);
                        break;
                    }
                }
            }
            let error = transaction
                .record_truncate(&storage, &[table], false, false, 1)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("table metadata pool"));
            assert_eq!(transaction.plan_events().count(), 0);
            assert!(transaction.truncates().is_empty());
            held.clear();

            transaction
                .record_truncate(&storage, &[table], false, false, 1)
                .unwrap();
            transaction
                .record_truncate(&storage, &[table], false, true, 2)
                .unwrap();
            assert_eq!(
                transaction
                    .record_truncate(&storage, &[table], false, false, 3)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(transaction.plan_events().count(), 2);
            transaction.rewind_truncates(1);
            assert_eq!(transaction.plan_events().count(), 1);
            transaction
                .record_truncate(&storage, &[table], false, false, 3)
                .unwrap();
        });
        let _dropped = storage.drop_table_in(table, 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for event in transaction.plan_events() {
                assert_eq!(event.scope, storage.runtime_input_scope());
                assert_eq!(event.selected, Some(target));
            }
            storage.publish_plan_invalidations(transaction.plan_events());
            transaction.invalidate_prepared_plan_inputs();
            transaction.clear();
            assert_eq!(transaction.plan_events().count(), 0);
        });
    }

    #[test]
    fn index_invalidations_keep_the_parent_identity_while_its_drop_is_pending() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "invalidation_parent",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let expected = SelectedPlanTarget::Relation {
            created_at: storage.table_def(table, 0).identity().created_at,
        };
        let mut definition = IndexDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("invalidation_parent_id").unwrap(),
            table: SqlName::parse("invalidation_parent").unwrap(),
            n_cols: 1,
            ..IndexDef::EMPTY
        };
        definition.columns[0] = 0;
        let index = storage.create_index(definition, 7).unwrap();
        let change = DdlUndo::IndexCreated(index as u32);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.selected_plan_target(&change, 7).unwrap(),
                Some(expected)
            );
        });
        let dropped = storage.drop_table_in(table, 7).unwrap();
        assert_eq!(storage.index_table_slot_to(index, 7), None);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.selected_plan_target(&change, 7).unwrap(),
                Some(expected)
            );
            assert_eq!(
                storage
                    .selected_plan_target(&DdlUndo::Dropped(dropped), 7)
                    .unwrap(),
                Some(expected)
            );
        });
    }
}
