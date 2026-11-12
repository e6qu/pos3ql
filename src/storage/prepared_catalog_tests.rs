//! Malformed prepared identities fail before changing committed catalog views.

use super::tests::{test_budget, test_config};
use super::*;
use crate::sql::txn::DdlUndo;

/// The first role catalog slot after `postgres` and the predefined roles.
const USER_SLOT: usize = 1 + PREDEFINED_ROLE_COUNT;

fn collation(storage: &Storage, name: &str) -> CollationDefinition {
    CollationDefinition {
        schema: SqlName::parse("public").unwrap(),
        name: SqlName::parse(name).unwrap(),
        owner: storage.role(0).oid().get(),
        provider: CollationProvider::Builtin,
        locale: StackStr::from_str("C"),
        ..CollationDefinition::EMPTY
    }
}

#[test]
fn restored_casts_reject_duplicate_births_across_distinct_type_pairs() {
    let mut config = test_config();
    config.max_casts = 8;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let first = CastDef {
        created_at: 41,
        source: RoutineResult::builtin(ColType::Text),
        target: RoutineResult::builtin(ColType::Int4),
        method: CastMethod::InOut,
        ..CastDef::EMPTY
    };
    let other = CastDef {
        target: RoutineResult::builtin(ColType::Bool),
        ..first
    };
    crate::mem::guard::forbid_alloc(|| {
        storage
            .restore_prepared_cast(CatalogPosition::new(3, 41).unwrap(), first, 7)
            .unwrap();
        let before = storage.catalog_identity_sequence();
        let error = storage
            .restore_prepared_cast(CatalogPosition::new(4, 41).unwrap(), other, 9)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.cast(4).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.catalog_identity_sequence(), before);
        storage.commit_cast_create(3);
        let error = storage
            .create_cast_from_image(CatalogPosition::new(4, 41).unwrap(), other)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.cast(4).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.cast(3).source, first.source);
        for (source, target) in [
            (ColType::Int4, ColType::Int4),
            (ColType::Int4, ColType::Int8),
        ] {
            let invalid = CastDef {
                created_at: 42,
                source: RoutineResult::builtin(source),
                target: RoutineResult::builtin(target),
                ..first
            };
            assert_eq!(
                storage
                    .restore_prepared_cast(CatalogPosition::new(4, 42).unwrap(), invalid, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_OBJECT
            );
            assert_eq!(
                storage
                    .create_cast_from_image(CatalogPosition::new(4, 42).unwrap(), invalid)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_OBJECT
            );
            assert_eq!(storage.cast(4).ddl_state, CatalogDdlState::Absent);
        }
    });
}

