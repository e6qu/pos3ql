//! One namespace for domain, enum and named composite identities.

use super::{
    CatalogDdlState, DatabaseOid, SqlError, SqlName, Storage, TypeCatalog, UserTypeName,
    current_database, sqlstate,
};
use crate::sql_err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NamedTypeSlot {
    Domain(usize),
    Enum(usize),
    Composite(usize),
}

struct NameReservation {
    slot: NamedTypeSlot,
    database: DatabaseOid,
    state: CatalogDdlState,
    current: UserTypeName,
    pending: Option<(u32, UserTypeName)>,
}

impl TypeCatalog {
    fn name_reservations(&self) -> impl Iterator<Item = NameReservation> + '_ {
        self.domains
            .iter()
            .enumerate()
            .map(|(slot, definition)| {
                let current = UserTypeName {
                    schema: definition.schema,
                    name: definition.name,
                };
                NameReservation {
                    slot: NamedTypeSlot::Domain(slot),
                    database: definition.database,
                    state: definition.ddl_state,
                    current,
                    pending: definition.pending_definition.map(|pending| {
                        (
                            pending.txid,
                            pending.identity.map_or(current, |identity| UserTypeName {
                                schema: identity.schema,
                                name: identity.name,
                            }),
                        )
                    }),
                }
            })
            .chain(
                self.enums
                    .iter()
                    .enumerate()
                    .map(|(slot, definition)| NameReservation {
                        slot: NamedTypeSlot::Enum(slot),
                        database: definition.database,
                        state: definition.ddl_state,
                        current: UserTypeName {
                            schema: definition.schema,
                            name: definition.name,
                        },
                        pending: definition.pending_definition.map(|pending| {
                            (
                                pending.txid,
                                UserTypeName {
                                    schema: pending.schema,
                                    name: pending.name,
                                },
                            )
                        }),
                    }),
            )
            .chain(
                self.composites
                    .iter()
                    .enumerate()
                    .map(|(slot, definition)| NameReservation {
                        slot: NamedTypeSlot::Composite(slot),
                        database: definition.database,
                        state: definition.ddl_state,
                        current: UserTypeName {
                            schema: definition.schema,
                            name: definition.name,
                        },
                        pending: definition.pending_definition.map(|pending| {
                            (
                                pending.txid,
                                UserTypeName {
                                    schema: pending.schema,
                                    name: pending.name,
                                },
                            )
                        }),
                    }),
            )
    }
}

impl Storage {
    // The caller keeps this catalog locked through validation and publication.
    // Other transactions reserve both names until a rename commits or rolls back.
    pub(super) fn require_named_type_name(
        &self,
        catalog: &TypeCatalog,
        schema: SqlName,
        name: SqlName,
        txid: u32,
        excluding: Option<NamedTypeSlot>,
    ) -> Result<(), SqlError> {
        let target = UserTypeName { schema, name };
        let mut exists = false;
        for reservation in catalog.name_reservations() {
            if reservation.database != current_database()
                || reservation.state == CatalogDdlState::Absent
                || Some(reservation.slot) == excluding
            {
                continue;
            }
            let reserved = reservation.current == target
                || reservation
                    .pending
                    .is_some_and(|(_, pending)| pending == target);
            let owner = reservation
                .pending
                .map(|(owner, _)| owner)
                .or_else(|| reservation.state.pending_txid());
            if reserved && let Some(owner) = owner.filter(|&owner| owner != txid) {
                return Err(self.catalog_ddl_wait_error(txid, owner, name.as_str()));
            }
            let visible = reservation
                .pending
                .filter(|&(owner, _)| owner == txid)
                .map_or(reservation.current, |(_, pending)| pending);
            exists |= reservation.state.visible_to(txid) && visible == target;
        }
        if exists {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                name.as_str()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_budget, test_config};
    use super::super::{
        CheckConstraint, ColType, CompositeFieldDef, CompositeSpec, DomainSpec, EnumMember,
        EnumSpec, MAX_COMPOSITE_FIELDS, MAX_DOMAIN_CHECKS,
    };
    use super::*;

    const KINDS: [NamedTypeSlot; 3] = [
        NamedTypeSlot::Domain(0),
        NamedTypeSlot::Enum(0),
        NamedTypeSlot::Composite(0),
    ];

    fn identity(name: &str) -> UserTypeName {
        UserTypeName {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse(name).unwrap(),
        }
    }

    fn fixture() -> Storage {
        let config = test_config();
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget).unwrap()
    }

