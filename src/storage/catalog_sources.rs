//! Catalog edits and connections retain their selected object lifetimes.

use core::num::{NonZeroU32, NonZeroU64};
use core::sync::atomic::{AtomicU64, Ordering};

use super::type_references::CatalogOrigin;
use super::{CatalogDdlState, DatabaseOid, ObjectLockTarget, SqlName, Storage, current_database};
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// A saved object's catalog position and nonzero creation stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogPosition {
    slot: u16,
    created_at: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CatalogAllocation {
    New,
    Restore(CatalogPosition),
}

impl CatalogPosition {
    pub(crate) const ENCODED_BYTES: usize = 10;

    pub(crate) fn new(slot: usize, created_at: u64) -> Result<Self, SqlError> {
        let slot = u16::try_from(slot).ok();
        let created_at = NonZeroU64::new(created_at);
        match (slot, created_at) {
            (Some(slot), Some(created_at)) => Ok(Self { slot, created_at }),
            _ => Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid durable catalog position"
            )),
        }
    }

    pub(crate) fn slot(self) -> usize {
        usize::from(self.slot)
    }
    pub(crate) fn created_at(self) -> u64 {
        self.created_at.get()
    }

    pub(crate) fn encode(self) -> [u8; Self::ENCODED_BYTES] {
        let mut output = [0; Self::ENCODED_BYTES];
        output[..2].copy_from_slice(&self.slot.to_le_bytes());
        output[2..].copy_from_slice(&self.created_at.get().to_le_bytes());
        output
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::ENCODED_BYTES {
            return None;
        }
        Self::new(
            usize::from(u16::from_le_bytes(bytes[..2].try_into().ok()?)),
            u64::from_le_bytes(bytes[2..].try_into().ok()?),
        )
        .ok()
    }
}

/// A durable PostgreSQL role identifier, independent of catalog slot placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoleOid(NonZeroU32);

impl RoleOid {
    pub(crate) const BOOTSTRAP: Self = Self(NonZeroU32::new(10).unwrap());
    pub(crate) const USER_FLOOR: u64 = 16_384;

    pub(crate) fn parse(value: i32) -> Option<Self> {
        (value == 10 || value > Self::USER_FLOOR as i32)
            .then(|| Self(NonZeroU32::new(value as u32).expect("a valid role OID is nonzero")))
    }

    pub(crate) fn get(self) -> i32 {
        self.0.get() as i32
    }
}

pub(super) struct CatalogSources {
    origin: CatalogOrigin,
    roles: AtomicU64,
}

impl CatalogSources {
    pub(super) fn origin(&self) -> CatalogOrigin {
        self.origin
    }

    pub(super) fn new(origin: CatalogOrigin, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(core::mem::size_of::<Self>(), "catalog source controls")?;
        Ok(Self {
            origin,
            roles: AtomicU64::new(RoleOid::USER_FLOOR),
        })
    }