#[test]
fn restored_operator_classes_keep_one_default_for_each_input_type() {
    let mut config = test_config();
    config.max_operator_families = 4;
    config.max_operator_classes = 8;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let family = OperatorFamilyDefinition {
        schema: SqlName::parse("public").unwrap(),
        name: SqlName::parse("saved_family").unwrap(),
        owner: storage.role(0).oid().get(),
        ..OperatorFamilyDefinition::EMPTY
    };
    crate::mem::guard::forbid_alloc(|| {
        storage
            .restore_prepared_operator_family(CatalogPosition::new(1, 31).unwrap(), family, 7)
            .unwrap();
        storage.commit_operator_family_create(1);
        let definition = OperatorClassDefinition {
            schema: family.schema,
            name: SqlName::parse("saved_default").unwrap(),
            owner: family.owner,
            family: storage.operator_family(1).oid(),
            input: RoutineResult::builtin(ColType::Json),
            storage: RoutineResult::builtin(ColType::Json),
            default: true,
            ..OperatorClassDefinition::EMPTY
        };
        storage
            .restore_prepared_operator_class(CatalogPosition::new(3, 41).unwrap(), definition, 7)
            .unwrap();
        storage.commit_operator_class_create(3);
        let other = OperatorClassDefinition {
            name: SqlName::parse("duplicate_default").unwrap(),
            ..definition
        };
        let before = storage.catalog_identity_sequence();
        let error = storage
            .restore_prepared_operator_class(CatalogPosition::new(4, 42).unwrap(), other, 9)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        let error = storage
            .replay_set_operator_class(CatalogPosition::new(4, 42).unwrap(), other)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.operator_class(4).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.catalog_identity_sequence(), before);
        let builtin = OperatorClassDefinition {
            input: RoutineResult::builtin(ColType::Int4),
            storage: RoutineResult::builtin(ColType::Int4),
            ..other
        };
        assert_eq!(
            storage
                .restore_prepared_operator_class(CatalogPosition::new(4, 42).unwrap(), builtin, 9)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(
            storage
                .replay_set_operator_class(CatalogPosition::new(4, 42).unwrap(), builtin)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(storage.operator_class(4).ddl_state, CatalogDdlState::Absent);
    });
}

#[test]
fn prepared_membership_regrant_keeps_the_committed_identity_until_publication() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (role, _) = storage
            .create_role(
                SqlName::parse("membership_role").unwrap(),
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap();
        storage.commit_role_change(role);
        let (member, _) = storage
            .create_role(
                SqlName::parse("membership_member").unwrap(),
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap();
        storage.commit_role_change(member);
        let (slot, _) = storage
            .change_role_membership(role, member, 0, RoleMembershipOptions::DEFAULT, true, 7)
            .unwrap();
        storage.commit_role_membership_change(slot);
        let original = storage.role_membership(slot).creation_to(0);
        let replacement = CatalogPosition::new(slot, original + 10).unwrap();
        let role_oid = storage.role(role).oid();
        let member_oid = storage.role(member).oid();
        let options = RoleMembershipOptions {
            admin: true,
            ..RoleMembershipOptions::DEFAULT
        };
        let (_, prior) = storage
            .restore_prepared_role_membership(
                replacement,
                role_oid,
                member_oid,
                RoleOid::BOOTSTRAP,
                options,
                7,
            )
            .unwrap();
        let pending = storage.role_membership(slot);
        assert_eq!(pending.creation_to(9), original);
        assert!(!pending.options_to(9).admin);
        assert_eq!(pending.creation_to(7), replacement.created_at());
        assert!(pending.options_to(7).admin);
        storage.rollback_role_membership_change(slot, prior);
        assert_eq!(storage.role_membership(slot).creation_to(7), original);
        storage
            .restore_prepared_role_membership(
                replacement,
                role_oid,
                member_oid,
                RoleOid::BOOTSTRAP,
                options,
                7,
            )
            .unwrap();
        storage.commit_role_membership_change(slot);
        assert_eq!(
            storage.role_membership(slot).creation_to(0),
            replacement.created_at()
        );
        let stale = storage
            .restore_prepared_role_membership(
                CatalogPosition::new(slot, original).unwrap(),
                role_oid,
                member_oid,
                RoleOid::BOOTSTRAP,
                options,
                8,
            )
            .unwrap_err();
        assert_eq!(stale.sqlstate, sqlstate::DATA_EXCEPTION);
    });
}

