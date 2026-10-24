//! A parameter has one catalog identity shared by all of its privilege grants.

use core::mem::size_of;
use core::num::NonZeroU32;

use super::{AclCatalog, BOOTSTRAP_ROLE, ParameterAclEntry, PendingParameterAcl, RoleOid, Storage};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::mem::mutex::Mutex;
use crate::sql::ast::{ParameterName, ParameterPrivileges};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParameterAclOid(NonZeroU32);

impl ParameterAclOid {
    const USER_FLOOR: u32 = 16_384;

    pub(crate) fn parse(value: u32) -> Option<Self> {
        (value > Self::USER_FLOOR).then(|| Self(NonZeroU32::new(value).unwrap()))
    }

    pub(crate) const fn get(self) -> u32 {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{RoleAttributes, SqlName};

    fn config() -> Config {
        let mut config = super::super::tests::test_config();
        config.max_parameter_acl_entries = 8;
        config.max_roles = 3;
        config
    }

    fn reader(storage: &Storage) -> u16 {
        let (slot, _) = storage
            .create_role(
                SqlName::parse("parameter_reader").unwrap(),
                RoleAttributes::ORDINARY,
                1,
            )
            .unwrap();
        storage.commit_role_change(slot);
        slot as u16
    }

    fn parameter() -> ParameterName {
        ParameterName::parse("log_min_messages").unwrap()
    }

    #[test]
    fn parameter_acl_identity_follows_row_lifetime_and_savepoint_undo() {
        let config = config();
        let mut budget = super::super::tests::test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let role = reader(&storage);
        let mut arena = Arena::new(&mut budget, "ACL images", 8192).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .is_none()
            );
            storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap();
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .is_none()
            );
            let first = storage
                .snapshot_parameter_acl_row(parameter(), 2, &arena)
                .unwrap()
                .unwrap();
            assert_eq!(first.grants().len(), 2);
            assert_eq!(
                first.grants().iter().next(),
                Some(ParameterAclGrant::INITIAL)
            );
            let first_oid = first.oid();
            storage.commit_parameter_acls(2);
            let (slot, undo) = storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::NONE,
                    ParameterPrivileges::NONE,
                    3,
                )
                .unwrap();
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 3, &arena)
                    .unwrap()
                    .is_none()
            );
            let (regrant_slot, regrant_undo) = storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    3,
                )
                .unwrap();
            assert_ne!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 3, &arena)
                    .unwrap()
                    .unwrap()
                    .oid(),
                first_oid
            );
            storage
                .restore_parameter_acl_pending(regrant_slot, regrant_undo)
                .unwrap();
            storage.restore_parameter_acl_pending(slot, undo).unwrap();
            assert_eq!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .unwrap()
                    .oid(),
                first_oid
            );
            storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::NONE,
                    ParameterPrivileges::NONE,
                    4,
                )
                .unwrap();
            storage.commit_parameter_acls(4);
            assert_eq!(storage.parameter_acl_rows_visible(0).count(), 0);
            storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    5,
                )
                .unwrap();
            storage.commit_parameter_acls(5);
            arena.reset();
            assert_ne!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .unwrap()
                    .oid(),
                first_oid
            );
        });
    }

    #[test]
    fn parameter_acl_replay_preserves_empty_rows_and_rejects_invalid_images_atomically() {
        let config = config();
        let mut budget = super::super::tests::test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let role = reader(&storage);
        let role_oid = storage.role_oid(usize::from(role)) as u32;
        let arena = Arena::new(&mut budget, "ACL images", 8192).unwrap();
        let oid = ParameterAclOid::parse(18000).unwrap();
        let empty = ParameterAclImage::new(oid, ParameterAclGrants::from_slice(&[])).unwrap();
        let grant = ParameterAclGrant::parse(role_oid, 10, 1, 0).unwrap();
        let duplicates = [grant, grant];
        let invalid =
            ParameterAclImage::new(oid, ParameterAclGrants::from_slice(&duplicates)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage
                .restore_parameter_acl_row(parameter(), None, Some(empty))
                .unwrap();
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .unwrap()
                    .grants()
                    .is_empty()
            );
            assert!(
                storage
                    .restore_parameter_acl_row(parameter(), Some(oid), Some(invalid))
                    .is_err()
            );
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .unwrap()
                    .grants()
                    .is_empty()
            );
            assert!(
                storage
                    .restore_parameter_acl_row(parameter(), None, None)
                    .is_err()
            );
            storage
                .restore_parameter_acl_row(parameter(), Some(oid), None)
                .unwrap();
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 0, &arena)
                    .unwrap()
                    .is_none()
            );
            storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap();
            assert!(
                storage
                    .snapshot_parameter_acl_row(parameter(), 2, &arena)
                    .unwrap()
                    .unwrap()
                    .oid()
                    .get()
                    > oid.get()
            );
        });
    }

    #[test]
    fn parameter_acl_undo_rejects_other_catalogs_and_out_of_order_changes() {
        let config = config();
        let mut budget_a = super::super::tests::test_budget(&config);
        let mut budget_b = super::super::tests::test_budget(&config);
        let first = Storage::new(&config, &mut budget_a, 0).unwrap();
        let second = Storage::new(&config, &mut budget_b, 0).unwrap();
        let role_a = reader(&first);
        let role_b = reader(&second);
        crate::mem::guard::forbid_alloc(|| {
            let (slot, undo) = first
                .change_parameter_acl(
                    parameter(),
                    role_a,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap();
            let (other_slot, other_undo) = second
                .change_parameter_acl(
                    parameter(),
                    role_b,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap();
            assert!(
                second
                    .restore_parameter_acl_pending(other_slot, undo)
                    .is_err()
            );
            let (later_slot, later) = first
                .change_parameter_acl(
                    parameter(),
                    role_a,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::ALL,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap();
            assert!(first.restore_parameter_acl_pending(slot, undo).is_err());
            first
                .restore_parameter_acl_pending(later_slot, later)
                .unwrap();
            first.restore_parameter_acl_pending(slot, undo).unwrap();
            assert!(first.restore_parameter_acl_pending(slot, undo).is_err());
            second
                .restore_parameter_acl_pending(other_slot, other_undo)
                .unwrap();
        });
    }

    #[test]
    fn parameter_acl_capacity_and_identity_exhaustion_leave_no_partial_grants() {
        let mut config = config();
        config.max_parameter_acl_entries = 1;
        let mut budget = super::super::tests::test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let role = reader(&storage);
        crate::mem::guard::forbid_alloc(|| {
            let error = storage
                .change_parameter_acl(
                    parameter(),
                    role,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::SET,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.parameter_acl_rows_visible(0).count(), 0);
            assert_eq!(storage.parameter_acl_entries_visible(2).count(), 0);
            storage.acl_catalog.lock().unwrap().parameter_rows.next_oid = u32::MAX as u64;
            let error = storage
                .change_parameter_acl(
                    parameter(),
                    BOOTSTRAP_ROLE,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::NONE,
                    ParameterPrivileges::NONE,
                    2,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.parameter_acl_entries_visible(2).count(), 0);
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grantee {
    Public,
    Role(RoleOid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParameterAclGrantIdentity {
    grantee: Grantee,
    grantor: RoleOid,
}

impl ParameterAclGrantIdentity {
    pub(super) const INITIAL: Self = Self {
        grantee: Grantee::Role(RoleOid::BOOTSTRAP),
        grantor: RoleOid::BOOTSTRAP,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParameterAclGrant {
    identity: ParameterAclGrantIdentity,
    privileges: ParameterPrivileges,
    options: ParameterPrivileges,
}

impl ParameterAclGrant {
    pub(crate) const BYTES: usize = 10;
    const INITIAL: Self = Self {
        identity: ParameterAclGrantIdentity::INITIAL,
        privileges: ParameterPrivileges::ALL,
        options: ParameterPrivileges::NONE,
    };

    pub(crate) fn parse(grantee: u32, grantor: u32, privileges: u8, options: u8) -> Option<Self> {
        let grantee = if grantee == 0 {
            Grantee::Public
        } else {
            Grantee::Role(RoleOid::parse(i32::try_from(grantee).ok()?)?)
        };
        let grantor = RoleOid::parse(i32::try_from(grantor).ok()?)?;
        let privileges = ParameterPrivileges::from_bits(privileges)?;
        let options = ParameterPrivileges::from_bits(options)?;
        if privileges == ParameterPrivileges::NONE || !privileges.contains(options) {
            return None;
        }
        Some(Self {
            identity: ParameterAclGrantIdentity { grantee, grantor },
            privileges,
            options,
        })
    }

    pub(crate) fn grantee(self) -> u32 {
        match self.identity.grantee {
            Grantee::Public => 0,
            Grantee::Role(role) => role.get() as u32,
        }
    }

    pub(crate) fn grantor(self) -> u32 {
        self.identity.grantor.get() as u32
    }
    pub(crate) fn privileges(self) -> ParameterPrivileges {
        self.privileges
    }
    pub(crate) fn options(self) -> ParameterPrivileges {
        self.options
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Self::parse(
            u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?),
            u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?),
            *bytes.get(8)?,
            *bytes.get(9)?,
        )
    }

    pub(crate) fn encode(self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes[..4].copy_from_slice(&self.grantee().to_le_bytes());
        bytes[4..8].copy_from_slice(&self.grantor().to_le_bytes());
        bytes[8] = self.privileges.bits();
        bytes[9] = self.options.bits();
        bytes
    }
}

#[derive(Clone, Copy, Debug)]
enum GrantEncoding<'a> {
    Native(&'a [ParameterAclGrant]),
    Encoded(&'a [u8]),
}

/// Encoded journal input is validated once; both representations expose the
/// same typed grants without allocating during replay or publication.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParameterAclGrants<'a>(GrantEncoding<'a>);

impl<'a> ParameterAclGrants<'a> {
    pub(crate) fn from_slice(grants: &'a [ParameterAclGrant]) -> Self {
        Self(GrantEncoding::Native(grants))
    }

    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        if !bytes.len().is_multiple_of(ParameterAclGrant::BYTES)
            || bytes
                .chunks_exact(ParameterAclGrant::BYTES)
                .any(|grant| ParameterAclGrant::decode(grant).is_none())
        {
            return None;
        }
        Some(Self(GrantEncoding::Encoded(bytes)))
    }

    pub(crate) fn len(self) -> usize {
        match self.0 {
            GrantEncoding::Native(grants) => grants.len(),
            GrantEncoding::Encoded(bytes) => bytes.len() / ParameterAclGrant::BYTES,
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = ParameterAclGrant> + 'a {
        (0..self.len()).map(move |index| match self.0 {
            GrantEncoding::Native(grants) => grants[index],
            GrantEncoding::Encoded(bytes) => ParameterAclGrant::decode(
                &bytes[index * ParameterAclGrant::BYTES..(index + 1) * ParameterAclGrant::BYTES],
            )
            .expect("parameter grant bytes were validated at their parse boundary"),
        })
    }

    pub(crate) fn encoded_len(self) -> usize {
        self.len().saturating_mul(ParameterAclGrant::BYTES)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParameterAclImage<'a> {
    oid: ParameterAclOid,
    grants: ParameterAclGrants<'a>,
}

impl<'a> ParameterAclImage<'a> {
    pub(crate) fn new(oid: ParameterAclOid, grants: ParameterAclGrants<'a>) -> Option<Self> {
        if grants.len() == 1 && grants.iter().next() == Some(ParameterAclGrant::INITIAL) {
            return None;
        }
        Some(Self { oid, grants })
    }

    pub(crate) fn oid(self) -> ParameterAclOid {
        self.oid
    }
    pub(crate) fn grants(self) -> ParameterAclGrants<'a> {
        self.grants
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingHeader {
    transaction: u32,
    change: u64,
    oid: Option<ParameterAclOid>,
}

#[derive(Clone, Copy, Debug)]
struct Header {
    parameter: ParameterName,
    committed: Option<ParameterAclOid>,
    pending: Option<PendingHeader>,
}

impl Header {
    const EMPTY: Self = Self {
        parameter: ParameterName::EMPTY,
        committed: None,
        pending: None,
    };

    fn oid_to(self, transaction: u32) -> Option<ParameterAclOid> {
        match self.pending {
            Some(pending) if pending.transaction == transaction => pending.oid,
            _ => self.committed,
        }
    }
}

pub(super) struct ParameterAclRows {
    origin: super::type_references::CatalogOrigin,
    headers: FixedVec<Header>,
    next_oid: u64,
    next_change: u64,
    next_order: u64,
    grant_keys: FixedMap<(u32, u32), ()>,
    replay: FixedVec<(u16, u16, ParameterPrivileges, ParameterPrivileges)>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParameterAclRow {
    pub(crate) parameter: ParameterName,
    pub(crate) oid: ParameterAclOid,
}

/// One mutation can also materialize the initial bootstrap grant. Both
/// entries and their shared catalog identity roll back as one change.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParameterAclUndo {
    origin: super::type_references::CatalogOrigin,
    header_slot: usize,
    header: Header,
    entries: [Option<(usize, ParameterAclEntry)>; 2],
    transaction: u32,
    change: u64,
}

impl ParameterAclRows {
    pub(super) fn oid_to(
        &self,
        parameter: ParameterName,
        transaction: u32,
    ) -> Option<ParameterAclOid> {
        self.headers
            .iter()
            .find(|header| header.parameter == parameter)
            .and_then(|header| header.oid_to(transaction))
    }
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            .saturating_add(
                config
                    .max_parameter_acl_entries
                    .saturating_mul(size_of::<Header>()),
            )
            .saturating_add(FixedMap::<(u32, u32), ()>::budget_bytes(
                config.max_parameter_acl_entries,
            ))
            .saturating_add(config.max_parameter_acl_entries.saturating_mul(size_of::<(
                u16,
                u16,
                ParameterPrivileges,
                ParameterPrivileges,
            )>()))
    }

    pub(super) fn new(
        config: &Config,
        budget: &mut Budget,
        origin: super::type_references::CatalogOrigin,
    ) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "parameter ACL row controls")?;
        Ok(Self {
            origin,
            headers: FixedVec::new(
                budget,
                "parameter ACL rows",
                config.max_parameter_acl_entries,
            )?,
            next_oid: u64::from(ParameterAclOid::USER_FLOOR),
            next_change: 0,
            next_order: 0,
            grant_keys: FixedMap::new(
                budget,
                "parameter ACL grant identities",
                config.max_parameter_acl_entries,
            )?,
            replay: FixedVec::new(
                budget,
                "parameter ACL replay grants",
                config.max_parameter_acl_entries,
            )?,
        })
    }

    fn reserve_oid(&mut self) -> Result<ParameterAclOid, SqlError> {
        let next = self
            .next_oid
            .checked_add(1)
            .and_then(|value| u32::try_from(value).ok())
            .and_then(ParameterAclOid::parse)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL identity space exhausted"
                )
            })?;
        self.next_oid = u64::from(next.get());
        Ok(next)
    }
}

pub(super) struct ParameterAclRowIter<'a> {
    catalog: &'a Mutex<AclCatalog>,
    next: usize,
    transaction: u32,
}

impl Iterator for ParameterAclRowIter<'_> {
    type Item = ParameterAclRow;

    fn next(&mut self) -> Option<Self::Item> {
        let catalog = self.catalog.lock().expect("ACL catalog lock poisoned");
        while let Some(header) = catalog.parameter_rows.headers.get(self.next).copied() {
            self.next += 1;
            if let Some(oid) = header.oid_to(self.transaction) {
                return Some(ParameterAclRow {
                    parameter: header.parameter,
                    oid,
                });
            }
        }
        None
    }
}

fn empty_entry() -> ParameterAclEntry {
    ParameterAclEntry {
        parameter: ParameterName::EMPTY,
        grantee: BOOTSTRAP_ROLE,
        grantor: BOOTSTRAP_ROLE,
        privileges: ParameterPrivileges::NONE,
        grant_options: ParameterPrivileges::NONE,
        live: false,
        pending: None,
        identity: ParameterAclGrantIdentity::INITIAL,
        order: 0,
    }
}

fn entry_slot(
    catalog: &AclCatalog,
    parameter: ParameterName,
    grantee: u16,
    grantor: u16,
    transaction: u32,
    reserved: Option<usize>,
) -> usize {
    catalog
        .parameters
        .iter()
        .position(|entry| {
            let (_, current_grantee, current_grantor, _, _) =
                Storage::parameter_acl_visible(entry, transaction);
            entry.parameter == parameter
                && current_grantee == grantee
                && current_grantor == grantor
                && (entry.live || entry.pending.is_some())
        })
        .or_else(|| {
            catalog
                .parameters
                .iter()
                .enumerate()
                .position(|(slot, entry)| {
                    Some(slot) != reserved
                        && entry.parameter.is_empty()
                        && !entry.live
                        && entry.pending.is_none()
                })
        })
        .unwrap_or_else(|| {
            let end = catalog.parameters.len();
            if reserved == Some(end) { end + 1 } else { end }
        })
}

impl Storage {
    pub(crate) fn parameter_acl_image_budget_bytes(config: &Config) -> usize {
        config
            .max_parameter_acl_entries
            .saturating_mul(size_of::<(u64, ParameterAclGrant)>() + size_of::<ParameterAclGrant>())
            .saturating_add(core::mem::align_of::<(u64, ParameterAclGrant)>())
    }

    pub(crate) fn snapshot_parameter_acl_row<'a>(
        &self,
        parameter: ParameterName,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<Option<ParameterAclImage<'a>>, SqlError> {
        let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let Some(oid) = catalog.parameter_rows.oid_to(parameter, transaction) else {
            return Ok(None);
        };
        let visible = |entry: &ParameterAclEntry| {
            entry.parameter == parameter && Self::parameter_acl_visible(entry, transaction).0
        };
        let count = catalog
            .parameters
            .iter()
            .filter(|entry| visible(entry))
            .count();
        let mut source = catalog.parameters.iter().filter(|entry| visible(entry));
        let ordered = arena
            .alloc_slice_with(count, |_| {
                let entry = source
                    .next()
                    .expect("parameter ACL row count is stable under its owner");
                match entry.pending.filter(|pending| pending.txid == transaction) {
                    Some(pending) => (
                        pending.order,
                        ParameterAclGrant {
                            identity: pending.identity,
                            privileges: pending.privileges,
                            options: pending.grant_options,
                        },
                    ),
                    None => (
                        entry.order,
                        ParameterAclGrant {
                            identity: entry.identity,
                            privileges: entry.privileges,
                            options: entry.grant_options,
                        },
                    ),
                }
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL row image arena is full"
                )
            })?;
        drop(catalog);
        ordered.sort_unstable_by_key(|(order, _)| *order);
        let grants = arena
            .alloc_slice_with(count, |index| ordered[index].1)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL row image arena is full"
                )
            })?;
        ParameterAclImage::new(oid, ParameterAclGrants::from_slice(grants))
            .map(Some)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "parameter ACL row contains its implicit default"
                )
            })
    }

    pub(crate) fn pending_parameter_acl_rows(
        &self,
        transaction: u32,
    ) -> impl Iterator<Item = (ParameterName, Option<ParameterAclOid>)> + '_ {
        let mut next = 0;
        core::iter::from_fn(move || {
            let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
            while let Some(header) = catalog.parameter_rows.headers.get(next).copied() {
                next += 1;
                if header
                    .pending
                    .is_some_and(|pending| pending.transaction == transaction)
                {
                    return Some((header.parameter, header.committed));
                }
            }
            None
        })
    }

    /// Validate the entire durable row before replacing any committed state.
    pub(crate) fn restore_parameter_acl_row(
        &mut self,
        parameter: ParameterName,
        expected: Option<ParameterAclOid>,
        image: Option<ParameterAclImage<'_>>,
    ) -> Result<(), SqlError> {
        let corrupt = || {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid durable parameter ACL row"
            )
        };
        if parameter.is_empty() {
            return Err(corrupt());
        }
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let existing = catalog
            .parameter_rows
            .headers
            .iter()
            .position(|header| header.parameter == parameter);
        if existing.is_some_and(|slot| catalog.parameter_rows.headers[slot].pending.is_some())
            || catalog.parameter_rows.oid_to(parameter, 0) != expected
            || image.is_some_and(|image| {
                catalog.parameter_rows.headers.iter().any(|header| {
                    header.parameter != parameter
                        && (header.committed == Some(image.oid())
                            || header
                                .pending
                                .is_some_and(|pending| pending.oid == Some(image.oid())))
                })
            })
        {
            return Err(corrupt());
        }
        let header_slot = existing
            .or_else(|| {
                catalog
                    .parameter_rows
                    .headers
                    .iter()
                    .position(|header| header.committed.is_none() && header.pending.is_none())
            })
            .unwrap_or(catalog.parameter_rows.headers.len());
        if image.is_some() && header_slot >= catalog.parameter_rows.headers.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "parameter ACL row capacity exhausted"
            ));
        }
        catalog.parameter_rows.grant_keys.clear();
        catalog.parameter_rows.replay.clear();
        if let Some(image) = image {
            for grant in image.grants().iter() {
                if catalog
                    .parameter_rows
                    .grant_keys
                    .insert((grant.grantee(), grant.grantor()), ())
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "parameter ACL grant capacity exhausted"
                        )
                    })?
                    .is_some()
                {
                    return Err(corrupt());
                }
                let grantee = if grant.grantee() == 0 {
                    super::PUBLIC_ROLE
                } else {
                    u16::try_from(
                        self.role_slot_by_oid(grant.grantee() as i32, 0)
                            .ok_or_else(corrupt)?,
                    )
                    .map_err(|_| corrupt())?
                };
                let grantor = u16::try_from(
                    self.role_slot_by_oid(grant.grantor() as i32, 0)
                        .ok_or_else(corrupt)?,
                )
                .map_err(|_| corrupt())?;
                catalog
                    .parameter_rows
                    .replay
                    .push((grantee, grantor, grant.privileges(), grant.options()))
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "parameter ACL grant capacity exhausted"
                        )
                    })?;
            }
        }
        let replaceable = catalog
            .parameters
            .iter()
            .filter(|entry| {
                entry.parameter == parameter
                    || (entry.parameter.is_empty() && !entry.live && entry.pending.is_none())
            })
            .count()
            + catalog.parameters.capacity()
            - catalog.parameters.len();
        let count = catalog.parameter_rows.replay.len();
        if count > replaceable {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "parameter ACL grant capacity exhausted"
            ));
        }
        let next_order = catalog
            .parameter_rows
            .next_order
            .checked_add(count as u64)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL grant order space exhausted"
                )
            })?;
        for entry in catalog
            .parameters
            .iter_mut()
            .filter(|entry| entry.parameter == parameter)
        {
            *entry = empty_entry();
        }
        if let Some(image) = image {
            if header_slot == catalog.parameter_rows.headers.len() {
                catalog
                    .parameter_rows
                    .headers
                    .push(Header::EMPTY)
                    .expect("parameter row capacity was checked");
            }
            catalog.parameter_rows.headers[header_slot] = Header {
                parameter,
                committed: Some(image.oid()),
                pending: None,
            };
            let mut free = 0;
            for (index, grant) in image.grants().iter().enumerate() {
                while free < catalog.parameters.len()
                    && !catalog.parameters[free].parameter.is_empty()
                {
                    free += 1;
                }
                let (grantee, grantor, privileges, grant_options) =
                    catalog.parameter_rows.replay[index];
                let entry = ParameterAclEntry {
                    parameter,
                    grantee,
                    grantor,
                    privileges,
                    grant_options,
                    live: true,
                    pending: None,
                    identity: grant.identity,
                    order: catalog.parameter_rows.next_order + index as u64 + 1,
                };
                if free == catalog.parameters.len() {
                    catalog
                        .parameters
                        .push(entry)
                        .expect("parameter grant capacity was checked");
                } else {
                    catalog.parameters[free] = entry;
                }
                free += 1;
            }
            catalog.parameter_rows.next_oid = catalog
                .parameter_rows
                .next_oid
                .max(u64::from(image.oid().get()));
        } else if let Some(slot) = existing {
            catalog.parameter_rows.headers[slot] = Header::EMPTY;
        }
        catalog.parameter_rows.next_order = next_order;
        catalog.parameter_rows.replay.clear();
        catalog.parameter_rows.grant_keys.clear();
        Ok(())
    }

    pub(crate) fn change_parameter_acl(
        &self,
        parameter: ParameterName,
        grantee: u16,
        grantor: u16,
        privileges: ParameterPrivileges,
        grant_options: ParameterPrivileges,
        transaction: u32,
    ) -> Result<(usize, ParameterAclUndo), SqlError> {
        if transaction == 0 || parameter.is_empty() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "parameter ACL changes require a parameter and transaction owner"
            ));
        }
        if !privileges.contains(grant_options) {
            return Err(sql_err!(
                sqlstate::INVALID_GRANT_OPERATION,
                "parameter grant options exceed the granted privileges"
            ));
        }
        let grantee_identity = if grantee == super::PUBLIC_ROLE {
            Grantee::Public
        } else {
            self.role_reference_ready(usize::from(grantee), transaction)?;
            Grantee::Role(self.role(usize::from(grantee)).oid())
        };
        self.role_reference_ready(usize::from(grantor), transaction)?;
        let identity = ParameterAclGrantIdentity {
            grantee: grantee_identity,
            grantor: self.role(usize::from(grantor)).oid(),
        };
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        if let Some(blocker) = catalog.parameter_rows.headers.iter().find_map(|header| {
            (header.parameter == parameter)
                .then_some(header.pending)
                .flatten()
                .filter(|pending| pending.transaction != transaction)
                .map(|pending| pending.transaction)
        }) {
            return Err(self.catalog_ddl_wait_error(transaction, blocker, parameter.as_str()));
        }
        let header_slot = catalog
            .parameter_rows
            .headers
            .iter()
            .position(|header| header.parameter == parameter)
            .or_else(|| {
                catalog
                    .parameter_rows
                    .headers
                    .iter()
                    .position(|header| header.committed.is_none() && header.pending.is_none())
            })
            .unwrap_or(catalog.parameter_rows.headers.len());
        if header_slot == catalog.parameter_rows.headers.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many parameter ACL rows (limit {})",
                catalog.parameter_rows.headers.capacity()
            ));
        }
        let header = catalog
            .parameter_rows
            .headers
            .get(header_slot)
            .copied()
            .unwrap_or(Header::EMPTY);
        let before = if header.parameter == parameter {
            header
        } else {
            Header::EMPTY
        };
        let default = if before.oid_to(transaction).is_none()
            && (grantee != BOOTSTRAP_ROLE || grantor != BOOTSTRAP_ROLE)
        {
            Some(entry_slot(
                &catalog,
                parameter,
                BOOTSTRAP_ROLE,
                BOOTSTRAP_ROLE,
                transaction,
                None,
            ))
        } else {
            None
        };
        let primary = entry_slot(&catalog, parameter, grantee, grantor, transaction, default);
        let required = primary
            .max(default.unwrap_or(primary))
            .checked_add(1)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL entry capacity overflow"
                )
            })?;
        if required > catalog.parameters.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many parameter privilege entries (limit {})",
                catalog.parameters.capacity()
            ));
        }
        let proposed = |slot: usize, entry: ParameterAclEntry| {
            if slot == primary {
                (
                    privileges.bits() != 0,
                    grantee,
                    grantor,
                    privileges,
                    grant_options,
                )
            } else if default == Some(slot) {
                (
                    true,
                    BOOTSTRAP_ROLE,
                    BOOTSTRAP_ROLE,
                    ParameterPrivileges::ALL,
                    ParameterPrivileges::NONE,
                )
            } else {
                Self::parameter_acl_visible(&entry, transaction)
            }
        };
        let mut count = 0;
        let mut initial = false;
        for slot in 0..required.max(catalog.parameters.len()) {
            let entry = catalog
                .parameters
                .get(slot)
                .copied()
                .unwrap_or_else(empty_entry);
            if entry.parameter != parameter && slot != primary && default != Some(slot) {
                continue;
            }
            let (visible, grantee, grantor, privileges, options) = proposed(slot, entry);
            if visible {
                count += 1;
                initial = grantee == BOOTSTRAP_ROLE
                    && grantor == BOOTSTRAP_ROLE
                    && privileges == ParameterPrivileges::ALL
                    && options == ParameterPrivileges::NONE;
            }
        }
        let oid = if count == 1 && initial {
            None
        } else {
            Some(match before.oid_to(transaction) {
                Some(oid) => oid,
                None => catalog.parameter_rows.reserve_oid()?,
            })
        };
        let change = catalog
            .parameter_rows
            .next_change
            .checked_add(1)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL change identity space exhausted"
                )
            })?;
        let old_order = |slot: usize| {
            catalog.parameters.get(slot).and_then(|entry| {
                let (visible, _, _, _, _) = Self::parameter_acl_visible(entry, transaction);
                visible.then_some(match entry.pending {
                    Some(pending) if pending.txid == transaction => pending.order,
                    _ => entry.order,
                })
            })
        };
        let primary_old_order = old_order(primary);
        let default_old_order = default.and_then(old_order);
        let needed_orders = usize::from(primary_old_order.is_none())
            + usize::from(default.is_some() && default_old_order.is_none());
        let next_order = catalog
            .parameter_rows
            .next_order
            .checked_add(needed_orders as u64)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "parameter ACL grant order space exhausted"
                )
            })?;
        let mut order = catalog.parameter_rows.next_order;
        let default_order = default.map(|_| {
            default_old_order.unwrap_or_else(|| {
                order += 1;
                order
            })
        });
        let primary_order = primary_old_order.unwrap_or_else(|| {
            order += 1;
            order
        });
        let undo = ParameterAclUndo {
            origin: catalog.parameter_rows.origin,
            header_slot,
            header: before,
            entries: [
                Some((
                    primary,
                    catalog
                        .parameters
                        .get(primary)
                        .copied()
                        .unwrap_or_else(empty_entry),
                )),
                default.map(|slot| {
                    (
                        slot,
                        catalog
                            .parameters
                            .get(slot)
                            .copied()
                            .unwrap_or_else(empty_entry),
                    )
                }),
            ],
            transaction,
            change,
        };
        while catalog.parameters.len() < required {
            catalog
                .parameters
                .push(empty_entry())
                .expect("parameter entry capacity was checked");
        }
        if let Some(slot) = default {
            let entry = &mut catalog.parameters[slot];
            entry.parameter = parameter;
            entry.pending = Some(PendingParameterAcl {
                txid: transaction,
                grantee: BOOTSTRAP_ROLE,
                grantor: BOOTSTRAP_ROLE,
                privileges: ParameterPrivileges::ALL,
                grant_options: ParameterPrivileges::NONE,
                identity: ParameterAclGrantIdentity::INITIAL,
                order: default_order.expect("a materialized bootstrap grant has an order"),
            });
        }
        let entry = &mut catalog.parameters[primary];
        entry.parameter = parameter;
        entry.pending = Some(PendingParameterAcl {
            txid: transaction,
            grantee,
            grantor,
            privileges,
            grant_options,
            identity,
            order: primary_order,
        });
        if header_slot == catalog.parameter_rows.headers.len() {
            catalog
                .parameter_rows
                .headers
                .push(Header::EMPTY)
                .expect("parameter row capacity was checked");
        }
        catalog.parameter_rows.headers[header_slot] = Header {
            parameter,
            committed: before.committed,
            pending: Some(PendingHeader {
                transaction,
                change,
                oid,
            }),
        };
        catalog.parameter_rows.next_change = change;
        catalog.parameter_rows.next_order = next_order;
        Ok((primary, undo))
    }

    pub(crate) fn restore_parameter_acl_pending(
        &self,
        primary: usize,
        undo: ParameterAclUndo,
    ) -> Result<(), SqlError> {
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let current = catalog
            .parameter_rows
            .headers
            .get(undo.header_slot)
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "parameter ACL undo row is missing"
                )
            })?;
        if undo.origin != catalog.parameter_rows.origin
            || undo.entries[0].map(|(slot, _)| slot) != Some(primary)
            || !current.pending.is_some_and(|pending| {
                pending.transaction == undo.transaction && pending.change == undo.change
            })
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "parameter ACL undo does not own the current change"
            ));
        }
        for (slot, previous) in undo.entries.iter().flatten().copied() {
            catalog.parameters[slot] = previous;
        }
        catalog.parameter_rows.headers[undo.header_slot] = undo.header;
        let released = undo.header.pending.is_none();
        drop(catalog);
        if released {
            self.wake_transaction_waiters(undo.transaction);
        }
        Ok(())
    }

    pub(crate) fn commit_parameter_acls(&self, transaction: u32) {
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        for entry in catalog.parameters.iter_mut() {
            let Some(pending) = entry.pending.filter(|pending| pending.txid == transaction) else {
                continue;
            };
            entry.grantee = pending.grantee;
            entry.grantor = pending.grantor;
            entry.privileges = pending.privileges;
            entry.grant_options = pending.grant_options;
            entry.live = pending.privileges.bits() != 0;
            entry.pending = None;
            entry.identity = pending.identity;
            entry.order = pending.order;
            if !entry.live {
                *entry = empty_entry();
            }
        }
        for slot in 0..catalog.parameter_rows.headers.len() {
            let header = catalog.parameter_rows.headers[slot];
            let Some(pending) = header
                .pending
                .filter(|pending| pending.transaction == transaction)
            else {
                continue;
            };
            if pending.oid.is_none() {
                for entry in catalog
                    .parameters
                    .iter_mut()
                    .filter(|entry| entry.parameter == header.parameter)
                {
                    assert!(entry.pending.is_none());
                    *entry = empty_entry();
                }
                catalog.parameter_rows.headers[slot] = Header::EMPTY;
            } else {
                catalog.parameter_rows.headers[slot].committed = pending.oid;
                catalog.parameter_rows.headers[slot].pending = None;
            }
        }
        drop(catalog);
        self.wake_transaction_waiters(transaction);
    }

    pub(crate) fn parameter_acl_rows_visible(
        &self,
        transaction: u32,
    ) -> impl Iterator<Item = ParameterAclRow> + '_ {
        ParameterAclRowIter {
            catalog: &self.acl_catalog,
            next: 0,
            transaction,
        }
    }

    pub(crate) fn parameter_acl_row_count(&self) -> usize {
        self.acl_catalog
            .lock()
            .expect("ACL catalog lock poisoned")
            .parameter_rows
            .headers
            .len()
    }
}
