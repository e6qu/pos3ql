//! Recovery installs recorded type lifetimes as private transaction changes.

use super::type_identities::TypeCreationAdmission;
use super::{
    CompositeDef, CompositeSpec, DomainDef, DomainSpec, EnumAlterUndo, EnumMember, EnumSpec,
    SqlError, SqlName, Storage, TypeDdlUndo, TypeEditTarget,
};

impl Storage {
    pub(crate) fn restore_prepared_domain(
        &self,
        slot: usize,
        definition: &DomainDef,
        transaction: u32,
    ) -> Result<TypeDdlUndo, SqlError> {
        self.create_domain_with_binding(
            definition.schema,
            definition.name,
            DomainSpec {
                base_domain: definition.base_domain,
                base_user_type: definition.base_user_type,
                base: definition.base,
                base_type_mod: definition.base_type_mod,
                not_null: definition.not_null,
                not_null_name: definition.not_null_name,
                not_null_identity: definition.not_null_identity,
                default_expr: definition.default_expr,
                checks: definition.checks,
                n_checks: definition.n_checks,
            },
            transaction,
            TypeCreationAdmission::restored(definition.created_at)?,
            false,
            Some(slot),
        )
    }

    pub(crate) fn restore_prepared_composite(
        &self,
        slot: usize,
        definition: &CompositeDef,
        transaction: u32,
    ) -> Result<TypeDdlUndo, SqlError> {
        self.create_composite_with_binding(
            definition.schema,
            definition.name,
            CompositeSpec {
                fields: definition.fields,
                n_fields: definition.n_fields,
            },
            transaction,
            TypeCreationAdmission::restored(definition.created_at)?,
            false,
            Some(slot),
        )
    }

    pub(crate) fn restore_prepared_enum(
        &self,
        slot: usize,
        created_at: u64,
        schema: SqlName,
        name: SqlName,
        members: &[EnumMember],
        transaction: u32,
    ) -> Result<TypeDdlUndo, SqlError> {
        let change = self.create_enum_with_creation(
            schema,
            name,
            EnumSpec { members },
            transaction,
            TypeCreationAdmission::restored(created_at)?,
            Some(slot),
        )?;
        for member in members {
            self.catalog_sequence.observe(
                member
                    .identity
                    .expect("restored member validated before installation")
                    .created_at(),
            );
        }
        Ok(change)
    }