#[test]
fn prepared_catalog_rejects_dropped_sources_and_duplicate_lifetimes_atomically() {
    let mut config = test_config();
    config.max_collations = 8;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let definition = collation(&storage, "saved_collation");
        let undo = storage
            .restore_prepared_collation(3, 41, definition, 7)
            .unwrap();
        assert!(matches!(undo, DdlUndo::CollationCreated(3)));
        assert!(
            storage
                .collation_slot("public", "saved_collation", 0)
                .is_none()
        );
        assert_eq!(
            storage.collation_slot("public", "saved_collation", 7),
            Some(3)
        );
        let sequence = storage.catalog_identity_sequence();
        let error = storage
            .restore_prepared_collation(4, 41, collation(&storage, "same_lifetime"), 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.collation(4).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.catalog_identity_sequence(), sequence);
        storage.drop_collation(3, 7);
        let dropped = storage.collation(3).ddl_state;
        let error = storage
            .restore_prepared_collation(3, 41, definition, 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.collation(3).ddl_state, dropped);
        assert!(storage.collation(3).pending.is_none());
        storage.rollback_collation_drop(3, 7);
        storage.commit_collation_create(3);
        let altered = CollationDefinition {
            name: SqlName::parse("private_name").unwrap(),
            ..definition
        };
        let undo = storage
            .restore_prepared_collation(3, 41, altered, 9)
            .unwrap();
        assert!(matches!(undo, DdlUndo::CollationAltered { slot: 3, .. }));
        assert_eq!(storage.collation(3).definition_for(0).name, definition.name);
        assert_eq!(storage.collation(3).definition_for(9).name, altered.name);
        for invalid in [
            CollationDefinition {
                schema: SqlName::parse("absent_schema").unwrap(),
                ..definition
            },
            CollationDefinition {
                owner: 2_000_000_000,
                ..definition
            },
        ] {
            assert!(
                storage
                    .restore_prepared_collation(5, 42, invalid, 9)
                    .is_err()
            );
            assert_eq!(storage.collation(5).ddl_state, CatalogDdlState::Absent);
            assert_eq!(storage.collation(3).definition_for(0).name, definition.name);
        }
        assert!(
            storage
                .restore_prepared_collation(8, 42, definition, 9)
                .is_err()
        );
        assert_eq!(storage.collation(3).definition_for(9).name, altered.name);
    });
}

#[test]
fn prepared_role_cannot_replace_its_own_uncommitted_identifier() {
    let mut config = test_config();
    config.max_roles = 4;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let name = SqlName::parse("saved_role").unwrap();
        let first = RoleOid::parse(20_000).unwrap();
        storage
            .restore_prepared_role(USER_SLOT + 2, first, name, RoleAttributes::ORDINARY, 7)
            .unwrap();
        let error = storage
            .restore_prepared_role(
                USER_SLOT + 2,
                RoleOid::parse(20_001).unwrap(),
                name,
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.role(USER_SLOT + 2).oid(), first);
        assert_eq!(storage.find_role_visible("saved_role", 0), None);
        assert_eq!(
            storage.find_role_visible("saved_role", 7),
            Some(USER_SLOT + 2)
        );
        assert!(
            storage
                .restore_prepared_role(
                    USER_SLOT + 1,
                    first,
                    SqlName::parse("duplicate_role").unwrap(),
                    RoleAttributes::ORDINARY,
                    7
                )
                .is_err()
        );
        assert_eq!(storage.find_role_visible("duplicate_role", 7), None);
        let error = storage
            .restore_prepared_role(
                USER_SLOT + 1,
                first,
                SqlName::parse("foreign_duplicate_role").unwrap(),
                RoleAttributes::ORDINARY,
                9,
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.role(USER_SLOT + 2).oid(), first);
        assert_eq!(storage.find_role_visible("foreign_duplicate_role", 9), None);
    });
}

#[test]
fn role_rename_targets_reserve_names_without_hiding_own_freed_names() {
    let mut config = test_config();
    config.max_roles = 5;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let original = SqlName::parse("rename_original").unwrap();
        let target = SqlName::parse("rename_target").unwrap();
        let (source, _) = storage
            .create_role(original, RoleAttributes::ORDINARY, 7)
            .unwrap();
        storage.commit_role_change(source);
        let (other, _) = storage
            .create_role(
                SqlName::parse("rename_other").unwrap(),
                RoleAttributes::ORDINARY,
                9,
            )
            .unwrap();
        storage.commit_role_change(other);
        let prior = storage.rename_role(source, target, 7).unwrap();
        let error = storage
            .create_role(target, RoleAttributes::ORDINARY, 9)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        let error = storage.rename_role(other, target, 9).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        assert_eq!(storage.role(other).name_to(9).as_str(), "rename_other");
        let (replacement, replacement_prior) = storage
            .create_role(original, RoleAttributes::ORDINARY, 7)
            .unwrap();
        assert_ne!(replacement, source);
        assert_eq!(
            storage.find_role_visible("rename_original", 7),
            Some(replacement)
        );
        storage.rollback_role_change(replacement, replacement_prior);
        storage.rollback_role_change(source, prior);
        assert_eq!(
            storage.find_role_visible("rename_original", 0),
            Some(source)
        );
        assert_eq!(storage.find_role_visible("rename_target", 0), None);
    });
}

