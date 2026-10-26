//! Checked creation stamps for named SQL type lifetimes.

use core::num::NonZeroU8;

pub(crate) const MAX_TYPE_CREATION: u64 = 9_999_999;

/// A nonzero stamp with a three-byte optional representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct TypeCreation {
    low: NonZeroU8,
    upper: [u8; 2],
}

impl TypeCreation {
    pub(crate) fn require(creation: Option<Self>) -> Result<Self, SqlError> {
        creation.ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT,
            "named type declaration has no bound identity"))
    }

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

    pub(crate) const fn to_le_bytes(self) -> [u8; 4] {
        (self.created_at() as u32).to_le_bytes()
    }

    pub(crate) const fn from_le_bytes(bytes: [u8; 4]) -> Option<Self> {
        Self::parse(u32::from_le_bytes(bytes) as u64)
    }

    pub(crate) const fn created_at(self) -> u64 {
        self.low.get() as u64 + 255 * u16::from_le_bytes(self.upper) as u64
    }
}

impl PartialOrd for TypeCreation {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for TypeCreation {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering { self.created_at().cmp(&other.created_at()) }
}

const _: [(); 3] = [(); core::mem::size_of::<TypeCreation>()];
const _: [(); 3] = [(); core::mem::size_of::<Option<TypeCreation>>()];
const _: [(); 1] = [(); core::mem::align_of::<TypeCreation>()];

use super::DatabaseOid;
use super::type_names::NamedTypeSlot;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::{FixedMap, FixedVec};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnumLabelIdentity(TypeCreation);

impl EnumLabelIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        TypeCreation::parse(created_at).map(Self)
    }
    pub(crate) fn created_at(self) -> u64 {
        self.0.created_at()
    }
    pub(crate) fn oid(self) -> i32 {
        360_000_000 + self.created_at() as i32
    }
    pub(super) fn reserve(sequence: &super::CatalogSequence) -> Result<Self, SqlError> {
        let created_at = sequence.next_bounded(MAX_TYPE_CREATION, "enum label")?;
        Ok(Self::parse(created_at).expect("bounded enum label reservation"))
    }
}

#[derive(Clone, Copy)]
pub(super) enum TypeCreationAdmission {
    Fresh,
    Restored(TypeCreation),
}

