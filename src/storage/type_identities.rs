//! Checked creation stamps for named SQL type lifetimes.

#[cfg(test)]
use core::num::NonZeroU8;
use core::num::NonZeroU64;

#[cfg(test)]
pub(crate) const MAX_TYPE_CREATION: u64 = 9_999_999;

/// A nonzero stamp with a three-byte optional representation.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub(crate) struct TypeCreation {
    low: NonZeroU8,
    upper: [u8; 2],
}

#[cfg(test)]
impl TypeCreation {
    /// Rejects zero and stamps outside the named-type identifier range.
    pub(crate) const fn parse(created_at: u64) -> Option<Self> {
        if created_at == 0 || created_at > MAX_TYPE_CREATION {
            return None;
        }
        let ordinal = created_at as u32 - 1;
        // Radix 255 leaves the first byte nonzero, so Option needs no tag byte.
        let low = match NonZeroU8::new((ordinal % 255 + 1) as u8) {
            Some(low) => low,
            None => return None,
        };
        Some(Self {
            low,
            upper: ((ordinal / 255) as u16).to_le_bytes(),
        })
    }

    pub(crate) const fn created_at(self) -> u64 {
        self.low.get() as u64 + 255 * u16::from_le_bytes(self.upper) as u64
    }
}

#[cfg(test)]
const _: [(); 3] = [(); core::mem::size_of::<TypeCreation>()];
#[cfg(test)]
const _: [(); 3] = [(); core::mem::size_of::<Option<TypeCreation>>()];
#[cfg(test)]
const _: [(); 1] = [(); core::mem::align_of::<TypeCreation>()];

use super::DatabaseOid;
use super::type_names::NamedTypeSlot;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::{FixedMap, FixedVec};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TypeLifetimeKey {
    database: DatabaseOid,
    creation: NonZeroU64,
}

/// Both directions share one startup reservation; slot replacement cannot grow it.
pub(super) struct TypeIdentityIndex {
    by_lifetime: FixedMap<TypeLifetimeKey, NamedTypeSlot>,
    by_slot: FixedVec<Option<TypeLifetimeKey>>,
    domains: usize,
    enums: usize,
}

impl core::fmt::Debug for TypeIdentityIndex {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("TypeIdentityIndex")
            .field("assigned", &self.by_lifetime.len())
            .field("capacity", &self.by_slot.len())
            .field("domains", &self.domains)
            .field("enums", &self.enums)
            .finish()
    }
}

impl TypeIdentityIndex {
    pub(super) fn budget_bytes(domains: usize, enums: usize, composites: usize) -> usize {
        let capacity = domains.saturating_add(enums).saturating_add(composites);
        FixedMap::<TypeLifetimeKey, NamedTypeSlot>::budget_bytes(capacity)
            .saturating_add(capacity.saturating_mul(core::mem::size_of::<Option<TypeLifetimeKey>>()))
    }

    pub(super) fn new(
        budget: &mut Budget,
        domains: usize,
        enums: usize,
        composites: usize,
    ) -> Result<Self, BudgetError> {
        let capacity = domains.saturating_add(enums).saturating_add(composites);
        let by_lifetime = FixedMap::new(budget, "type_creation_index", capacity)?;
        let mut by_slot = FixedVec::new(budget, "type_creation_slots", capacity)?;
        for _ in 0..capacity {
            by_slot.push(None).expect("sized to named type catalog slots");
        }
        Ok(Self { by_lifetime, by_slot, domains, enums })
    }