#[test]
fn restored_role_renames_keep_original_and_target_names_reserved() {
    let mut config = test_config();
    config.max_roles = 5;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let original = SqlName::parse("restored_original").unwrap();
        let target = SqlName::parse("restored_target").unwrap();
        let first = RoleOid::parse(20_000).unwrap();
        storage
            .restore_prepared_role(USER_SLOT + 2, first, original, RoleAttributes::ORDINARY, 7)
            .unwrap();
        storage
            .restore_prepared_role(USER_SLOT + 2, first, target, RoleAttributes::ORDINARY, 7)
            .unwrap();
        assert_eq!(storage.role(USER_SLOT + 2).name, original);
        for (index, name) in [original, target].into_iter().enumerate() {
            let error = storage
                .restore_prepared_role(
                    USER_SLOT + 1,
                    RoleOid::parse(20_001 + index as i32).unwrap(),
                    name,
                    RoleAttributes::ORDINARY,
                    9,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
            let error = storage
                .create_role(name, RoleAttributes::ORDINARY, 9)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        }
        storage
            .restore_prepared_role(
                USER_SLOT + 1,
                RoleOid::parse(20_003).unwrap(),
                original,
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap();
        assert_eq!(
            storage.find_role_visible("restored_original", 7),
            Some(USER_SLOT + 1)
        );
        assert_eq!(
            storage.find_role_visible("restored_target", 7),
            Some(USER_SLOT + 2)
        );
    });
}

#[test]
fn prepared_default_privileges_cannot_rekey_a_committed_grant() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let arena = crate::mem::Arena::new(&mut budget, "default privilege views", 4096).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let original = DefaultAclGrantImage {
            identity: DefaultAclIdentity {
                creation: CatalogIdentity::require(41).unwrap(),
                owner: RoleOid::BOOTSTRAP,
                schema: None,
                class: DefaultPrivilegeClass::Table,
            },
            grantee: None,
            privileges: PrivilegeSet::SELECT,
            grant_options: PrivilegeSet::NONE,
        };
        storage
            .install_default_acl_image(0, None, Some(original), 0)
            .unwrap();
        let redirected = DefaultAclGrantImage {
            grantee: Some(RoleOid::BOOTSTRAP),
            privileges: PrivilegeSet::NONE,
            ..original
        };
        let error = storage
            .stage_default_acl_image(0, Some(original), Some(redirected), 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(
            storage.snapshot_default_acl_grants(9, &arena).unwrap(),
            [original]
        );
        let reduced = DefaultAclGrantImage {
            privileges: PrivilegeSet::UPDATE,
            ..original
        };
        storage
            .stage_default_acl_image(0, Some(original), Some(reduced), 7)
            .unwrap();
        assert_eq!(
            storage.snapshot_default_acl_grants(9, &arena).unwrap(),
            [original]
        );
        assert_eq!(
            storage.snapshot_default_acl_grants(7, &arena).unwrap(),
            [reduced]
        );
        let error = storage
            .stage_default_acl_image(0, Some(reduced), Some(redirected), 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(
            storage.snapshot_default_acl_grants(9, &arena).unwrap(),
            [original]
        );
        assert_eq!(
            storage.snapshot_default_acl_grants(7, &arena).unwrap(),
            [reduced]
        );
        let catalog = storage.acl_catalog.lock().unwrap();
        assert_eq!(catalog.defaults[0].grantee, PUBLIC_ROLE);
        assert_eq!(catalog.defaults[0].owner, BOOTSTRAP_ROLE);
    });
}

