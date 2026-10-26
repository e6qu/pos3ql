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
            Self::Restore(identity) => {
                let created_at = u64::from(identity.0.get());
                sequence.observe(created_at);
                Ok(created_at)
            }
        }
    }
}