    fn create(
        storage: &mut Storage,
        kind: NamedTypeSlot,
        name: &str,
        txid: u32,
    ) -> Result<NamedTypeSlot, SqlError> {
        let UserTypeName { schema, name } = identity(name);
        match kind {
            NamedTypeSlot::Domain(_) => storage
                .create_domain(
                    schema,
                    name,
                    DomainSpec {
                        base_domain: None,
                        base_user_type: None,
                        base: ColType::Int4,
                        base_type_mod: -1,
                        not_null: false,
                        default_expr: None,
                        checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                        n_checks: 0,
                    },
                    txid,
                )
                .map(NamedTypeSlot::Domain),
            NamedTypeSlot::Enum(_) => storage
                .create_enum(
                    schema,
                    name,
                    EnumSpec {
                        members: &[EnumMember {
                            label: SqlName::parse("ready").unwrap(),
                            legacy_label: None,
                            sort: 1.0,
                        }],
                    },
                    txid,
                )
                .map(NamedTypeSlot::Enum),
            NamedTypeSlot::Composite(_) => storage
                .create_composite(
                    schema,
                    name,
                    CompositeSpec {
                        fields: [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS],
                        n_fields: 0,
                    },
                    txid,
                )
                .map(NamedTypeSlot::Composite),
        }
    }

    fn rename(
        storage: &mut Storage,
        slot: NamedTypeSlot,
        name: &str,
        txid: u32,
    ) -> Result<(), SqlError> {
        let UserTypeName { schema, name } = identity(name);
        match slot {
            NamedTypeSlot::Domain(slot) => {
                storage.stage_domain_identity(slot, schema, name, txid)?;
            }
            NamedTypeSlot::Enum(slot) => {
                storage.stage_enum_alter(
                    slot,
                    schema,
                    name,
                    &[EnumMember {
                        label: SqlName::parse("ready").unwrap(),
                        legacy_label: None,
                        sort: 1.0,
                    }],
                    txid,
                )?;
            }
            NamedTypeSlot::Composite(slot) => {
                let mut definition = storage.composite_for(slot, txid);
                definition.schema = schema;
                definition.name = name;
                storage.stage_composite_alter(slot, definition, txid)?;
            }
        }
        Ok(())
    }

    fn rollback(storage: &mut Storage, slot: NamedTypeSlot) {
        match slot {
            NamedTypeSlot::Domain(slot) => storage.rollback_domain_alter(slot, None),
            NamedTypeSlot::Enum(slot) => {
                let undo = {
                    let catalog = storage.type_catalog.lock().unwrap();
                    let definition = catalog.enums[slot];
                    let pending = definition.pending_definition.unwrap();
                    super::super::EnumAlterUndo {
                        created_at: definition.created_at,
                        transaction: pending.txid,
                        version: pending.version,
                        prior: None,
                    }
                };
                storage.rollback_enum_alter(slot, undo);
            }
            NamedTypeSlot::Composite(slot) => storage.rollback_composite_alter(slot, None),
        }
    }

    #[test]
    fn named_type_namespace_creation_rejects_every_kind_pair() {
        for existing in KINDS {
            for requested in KINDS {
                for txid in [0, 7] {
                    let mut storage = fixture();
                    storage.begin_transaction_identity(7).unwrap();
                    storage.begin_transaction_identity(9).unwrap();
                    crate::mem::guard::forbid_alloc(|| {
                        create(&mut storage, existing, "occupied", txid).unwrap();
                        assert_eq!(
                            create(&mut storage, requested, "occupied", txid)
                                .unwrap_err()
                                .sqlstate,
                            sqlstate::DUPLICATE_OBJECT
                        );
                        if txid == 7 {
                            assert_eq!(
                                create(&mut storage, requested, "occupied", 9)
                                    .unwrap_err()
                                    .sqlstate,
                                sqlstate::INTERNAL_LOCK_WAIT
                            );
                        }
                    });
                }
            }
        }
    }

    #[test]
    fn named_type_namespace_rename_rejects_every_kind_pair() {
        for existing in KINDS {
            for requested in KINDS {
                let mut storage = fixture();
                crate::mem::guard::forbid_alloc(|| {
                    create(&mut storage, existing, "occupied", 0).unwrap();
                    let source = create(&mut storage, requested, "source", 0).unwrap();
                    assert_eq!(
                        rename(&mut storage, source, "occupied", 7)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DUPLICATE_OBJECT
                    );
                    // A failed attempt leaves no staged definition or label image.
                    rename(&mut storage, source, "available", 7).unwrap();
                    rollback(&mut storage, source);
                });
            }
        }
    }

