//! Public identifiers follow durable object lifetimes, never reusable catalog slots.

use core::num::NonZeroU32;

use super::*;

pub(super) const MAX_CATALOG_CREATION: u64 = 9_999_999;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogIdentity(NonZeroU32);

impl CatalogIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_CATALOG_CREATION {
            return None;
        }
        NonZeroU32::new(u32::try_from(created_at).ok()?).map(Self)
    }

    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }

    pub(crate) fn require(created_at: u64) -> Result<Self, SqlError> {
        Self::parse(created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored catalog creation identity is invalid"
            )
        })
    }
}

impl ExtendedStatisticsDef {
    pub(crate) fn oid(&self) -> i32 {
        440_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Statistics has a checked creation identity")
                .0
                .get() as i32
    }
}

impl ExtensionDef {
    pub(crate) fn oid(&self) -> i32 {
        460_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Extension has a checked creation identity")
                .0
                .get() as i32
    }
}

impl foreign::ForeignCatalogEntry<foreign::ForeignDataWrapperDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        480_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted ForeignDataWrapper has a checked creation identity")
                .0
                .get() as i32
    }
}

impl foreign::ForeignCatalogEntry<foreign::ForeignServerDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        500_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted ForeignServer has a checked creation identity")
                .0
                .get() as i32
    }
}

impl foreign::ForeignCatalogEntry<foreign::UserMappingDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        520_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted UserMapping has a checked creation identity")
                .0
                .get() as i32
    }
}

impl PublicationDef {
    pub(crate) fn oid(&self) -> i32 {
        540_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Publication has a checked creation identity")
                .0
                .get() as i32
    }
}

impl SubscriptionDef {
    pub(crate) fn oid(&self) -> i32 {
        560_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Subscription has a checked creation identity")
                .0
                .get() as i32
    }
}

/// New SQL declarations reserve a birth; recovery restores its checked durable birth.
#[derive(Clone, Copy)]
pub(crate) enum CatalogCreation {
    New,
    Restore(CatalogIdentity),
}

impl CatalogCreation {
    pub(super) fn resolve(self, sequence: &CatalogSequence) -> Result<u64, SqlError> {
        match self {
            Self::New => sequence.next_bounded(MAX_CATALOG_CREATION, "catalog object"),
            Self::Restore(identity) => Ok(identity.created_at()),
        }
    }
}