#[test]
fn restoring_prepared_large_objects_does_not_reserve_a_fresh_identity() {
    let mut config = test_config();
    config.max_large_objects = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage.replace_catalog_sequence_for_test(u64::MAX);
        let oid = LargeObjectOid::parse(40_000).unwrap();
        let slot = storage
            .restore_prepared_large_object(oid, 41, true, 7)
            .unwrap();
        assert_eq!(storage.catalog_identity_sequence(), u64::MAX);
        assert_eq!(storage.large_object(slot).created_at, 41);
        assert!(storage.large_object(slot).allocated);
        assert_eq!(storage.large_object_slot(oid, 0), None);
        assert_eq!(storage.large_object_slot(oid, 7), Some(slot));
        let next = LargeObjectOid::parse(40_001).unwrap();
        for state in 0..3 {
            if state == 1 {
                storage.commit_large_object_create(slot);
            } else if state == 2 {
                assert_eq!(storage.drop_large_object(oid, 7).unwrap(), Some(slot));
            }
            let before = storage.large_object(slot);
            let error = storage
                .restore_prepared_large_object(next, 41, false, 9)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
            let after = storage.large_object(slot);
            assert_eq!(after.oid, before.oid);
            assert_eq!(after.created_at, before.created_at);
            assert_eq!(after.ddl_state, before.ddl_state);
            assert_eq!(after.allocated, before.allocated);
            assert_eq!(storage.large_object_slot(next, 9), None);
            assert_eq!(storage.catalog_identity_sequence(), u64::MAX);
        }
        let error = storage
            .create_large_object(CatalogCreation::New, Some(next), 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(storage.large_object_slot(next, 7), None);
        assert_eq!(storage.large_object(slot).created_at, 41);
        storage.rollback_large_object_drop(slot, 7);
        assert_eq!(storage.large_object_slot(oid, 0), Some(slot));
        storage.drop_large_object(oid, 7).unwrap();
        storage.commit_large_object_drop(slot);
        let replacement = storage
            .restore_prepared_large_object(next, 41, false, 9)
            .unwrap();
        assert_eq!(replacement, slot);
        assert_eq!(storage.large_object_slot(next, 9), Some(slot));
        storage.rollback_large_object_create(replacement);
        assert_eq!(storage.large_object_slot(next, 9), None);
    });
}

#[test]
fn restored_subscription_owners_require_a_role_visible_to_the_reference_transaction() {
    let mut config = test_config();
    config.max_roles = 4;
    config.max_subscriptions = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let publications = [SqlName::parse("saved_publication").unwrap()];
        let slot = storage
            .create_subscription(
                SubscriptionSpec {
                    name: SqlName::parse("saved_subscription").unwrap(),
                    connection: SubscriptionConnInfo::parse("host=127.0.0.1 port=1").unwrap(),
                    publications: &publications,
                    enabled: false,
                    slot: SubscriptionSlot::Absent,
                    behavior: SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                    bootstrap: SubscriptionBootstrap::Deferred,
                },
                7,
            )
            .unwrap();
        let owner = |storage: &Storage| {
            storage
                .subscription("saved_subscription", 7)
                .unwrap()
                .1
                .ownership
                .owner
        };
        for invalid in [u16::MAX, (config.role_catalog_slots() - 1) as u16] {
            let error = storage
                .restore_subscription_owner(slot, invalid, 0)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::UNDEFINED_OBJECT);
            assert_eq!(owner(&storage), BOOTSTRAP_ROLE);
        }
        let (role, _) = storage
            .create_role(
                SqlName::parse("prepared_subscription_owner").unwrap(),
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap();
        let error = storage
            .restore_subscription_owner(slot, role as u16, 0)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(owner(&storage), BOOTSTRAP_ROLE);
        storage
            .restore_subscription_owner(slot, role as u16, 7)
            .unwrap();
        assert_eq!(owner(&storage), role as u16);
        storage
            .restore_subscription_owner(slot, BOOTSTRAP_ROLE, 0)
            .unwrap();
        storage.commit_role_change(role);
        storage
            .restore_subscription_owner(slot, role as u16, 0)
            .unwrap();
        assert_eq!(owner(&storage), role as u16);
        let error = storage
            .restore_subscription_owner(slot, u16::MAX, 0)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(owner(&storage), role as u16);
    });
}