    #[test]
    fn named_type_namespace_pending_rename_reserves_both_names() {
        for existing in KINDS {
            for requested in KINDS {
                let mut storage = fixture();
                storage.begin_transaction_identity(7).unwrap();
                storage.begin_transaction_identity(9).unwrap();
                crate::mem::guard::forbid_alloc(|| {
                    let source = create(&mut storage, existing, "before", 0).unwrap();
                    let contender = create(&mut storage, requested, "contender", 0).unwrap();
                    rename(&mut storage, source, "after", 7).unwrap();
                    for name in ["before", "after"] {
                        assert_eq!(
                            create(&mut storage, requested, name, 9)
                                .unwrap_err()
                                .sqlstate,
                            sqlstate::INTERNAL_LOCK_WAIT
                        );
                        assert_eq!(
                            rename(&mut storage, contender, name, 9)
                                .unwrap_err()
                                .sqlstate,
                            sqlstate::INTERNAL_LOCK_WAIT
                        );
                    }
                    rollback(&mut storage, source);
                    create(&mut storage, requested, "after", 9).unwrap();
                    assert_eq!(
                        create(&mut storage, requested, "before", 9)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DUPLICATE_OBJECT
                    );
                });
            }
        }
    }

    #[test]
    fn named_type_namespace_own_rename_releases_old_name() {
        for existing in KINDS {
            for requested in KINDS {
                let mut storage = fixture();
                crate::mem::guard::forbid_alloc(|| {
                    let source = create(&mut storage, existing, "before", 0).unwrap();
                    rename(&mut storage, source, "after", 7).unwrap();
                    create(&mut storage, requested, "before", 7).unwrap();
                    assert_eq!(
                        create(&mut storage, requested, "after", 7)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DUPLICATE_OBJECT
                    );
                });
            }
        }
    }

    #[test]
    fn named_type_namespace_replay_rejects_cross_kind_duplicates() {
        for existing in KINDS {
            let mut storage = fixture();
            crate::mem::guard::forbid_alloc(|| {
                create(&mut storage, existing, "occupied", 0).unwrap();
                let NamedTypeSlot::Enum(source) =
                    create(&mut storage, NamedTypeSlot::Enum(0), "source", 0).unwrap()
                else {
                    unreachable!()
                };
                let target = identity("occupied");
                assert_eq!(
                    storage
                        .replay_enum_identity(source, target.schema, target.name)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DUPLICATE_OBJECT
                );
                let free = storage
                    .type_catalog
                    .lock()
                    .unwrap()
                    .composites
                    .iter()
                    .position(|entry| entry.ddl_state == CatalogDdlState::Absent)
                    .unwrap();
                assert_eq!(
                    storage
                        .create_composite_at(
                            free,
                            target.schema,
                            target.name,
                            CompositeSpec {
                                fields: [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS],
                                n_fields: 0,
                            },
                            0,
                        )
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DUPLICATE_OBJECT
                );
            });
        }
    }

    #[test]
    fn named_type_namespace_ignores_foreign_and_absent_reservations() {
        for kind in KINDS {
            let mut storage = fixture();
            storage.select_database(DatabaseOid::TEMPLATE1).unwrap();
            create(&mut storage, kind, "occupied", 0).unwrap();
            storage.select_database(DatabaseOid::POSTGRES).unwrap();
            storage.begin_transaction_identity(7).unwrap();
            storage.begin_transaction_identity(9).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let slot = create(&mut storage, kind, "occupied", 0).unwrap();
                match slot {
                    NamedTypeSlot::Domain(slot) => {
                        assert_eq!(
                            storage.drop_domain("public", "occupied", 7).unwrap(),
                            Some(slot)
                        );
                    }
                    NamedTypeSlot::Enum(slot) => {
                        assert_eq!(
                            storage.drop_enum("public", "occupied", 7).unwrap(),
                            Some(slot)
                        );
                    }
                    NamedTypeSlot::Composite(slot) => {
                        assert_eq!(
                            storage.drop_composite("public", "occupied", 7).unwrap(),
                            Some(slot)
                        );
                    }
                }
                for requested in KINDS {
                    assert_eq!(
                        create(&mut storage, requested, "occupied", 9)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::INTERNAL_LOCK_WAIT
                    );
                }
                match slot {
                    NamedTypeSlot::Domain(slot) => storage.commit_domain_drop(slot),
                    NamedTypeSlot::Enum(slot) => storage.commit_enum_drop(slot),
                    NamedTypeSlot::Composite(slot) => storage.commit_composite_drop(slot),
                }
                let replacement = create(&mut storage, kind, "occupied", 0).unwrap();
                assert_eq!(replacement, slot);
            });
        }
    }
}
