//! PostgreSQL 18.6's predefined roles and the memberships initdb grants
//! among them. Names, OIDs and order come from
//! [`pg_authid.dat`](https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_authid.dat)
//! and [`pg_auth_members.dat`](https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_auth_members.dat).
//! The membership OIDs are the ones genbki assigns in 18.6
//! (`SELECT oid FROM pg_auth_members` after initdb).
//!
//! Predefined roles occupy role catalog slots `1..=16`, right after the
//! bootstrap superuser, in `pg_authid` order. They are bootstrap facts: never
//! written to checkpoints or the journal, and they cannot be created, altered,
//! renamed or dropped.

use super::catalog_sources::RoleOid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredefinedRole {
    DatabaseOwner,
    ReadAllData,
    WriteAllData,
    Monitor,
    ReadAllSettings,
    ReadAllStats,
    StatScanTables,
    ReadServerFiles,
    WriteServerFiles,
    ExecuteServerProgram,
    SignalBackend,
    Checkpoint,
    Maintain,
    UseReservedConnections,
    CreateSubscription,
    SignalAutovacuumWorker,
}

pub(crate) const PREDEFINED_ROLE_COUNT: usize = 16;

impl PredefinedRole {
    pub(crate) const ALL: [Self; PREDEFINED_ROLE_COUNT] = [
        Self::DatabaseOwner,
        Self::ReadAllData,
        Self::WriteAllData,
        Self::Monitor,
        Self::ReadAllSettings,
        Self::ReadAllStats,
        Self::StatScanTables,
        Self::ReadServerFiles,
        Self::WriteServerFiles,
        Self::ExecuteServerProgram,
        Self::SignalBackend,
        Self::Checkpoint,
        Self::Maintain,
        Self::UseReservedConnections,
        Self::CreateSubscription,
        Self::SignalAutovacuumWorker,
    ];

    pub(crate) const fn slot(self) -> usize {
        1 + self as usize
    }

    pub(crate) const fn oid(self) -> i32 {
        match self {
            Self::DatabaseOwner => 6171,
            Self::ReadAllData => 6181,
            Self::WriteAllData => 6182,
            Self::Monitor => 3373,
            Self::ReadAllSettings => 3374,
            Self::ReadAllStats => 3375,
            Self::StatScanTables => 3377,
            Self::ReadServerFiles => 4569,
            Self::WriteServerFiles => 4570,
            Self::ExecuteServerProgram => 4571,
            Self::SignalBackend => 4200,
            Self::Checkpoint => 4544,
            Self::Maintain => 6337,
            Self::UseReservedConnections => 4550,
            Self::CreateSubscription => 6304,
            Self::SignalAutovacuumWorker => 6392,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::DatabaseOwner => "pg_database_owner",
            Self::ReadAllData => "pg_read_all_data",
            Self::WriteAllData => "pg_write_all_data",
            Self::Monitor => "pg_monitor",
            Self::ReadAllSettings => "pg_read_all_settings",
            Self::ReadAllStats => "pg_read_all_stats",
            Self::StatScanTables => "pg_stat_scan_tables",
            Self::ReadServerFiles => "pg_read_server_files",
            Self::WriteServerFiles => "pg_write_server_files",
            Self::ExecuteServerProgram => "pg_execute_server_program",
            Self::SignalBackend => "pg_signal_backend",
            Self::Checkpoint => "pg_checkpoint",
            Self::Maintain => "pg_maintain",
            Self::UseReservedConnections => "pg_use_reserved_connections",
            Self::CreateSubscription => "pg_create_subscription",
            Self::SignalAutovacuumWorker => "pg_signal_autovacuum_worker",
        }
    }

    pub(crate) fn from_slot(slot: usize) -> Option<Self> {
        slot.checked_sub(1)
            .and_then(|index| Self::ALL.get(index))
            .copied()
    }

    pub(crate) fn from_oid(oid: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.oid() == oid)
    }

    pub(crate) fn role_oid(self) -> RoleOid {
        RoleOid::parse(self.oid()).expect("a predefined role OID is a valid role OID")
    }
}

/// A membership initdb grants: `role` to `member`, by the bootstrap
/// superuser, with INHERIT and SET but without ADMIN.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InitdbMembership {
    pub(crate) role: PredefinedRole,
    pub(crate) member: PredefinedRole,
    pub(crate) oid: i32,
}

pub(crate) const INITDB_MEMBERSHIPS: [InitdbMembership; 3] = [
    InitdbMembership {
        role: PredefinedRole::ReadAllSettings,
        member: PredefinedRole::Monitor,
        oid: 10_308,
    },
    InitdbMembership {
        role: PredefinedRole::ReadAllStats,
        member: PredefinedRole::Monitor,
        oid: 10_309,
    },
    InitdbMembership {
        role: PredefinedRole::StatScanTables,
        member: PredefinedRole::Monitor,
        oid: 10_310,
    },
];

/// Creation identities of the initdb memberships lie just above the catalog
/// creation range, so no `CREATE`/`GRANT` stamp can collide with them. The
/// membership at slot `i` has identity `INITDB_MEMBERSHIP_BASE + i`.
pub(crate) const INITDB_MEMBERSHIP_BASE: u64 = super::MAX_CATALOG_CREATION + 1;

/// The initdb membership a creation identity names, if any.
pub(crate) fn initdb_membership(created_at: u64) -> Option<(usize, InitdbMembership)> {
    let index = usize::try_from(created_at.checked_sub(INITDB_MEMBERSHIP_BASE)?).ok()?;
    INITDB_MEMBERSHIPS
        .get(index)
        .map(|membership| (index, *membership))
}

/// Whether `created_at` identifies a role membership: a catalog creation
/// stamp, or an initdb membership's reserved identity.
pub(crate) fn membership_identity_valid(created_at: u64) -> bool {
    super::CatalogIdentity::parse(created_at).is_some() || initdb_membership(created_at).is_some()
}
