//! Private declarations reserve both original and staged uniqueness keys.

use super::*;

macro_rules! pending_key_owner {
    ($($entry:ident => $definition:ident),+ $(,)?) => {$(
        impl $entry {
            pub(super) fn pending_key_owner(
                &self,
                transaction: u32,
                conflicts: impl Fn($definition) -> bool,
            ) -> Option<u32> {
                if self.database != current_database()
                    || self.ddl_state == CatalogDdlState::Absent
                {
                    return None;
                }
                let owner = self.pending
                    .map(|pending| pending.txid)
                    .filter(|owner| *owner != transaction)
                    .or_else(|| self.ddl_state.pending_txid().filter(|owner| *owner != transaction))?;
                (conflicts(self.definition)
                    || self.pending.is_some_and(|pending| conflicts(pending.definition)))
                    .then_some(owner)
            }
        }
    )+};
}

pending_key_owner! {
    OperatorDef => OperatorDefinition,
    OperatorFamilyDef => OperatorFamilyDefinition,
    OperatorClassDef => OperatorClassDefinition,
    CollationDef => CollationDefinition,
    ConversionDef => ConversionDefinition,
    TextSearchDef => TextSearchDefinition,
    EventTriggerDef => EventTriggerDefinition,
}

impl OperatorDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.schema == other.schema && self.name == other.name && self.signature == other.signature
    }
}

impl OperatorFamilyDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.schema == other.schema && self.name == other.name
    }
}

impl OperatorClassDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.schema == other.schema && self.name == other.name
            || self.default && other.default && self.input == other.input
    }
}

impl CollationDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.schema == other.schema && self.name == other.name
    }
}

impl ConversionDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.schema == other.schema
            && (self.name == other.name
                || self.default
                    && other.default
                    && self.source == other.source
                    && self.destination == other.destination)
    }
}

impl TextSearchDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.kind() == other.kind()
            && self.schema() == other.schema()
            && self.name() == other.name()
    }
}