impl TypeCreationAdmission {
    pub(super) fn restored(created_at: u64) -> Result<Self, SqlError> {
        TypeCreation::parse(created_at)
            .map(Self::Restored)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored type creation identity is outside 1..=9999999"
                )
            })
    }

    pub(super) fn stamp(
        self,
        sequence: &super::CatalogSequence,
        maximum: u64,
        family: &str,
    ) -> Result<u64, SqlError> {
        match self {
            Self::Fresh => sequence.next_bounded(maximum.min(MAX_TYPE_CREATION), family),
            Self::Restored(creation) => {
                super::bounded_catalog_generation(creation.created_at(), maximum, family)
            }
        }
    }

    pub(super) fn installed(self, sequence: &super::CatalogSequence, created_at: u64) {
        if matches!(self, Self::Restored(_)) {
            sequence.observe(created_at);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TypeLifetimeKey {
    database: DatabaseOid,
    creation: TypeCreation,
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
        formatter
            .debug_struct("TypeIdentityIndex")
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
        FixedMap::<TypeLifetimeKey, NamedTypeSlot>::budget_bytes(capacity).saturating_add(
            capacity.saturating_mul(core::mem::size_of::<Option<TypeLifetimeKey>>()),
        )
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
            by_slot
                .push(None)
                .expect("sized to named type catalog slots");
        }
        Ok(Self {
            by_lifetime,
            by_slot,
            domains,
            enums,
        })
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
        position.ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type identity slot is outside its startup reservation"
            )
        })
    }

    fn validate_slot(&self, slot: NamedTypeSlot, position: usize) -> Result<(), SqlError> {
        if self.by_slot[position].is_some_and(|key| self.by_lifetime.get(&key) != Some(&slot)) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type identity index does not match its slot"
            ));
        }
        Ok(())
    }

    pub(super) fn lookup(&self, database: DatabaseOid, created_at: u64) -> Option<NamedTypeSlot> {
        let creation = TypeCreation::parse(created_at)?;
        self.by_lifetime
            .get(&TypeLifetimeKey { database, creation })
            .copied()
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
        let creation = TypeCreation::parse(created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type creation identity is outside 1..=9999999"
            )
        })?;
        let key = TypeLifetimeKey { database, creation };
        if let Some(owner) = self.by_lifetime.get(&key)
            && (*owner != slot || old != Some(key))
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type creation identity is duplicated"
            ));
        }
        if old == Some(key) {
            return Ok(());
        }
        if old.is_none() && self.by_lifetime.len() == self.by_slot.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "named type identity capacity exhausted"
            ));
        }
        if let Some(old) = old {
            self.by_lifetime
                .remove(&old)
                .expect("slot membership was checked");
        }
        self.by_lifetime
            .insert(key, slot)
            .expect("a catalog slot reserves one identity entry");
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
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored type identity changes its catalog family"
            ));
        }
        if allocated == target {
            return self.replace(target, database, created_at);
        }
        let old_position = self.position(allocated)?;
        let new_position = self.position(target)?;
        self.validate_slot(allocated, old_position)?;
        self.validate_slot(target, new_position)?;
        let old = self.by_slot[old_position].ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "restored type source has no creation identity"
            )
        })?;
        let destination = self.by_slot[new_position];
        let creation = TypeCreation::parse(created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type creation identity is outside 1..=9999999"
            )
        })?;
        let key = TypeLifetimeKey { database, creation };
        if let Some(owner) = self.by_lifetime.get(&key)
            && (*owner != allocated || old != key)
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "named type creation identity is duplicated"
            ));
        }
        self.by_lifetime
            .remove(&old)
            .expect("source membership was checked");
        if let Some(destination) = destination {
            self.by_lifetime
                .remove(&destination)
                .expect("destination membership was checked");
        }
        self.by_lifetime
            .insert(key, target)
            .expect("relocation retains a reserved identity entry");
        self.by_slot[old_position] = None;
        self.by_slot[new_position] = Some(key);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayElementIdentity {
    Builtin(u8),
    Enum(TypeCreation),
    Composite(TypeCreation),
    DomainBuiltin {
        domain: TypeCreation,
        base_code: u8,
    },
    DomainEnum {
        domain: TypeCreation,
        base: TypeCreation,
    },
    DomainComposite {
        domain: TypeCreation,
        base: TypeCreation,
    },
}

/// A checked six-byte descriptor for inline cached array defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedArrayElement([u8; 6]);

impl CachedArrayElement {
    pub(crate) fn from_element(element: crate::sql::types::ArrElem) -> Result<Self, SqlError> {
        use crate::sql::types::{ArrElem, ColType};
        use ArrayElementIdentity::*;
        let invalid = || sql_err!(sqlstate::UNDEFINED_OBJECT,
            "array element has no bound type identity");
        let identity = match element {
            ArrElem::Enum(creation) => Enum(creation.ok_or_else(invalid)?),
            ArrElem::Composite(creation) => Composite(creation.ok_or_else(invalid)?),
            ArrElem::Domain { identity, base_code, base_identity } => {
                let domain = identity.ok_or_else(invalid)?;
                match ColType::from_code(base_code).ok_or_else(invalid)? {
                    ColType::Enum(_) => DomainEnum { domain, base: base_identity.ok_or_else(invalid)? },
                    ColType::Composite(_) => DomainComposite { domain, base: base_identity.ok_or_else(invalid)? },
                    _ if base_identity.is_none() => DomainBuiltin { domain, base_code },
                    _ => return Err(invalid()),
                }
            }
            _ => Builtin(element.code()),
        };
        Self::from_identity(identity).ok_or_else(invalid)
    }