    fn position(&self, slot: NamedTypeSlot) -> Result<usize, SqlError> {
        let position = match slot {
            NamedTypeSlot::Domain(index) if index < self.domains => Some(index),
            NamedTypeSlot::Enum(index) if index < self.enums => Some(self.domains + index),
            NamedTypeSlot::Composite(index)
                if index < self.by_slot.len() - self.domains - self.enums =>
            {
                Some(self.domains + self.enums + index)
            }
            _ => None,
        };
        position.ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION,
            "named type identity slot is outside its startup reservation"))
    }

    fn validate_slot(&self, slot: NamedTypeSlot, position: usize) -> Result<(), SqlError> {
        if self.by_slot[position].is_some_and(|key| self.by_lifetime.get(&key) != Some(&slot)) {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION,
                "named type identity index does not match its slot"));
        }
        Ok(())
    }

    pub(super) fn lookup(&self, database: DatabaseOid, created_at: u64) -> Option<NamedTypeSlot> {
        let creation = NonZeroU64::new(created_at)?;
        self.by_lifetime.get(&TypeLifetimeKey { database, creation }).copied()
    }

    /// The catalog owner has already validated that this slot can be replaced.
    pub(super) fn replace(
        &mut self,
        slot: NamedTypeSlot,
        database: DatabaseOid,
        created_at: u64,
    ) -> Result<(), SqlError> {
        let position = self.position(slot)?;
        self.validate_slot(slot, position)?;
        let old = self.by_slot[position];
        let creation = NonZeroU64::new(created_at).ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION,
            "named type creation identity is zero"))?;
        let key = TypeLifetimeKey { database, creation };
        if let Some(owner) = self.by_lifetime.get(&key)
            && (*owner != slot || old != Some(key))
        {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION,
                "named type creation identity is duplicated"));
        }
        if old == Some(key) { return Ok(()); }
        if old.is_none() && self.by_lifetime.len() == self.by_slot.len() {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "named type identity capacity exhausted"));
        }
        if let Some(old) = old {
            self.by_lifetime.remove(&old).expect("slot membership was checked");
        }
        self.by_lifetime.insert(key, slot).expect("a catalog slot reserves one identity entry");
        self.by_slot[position] = Some(key);
        Ok(())
    }

    /// Recovery owns the catalog and has checked the destination's availability.
    pub(super) fn relocate(
        &mut self,
        allocated: NamedTypeSlot,
        target: NamedTypeSlot,
        database: DatabaseOid,
        created_at: u64,
    ) -> Result<(), SqlError> {
        if allocated.kind() != target.kind() {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION,
                "stored type identity changes its catalog family"));
        }
        if allocated == target { return self.replace(target, database, created_at); }
        let old_position = self.position(allocated)?;
        let new_position = self.position(target)?;
        self.validate_slot(allocated, old_position)?;
        self.validate_slot(target, new_position)?;
        let old = self.by_slot[old_position].ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION,
            "restored type source has no creation identity"))?;
        let destination = self.by_slot[new_position];
        let creation = NonZeroU64::new(created_at).ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION,
            "named type creation identity is zero"))?;
        let key = TypeLifetimeKey { database, creation };
        if let Some(owner) = self.by_lifetime.get(&key)
            && (*owner != allocated || old != key)
        {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION,
                "named type creation identity is duplicated"));
        }
        self.by_lifetime.remove(&old).expect("source membership was checked");
        if let Some(destination) = destination {
            self.by_lifetime.remove(&destination).expect("destination membership was checked");
        }
        self.by_lifetime.insert(key, target).expect("relocation retains a reserved identity entry");
        self.by_slot[old_position] = None;
        self.by_slot[new_position] = Some(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_identity_index_replaces_at_capacity_and_isolates_databases_without_allocation() {
        fn creation(value: u64) -> u64 { TypeCreation::parse(value).unwrap().created_at() }
        let bytes = TypeIdentityIndex::budget_bytes(2, 1, 1);
        let mut budget = Budget::new(bytes);
        let mut index = TypeIdentityIndex::new(&mut budget, 2, 1, 1).unwrap();
        assert_eq!(budget.remaining(), 0);
        let database = DatabaseOid::POSTGRES;
        let other = DatabaseOid::TEMPLATE1;
        crate::mem::guard::forbid_alloc(|| {
            index.replace(NamedTypeSlot::Domain(0), database, creation(1)).unwrap();
            index.replace(NamedTypeSlot::Domain(1), other, creation(1)).unwrap();
            index.replace(NamedTypeSlot::Enum(0), database, creation(2)).unwrap();
            index.replace(NamedTypeSlot::Composite(0), database, creation(3)).unwrap();
            let mut old = creation(1);
            for value in 4..1_004 {
                index.replace(NamedTypeSlot::Domain(0), database, creation(value)).unwrap();
                assert_eq!(index.lookup(database, old), None);
                assert_eq!(index.lookup(other, creation(1)), Some(NamedTypeSlot::Domain(1)));
                assert_eq!(index.by_lifetime.len(), 4);
                old = creation(value);
            }
            let duplicate = index.replace(NamedTypeSlot::Enum(0), database, old).unwrap_err();
            assert_eq!(duplicate.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(index.lookup(database, creation(2)), Some(NamedTypeSlot::Enum(0)));
            assert_eq!(index.lookup(database, old), Some(NamedTypeSlot::Domain(0)));
            index.relocate(NamedTypeSlot::Domain(0), NamedTypeSlot::Domain(1), database, creation(1_500)).unwrap();
            assert_eq!(index.lookup(database, old), None);
            assert_eq!(index.lookup(other, creation(1)), None);
            assert_eq!(index.lookup(database, creation(1_500)), Some(NamedTypeSlot::Domain(1)));
            index.replace(NamedTypeSlot::Domain(0), other, creation(1_500)).unwrap();
            assert_eq!(index.lookup(other, creation(1_500)), Some(NamedTypeSlot::Domain(0)));
            let duplicate = index.relocate(NamedTypeSlot::Domain(1), NamedTypeSlot::Domain(0), database, creation(2)).unwrap_err();
            assert_eq!(duplicate.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(index.lookup(database, creation(1_500)), Some(NamedTypeSlot::Domain(1)));
            assert_eq!(index.lookup(other, creation(1_500)), Some(NamedTypeSlot::Domain(0)));
            assert_eq!(index.lookup(database, creation(2)), Some(NamedTypeSlot::Enum(0)));
            let wrong_family = index.relocate(NamedTypeSlot::Domain(0), NamedTypeSlot::Enum(0), other, creation(1_600)).unwrap_err();
            assert_eq!(wrong_family.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(index.by_lifetime.len(), 4);
        });
    }

    #[test]
    fn type_creation_identity_roundtrips_without_allocation() {
        crate::mem::guard::forbid_alloc(|| {
            for upper in [0, 1, 254, 255, 256, 32_768, 39_214, 39_215] {
                for low in [0, 1, 253, 254] {
                    let created_at = upper * 255 + low + 1;
                    if created_at <= MAX_TYPE_CREATION {
                        assert_eq!(
                            TypeCreation::parse(created_at).unwrap().created_at(),
                            created_at
                        );
                    }
                }
            }
            assert_eq!(
                TypeCreation::parse(MAX_TYPE_CREATION).unwrap().created_at(),
                MAX_TYPE_CREATION
            );
            for rejected in [0, MAX_TYPE_CREATION + 1, u64::MAX] {
                assert!(TypeCreation::parse(rejected).is_none());
            }
        });
    }
}
