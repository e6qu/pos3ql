//! The single registry of synthesized OID bands.
//!
//! Every OID pos3ql invents for a user object lies in exactly one band below,
//! so an OID alone names its object family. Bands never overlap, never reach
//! PostgreSQL's built-in range (below 16,384) and fit a signed 32-bit OID; the
//! compile-time check at the end of this file rejects a table that breaks any
//! of those rules. A family that runs out of ordinals fails with `54000`
//! before it installs anything.
//!
//! Large-object OIDs are outside the registry: `lo_create(oid)` and `lo_import`
//! accept any OID the client names, exactly as PostgreSQL does.

use super::MAX_CATALOG_CREATION;

/// One family's OIDs: `base + ordinal` for each ordinal in `1..=capacity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OidBand {
    /// Roles created by `CREATE ROLE`; the ordinal is the role sequence value
    /// past 16,384, so the first user role is 16,385.
    UserRole,
    UserDatabase,
    ParameterAcl,
    /// `pg_temp_N` namespaces; the ordinal is the live namespace's registry
    /// number, reused after its connection ends, like PostgreSQL's backend
    /// number.
    TemporaryNamespace,
    Schema,
    Routine,
    Trigger,
    /// `pg_constraint` rows of `CREATE CONSTRAINT TRIGGER`, keyed by the
    /// trigger's stamp.
    TriggerConstraint,
    Policy,
    Tablespace,
    /// `pg_attrdef` rows; every default or generation expression gets a new
    /// stamp when it is set.
    AttributeDefault,
    PartitionTrigger,
    /// Constraint rows of inherited constraint triggers, keyed by the clone's
    /// stamp.
    PartitionTriggerConstraint,
    Domain,
    Enum,
    Composite,
    DomainArray,
    EnumArray,
    CompositeArray,
    /// Tables, views, sequences, composite backing relations and indexes share
    /// one relation stamp sequence.
    Relation,
    ToastRelation,
    ToastIndex,
    EnumLabel,
    Constraint,
    /// Four internal triggers per foreign key: `4 × stamp + 0..=3`.
    ForeignKeyTrigger,
    ExtendedStatistics,
    Extension,
    ForeignDataWrapper,
    ForeignServer,
    UserMapping,
    Publication,
    Subscription,
    Collation,
    Conversion,
    TextSearchParser,
    TextSearchTemplate,
    TextSearchDictionary,
    TextSearchConfiguration,
    Cast,
    Operator,
    OperatorFamily,
    OperatorClass,
    AccessMethod,
    EventTrigger,
    DefaultAcl,
    RoleMembership,
    RewriteRule,
}

/// The largest checked creation stamp, as an ordinal.
const STAMPS: u32 = MAX_CATALOG_CREATION as u32;
/// PostgreSQL's first OID for user objects (`FirstNormalObjectId`).
pub(crate) const FIRST_NORMAL_OBJECT_ID: i32 = 16_384;

impl OidBand {
    pub(crate) const ALL: [Self; 47] = [
        Self::UserRole,
        Self::UserDatabase,
        Self::ParameterAcl,
        Self::TemporaryNamespace,
        Self::Schema,
        Self::Routine,
        Self::Trigger,
        Self::TriggerConstraint,
        Self::Policy,
        Self::Tablespace,
        Self::AttributeDefault,
        Self::PartitionTrigger,
        Self::PartitionTriggerConstraint,
        Self::Domain,
        Self::Enum,
        Self::Composite,
        Self::DomainArray,
        Self::EnumArray,
        Self::CompositeArray,
        Self::Relation,
        Self::ToastRelation,
        Self::ToastIndex,
        Self::EnumLabel,
        Self::Constraint,
        Self::ForeignKeyTrigger,
        Self::ExtendedStatistics,
        Self::Extension,
        Self::ForeignDataWrapper,
        Self::ForeignServer,
        Self::UserMapping,
        Self::Publication,
        Self::Subscription,
        Self::Collation,
        Self::Conversion,
        Self::TextSearchParser,
        Self::TextSearchTemplate,
        Self::TextSearchDictionary,
        Self::TextSearchConfiguration,
        Self::Cast,
        Self::Operator,
        Self::OperatorFamily,
        Self::OperatorClass,
        Self::AccessMethod,
        Self::EventTrigger,
        Self::DefaultAcl,
        Self::RoleMembership,
        Self::RewriteRule,
    ];