    pub(crate) fn element(self) -> crate::sql::types::ArrElem {
        use crate::sql::types::{ArrElem, ColType};
        use ArrayElementIdentity::*;
        match self.identity() {
            Builtin(code) => ArrElem::from_code(code).expect("checked builtin array element"),
            Enum(creation) => ArrElem::Enum(Some(creation)),
            Composite(creation) => ArrElem::Composite(Some(creation)),
            DomainBuiltin { domain, base_code } => ArrElem::Domain {
                identity: Some(domain), base_code, base_identity: None,
            },
            DomainEnum { domain, base } => ArrElem::Domain {
                identity: Some(domain), base_code: ColType::Enum(None).code(), base_identity: Some(base),
            },
            DomainComposite { domain, base } => ArrElem::Domain {
                identity: Some(domain), base_code: ColType::Composite(None).code(), base_identity: Some(base),
            },
        }
    }

    pub(crate) fn bytes(self) -> [u8; 6] { self.0 }

    const ENUM: u64 = 256;
    const COMPOSITE: u64 = Self::ENUM + MAX_TYPE_CREATION;
    const DOMAIN_BUILTIN: u64 = Self::COMPOSITE + MAX_TYPE_CREATION;
    const DOMAIN_ENUM: u64 = Self::DOMAIN_BUILTIN + MAX_TYPE_CREATION * 256;
    const DOMAIN_COMPOSITE: u64 = Self::DOMAIN_ENUM + MAX_TYPE_CREATION * MAX_TYPE_CREATION;
    const END: u64 = Self::DOMAIN_COMPOSITE + MAX_TYPE_CREATION * MAX_TYPE_CREATION;

    fn builtin(code: u8) -> bool {
        use crate::sql::types::ArrElem;
        ArrElem::from_code(code).is_some_and(|element| {
            !matches!(
                element,
                ArrElem::Enum(_) | ArrElem::Composite(_) | ArrElem::Domain { .. }
            )
        })
    }

    fn builtin_base(code: u8) -> bool {
        use crate::sql::types::ColType;
        crate::sql::types::ColType::from_code(code).is_some_and(|base| match base {
            ColType::Enum(_) | ColType::Composite(_) | ColType::Record | ColType::Void
                | ColType::Internal | ColType::PgDdlCommand | ColType::AnyArray => false,
            ColType::Array(element) => !matches!(
                element,
                crate::sql::types::ArrElem::Enum(_)
                    | crate::sql::types::ArrElem::Composite(_)
                    | crate::sql::types::ArrElem::Domain { .. }
            ),
            _ => true,
        })
    }

    fn from_identity(identity: ArrayElementIdentity) -> Option<Self> {
        use ArrayElementIdentity::*;
        let value = match identity {
            Builtin(code) if Self::builtin(code) => u64::from(code),
            Enum(creation) => Self::ENUM + creation.created_at() - 1,
            Composite(creation) => Self::COMPOSITE + creation.created_at() - 1,
            DomainBuiltin { domain, base_code } if Self::builtin_base(base_code) => {
                Self::DOMAIN_BUILTIN + (domain.created_at() - 1) * 256 + u64::from(base_code)
            }
            DomainEnum { domain, base } => {
                Self::DOMAIN_ENUM
                    + (domain.created_at() - 1) * MAX_TYPE_CREATION
                    + base.created_at()
                    - 1
            }
            DomainComposite { domain, base } => {
                Self::DOMAIN_COMPOSITE
                    + (domain.created_at() - 1) * MAX_TYPE_CREATION
                    + base.created_at()
                    - 1
            }
            _ => return None,
        };
        let bytes = value.to_le_bytes();
        Some(Self([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
        ]))
    }

    fn value(self) -> u64 {
        let [a, b, c, d, e, f] = self.0;
        u64::from_le_bytes([a, b, c, d, e, f, 0, 0])
    }