impl Storage {
    pub(crate) fn validate_restored_catalog_identities(
        &self,
        saved_sequence: u64,
    ) -> Result<(), SqlError> {
        const WORDS: usize = 1024;
        const WIDTH: usize = WORDS * u64::BITS as usize;
        let statistics = self
            .extended_statistics_catalog
            .lock()
            .expect("statistics catalog lock poisoned");
        let extensions = self.extension_catalog();
        let foreign = self.foreign_catalog();
        let publications = self
            .publications
            .lock()
            .expect("publication catalog lock poisoned");
        let subscriptions = self
            .subscription_catalog
            .lock()
            .expect("subscription catalog lock poisoned");
        // Subscription OIDs belong to a shared catalog. Other families are
        // database-local and template copies may preserve their numeric OIDs.
        let scopes = core::iter::once(None).chain(
            self.databases_visible_to(0)
                .map(|(_, database)| Some(database.oid)),
        );
        for database in scopes {
            for first in (1..=saved_sequence.clamp(1, MAX_CATALOG_CREATION)).step_by(WIDTH) {
                let mut seen = [0u64; WORDS];
                let mut check = |database_oid: DatabaseOid,
                                 state: CatalogDdlState,
                                 created_at: u64|
                 -> Result<(), SqlError> {
                    if database.is_some_and(|selected| database_oid != selected)
                        || state == CatalogDdlState::Absent
                    {
                        return Ok(());
                    }
                    CatalogIdentity::require(created_at)?;
                    if created_at > saved_sequence {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored catalog identity exceeds its reserved counter"
                        ));
                    }
                    if !(first..first + WIDTH as u64).contains(&created_at) {
                        return Ok(());
                    }
                    let offset = (created_at - first) as usize;
                    let bit = 1u64 << (offset % u64::BITS as usize);
                    let word = &mut seen[offset / u64::BITS as usize];
                    if *word & bit != 0 {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored catalog creation identity is duplicated"
                        ));
                    }
                    *word |= bit;
                    Ok(())
                };
                if database.is_some() {
                    for definition in statistics.definitions.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in extensions.definitions.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for slot in 0..foreign.wrapper_capacity() {
                        let entry = foreign.entry_wrapper(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for slot in 0..foreign.server_capacity() {
                        let entry = foreign.entry_server(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for slot in 0..foreign.mapping_capacity() {
                        let entry = foreign.entry_mapping(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for definition in publications.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                }
                for definition in subscriptions.definitions.iter() {
                    check(
                        definition.database,
                        definition.ddl_state,
                        definition.created_at,
                    )?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    fn storage() -> Storage {
        let mut config = test_config();
        config.max_publications = 2;
        config.max_subscriptions = 2;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    #[test]
    fn catalog_lifetime_corruption_is_rejected_without_allocation() {
        let storage = storage();
        storage.publications.lock().unwrap()[0] = PublicationDef {
            created_at: 7,
            ddl_state: CatalogDdlState::Present,
            ..PublicationDef::EMPTY
        };
        crate::mem::guard::forbid_alloc(|| {
            storage.validate_restored_catalog_identities(7).unwrap();
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(6)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            for created_at in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                storage.publications.lock().unwrap()[0].created_at = created_at;
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(u64::MAX)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
            storage.publications.lock().unwrap()[0].created_at = 7;
            storage.subscription_catalog.lock().unwrap().definitions[0] = SubscriptionDef {
                created_at: 7,
                ddl_state: CatalogDdlState::PendingCreate { txid: 9 },
                ..SubscriptionDef::EMPTY
            };
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            storage.subscription_catalog.lock().unwrap().definitions[0].created_at = 8;
            storage.validate_restored_catalog_identities(8).unwrap();
        });
    }

    #[test]
    fn catalog_lifetime_database_scopes_preserve_clones_and_shared_identifiers() {
        let mut storage = storage();
        for (slot, database) in [DatabaseOid::POSTGRES, DatabaseOid::TEMPLATE1]
            .into_iter()
            .enumerate()
        {
            storage.publications.lock().unwrap()[slot] = PublicationDef {
                database,
                created_at: 7,
                ddl_state: CatalogDdlState::Present,
                ..PublicationDef::EMPTY
            };
        }
        crate::mem::guard::forbid_alloc(|| storage.validate_restored_catalog_identities(7))
            .unwrap();
        for (slot, database) in [DatabaseOid::POSTGRES, DatabaseOid::TEMPLATE1]
            .into_iter()
            .enumerate()
        {
            storage.subscription_catalog.lock().unwrap().definitions[slot] = SubscriptionDef {
                database,
                created_at: 8,
                definition_generation: 1,
                ddl_state: CatalogDdlState::Present,
                ..SubscriptionDef::EMPTY
            };
        }
        let sequence = storage.catalog_identity_sequence();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(8)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                storage
                    .restore_subscription_stream_identity(1, 8, 2)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), sequence);
            assert_eq!(
                storage.subscription_catalog.lock().unwrap().definitions[1].definition_generation,
                1
            );
            storage
                .restore_subscription_stream_identity(1, 9, 2)
                .unwrap();
            storage.validate_restored_catalog_identities(9).unwrap();
        });
    }

    #[test]
    fn catalog_lifetime_foreign_replay_rejects_invalid_births_atomically() {
        let storage = storage();
        let definition = foreign::ForeignDataWrapperDefinition {
            name: SqlName::parse("original").unwrap(),
            handler: foreign::ForeignDataHandler::None,
            validator: foreign::ForeignDataValidator::None,
            options: foreign::ForeignOptions::EMPTY,
        };
        storage
            .restore_foreign_wrapper(0, 7, definition, 0)
            .unwrap();
        let oid = storage.foreign_wrapper_entry(0).oid();
        let sequence = storage.catalog_identity_sequence();
        crate::mem::guard::forbid_alloc(|| {
            for created_at in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                assert_eq!(
                    storage
                        .replay_set_foreign_wrapper(0, created_at, 0, Some(definition))
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), sequence);
                assert_eq!(storage.foreign_wrapper_entry(0).oid(), oid);
                assert_eq!(
                    storage.foreign_wrapper_entry(0).definition.name.as_str(),
                    "original"
                );
            }
            let duplicate = foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("duplicate").unwrap(),
                ..definition
            };
            assert_eq!(
                storage
                    .restore_foreign_wrapper(1, 7, duplicate, 0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                storage.foreign_wrapper_entry(1).ddl_state,
                CatalogDdlState::Absent
            );
            assert_eq!(storage.catalog_identity_sequence(), sequence);
        });
    }

    #[test]
    fn catalog_lifetime_exhaustion_installs_no_replacement() {
        let storage = storage();
        storage.restore_catalog_identity_sequence(MAX_CATALOG_CREATION - 1);
        let original = foreign::ForeignDataWrapperDefinition {
            name: SqlName::parse("last_wrapper").unwrap(),
            handler: foreign::ForeignDataHandler::None,
            validator: foreign::ForeignDataValidator::None,
            options: foreign::ForeignOptions::EMPTY,
        };
        crate::mem::guard::forbid_alloc(|| {
            let slot = storage.create_foreign_wrapper(original, 9).unwrap();
            let last = storage.foreign_wrapper_entry(slot).oid();
            let replacement = foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("overflow").unwrap(),
                ..original
            };
            assert_eq!(
                storage
                    .create_foreign_wrapper(replacement, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.catalog_identity_sequence(), MAX_CATALOG_CREATION);
            assert_eq!(storage.foreign_wrapper_entry(slot).oid(), last);
            assert!(storage.foreign_wrapper("overflow", 9).is_none());
        });
    }
}
