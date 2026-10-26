//! Durable identities for constraints and their foreign-key triggers.

use core::num::NonZeroU32;

use super::{CheckConstraint, DetachedPartitionBound, DomainDef, DomainSpec, ForeignKey, Storage, TableDef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) const MAX_CONSTRAINT_CREATION: u64 = 9_999_999;
const FIRST_CONSTRAINT_OID: i32 = 380_000_000;
const FIRST_FOREIGN_KEY_TRIGGER_OID: i32 = 400_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConstraintIdentity(NonZeroU32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForeignKeyTrigger {
    CheckInsert,
    CheckUpdate,
    ActionDelete,
    ActionUpdate,
}

impl ForeignKeyTrigger {
    pub(crate) const ALL: [Self; 4] = [Self::ActionDelete, Self::ActionUpdate, Self::CheckInsert, Self::CheckUpdate];

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::ActionDelete => 0,
            Self::ActionUpdate => 1,
            Self::CheckInsert => 2,
            Self::CheckUpdate => 3,
        }
    }
}

impl ConstraintIdentity {
    pub(crate) const ENCODED_BYTES: usize = core::mem::size_of::<u32>();

    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_CONSTRAINT_CREATION {
            return None;
        }
        NonZeroU32::new(u32::try_from(created_at).ok()?).map(Self)
    }

    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }

    pub(crate) fn to_le_bytes(self) -> [u8; 4] {
        self.0.get().to_le_bytes()
    }

    pub(crate) fn from_le_bytes(bytes: [u8; 4]) -> Option<Self> {
        Self::parse(u64::from(u32::from_le_bytes(bytes)))
    }

    pub(crate) fn oid(self) -> i32 {
        FIRST_CONSTRAINT_OID + self.0.get() as i32
    }

    pub(crate) fn foreign_key_trigger_oid(self, trigger: ForeignKeyTrigger) -> i32 {
        FIRST_FOREIGN_KEY_TRIGGER_OID + self.0.get() as i32 * 4 + trigger.ordinal() as i32
    }

    fn reserve(storage: &Storage) -> Result<Self, SqlError> {
        let created_at = storage.catalog_sequence.next_bounded(MAX_CONSTRAINT_CREATION, "constraint")?;
        Ok(Self::parse(created_at).expect("bounded constraint reservation"))
    }
}

impl CheckConstraint {
    pub(crate) fn oid(&self) -> i32 {
        self.identity.expect("admitted CHECK has a creation identity").oid()
    }
}

impl ForeignKey {
    pub(crate) fn oid(&self) -> i32 {
        self.identity.expect("admitted foreign key has a creation identity").oid()
    }

    pub(crate) fn trigger_oid(&self, trigger: ForeignKeyTrigger) -> i32 {
        self.identity.expect("admitted foreign key has a creation identity").foreign_key_trigger_oid(trigger)
    }
}

impl DetachedPartitionBound {
    pub(crate) fn oid(&self) -> i32 {
        self.identity.expect("admitted detached partition CHECK has a creation identity").oid()
    }
}

impl TableDef {
    pub(crate) fn not_null_constraint_oid(&self, column: usize) -> i32 {
        let attribute = usize::from(self.attribute_number(column) - 1);
        self.not_null_identities[attribute].expect("admitted NOT NULL has a creation identity").oid()
    }

    pub(crate) fn constraint_identities(&self) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks().iter().map(|check| check.identity)
            .chain(self.fkeys().iter().map(|key| key.identity))
            .chain(self.not_null_identities.iter().zip(&self.attributes.slots).filter_map(|(identity, slot)| match *slot {
                super::column_attributes::AttributeSlot::Live(column) if self.columns[usize::from(column)].not_null.is_required() => Some(*identity),
                _ => None,
            }))
            .chain(self.partition.detached_bound.iter().map(|check| check.identity))
    }

    pub(crate) fn constraint_identities_mut(&mut self) -> impl Iterator<Item = &mut Option<ConstraintIdentity>> {
        let columns = &self.columns;
        self.checks[..self.n_checks].iter_mut().map(|check| &mut check.identity)
            .chain(self.fkeys[..self.n_fkeys].iter_mut().map(|key| &mut key.identity))
            .chain(self.not_null_identities.iter_mut().zip(&self.attributes.slots).filter_map(move |(identity, slot)| match *slot {
                super::column_attributes::AttributeSlot::Live(column) if columns[usize::from(column)].not_null.is_required() => Some(identity),
                _ => None,
            }))
            .chain(self.partition.detached_bound.iter_mut().map(|check| &mut check.identity))
    }
}

impl DomainDef {
    pub(crate) fn not_null_constraint_oid(&self) -> i32 {
        self.not_null_identity.expect("admitted domain NOT NULL has a creation identity").oid()
    }

    pub(crate) fn constraint_identities(&self) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks().iter().map(|check| check.identity)
            .chain(self.not_null.then_some(self.not_null_identity))
    }
}

impl DomainSpec {
    pub(crate) fn constraint_identities(&self) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks[..self.n_checks].iter().map(|check| check.identity)
            .chain(self.not_null.then_some(self.not_null_identity))
    }

    pub(crate) fn constraint_identities_mut(&mut self) -> impl Iterator<Item = &mut Option<ConstraintIdentity>> {
        self.checks[..self.n_checks].iter_mut().map(|check| &mut check.identity)
            .chain(self.not_null.then_some(&mut self.not_null_identity))
    }
}

pub(crate) fn validate_constraint_identities(identities: impl Iterator<Item = Option<ConstraintIdentity>> + Clone) -> Result<(), SqlError> {
    for (position, identity) in identities.clone().enumerate() {
        let identity = identity.ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "saved constraint has no creation identity"))?;
        if identities.clone().take(position).any(|earlier| earlier == Some(identity)) {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION, "saved constraint creation identity is duplicated"));
        }
    }
    Ok(())
}

impl Storage {
    pub(super) fn reserve_constraint_identities(&self, definition: &mut TableDef) -> Result<(), SqlError> {
        for (identity, slot) in definition.not_null_identities.iter_mut().zip(&definition.attributes.slots) {
            let active = match *slot {
                super::column_attributes::AttributeSlot::Live(column) => definition.columns[usize::from(column)].not_null.is_required(),
                _ => false,
            };
            if !active { *identity = None; }
        }
        for identity in definition.constraint_identities_mut() {
            if identity.is_none() { *identity = Some(ConstraintIdentity::reserve(self)?); }
        }
        validate_constraint_identities(definition.constraint_identities())
    }

    pub(super) fn reserve_domain_constraint_identities(&self, definition: &mut DomainSpec) -> Result<(), SqlError> {
        if !definition.not_null { definition.not_null_identity = None; }
        for identity in definition.constraint_identities_mut() {
            if identity.is_none() { *identity = Some(ConstraintIdentity::reserve(self)?); }
        }
        validate_constraint_identities(definition.constraint_identities())
    }
}