impl EventTriggerDefinition {
    pub(super) fn shares_catalog_key(self, other: Self) -> bool {
        self.name == other.name
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::super::tests::{test_budget, test_config};
    use super::*;

    #[test]
    fn private_catalog_keys_wait_before_allocating_and_release_on_rollback() {
        let mut config = test_config();
        config.max_operator_families = 4;
        config.max_operator_classes = 4;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let schema = SqlName::parse("public").unwrap();
            let name = SqlName::parse("private_key").unwrap();
            let owner = storage.role(0).oid().get();
            macro_rules! check {
                ($create:ident, $rollback:ident, $commit:ident, $definition:expr, $duplicate:expr) => {{
                    let definition = $definition;
                    let slot = storage.$create(definition, 7).unwrap();
                    let sequence = storage.catalog_identity_sequence();
                    assert_eq!(
                        storage.$create(definition, 9).unwrap_err().sqlstate,
                        sqlstate::INTERNAL_LOCK_WAIT
                    );
                    assert_eq!(storage.catalog_identity_sequence(), sequence);
                    storage.$rollback(slot);
                    let slot = storage.$create(definition, 9).unwrap();
                    storage.$commit(slot);
                    assert_eq!(
                        storage.$create(definition, 7).unwrap_err().sqlstate,
                        $duplicate
                    );
                }};
            }
            check!(
                create_operator,
                rollback_operator_create,
                commit_operator_create,
                OperatorDefinition {
                    schema,
                    name,
                    owner,
                    signature: OperatorSignature {
                        left: Some(RoutineResult::TEXT),
                        right: Some(RoutineResult::TEXT)
                    },
                    ..OperatorDefinition::EMPTY
                },
                sqlstate::DUPLICATE_FUNCTION
            );
            let family_definition = OperatorFamilyDefinition {
                schema,
                name,
                owner,
                ..OperatorFamilyDefinition::EMPTY
            };
            check!(
                create_operator_family,
                rollback_operator_family_create,
                commit_operator_family_create,
                family_definition,
                sqlstate::DUPLICATE_OBJECT
            );
            let family = storage
                .operator_family_slot_exact("public", "private_key", 0)
                .unwrap();
            check!(
                create_operator_class,
                rollback_operator_class_create,
                commit_operator_class_create,
                OperatorClassDefinition {
                    schema,
                    name,
                    owner,
                    family: storage.operator_family(family).oid(),
                    ..OperatorClassDefinition::EMPTY
                },
                sqlstate::DUPLICATE_OBJECT
            );
            check!(
                create_collation,
                rollback_collation_create,
                commit_collation_create,
                CollationDefinition {
                    schema,
                    name,
                    owner,
                    provider: CollationProvider::Builtin,
                    locale: StackStr::from_str("C"),
                    ..CollationDefinition::EMPTY
                },
                sqlstate::DUPLICATE_OBJECT
            );
            check!(
                create_conversion,
                rollback_conversion_create,
                commit_conversion_create,
                ConversionDefinition {
                    schema,
                    name,
                    owner,
                    source: PgEncoding::LATIN1,
                    destination: PgEncoding::UTF8,
                    ..ConversionDefinition::EMPTY
                },
                sqlstate::DUPLICATE_OBJECT
            );
            check!(
                create_text_search_object,
                rollback_text_search_create,
                commit_text_search_create,
                TextSearchDefinition::Parser {
                    schema,
                    name,
                    oid: 0,
                    start: 3717,
                    gettoken: 3718,
                    end: 3719,
                    headline: 3720,
                    lextypes: 3721
                },
                sqlstate::DUPLICATE_OBJECT
            );
            {
                let mut routines = storage.routines.lock().unwrap();
                routines[0].database = current_database();
                routines[0].kind = RoutineKind::EventTrigger;
                routines[0].ddl_state = CatalogDdlState::Present;
            }
            check!(
                create_event_trigger,
                rollback_event_trigger_create,
                commit_event_trigger_create,
                EventTriggerDefinition {
                    name,
                    event: crate::sql::ast::EventTriggerEvent::DdlCommandStart,
                    function: 0,
                    tags: EventTriggerTags::EMPTY,
                    enabled: TriggerEnabled::Origin,
                    ownership: Ownership::BOOTSTRAP
                },
                sqlstate::DUPLICATE_OBJECT
            );
        });
    }

    #[test]
    fn default_catalog_keys_wait_across_names_and_check_staged_targets() {
        let mut config = test_config();
        config.max_operator_families = 4;
        config.max_operator_classes = 4;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let schema = SqlName::parse("public").unwrap();
            let owner = storage.role(0).oid().get();
            let first = SqlName::parse("first_default").unwrap();
            let second = SqlName::parse("second_default").unwrap();
            let family = storage
                .create_operator_family(
                    OperatorFamilyDefinition {
                        schema,
                        name: first,
                        owner,
                        ..OperatorFamilyDefinition::EMPTY
                    },
                    7,
                )
                .unwrap();
            storage.commit_operator_family_create(family);
            let definition = OperatorClassDefinition {
                schema,
                name: first,
                owner,
                family: storage.operator_family(family).oid(),
                default: true,
                input: RoutineResult::builtin(ColType::Json),
                storage: RoutineResult::builtin(ColType::Json),
                ..OperatorClassDefinition::EMPTY
            };
            let source = storage.create_operator_class(definition, 7).unwrap();
            let other = OperatorClassDefinition {
                name: second,
                ..definition
            };
            let before = storage.catalog_identity_sequence();
            assert_eq!(
                storage
                    .create_operator_class(other, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert_eq!(storage.catalog_identity_sequence(), before);
            storage.rollback_operator_class_create(source);
            let source = storage
                .create_operator_class(
                    OperatorClassDefinition {
                        default: false,
                        ..definition
                    },
                    7,
                )
                .unwrap();
            storage.commit_operator_class_create(source);
            let target = storage
                .create_operator_class(
                    OperatorClassDefinition {
                        default: false,
                        ..other
                    },
                    9,
                )
                .unwrap();
            storage.commit_operator_class_create(target);
            let prior = storage
                .stage_operator_class_definition(source, definition, 7)
                .unwrap();
            assert_eq!(
                storage
                    .stage_operator_class_definition(target, other, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.rollback_operator_class_alter(source, prior);
            let conversion = ConversionDefinition {
                schema,
                name: first,
                owner,
                source: PgEncoding::LATIN1,
                destination: PgEncoding::UTF8,
                default: true,
                ..ConversionDefinition::EMPTY
            };
            let source = storage.create_conversion(conversion, 7).unwrap();
            let other = ConversionDefinition {
                name: second,
                ..conversion
            };
            assert_eq!(
                storage.create_conversion(other, 9).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.rollback_conversion_create(source);
            let source = storage
                .create_conversion(
                    ConversionDefinition {
                        default: false,
                        ..conversion
                    },
                    7,
                )
                .unwrap();
            storage.commit_conversion_create(source);
            let target = storage
                .create_conversion(
                    ConversionDefinition {
                        default: false,
                        ..other
                    },
                    9,
                )
                .unwrap();
            storage.commit_conversion_create(target);
            let prior = storage.alter_conversion(source, conversion, 7).unwrap();
            assert_eq!(
                storage
                    .alter_conversion(target, other, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.rollback_conversion_alter(source, prior);
        });
    }

    #[test]
    fn catalog_renames_reserve_both_keys_for_foreign_writers_and_release_own_old_names() {
        let mut config = test_config();
        config.max_operator_families = 4;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let schema = SqlName::parse("public").unwrap();
            let owner = storage.role(0).oid().get();
            let original = SqlName::parse("original_key").unwrap();
            let renamed = SqlName::parse("renamed_key").unwrap();
            macro_rules! check {
                ($create:ident, $commit:ident, $alter:ident, $rollback_create:ident, $rollback_alter:ident, $first:expr, $renamed:expr) => {{
                    let definition = $first;
                    let renamed = $renamed;
                    let source = storage.$create(definition, 7).unwrap();
                    storage.$commit(source);
                    let prior = storage.$alter(source, renamed, 7).unwrap();
                    assert_eq!(
                        storage.$create(renamed, 9).unwrap_err().sqlstate,
                        sqlstate::INTERNAL_LOCK_WAIT
                    );
                    let replacement = storage.$create(definition, 7).unwrap();
                    storage.$rollback_create(replacement);
                    storage.$rollback_alter(source, prior);
                }};
            }
            let family = OperatorFamilyDefinition {
                schema,
                name: original,
                owner,
                ..OperatorFamilyDefinition::EMPTY
            };
            check!(
                create_operator_family,
                commit_operator_family_create,
                stage_operator_family_definition,
                rollback_operator_family_create,
                rollback_operator_family_alter,
                family,
                OperatorFamilyDefinition {
                    name: renamed,
                    ..family
                }
            );
            let operator = OperatorDefinition {
                schema,
                name: original,
                owner,
                signature: OperatorSignature {
                    left: Some(RoutineResult::TEXT),
                    right: Some(RoutineResult::TEXT),
                },
                ..OperatorDefinition::EMPTY
            };
            check!(
                create_operator,
                commit_operator_create,
                stage_operator_definition,
                rollback_operator_create,
                rollback_operator_alter,
                operator,
                OperatorDefinition {
                    name: renamed,
                    ..operator
                }
            );
            let collation = CollationDefinition {
                schema,
                name: original,
                owner,
                provider: CollationProvider::Builtin,
                locale: StackStr::from_str("C"),
                ..CollationDefinition::EMPTY
            };
            check!(
                create_collation,
                commit_collation_create,
                alter_collation,
                rollback_collation_create,
                rollback_collation_alter,
                collation,
                CollationDefinition {
                    name: renamed,
                    ..collation
                }
            );
            {
                let mut routines = storage.routines.lock().unwrap();
                routines[0].database = current_database();
                routines[0].kind = RoutineKind::EventTrigger;
                routines[0].ddl_state = CatalogDdlState::Present;
            }
            let event = EventTriggerDefinition {
                name: original,
                event: crate::sql::ast::EventTriggerEvent::DdlCommandStart,
                function: 0,
                tags: EventTriggerTags::EMPTY,
                enabled: TriggerEnabled::Origin,
                ownership: Ownership::BOOTSTRAP,
            };
            check!(
                create_event_trigger,
                commit_event_trigger_create,
                alter_event_trigger,
                rollback_event_trigger_create,
                rollback_event_trigger_alter,
                event,
                EventTriggerDefinition {
                    name: renamed,
                    ..event
                }
            );
        });
    }
}
