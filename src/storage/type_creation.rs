//! Creation keeps permission cleanup and type installation in the same publication.

use super::row_ownership::{LockWait, try_mutex};
use super::{AclCatalog, Storage, TypeCatalog};
use crate::mem::mutex::MutexGuard;

impl Storage {
    pub(super) fn type_creation_catalogs(
        &self,
    ) -> (MutexGuard<'_, AclCatalog>, MutexGuard<'_, TypeCatalog>) {
        loop {
            let attempt: Result<_, LockWait<'_>> = (|| {
                let permissions = try_mutex(&self.acl_catalog, "ACL catalog lock poisoned")?;
                let types = try_mutex(&self.type_catalog, "type catalog lock poisoned")?;
                Ok((permissions, types))
            })();
            match attempt {
                Ok(owners) => return owners,
                Err(wait) => {
                    #[cfg(test)]
                    self.definition_publication
                        .waits
                        .fetch_add(1, core::sync::atomic::Ordering::Release);
                    wait.wait();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config, test_view_definition};
    use super::super::{
        CheckConstraint, ColType, CompositeFieldDef, CompositeSpec, DomainSpec, EnumMember,
        EnumSpec, MAX_COMPOSITE_FIELDS, MAX_DOMAIN_CHECKS, NamedTypeKind, ObjectLockTarget,
        RoleAttributes, SessionIdentity, SqlName, TypeDdlDecision, TypeDdlUndo,
    };
    use super::*;
    use crate::config::TransactionMode;
    use crate::sql::eval::{SqlError, sqlstate};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const KINDS: [NamedTypeKind; 3] = [
        NamedTypeKind::Domain,
        NamedTypeKind::Enum,
        NamedTypeKind::Composite,
    ];

    #[test]
    fn shared_type_creation_excludes_racing_table_and_view_names() {
        #[derive(Debug)]
        enum RowCreation {
            Table(usize),
            View(super::super::CatalogDdlUndo),
        }
        for kind in KINDS {
            for view in [false, true] {
                let storage = fixture(TransactionMode::Mvcc);
                let start = std::sync::Barrier::new(2);
                let (named, row) = std::thread::scope(|scope| {
                    let named = scope.spawn(|| {
                        start.wait();
                        crate::mem::guard::forbid_alloc(|| {
                            create(&storage, kind, "racing_row_name", 7)
                        })
                    });
                    let row = scope.spawn(|| {
                        start.wait();
                        crate::mem::guard::forbid_alloc(|| {
                            if view {
                                storage
                                    .create_view(
                                        SqlName::parse("public").unwrap(),
                                        SqlName::parse("racing_row_name").unwrap(),
                                        test_view_definition(
                                            super::super::StoredQueryDependencies::EMPTY,
                                        ),
                                        false,
                                        9,
                                    )
                                    .map(|publication| match publication {
                                        super::super::ViewPublication::Created(undo) => {
                                            RowCreation::View(undo)
                                        }
                                        super::super::ViewPublication::Altered(_) => {
                                            panic!("creation replaced a view")
                                        }
                                    })
                            } else {
                                storage
                                    .create_table_in(
                                        make_def(
                                            "racing_row_name",
                                            &[("id", ColType::Int4, false)],
                                        ),
                                        9,
                                    )
                                    .map(RowCreation::Table)
                            }
                        })
                    });
                    (named.join().unwrap(), row.join().unwrap())
                });
                match (named, row) {
                    (Ok(created), Err(error)) => {
                        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
                        storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
                    }
                    (Err(error), Ok(row)) => {
                        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
                        match row {
                            RowCreation::View(undo) => storage.finish_view_publication(
                                super::super::ViewPublication::Created(undo),
                                super::super::CatalogDdlDecision::Rollback,
                            ),
                            RowCreation::Table(slot) => storage.rollback_create(slot),
                        }
                    }
                    outcomes => panic!(
                        "unexpected namespace outcomes for {kind:?}, view={view}: {outcomes:?}"
                    ),
                }
                let retry = create(&storage, kind, "racing_row_name", 11).unwrap();
                storage.finish_type_ddl(retry, TypeDdlDecision::Rollback);
            }
        }
    }

    #[test]
    fn shared_type_creation_uses_names_without_waiting_for_unrelated_definitions() {
        for kind in KINDS {
            let mut storage = fixture(TransactionMode::Mvcc);
            let table = storage
                .create_table(make_def("unrelated_name", &[("id", ColType::Int4, false)]))
                .unwrap();
            let Ok(held) = storage.tables[table].definition.test_write() else {
                panic!("fixture definition owner");
            };
            let (finished, received) = std::sync::mpsc::sync_channel(1);
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    let created = crate::mem::guard::forbid_alloc(|| {
                        create(&storage, kind, "independent_name", 7)
                    });
                    finished.send(()).unwrap();
                    created
                });
                let progress = received.recv_timeout(Duration::from_secs(5));
                drop(held);
                let created = writer.join().unwrap().unwrap();
                assert!(progress.is_ok());
                storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
            });
        }
    }

    #[test]
    fn shared_type_creation_keeps_retiring_names_until_comment_cleanup() {
        for kind in KINDS {
            let mut storage = fixture(TransactionMode::Mvcc);
            let table = storage
                .create_table(make_def("cleanup_name", &[("id", ColType::Int4, false)]))
                .unwrap();
            storage
                .apply_comment(
                    super::super::CommentClass::Relation,
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("cleanup_name").unwrap(),
                    0,
                    Some(super::super::comment_stackstr("old comment").unwrap()),
                )
                .unwrap();
            let comments = storage.comments.lock().unwrap();
            let before = storage.definition_publication.waits.load(Ordering::Acquire);
            let (finished, received) = std::sync::mpsc::sync_channel(1);
            std::thread::scope(|scope| {
                let dropper =
                    scope.spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_drop(table)));
                let waited = saw_wait(&storage, before);
                let retained = storage.tables[table].live();
                let writer = scope.spawn(|| {
                    let result = crate::mem::guard::forbid_alloc(|| {
                        (
                            create(&storage, kind, "independent_name", 7),
                            create(&storage, kind, "cleanup_name", 7),
                        )
                    });
                    finished.send(()).unwrap();
                    result
                });
                let progress = received.recv_timeout(Duration::from_secs(5));
                drop(comments);
                dropper.join().unwrap();
                let (independent, conflicting) = writer.join().unwrap();
                assert!(waited && retained && progress.is_ok());
                assert_eq!(
                    conflicting.unwrap_err().sqlstate,
                    sqlstate::DUPLICATE_OBJECT
                );
                storage.finish_type_ddl(independent.unwrap(), TypeDdlDecision::Rollback);
                let created =
                    crate::mem::guard::forbid_alloc(|| create(&storage, kind, "cleanup_name", 7))
                        .unwrap();
                assert!(
                    storage
                        .comment_text(
                            super::super::CommentClass::Relation,
                            "public",
                            "cleanup_name",
                            0,
                            7
                        )
                        .is_none()
                );
                storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
            });
        }
    }

    fn fixture(mode: TransactionMode) -> Storage {
        let mut config = test_config();
        config.transaction_mode = mode;
        config.max_roles = 4;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    fn role(storage: &Storage, name: &str) -> usize {
        let (slot, _) = storage
            .create_role(
                SqlName::parse(name).unwrap(),
                RoleAttributes {
                    superuser: true,
                    ..RoleAttributes::ORDINARY
                },
                1,
            )
            .unwrap();
        storage.commit_role_change(slot);
        slot
    }

    fn create(
        storage: &Storage,
        kind: NamedTypeKind,
        name: &str,
        transaction: u32,
    ) -> Result<TypeDdlUndo, SqlError> {
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse(name).unwrap();
        match kind {
            NamedTypeKind::Domain => storage.create_domain(
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
                transaction,
            ),
            NamedTypeKind::Enum => storage.create_enum(
                schema,
                name,
                EnumSpec {
                    members: &[EnumMember {
                        identity: None,
                        label: SqlName::parse("ready").unwrap(),
                        legacy_label: None,
                        sort: 1.0,
                    }],
                },
                transaction,
            ),
            NamedTypeKind::Composite => storage.create_composite(
                schema,
                name,
                CompositeSpec {
                    fields: [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS],
                    n_fields: 0,
                },
                transaction,
            ),
        }
    }

    fn saw_wait(storage: &Storage, before: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while storage.definition_publication.waits.load(Ordering::Acquire) == before {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    #[test]
    fn shared_type_creation_reserves_one_name_across_every_kind_pair() {
        for left in KINDS {
            for right in KINDS {
                let storage = fixture(TransactionMode::Mvcc);
                let start = std::sync::Barrier::new(2);
                std::thread::scope(|scope| {
                    let first = scope.spawn(|| {
                        start.wait();
                        crate::mem::guard::forbid_alloc(|| create(&storage, left, "shared_name", 7))
                    });
                    let second = scope.spawn(|| {
                        start.wait();
                        crate::mem::guard::forbid_alloc(|| {
                            create(&storage, right, "shared_name", 9)
                        })
                    });
                    let outcomes = [first.join().unwrap(), second.join().unwrap()];
                    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
                    for outcome in outcomes {
                        match outcome {
                            Ok(created) => {
                                storage.finish_type_ddl(created, TypeDdlDecision::Rollback)
                            }
                            Err(error) => assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT),
                        }
                    }
                });
            }
        }
    }

    #[test]
    fn shared_type_creation_releases_partial_catalog_owners_before_waiting() {
        for kind in KINDS {
            for blocked in 0..2 {
                let storage = fixture(TransactionMode::Mvcc);
                let permissions = (blocked == 0).then(|| storage.acl_catalog.lock().unwrap());
                let types = (blocked == 1).then(|| storage.type_catalog.lock().unwrap());
                let before = storage.definition_publication.waits.load(Ordering::Acquire);
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::mem::guard::forbid_alloc(|| {
                            create(&storage, kind, "waiting_type", 7)
                        })
                    });
                    let waited = saw_wait(&storage, before);
                    let released = if blocked == 0 {
                        storage.type_catalog.try_lock().is_ok()
                    } else {
                        storage.acl_catalog.try_lock().is_ok()
                    };
                    drop(types);
                    drop(permissions);
                    let created = writer.join().unwrap().unwrap();
                    assert!(waited && released);
                    storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
                });
            }
        }
    }

    #[test]
    fn shared_type_creation_protects_its_owner_while_waiting_for_catalogs() {
        for kind in KINDS {
            let storage = fixture(TransactionMode::Mvcc);
            let owner = role(&storage, "waiting_owner");
            let actor = SessionIdentity::bound(storage.capture_role_slot(owner, 0).unwrap());
            let types = storage.type_catalog.lock().unwrap();
            let before = storage.definition_publication.waits.load(Ordering::Acquire);
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    let _actor = crate::sql::eval::funcs::system::enter_current_user(actor);
                    crate::mem::guard::forbid_alloc(|| create(&storage, kind, "owned_type", 7))
                });
                let waited = saw_wait(&storage, before);
                let deletion = crate::mem::guard::forbid_alloc(|| {
                    storage.lock_object(
                        8,
                        ObjectLockTarget::Role(storage.role(owner).oid()),
                        crate::sql::ast::TableLockMode::AccessExclusive,
                        true,
                    )
                });
                drop(types);
                let created = writer.join().unwrap().unwrap();
                assert!(waited);
                assert_eq!(deletion.unwrap_err().sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
                storage.finish_type_ddl(created, TypeDdlDecision::Rollback);
                storage.release_object_locks(7);
                let dropped = storage.drop_role_in(owner, 8).unwrap();
                storage.rollback_role_change(owner, dropped);
                storage.release_object_locks(8);
            });
        }
    }

    #[test]
    fn role_reference_locks_allow_shared_creators_and_block_deletion_in_both_modes() {
        for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
            let storage = fixture(mode);
            let owner = role(&storage, "shared_owner");
            crate::mem::guard::forbid_alloc(|| {
                storage.role_reference_ready(owner, 7).unwrap();
                storage.role_reference_ready(owner, 9).unwrap();
                assert_eq!(
                    storage.drop_role_in(owner, 8).unwrap_err().sqlstate,
                    sqlstate::INTERNAL_LOCK_WAIT
                );
                storage.release_object_locks(7);
                assert_eq!(
                    storage.drop_role_in(owner, 8).unwrap_err().sqlstate,
                    sqlstate::INTERNAL_LOCK_WAIT
                );
                storage.release_object_locks(9);
                let dropped = storage.drop_role_in(owner, 8).unwrap();
                assert_eq!(
                    storage.role_reference_ready(owner, 7).unwrap_err().sqlstate,
                    sqlstate::INTERNAL_LOCK_WAIT
                );
                storage.rollback_role_change(owner, dropped);
                storage.release_object_locks(8);
            });
        }
    }

    #[test]
    fn role_reference_locks_follow_savepoint_boundaries() {
        let storage = fixture(TransactionMode::Mvcc);
        let first = role(&storage, "first_owner");
        let second = role(&storage, "second_owner");
        crate::mem::guard::forbid_alloc(|| {
            storage.role_reference_ready(first, 7).unwrap();
            let mark = storage.lock_mark();
            storage.role_reference_ready(second, 7).unwrap();
            storage.rollback_locks_to(7, mark);
            let dropped = storage.drop_role_in(second, 8).unwrap();
            storage.rollback_role_change(second, dropped);
            storage.release_object_locks(8);
            assert_eq!(
                storage.drop_role_in(first, 9).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.release_object_locks(7);
            let dropped = storage.drop_role_in(first, 9).unwrap();
            storage.rollback_role_change(first, dropped);
            storage.release_object_locks(9);
        });
    }

    #[test]
    fn role_reference_locks_do_not_transfer_to_reused_catalog_positions() {
        let storage = fixture(TransactionMode::Mvcc);
        let old = role(&storage, "old_owner");
        let original = storage.role(old).oid();
        crate::mem::guard::forbid_alloc(|| {
            storage.drop_role_in(old, 7).unwrap();
            storage.commit_role_change(old);
            let replacement = role(&storage, "new_owner");
            assert_eq!(replacement, old);
            assert_ne!(storage.role(replacement).oid(), original);
            storage.role_reference_ready(replacement, 8).unwrap();
            storage.release_object_locks(7);
            assert_eq!(
                storage.drop_role_in(replacement, 9).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            storage.release_object_locks(8);
            let dropped = storage.drop_role_in(replacement, 9).unwrap();
            storage.rollback_role_change(replacement, dropped);
            storage.release_object_locks(9);
        });
    }

    #[test]
    fn role_reference_locks_survive_prepared_inventory_recovery() {
        let storage = fixture(TransactionMode::Mvcc);
        let first = role(&storage, "prepared_first_owner");
        let second = role(&storage, "prepared_second_owner");
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut encoded =
            crate::mem::buffer::FixedBuf::new(&mut budget, "prepared account locks", 512).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.role_reference_ready(first, 7).unwrap();
            storage.role_reference_ready(second, 7).unwrap();
            storage.encode_transaction_locks(7, &mut encoded).unwrap();
            assert_eq!(encoded.len(), 24);
            storage.release_object_locks(7);
            storage.begin_prepared_transaction_identity(15).unwrap();
            storage
                .restore_transaction_locks(15, encoded.readable())
                .unwrap();
            for (owner, transaction) in [(first, 8), (second, 9)] {
                assert_eq!(
                    storage
                        .drop_role_in(owner, transaction)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::INTERNAL_LOCK_WAIT
                );
            }
            storage.release_object_locks(15);
            storage.finish_transaction_identity(15, false);
            for (owner, transaction) in [(first, 8), (second, 9)] {
                let dropped = storage.drop_role_in(owner, transaction).unwrap();
                storage.rollback_role_change(owner, dropped);
                storage.release_object_locks(transaction);
            }
        });
    }

    #[test]
    fn role_reference_lock_recovery_rejects_invalid_identities_modes_and_names() {
        let storage = fixture(TransactionMode::Mvcc);
        let owner = role(&storage, "restored_owner");
        let oid = storage.role(owner).oid();
        let sequence = storage.role_identity_sequence();
        crate::mem::guard::forbid_alloc(|| {
            let mut encoded = [0u8; 12];
            encoded[0] = 4;
            for invalid in [0u64, 10, 16_384, i32::MAX as u64 + 1, u64::MAX] {
                encoded[2..10].copy_from_slice(&invalid.to_le_bytes());
                assert_eq!(
                    storage
                        .restore_transaction_locks(7, &encoded)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
            encoded[2..10].copy_from_slice(&(oid.get() as u64).to_le_bytes());
            for invalid in [1, 2, 3, 4, 5, 6, 8, u8::MAX] {
                encoded[1] = invalid;
                assert_eq!(
                    storage
                        .restore_transaction_locks(7, &encoded)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
            let mut named = [0u8; 13];
            named[..10].copy_from_slice(&encoded[..10]);
            named[1] = 0;
            named[10] = 1;
            named[11] = b'x';
            assert_eq!(
                storage
                    .restore_transaction_locks(7, &named)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.object_lock_view_count(), 0);
            assert_eq!(storage.role_identity_sequence(), sequence);
        });
    }
}
