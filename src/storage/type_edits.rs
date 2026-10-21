//! Definition revisions and undo records identify one source and one edit.

use core::mem::size_of;

use super::type_names::NamedTypeSlot;
use super::type_references::TypeIdentity;
use super::{PendingCompositeDefinition, PendingDomainDefinition, PendingEnumDefinition};
use crate::config::Config;
use crate::mem::FixedVec;
use crate::mem::budget::{Budget, BudgetError};

#[derive(Debug, Clone, Copy)]
pub(crate) struct TypeAlterUndo<Definition> {
    pub(super) source: TypeIdentity,
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) prior: Option<Definition>,
}

pub(crate) type DomainAlterUndo = TypeAlterUndo<PendingDomainDefinition>;
pub(crate) type EnumAlterUndo = TypeAlterUndo<PendingEnumDefinition>;
pub(crate) type CompositeAlterUndo = TypeAlterUndo<PendingCompositeDefinition>;

#[derive(Debug)]
pub(super) struct TypeDefinitionVersions {
    domains: FixedVec<u64>,
    enums: FixedVec<u64>,
    composites: FixedVec<u64>,
}

impl TypeDefinitionVersions {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>().saturating_add(
            config.max_domains.saturating_add(config.max_enums)
                .saturating_add(config.max_composites).saturating_mul(size_of::<u64>()),
        )
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "type definition revision controls")?;
        fn reserve(budget: &mut Budget, count: usize) -> Result<FixedVec<u64>, BudgetError> {
            let mut values = FixedVec::new(budget, "type definition revisions", count)?;
            for _ in 0..count { values.push(0).expect("reserved type revisions"); }
            Ok(values)
        }
        Ok(Self {
            domains: reserve(budget, config.max_domains)?,
            enums: reserve(budget, config.max_enums)?,
            composites: reserve(budget, config.max_composites)?,
        })
    }

    pub(super) fn get(&self, source: NamedTypeSlot) -> u64 {
        match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot],
            NamedTypeSlot::Enum(slot) => self.enums[slot],
            NamedTypeSlot::Composite(slot) => self.composites[slot],
        }
    }

    pub(super) fn set(&mut self, source: NamedTypeSlot, version: u64) {
        match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot] = version,
            NamedTypeSlot::Enum(slot) => self.enums[slot] = version,
            NamedTypeSlot::Composite(slot) => self.composites[slot] = version,
        }
    }

    pub(super) fn swap(&mut self, source: NamedTypeSlot, target: NamedTypeSlot) {
        match (source, target) {
            (NamedTypeSlot::Domain(old), NamedTypeSlot::Domain(new)) => self.domains.swap(old, new),
            (NamedTypeSlot::Enum(old), NamedTypeSlot::Enum(new)) => self.enums.swap(old, new),
            (NamedTypeSlot::Composite(old), NamedTypeSlot::Composite(new)) => self.composites.swap(old, new),
            _ => unreachable!("type relocation preserves its source kind"),
        }
    }
}

impl super::TypeCatalog {
    pub(super) fn type_definition_version(&self, source: NamedTypeSlot, transaction: u32) -> u64 {
        let pending = match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot].pending_definition
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
            NamedTypeSlot::Enum(slot) => self.enums[slot].pending_definition
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
            NamedTypeSlot::Composite(slot) => self.composites[slot].pending_definition
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
        };
        pending.unwrap_or_else(|| self.versions.get(source))
    }

    pub(super) fn contains_type_source(&self, source: TypeIdentity) -> bool {
        if self.origin != source.origin() { return false; }
        let matches = |database, created_at, state| {
            database == source.database() && created_at == source.created_at()
                && state != super::CatalogDdlState::Absent
        };
        match source.slot() {
            NamedTypeSlot::Domain(slot) => self.domains.get(slot)
                .is_some_and(|definition| matches(definition.database, definition.created_at, definition.ddl_state)),
            NamedTypeSlot::Enum(slot) => self.enums.get(slot)
                .is_some_and(|definition| matches(definition.database, definition.created_at, definition.ddl_state)),
            NamedTypeSlot::Composite(slot) => self.composites.get(slot)
                .is_some_and(|definition| matches(definition.database, definition.created_at, definition.ddl_state)),
        }
    }
}
