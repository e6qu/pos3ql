//! One namespace for domain, enum and named composite identities.

use super::{
    CatalogDdlState, DatabaseOid, SqlError, SqlName, Storage, TypeCatalog, UserTypeName,
    current_database, sqlstate,
};
use crate::sql_err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamedTypeKind {
    Domain,
    Enum,
    Composite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamedTypeSlot {
    Domain(usize),
    Enum(usize),
    Composite(usize),
}

impl NamedTypeSlot {
    pub(super) fn kind(self) -> NamedTypeKind {
        match self {
            Self::Domain(_) => NamedTypeKind::Domain,
            Self::Enum(_) => NamedTypeKind::Enum,
            Self::Composite(_) => NamedTypeKind::Composite,
        }
    }

    pub(super) fn index(self) -> usize {
        match self {
            Self::Domain(slot) | Self::Enum(slot) | Self::Composite(slot) => slot,
        }
    }
}

pub(super) struct NameReservation {
    pub(super) slot: NamedTypeSlot,
    pub(super) database: DatabaseOid,
    pub(super) state: CatalogDdlState,
    pub(super) current: UserTypeName,
    pub(super) pending: Option<(u32, UserTypeName)>,
}

impl TypeCatalog {
    pub(super) fn name_reservations(&self) -> impl Iterator<Item = NameReservation> + '_ {
        self.domains
            .iter()
            .enumerate()
            .map(|(slot, definition)| {
                let current = UserTypeName::declaration(definition.schema, definition.name);
                NameReservation {
                    slot: NamedTypeSlot::Domain(slot),
                    database: definition.database,
                    state: definition.ddl_state,
                    current,
                    pending: definition.pending_definition.as_ref().map(|pending| {
                        (
                            pending.txid,
                            pending.identity.map_or(current, |identity| {
                                UserTypeName::declaration(identity.schema, identity.name)
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
                        current: UserTypeName::declaration(definition.schema, definition.name),
                        pending: definition.pending_definition.as_ref().map(|pending| {
                            (
                                pending.txid,
                                UserTypeName::declaration(pending.schema, pending.name),
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
                        current: UserTypeName::declaration(definition.schema, definition.name),
                        pending: definition.pending_definition.as_ref().map(|pending| {
                            (
                                pending.txid,
                                UserTypeName::declaration(pending.schema, pending.name),
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
        let target = UserTypeName::declaration(schema, name);
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
        UserTypeName::declaration(
            SqlName::parse("public").unwrap(),
            SqlName::parse(name).unwrap(),
        )
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
        let UserTypeName { schema, name, .. } = identity(name);
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
                .map(|created| NamedTypeSlot::Domain(created.slot())),
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
                .map(|created| NamedTypeSlot::Enum(created.slot())),
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
                .map(|created| NamedTypeSlot::Composite(created.slot())),
        }
    }

    #[derive(Debug)]
    enum NameUndo {
        Domain(super::super::DomainAlterUndo),
        Enum(super::super::EnumAlterUndo),
        Composite(super::super::CompositeAlterUndo),
    }

    fn rename(
        storage: &mut Storage,
        slot: NamedTypeSlot,
        name: &str,
        txid: u32,
    ) -> Result<NameUndo, SqlError> {
        let UserTypeName { schema, name, .. } = identity(name);
        Ok(match slot {
            NamedTypeSlot::Domain(slot) => NameUndo::Domain(storage.stage_domain_identity(
                storage.prepare_domain_edit(slot, txid).unwrap().1,
                schema,
                name,
                txid,
            )?),
            NamedTypeSlot::Enum(slot) => NameUndo::Enum(storage.stage_enum_alter(
                storage.prepare_enum_edit(slot, txid).unwrap().1,
                schema,
                name,
                &[EnumMember {
                    label: SqlName::parse("ready").unwrap(),
                    legacy_label: None,
                    sort: 1.0,
                }],
                txid,
            )?),
            NamedTypeSlot::Composite(slot) => {
                let mut definition = storage.composite_for(slot, txid);
                definition.schema = schema;
                definition.name = name;
                NameUndo::Composite(storage.stage_composite_alter(
                    storage.prepare_composite_edit(slot, txid).unwrap().1,
                    definition,
                    txid,
                )?)
            }
        })
    }

    fn rollback(storage: &mut Storage, undo: NameUndo) {
        match undo {
            NameUndo::Domain(undo) => storage.rollback_domain_alter(undo),
            NameUndo::Enum(undo) => storage.rollback_enum_alter(undo),
            NameUndo::Composite(undo) => storage.rollback_composite_alter(undo),
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
                    let undo = rename(&mut storage, source, "available", 7).unwrap();
                    rollback(&mut storage, undo);
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
                    let undo = rename(&mut storage, source, "after", 7).unwrap();
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
                    rollback(&mut storage, undo);
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
    fn named_type_drop_uses_own_pending_name_and_rollback_restores_it() {
        for kind in KINDS {
            let mut storage = fixture();
            crate::mem::guard::forbid_alloc(|| {
                let source = create(&mut storage, kind, "before", 0).unwrap();
                let undo = rename(&mut storage, source, "after", 7).unwrap();
                assert_eq!(
                    storage
                        .stage_type_drop(kind.kind(), "public", "before", 7)
                        .unwrap(),
                    None
                );
                let dropped = storage
                    .stage_type_drop(kind.kind(), "public", "after", 7)
                    .unwrap()
                    .unwrap();
                assert_eq!(dropped.slot(), source.index());
                storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Rollback);
                rollback(&mut storage, undo);
                assert_eq!(
                    storage
                        .stage_type_drop(kind.kind(), "public", "after", 7)
                        .unwrap(),
                    None
                );
                assert_eq!(
                    storage
                        .stage_type_drop(kind.kind(), "public", "before", 7)
                        .unwrap()
                        .unwrap()
                        .slot(),
                    source.index()
                );
            });
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
                let dropped = storage
                    .stage_type_drop(kind.kind(), "public", "occupied", 7)
                    .unwrap()
                    .unwrap();
                assert_eq!(dropped.slot(), slot.index());
                for requested in KINDS {
                    assert_eq!(
                        create(&mut storage, requested, "occupied", 9)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::INTERNAL_LOCK_WAIT
                    );
                }
                storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                let replacement = create(&mut storage, kind, "occupied", 0).unwrap();
                assert_eq!(replacement, slot);
            });
        }
    }
}