    /// The OID just below the band's first OID.
    pub(crate) const fn base(self) -> i32 {
        match self {
            Self::UserRole => FIRST_NORMAL_OBJECT_ID,
            Self::UserDatabase => 10_000_000,
            Self::ParameterAcl => 20_000_000,
            Self::TemporaryNamespace => 30_000_000,
            Self::Schema => 40_000_000,
            Self::Routine => 50_000_000,
            Self::Trigger => 60_000_000,
            Self::TriggerConstraint => 70_000_000,
            Self::Policy => 80_000_000,
            Self::Tablespace => 90_000_000,
            Self::AttributeDefault => 100_000_000,
            Self::PartitionTrigger => 110_000_000,
            Self::PartitionTriggerConstraint => 120_000_000,
            Self::Domain => 180_000_000,
            Self::Enum => 200_000_000,
            Self::Composite => 220_000_000,
            Self::DomainArray => 240_000_000,
            Self::EnumArray => 260_000_000,
            Self::CompositeArray => 280_000_000,
            Self::Relation => 300_000_000,
            Self::ToastRelation => 320_000_000,
            Self::ToastIndex => 340_000_000,
            Self::EnumLabel => 360_000_000,
            Self::Constraint => 380_000_000,
            Self::ForeignKeyTrigger => 400_000_000,
            Self::ExtendedStatistics => 440_000_000,
            Self::Extension => 460_000_000,
            Self::ForeignDataWrapper => 480_000_000,
            Self::ForeignServer => 500_000_000,
            Self::UserMapping => 520_000_000,
            Self::Publication => 540_000_000,
            Self::Subscription => 560_000_000,
            Self::Collation => 580_000_000,
            Self::Conversion => 620_000_000,
            Self::TextSearchParser => 640_000_000,
            Self::TextSearchTemplate => 660_000_000,
            Self::TextSearchDictionary => 680_000_000,
            Self::TextSearchConfiguration => 700_000_000,
            Self::Cast => 720_000_000,
            Self::Operator => 740_000_000,
            Self::OperatorFamily => 760_000_000,
            Self::OperatorClass => 780_000_000,
            Self::AccessMethod => 800_000_000,
            Self::EventTrigger => 820_000_000,
            Self::DefaultAcl => 840_000_000,
            Self::RoleMembership => 860_000_000,
            Self::RewriteRule => 880_000_000,
        }
    }

    /// The largest ordinal: OIDs run from `base + 1` to `base + capacity`.
    pub(crate) const fn capacity(self) -> u32 {
        match self {
            Self::UserRole => 10_000_000 - FIRST_NORMAL_OBJECT_ID as u32 - 1,
            Self::ForeignKeyTrigger => 4 * STAMPS + 3,
            _ => STAMPS,
        }
    }

    pub(crate) const fn first(self) -> i32 {
        self.base() + 1
    }

    pub(crate) const fn last(self) -> i32 {
        self.base() + self.capacity() as i32
    }

    /// The OID of an ordinal already checked against the family's bound.
    pub(crate) const fn oid(self, ordinal: u64) -> i32 {
        assert!(ordinal >= 1 && ordinal <= self.capacity() as u64);
        self.base() + ordinal as i32
    }

    /// The ordinal of an OID in this band.
    pub(crate) const fn ordinal(self, oid: i32) -> Option<u64> {
        if oid >= self.first() && oid <= self.last() {
            Some((oid - self.base()) as u64)
        } else {
            None
        }
    }

    pub(crate) const fn contains(self, oid: i32) -> bool {
        self.ordinal(oid).is_some()
    }

    /// The band holding `oid`, if pos3ql synthesizes it.
    pub(crate) fn of(oid: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|band| band.contains(oid))
    }

    /// Where `oid` falls in PostgreSQL's single OID counter. Every band's
    /// ordinal is the catalog creation stamp (four foreign-key triggers share
    /// one), so stamps order objects of different bands by creation. Built-in
    /// OIDs precede every synthesized one.
    pub(crate) fn creation_order(oid: i32) -> u64 {
        match Self::of(oid) {
            Some(Self::ForeignKeyTrigger) => {
                Self::ForeignKeyTrigger.ordinal(oid).expect("band member") / 4
                    + FIRST_NORMAL_OBJECT_ID as u64
            }
            Some(band) => band.ordinal(oid).expect("band member") + FIRST_NORMAL_OBJECT_ID as u64,
            None => u64::from(oid as u32),
        }
    }
}

/// Rejects a registry with a band below `FirstNormalObjectId`, past the
/// largest signed OID, missing from `ALL`, or overlapping another band.
const fn check_registry() {
    let bands = &OidBand::ALL;
    // `ALL` lists every variant in declaration order; the last one closes it.
    assert!(OidBand::RewriteRule as usize + 1 == bands.len());
    let mut index = 0;
    while index < bands.len() {
        let band = bands[index];
        assert!(band as usize == index);
        assert!(band.base() >= FIRST_NORMAL_OBJECT_ID);
        assert!(band.capacity() >= 1);
        assert!(band.base() as i64 + band.capacity() as i64 <= i32::MAX as i64);
        let mut other = index + 1;
        while other < bands.len() {
            let next = bands[other];
            assert!(band.last() < next.first() || next.last() < band.first());
            other += 1;
        }
        index += 1;
    }
}

const _: () = check_registry();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_bands_are_disjoint_user_range_and_classify_every_oid() {
        let mut sorted = OidBand::ALL;
        sorted.sort_by_key(|band| band.base());
        assert!(sorted[0].first() > FIRST_NORMAL_OBJECT_ID);
        for pair in sorted.windows(2) {
            assert!(
                pair[0].last() < pair[1].first(),
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        for band in OidBand::ALL {
            assert_eq!(OidBand::of(band.first()), Some(band));
            assert_eq!(OidBand::of(band.last()), Some(band));
            assert_eq!(band.ordinal(band.oid(1)), Some(1));
            assert_eq!(
                band.ordinal(band.oid(u64::from(band.capacity()))),
                Some(u64::from(band.capacity()))
            );
            assert!(!band.contains(band.base()));
        }
        assert_eq!(OidBand::of(FIRST_NORMAL_OBJECT_ID), None);
        assert_eq!(OidBand::UserRole.first(), 16_385);
    }
}