    fn reserve(counter: &AtomicU64, object: &str) -> Result<NonZeroU64, SqlError> {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(1)
                .filter(|next| *next <= i32::MAX as u64)
                .ok_or_else(|| {
                    crate::sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "{} identity space exhausted",
                        object
                    )
                })?;
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(NonZeroU64::new(next).expect("a checked increment is nonzero")),
                Err(observed) => current = observed,
            }
        }
    }

    pub(super) fn next_role(&self) -> Result<RoleOid, SqlError> {
        let next = Self::reserve(&self.roles, "role")?;
        Ok(RoleOid::parse(next.get() as i32).expect("a reserved role OID is in range"))
    }

    pub(super) fn role(&self, slot: usize, lifetime: RoleOid) -> RoleIdentity {
        RoleIdentity {
            origin: self.origin,
            slot: u16::try_from(slot).expect("configured role slot is representable"),
            lifetime,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoleIdentity {
    origin: CatalogOrigin,
    slot: u16,
    lifetime: RoleOid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoleReference {
    identity: RoleIdentity,
    name: SqlName,
}

impl RoleReference {
    pub(crate) fn slot(self) -> u16 {
        self.identity.slot
    }
    pub(crate) fn oid(self) -> RoleOid {
        self.identity.lifetime
    }
    pub(crate) fn source(self) -> SessionRole {
        SessionRole::Bound(self.identity)
    }
    pub(crate) fn name(self) -> SqlName {
        self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRole {
    Bootstrap,
    Unauthenticated,
    Bound(RoleIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionIdentity {
    source: SessionRole,
    name: crate::util::StackStr<64>,
}

impl SessionIdentity {
    pub(crate) fn bootstrap() -> Self {
        Self {
            source: SessionRole::Bootstrap,
            name: crate::util::StackStr::from_str("postgres"),
        }
    }

    pub(crate) fn unauthenticated(name: &str) -> Self {
        Self {
            source: SessionRole::Unauthenticated,
            name: crate::util::StackStr::from_str(name),
        }
    }

    pub(crate) fn bound(role: RoleReference) -> Self {
        Self {
            source: SessionRole::Bound(role.identity),
            name: crate::util::StackStr::from_str(role.name.as_str()),
        }
    }

    pub(crate) fn source(self) -> SessionRole {
        self.source
    }
    pub(crate) fn name(self) -> crate::util::StackStr<64> {
        self.name
    }
    pub(crate) fn as_str(&self) -> &str {
        self.name.as_str()
    }

    pub(crate) fn refresh(self, storage: &Storage, transaction: u32) -> Self {
        // An absent identity has no usable spelling. Never resolve its old
        // spelling against a replacement account to recover permissions.
        let name = match storage.resolve_session_role(self.source, transaction) {
            Ok(role) => crate::util::StackStr::from_str(role.name.as_str()),
            Err(_) => crate::util::StackStr::new(),
        };
        Self {
            source: self.source,
            name,
        }
    }

    pub(crate) fn require_name(self) -> Result<crate::util::StackStr<64>, SqlError> {
        if self.source != SessionRole::Unauthenticated && !self.name.as_str().is_empty() {
            return Ok(self.name);
        }
        match self.source {
            SessionRole::Bound(identity) => Err(crate::sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "invalid role OID: {}",
                identity.lifetime.get()
            )),
            SessionRole::Bootstrap | SessionRole::Unauthenticated => Err(crate::sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "session role is not authenticated"
            )),
        }
    }
}

impl Storage {
    pub(crate) fn role_reference_ready(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let role = catalog
            .roles
            .get(slot)
            .filter(|role| role.visible_to(transaction))
            .ok_or_else(|| {
                crate::sql_err!(sqlstate::UNDEFINED_OBJECT, "referenced role is not visible")
            })?;
        if let Some(pending) = role.pending
            && pending.txid != transaction
            && !pending.exists
        {
            let owner = pending.txid;
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, "role reference"));
        }
        let source = role.oid();
        drop(catalog);
        if transaction != 0 && source != RoleOid::BOOTSTRAP {
            self.lock_object(
                transaction,
                ObjectLockTarget::Role(source),
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )?;
            let catalog = self
                .role_catalog
                .lock()
                .expect("role catalog lock poisoned");
            if !catalog.roles[slot].visible_to(transaction) || catalog.roles[slot].oid() != source {
                return Err(crate::sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "referenced role no longer exists"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn lock_role_for_drop(&self, slot: usize, transaction: u32) -> Result<(), SqlError> {
        let source = self.role(slot).oid();
        if source == RoleOid::BOOTSTRAP {
            return Err(crate::sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop role because it is required by the database system"
            ));
        }
        if transaction != 0 {
            self.lock_object(
                transaction,
                ObjectLockTarget::Role(source),
                crate::sql::ast::TableLockMode::AccessExclusive,
                false,
            )?;
        }
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        if !catalog.roles[slot].visible_to(transaction) || catalog.roles[slot].oid() != source {
            return Err(crate::sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role selected for deletion no longer exists"
            ));
        }
        Ok(())
    }

    pub(crate) fn role_identity_sequence(&self) -> u64 {
        self.catalog_sources.roles.load(Ordering::Relaxed)
    }

    pub(crate) fn restore_role_identity_sequence(&self, sequence: u64) -> Result<(), SqlError> {
        if !(RoleOid::USER_FLOOR..=i32::MAX as u64).contains(&sequence) {
            return Err(crate::sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid role identity allocator"
            ));
        }
        self.catalog_sources
            .roles
            .fetch_max(sequence, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn observe_role_oid(&self, oid: RoleOid) {
        self.catalog_sources
            .roles
            .fetch_max(oid.get() as u64, Ordering::Relaxed);
    }

    pub(crate) fn role_login_snapshot(
        &self,
        name: &str,
    ) -> Option<(RoleReference, super::RoleAttributes)> {
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let (slot, role) = catalog
            .roles
            .iter()
            .enumerate()
            .find(|(_, role)| role.visible_to(0) && role.name_to(0).as_str() == name)?;
        Some((
            RoleReference {
                identity: self.catalog_sources.role(
                    slot,
                    role.lifetime.expect("a visible role has a source lifetime"),
                ),
                name: role.name_to(0),
            },
            role.attributes_to(0),
        ))
    }

    pub(crate) fn capture_role(
        &self,
        name: &str,
        transaction: u32,
    ) -> Result<RoleReference, SqlError> {
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let (slot, role) = catalog
            .roles
            .iter()
            .enumerate()
            .find(|(_, role)| {
                role.visible_to(transaction) && role.name_to(transaction).as_str() == name
            })
            .ok_or_else(|| {
                crate::sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    name
                )
            })?;
        let lifetime = role.lifetime.expect("a visible role has a source lifetime");
        Ok(RoleReference {
            identity: self.catalog_sources.role(slot, lifetime),
            name: role.name_to(transaction),
        })
    }

    pub(crate) fn capture_role_slot(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<RoleReference, SqlError> {
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let role = catalog
            .roles
            .get(slot)
            .filter(|role| role.visible_to(transaction))
            .ok_or_else(|| {
                crate::sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role is not visible in the role catalog"
                )
            })?;
        let lifetime = role.lifetime.expect("a visible role has a source lifetime");
        Ok(RoleReference {
            identity: self.catalog_sources.role(slot, lifetime),
            name: role.name_to(transaction),
        })
    }

    pub(crate) fn session_role_slot(&self, source: SessionRole, transaction: u32) -> Option<usize> {
        let identity = match source {
            SessionRole::Bootstrap => self.catalog_sources.role(0, RoleOid::BOOTSTRAP),
            SessionRole::Unauthenticated => return None,
            SessionRole::Bound(identity) => identity,
        };
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let slot = usize::from(identity.slot);
        catalog
            .roles
            .get(slot)
            .filter(|role| {
                role.visible_to(transaction)
                    && role.lifetime == Some(identity.lifetime)
                    && identity.origin == self.catalog_sources.origin
            })
            .map(|_| slot)
    }

    pub(crate) fn resolve_session_role(
        &self,
        source: SessionRole,
        transaction: u32,
    ) -> Result<RoleReference, SqlError> {
        let identity = match source {
            SessionRole::Bootstrap => self.catalog_sources.role(0, RoleOid::BOOTSTRAP),
            SessionRole::Unauthenticated => {
                return Err(crate::sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "session role is not authenticated"
                ));
            }
            SessionRole::Bound(identity) => identity,
        };
        let slot = usize::from(identity.slot);
        let catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let role = catalog
            .roles
            .get(slot)
            .filter(|role| {
                role.visible_to(transaction)
                    && role.lifetime == Some(identity.lifetime)
                    && identity.origin == self.catalog_sources.origin
            })
            .ok_or_else(|| {
                crate::sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "invalid role OID: {}",
                    identity.lifetime.get()
                )
            })?;
        Ok(RoleReference {
            identity,
            name: role.name_to(transaction),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatalogObjectSlot {
    Table(usize),
    View(usize),
    Rule(usize),
    MaterializedView(usize),
    Sequence(usize),
    Routine(usize),
}

impl CatalogObjectSlot {
    fn index(self) -> usize {
        match self {
            Self::Table(slot)
            | Self::View(slot)
            | Self::Rule(slot)
            | Self::MaterializedView(slot)
            | Self::Sequence(slot)
            | Self::Routine(slot) => slot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CatalogObjectIdentity {
    origin: CatalogOrigin,
    database: DatabaseOid,
    slot: CatalogObjectSlot,
    created_at: u64,
}

impl CatalogObjectIdentity {
    pub(super) fn slot(self) -> CatalogObjectSlot {
        self.slot
    }

    pub(super) fn view_slot(self) -> Option<usize> {
        match self.slot {
            CatalogObjectSlot::View(slot) => Some(slot),
            _ => None,
        }
    }

    pub(super) fn matches(
        self,
        storage: &Storage,
        database: DatabaseOid,
        created_at: u64,
        state: CatalogDdlState,
    ) -> bool {
        self.origin == storage.catalog_sources.origin()
            && self.database == current_database()
            && self.database == database
            && self.created_at == created_at
            && state != CatalogDdlState::Absent
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CatalogEditTarget {
    pub(super) source: CatalogObjectIdentity,
    pub(super) transaction: u32,
    pub(super) version: u64,
}

impl CatalogEditTarget {
    pub(crate) fn slot(self) -> usize {
        self.source.slot.index()
    }

    pub(super) fn view_slot(self) -> Result<usize, SqlError> {
        match self.source.slot {
            CatalogObjectSlot::View(slot) => Ok(slot),
            _ => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "edit source is not a view"
            )),
        }
    }

    pub(super) fn sequence_slot(self) -> Result<usize, SqlError> {
        match self.source.slot {
            CatalogObjectSlot::Sequence(slot) => Ok(slot),
            _ => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "edit source is not a sequence"
            )),
        }
    }

    pub(super) fn routine_slot(self) -> Result<usize, SqlError> {
        match self.source.slot {
            CatalogObjectSlot::Routine(slot) => Ok(slot),
            _ => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "edit source is not a routine"
            )),
        }
    }

    pub(super) fn rule_slot(self) -> Result<usize, SqlError> {
        match self.source.slot {
            CatalogObjectSlot::Rule(slot) => Ok(slot),
            _ => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "edit source is not a rule"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatalogDdlOperation {
    Create,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatalogDdlUndo {
    pub(super) source: CatalogObjectIdentity,
    pub(super) transaction: u32,
    pub(super) operation: CatalogDdlOperation,
    pub(super) version: u64,
    pub(super) prior_version: u64,
}

impl CatalogDdlUndo {
    pub(crate) fn slot(self) -> usize {
        self.source.slot.index()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogDdlDecision {
    Commit,
    Rollback,
}

impl Storage {
    pub(super) fn catalog_object_identity(
        &self,
        slot: CatalogObjectSlot,
        database: DatabaseOid,
        created_at: u64,
    ) -> CatalogObjectIdentity {
        CatalogObjectIdentity {
            origin: self.catalog_sources.origin(),
            database,
            slot,
            created_at,
        }
    }
}

impl Storage {
    pub(super) fn stage_catalog_comment_drop(
        &self,
        comments: &mut super::CommentCatalog,
        source: CatalogObjectIdentity,
        transaction: u32,
        matches: impl Fn(&super::CommentEntry) -> bool,
    ) -> Result<(), SqlError> {
        if comments
            .entries
            .iter()
            .filter(|comment| matches(comment))
            .any(|comment| {
                comment
                    .pending
                    .is_some_and(|pending| pending.txid != transaction)
                    || comment
                        .pending_identity
                        .is_some_and(|identity| identity.txid != transaction)
                    || comment
                        .pending_drop
                        .is_some_and(|pending| pending.txid != transaction)
            })
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "object comment is owned by another transaction"
            ));
        }
        for comment in comments
            .entries
            .iter_mut()
            .filter(|comment| matches(comment))
        {
            comment.pending_drop = Some(super::PendingCommentDrop {
                txid: transaction,
                source,
            });
        }
        Ok(())
    }

    pub(super) fn finish_catalog_comment_drop(
        &self,
        comments: &mut super::CommentCatalog,
        source: CatalogObjectIdentity,
        transaction: u32,
        decision: CatalogDdlDecision,
    ) {
        for comment in comments.entries.iter_mut().filter(|comment| {
            comment
                .pending_drop
                .is_some_and(|pending| pending.txid == transaction && pending.source == source)
        }) {
            match decision {
                CatalogDdlDecision::Commit => *comment = super::CommentEntry::empty(),
                CatalogDdlDecision::Rollback => comment.pending_drop = None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_creation_role_oid_boundary_rejects_unreserved_identifiers() {
        for value in [i32::MIN, -1, 0, 1, 9, 11, 16_384] {
            assert!(RoleOid::parse(value).is_none());
        }
        for value in [10, 16_385, i32::MAX] {
            assert_eq!(RoleOid::parse(value).unwrap().get(), value);
        }
    }

    #[test]
    fn catalog_creation_role_lifetimes_reject_exhaustion_without_reuse() {
        let counter = AtomicU64::new(i32::MAX as u64 - 1);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                CatalogSources::reserve(&counter, "role").unwrap().get(),
                i32::MAX as u64
            );
            for _ in 0..2 {
                let error = CatalogSources::reserve(&counter, "role").unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(error.message.as_str(), "role identity space exhausted");
                assert_eq!(counter.load(Ordering::Relaxed), i32::MAX as u64);
            }
        });
    }
}