    pub(crate) fn restore_prepared_enum_definition(
        &self,
        target: TypeEditTarget,
        schema: SqlName,
        name: SqlName,
        members: &[EnumMember],
        transaction: u32,
    ) -> Result<EnumAlterUndo, SqlError> {
        self.stage_enum_definition(target, schema, name, members, transaction, true)
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::super::tests::{test_budget, test_config};
    use super::super::{
        CheckConstraint, ColType, CompositeFieldDef, EnumLabelIdentity, MAX_COMPOSITE_FIELDS,
        MAX_DOMAIN_CHECKS, NamedTypeKind, TypeDdlDecision,
    };
    use super::*;

    #[test]
    fn restored_types_keep_recorded_slots_and_lifetimes_private_until_resolution() {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config();
            config.transaction_mode = mode;
            let mut budget = test_budget(&config);
            let storage = Storage::new(&config, &mut budget, 0).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let schema = SqlName::parse("public").unwrap();
                let name = SqlName::parse("recorded_type").unwrap();
                let original = storage
                    .create_domain(
                        schema,
                        name,
                        DomainSpec {
                            base_domain: None,
                            base_user_type: None,
                            base: ColType::Int4,
                            base_type_mod: -1,
                            not_null: false,
                            not_null_name: None,
                            not_null_identity: None,
                            default_expr: None,
                            checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                            n_checks: 0,
                        },
                        0,
                    )
                    .unwrap();
                let definition = storage.domain(original.slot());
                let dropped = storage
                    .stage_type_drop(NamedTypeKind::Domain, "public", "recorded_type", 0)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(dropped, TypeDdlDecision::Commit);
                let created = storage.restore_prepared_domain(3, &definition, 7).unwrap();
                assert_eq!(created.slot(), 3);
                assert_eq!(storage.domain_slot("public", "recorded_type", 0), None);
                assert_eq!(storage.domain_slot("public", "recorded_type", 7), Some(3));
                assert_eq!(storage.domain_for(3, 7).created_at, definition.created_at);
                storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
                assert_eq!(storage.domain_slot("public", "recorded_type", 7), None);

                let mut fields = [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS];
                fields[0] = CompositeFieldDef {
                    name: SqlName::parse("id").unwrap(),
                    attribute_number: 1,
                    ctype: ColType::Int4,
                    ..CompositeFieldDef::EMPTY
                };
                let original = storage
                    .create_composite(
                        schema,
                        name,
                        CompositeSpec {
                            fields,
                            n_fields: 1,
                        },
                        0,
                    )
                    .unwrap();
                let definition = storage.composite_for(original.slot(), 0);
                let dropped = storage
                    .stage_type_drop(NamedTypeKind::Composite, "public", "recorded_type", 0)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(dropped, TypeDdlDecision::Commit);
                let created = storage
                    .restore_prepared_composite(3, &definition, 7)
                    .unwrap();
                assert_eq!(created.slot(), 3);
                assert_eq!(storage.composite_slot("public", "recorded_type", 0), None);
                assert_eq!(
                    storage.composite_slot("public", "recorded_type", 7),
                    Some(3)
                );
                assert_eq!(
                    storage.composite_for(3, 7).created_at,
                    definition.created_at
                );
                storage.finish_type_ddl(created, TypeDdlDecision::Commit);
                assert_eq!(
                    storage.composite_slot("public", "recorded_type", 0),
                    Some(3)
                );
                let dropped = storage
                    .stage_type_drop(NamedTypeKind::Composite, "public", "recorded_type", 0)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(dropped, TypeDdlDecision::Commit);

                let original = storage
                    .create_enum(
                        schema,
                        name,
                        EnumSpec {
                            members: &[EnumMember {
                                label: SqlName::parse("one").unwrap(),
                                sort: 1.0,
                                ..EnumMember::EMPTY
                            }],
                        },
                        0,
                    )
                    .unwrap();
                let definition = storage.enum_for(original.slot(), 0);
                let created_at = definition.created_at;
                let first = definition.members().next().unwrap();
                drop(definition);
                let dropped = storage
                    .stage_type_drop(NamedTypeKind::Enum, "public", "recorded_type", 0)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(dropped, TypeDdlDecision::Commit);
                let created = storage
                    .restore_prepared_enum(3, created_at, schema, name, &[first], 7)
                    .unwrap();
                assert_eq!(created.slot(), 3);
                assert_eq!(storage.enum_slot("public", "recorded_type", 0), None);
                assert_eq!(storage.enum_slot("public", "recorded_type", 7), Some(3));
                assert_eq!(
                    storage.enum_for(3, 7).members().next().unwrap().identity,
                    first.identity
                );
                storage.finish_type_ddl(created, TypeDdlDecision::Commit);
                let (_, target) = storage.prepare_enum_edit(3, 9).unwrap();
                let next_identity =
                    EnumLabelIdentity::parse(storage.catalog_identity_sequence() + 1).unwrap();
                let second = EnumMember {
                    identity: Some(next_identity),
                    label: SqlName::parse("two").unwrap(),
                    sort: 2.0,
                    ..EnumMember::EMPTY
                };
                let altered = storage
                    .restore_prepared_enum_definition(target, schema, name, &[first, second], 9)
                    .unwrap();
                assert_eq!(storage.enum_for(3, 0).members().len(), 1);
                assert_eq!(storage.enum_for(3, 9).members().len(), 2);
                assert!(storage.catalog_identity_sequence() >= next_identity.created_at());
                storage.rollback_enum_alter(altered);
                assert_eq!(storage.enum_for(3, 9).members().len(), 1);
                let (_, target) = storage.prepare_enum_edit(3, 9).unwrap();
                let changed_prefix = EnumMember {
                    identity: Some(next_identity),
                    ..first
                };
                let error = storage
                    .restore_prepared_enum_definition(target, schema, name, &[changed_prefix], 9)
                    .unwrap_err();
                assert_eq!(error.sqlstate, crate::sql::eval::sqlstate::DATA_EXCEPTION);
                assert_eq!(
                    storage.enum_for(3, 9).members().next().unwrap().identity,
                    first.identity
                );
            });
        }
    }
}