    fn decode(value: u64) -> Option<ArrayElementIdentity> {
        use ArrayElementIdentity::*;
        if value < Self::ENUM {
            return Self::builtin(value as u8).then_some(Builtin(value as u8));
        }
        if value < Self::COMPOSITE {
            return TypeCreation::parse(value - Self::ENUM + 1).map(Enum);
        }
        if value < Self::DOMAIN_BUILTIN {
            return TypeCreation::parse(value - Self::COMPOSITE + 1).map(Composite);
        }
        if value < Self::DOMAIN_ENUM {
            let offset = value - Self::DOMAIN_BUILTIN;
            let base_code = (offset % 256) as u8;
            return Self::builtin_base(base_code).then(|| DomainBuiltin {
                domain: TypeCreation::parse(offset / 256 + 1).expect("bounded domain identity"),
                base_code,
            });
        }
        let (start, composite) = if value < Self::DOMAIN_COMPOSITE {
            (Self::DOMAIN_ENUM, false)
        } else if value < Self::END {
            (Self::DOMAIN_COMPOSITE, true)
        } else {
            return None;
        };
        let offset = value - start;
        let domain = TypeCreation::parse(offset / MAX_TYPE_CREATION + 1)?;
        let base = TypeCreation::parse(offset % MAX_TYPE_CREATION + 1)?;
        Some(if composite {
            DomainComposite { domain, base }
        } else {
            DomainEnum { domain, base }
        })
    }

    pub(crate) fn from_bytes(bytes: [u8; 6]) -> Option<Self> {
        let encoded = Self(bytes);
        Self::decode(encoded.value())?;
        Some(encoded)
    }

    fn identity(self) -> ArrayElementIdentity {
        Self::decode(self.value()).expect("cached array descriptor was checked")
    }
}

