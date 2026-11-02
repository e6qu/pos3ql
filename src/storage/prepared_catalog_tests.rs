//! Malformed prepared identities fail before changing committed catalog views.

use super::tests::{test_budget, test_config};
use super::*;
use crate::sql::txn::DdlUndo;

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
        let original = storage.role_membership(slot).creation_to(0).created_at();
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
        assert_eq!(pending.creation_to(9).created_at(), original);
        assert!(!pending.options_to(9).admin);
        assert_eq!(
            pending.creation_to(7).created_at(),
            replacement.created_at()
        );
        assert!(pending.options_to(7).admin);
        storage.rollback_role_membership_change(slot, prior);
        assert_eq!(
            storage.role_membership(slot).creation_to(7).created_at(),
            original
        );
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
            storage.role_membership(slot).creation_to(0).created_at(),
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
            .restore_prepared_role(3, first, name, RoleAttributes::ORDINARY, 7)
            .unwrap();
        let error = storage
            .restore_prepared_role(
                3,
                RoleOid::parse(20_001).unwrap(),
                name,
                RoleAttributes::ORDINARY,
                7,
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert_eq!(storage.role(3).oid(), first);
        assert_eq!(storage.find_role_visible("saved_role", 0), None);
        assert_eq!(storage.find_role_visible("saved_role", 7), Some(3));
        assert!(
            storage
                .restore_prepared_role(
                    2,
                    first,
                    SqlName::parse("duplicate_role").unwrap(),
                    RoleAttributes::ORDINARY,
                    7
                )
                .is_err()
        );
        assert_eq!(storage.find_role_visible("duplicate_role", 7), None);
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
        let error = storage
            .create_large_object(CatalogCreation::New, Some(next), 7)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(storage.large_object_slot(next, 7), None);
        assert_eq!(storage.large_object(slot).created_at, 41);
        storage.rollback_large_object_create(slot);
        assert_eq!(storage.large_object_slot(oid, 7), None);
    });
}
