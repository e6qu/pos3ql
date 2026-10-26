//! Explicit fresh and restored tablespace creation identities.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::num::NonZeroU32;

const FIRST_CUSTOM_OID: i32 = 210_000;
const MAX_CREATION: u64 = i32::MAX as u64 - FIRST_CUSTOM_OID as u64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TablespaceIdentity(NonZeroU32);

impl TablespaceIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_CREATION {
            return None;
        }
        NonZeroU32::new(created_at as u32).map(Self)
    }
    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }
    pub(crate) fn oid(self) -> i32 {
        FIRST_CUSTOM_OID + self.0.get() as i32
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TablespaceCreation {
    Fresh,
    Restored(TablespaceIdentity),
}

impl TablespaceCreation {
    pub(crate) fn restored(created_at: u64) -> Result<Self, SqlError> {
        TablespaceIdentity::parse(created_at)
            .map(Self::Restored)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored tablespace creation identity is outside its OID range"
                )
            })
    }
    pub(super) fn value(self, sequence: &super::CatalogSequence) -> Result<u64, SqlError> {
        match self {
            Self::Fresh => sequence.next_bounded(MAX_CREATION, "tablespace"),
            Self::Restored(identity) => Ok(identity.created_at()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};
    use crate::storage::{BOOTSTRAP_ROLE, SqlName, Storage, TablespaceOptions};
    use crate::util::StackStr;

    #[test]
    fn tablespace_identity_bounds_reject_restore_without_changing_live_catalogs() {
        assert!(TablespaceIdentity::parse(0).is_none());
        assert_eq!(
            TablespaceIdentity::parse(MAX_CREATION).unwrap().oid(),
            i32::MAX
        );
        assert!(TablespaceIdentity::parse(MAX_CREATION + 1).is_none());
        assert!(TablespaceIdentity::parse(u64::MAX).is_none());
        let mut config = test_config();
        config.max_tablespaces = 4;
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let name = SqlName::parse("kept_space").unwrap();
            let slot = storage
                .create_tablespace(
                    TablespaceCreation::Fresh,
                    name,
                    StackStr::from_str("/kept"),
                    TablespaceOptions::DEFAULT,
                    BOOTSTRAP_ROLE,
                    0,
                )
                .unwrap();
            storage.commit_tablespace_create(slot);
            let original = storage
                .tablespaces_visible_to(0)
                .find(|(index, _)| *index == slot)
                .unwrap()
                .1;
            let original_oid = crate::sql::catalog::tablespace_oid(slot, original);
            let before = storage.catalog_identity_sequence();
            for rejected in [0, MAX_CREATION + 1, u64::MAX] {
                assert_eq!(
                    storage
                        .restore_tablespace(
                            3,
                            rejected,
                            SqlName::parse("bad_space").unwrap(),
                            StackStr::from_str("/bad"),
                            TablespaceOptions::DEFAULT,
                            BOOTSTRAP_ROLE
                        )
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), before);
                assert_eq!(storage.tablespace_slot("bad_space", 0), None);
            }
            assert_eq!(
                storage
                    .restore_tablespace(
                        3,
                        original.created_at,
                        SqlName::parse("other_space").unwrap(),
                        StackStr::from_str("/other"),
                        TablespaceOptions::DEFAULT,
                        BOOTSTRAP_ROLE
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), before);
            storage.restore_catalog_identity_sequence(MAX_CREATION);
            assert_eq!(
                storage
                    .create_tablespace(
                        TablespaceCreation::Fresh,
                        SqlName::parse("full_space").unwrap(),
                        StackStr::from_str("/full"),
                        TablespaceOptions::DEFAULT,
                        BOOTSTRAP_ROLE,
                        0
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.catalog_identity_sequence(), MAX_CREATION);
            let selected = storage
                .tablespaces_visible_to(0)
                .find(|(index, _)| *index == slot)
                .unwrap()
                .1;
            assert_eq!(
                crate::sql::catalog::tablespace_oid(slot, selected),
                original_oid
            );
            for (slot, builtin) in storage
                .tablespaces_visible_to(0)
                .filter(|(slot, _)| *slot < 2)
            {
                assert_eq!(
                    crate::sql::catalog::tablespace_oid(slot, builtin),
                    1663 + slot as i32
                );
            }
        });
    }
}