const _: [(); 6] = [(); core::mem::size_of::<CachedArrayElement>()];
const _: () = assert!(CachedArrayElement::END <= 1 << 48);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_array_descriptors_preserve_maximum_creation_identities_without_allocation() {
        fn check(identity: ArrayElementIdentity) {
            let encoded = CachedArrayElement::from_identity(identity).unwrap();
            assert_eq!(CachedArrayElement::from_element(encoded.element()).unwrap(), encoded);
            assert_eq!(encoded.identity(), identity);
            assert_eq!(
                CachedArrayElement::from_bytes(encoded.0)
                    .unwrap()
                    .identity(),
                identity
            );
        }
        crate::mem::guard::forbid_alloc(|| {
            for element in crate::sql::types::ArrElem::BUILTIN {
                check(ArrayElementIdentity::Builtin(element.code()));
            }
            for code in u8::MIN..=u8::MAX {
                let encoded =
                    CachedArrayElement::from_identity(ArrayElementIdentity::Builtin(code));
                // Anonymous records also have a transient array representation.
                let builtin = code == crate::sql::types::ArrElem::Record.code()
                    || crate::sql::types::ArrElem::BUILTIN
                        .iter()
                        .any(|element| element.code() == code);
                assert_eq!(encoded.is_some(), builtin, "array opcode {code}");
                if let Some(encoded) = encoded {
                    assert_eq!(encoded.identity(), ArrayElementIdentity::Builtin(code));
                }
            }
            for domain in [1, 255, 256, 65_535, MAX_TYPE_CREATION] {
                let domain = TypeCreation::parse(domain).unwrap();
                check(ArrayElementIdentity::Enum(domain));
                check(ArrayElementIdentity::Composite(domain));
                for base in [1, 255, 256, 65_535, MAX_TYPE_CREATION] {
                    let base = TypeCreation::parse(base).unwrap();
                    check(ArrayElementIdentity::DomainEnum { domain, base });
                    check(ArrayElementIdentity::DomainComposite { domain, base });
                }
                for base in [
                    crate::sql::types::ColType::Bool,
                    crate::sql::types::ColType::Text,
                    crate::sql::types::ColType::Array(crate::sql::types::ArrElem::Int4),
                ] {
                    check(ArrayElementIdentity::DomainBuiltin {
                        domain,
                        base_code: base.code(),
                    });
                }
            }
            assert!(CachedArrayElement::from_bytes([255; 6]).is_none());
            let invalid_builtin = crate::sql::types::ArrElem::Enum(None).code();
            assert!(
                CachedArrayElement::from_identity(ArrayElementIdentity::Builtin(invalid_builtin))
                    .is_none()
            );
        });
    }

    #[test]
    fn type_identity_index_replaces_at_capacity_and_isolates_databases_without_allocation() {
        fn creation(value: u64) -> u64 {
            TypeCreation::parse(value).unwrap().created_at()
        }
        let bytes = TypeIdentityIndex::budget_bytes(2, 1, 1);
        let mut budget = Budget::new(bytes);
        let mut index = TypeIdentityIndex::new(&mut budget, 2, 1, 1).unwrap();
        assert_eq!(budget.remaining(), 0);
        let database = DatabaseOid::POSTGRES;
        let other = DatabaseOid::TEMPLATE1;
        crate::mem::guard::forbid_alloc(|| {
            index
                .replace(NamedTypeSlot::Domain(0), database, creation(1))
                .unwrap();
            index
                .replace(NamedTypeSlot::Domain(1), other, creation(1))
                .unwrap();
            index
                .replace(NamedTypeSlot::Enum(0), database, creation(2))
                .unwrap();
            index
                .replace(NamedTypeSlot::Composite(0), database, creation(3))
                .unwrap();
            let mut old = creation(1);
            for value in 4..1_004 {
                index
                    .replace(NamedTypeSlot::Domain(0), database, creation(value))
                    .unwrap();
                assert_eq!(index.lookup(database, old), None);
                assert_eq!(
                    index.lookup(other, creation(1)),
                    Some(NamedTypeSlot::Domain(1))
                );
                assert_eq!(index.by_lifetime.len(), 4);
                old = creation(value);
            }
            let duplicate = index
                .replace(NamedTypeSlot::Enum(0), database, old)
                .unwrap_err();
            assert_eq!(duplicate.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(
                index.lookup(database, creation(2)),
                Some(NamedTypeSlot::Enum(0))
            );
            assert_eq!(index.lookup(database, old), Some(NamedTypeSlot::Domain(0)));
            index
                .relocate(
                    NamedTypeSlot::Domain(0),
                    NamedTypeSlot::Domain(1),
                    database,
                    creation(1_500),
                )
                .unwrap();
            assert_eq!(index.lookup(database, old), None);
            assert_eq!(index.lookup(other, creation(1)), None);
            assert_eq!(
                index.lookup(database, creation(1_500)),
                Some(NamedTypeSlot::Domain(1))
            );
            index
                .replace(NamedTypeSlot::Domain(0), other, creation(1_500))
                .unwrap();
            assert_eq!(
                index.lookup(other, creation(1_500)),
                Some(NamedTypeSlot::Domain(0))
            );
            let duplicate = index
                .relocate(
                    NamedTypeSlot::Domain(1),
                    NamedTypeSlot::Domain(0),
                    database,
                    creation(2),
                )
                .unwrap_err();
            assert_eq!(duplicate.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(
                index.lookup(database, creation(1_500)),
                Some(NamedTypeSlot::Domain(1))
            );
            assert_eq!(
                index.lookup(other, creation(1_500)),
                Some(NamedTypeSlot::Domain(0))
            );
            assert_eq!(
                index.lookup(database, creation(2)),
                Some(NamedTypeSlot::Enum(0))
            );
            let wrong_family = index
                .relocate(
                    NamedTypeSlot::Domain(0),
                    NamedTypeSlot::Enum(0),
                    other,
                    creation(1_600),
                )
                .unwrap_err();
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
