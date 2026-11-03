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

impl Storage {
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
            DdlUndo::IndexCreated(slot)
            | DdlUndo::IndexDropped(slot)
            | DdlUndo::IndexRenamed { slot, .. }
            | DdlUndo::IndexAltered { slot, .. } => {
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
            | DdlUndo::TriggerAltered { slot, .. }
            | DdlUndo::PartitionTriggerAltered { slot, .. } => self.selected_access_target(
                self.trigger(*slot as usize).target.access_object(),
                transaction,
            )?,
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
