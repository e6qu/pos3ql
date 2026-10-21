//! Definition revisions and undo records identify one source and one edit.

use core::mem::size_of;

use super::type_names::NamedTypeSlot;
use super::type_references::TypeIdentity;
use super::{PendingCompositeDefinition, PendingDomainDefinition, PendingEnumDefinition};
use crate::config::Config;
use crate::mem::FixedVec;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TypeEditTarget {
    source: TypeIdentity,
    transaction: u32,
    version: u64,
}

impl TypeEditTarget {
    pub(super) fn slot(self, kind: super::NamedTypeKind) -> Result<usize, SqlError> {
        if self.source.slot().kind() != kind {
            return Err(sql_err!(sqlstate::WRONG_OBJECT_TYPE, "type edit has the wrong source kind"));
        }
        Ok(self.source.slot().index())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeDdlOperation {
    Create,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypeDdlUndo {
    source: TypeIdentity,
    transaction: u32,
    operation: TypeDdlOperation,
    version: u64,
    prior_version: u64,
}

impl TypeDdlUndo {
    pub(super) fn created(source: TypeIdentity, transaction: u32) -> Self {
        Self { source, transaction, operation: TypeDdlOperation::Create,
            version: source.created_at(), prior_version: 0 }
    }

    pub(crate) fn slot(self) -> usize {
        self.source.slot().index()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeDdlDecision {
    Commit,
    Rollback,
}

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
    domains: FixedVec<TypeRevision>,
    enums: FixedVec<TypeRevision>,
    composites: FixedVec<TypeRevision>,
}

#[derive(Debug, Clone, Copy)]
struct TypeRevision {
    definition: u64,
    ddl: u64,
}

impl TypeDefinitionVersions {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>().saturating_add(
            config.max_domains.saturating_add(config.max_enums)
                .saturating_add(config.max_composites).saturating_mul(size_of::<TypeRevision>()),
        )
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "type definition revision controls")?;
        fn reserve(budget: &mut Budget, count: usize) -> Result<FixedVec<TypeRevision>, BudgetError> {
            let mut values = FixedVec::new(budget, "type definition revisions", count)?;
            for _ in 0..count { values.push(TypeRevision { definition: 0, ddl: 0 }).expect("reserved type revisions"); }
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
            NamedTypeSlot::Domain(slot) => self.domains[slot].definition,
            NamedTypeSlot::Enum(slot) => self.enums[slot].definition,
            NamedTypeSlot::Composite(slot) => self.composites[slot].definition,
        }
    }

    pub(super) fn set(&mut self, source: NamedTypeSlot, version: u64) {
        match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot].definition = version,
            NamedTypeSlot::Enum(slot) => self.enums[slot].definition = version,
            NamedTypeSlot::Composite(slot) => self.composites[slot].definition = version,
        }
    }

    pub(super) fn initialize(&mut self, source: NamedTypeSlot, version: u64) {
        self.set(source, version);
        self.set_ddl(source, version);
    }

    fn ddl(&self, source: NamedTypeSlot) -> u64 {
        match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot].ddl,
            NamedTypeSlot::Enum(slot) => self.enums[slot].ddl,
            NamedTypeSlot::Composite(slot) => self.composites[slot].ddl,
        }
    }

    fn set_ddl(&mut self, source: NamedTypeSlot, version: u64) {
        match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot].ddl = version,
            NamedTypeSlot::Enum(slot) => self.enums[slot].ddl = version,
            NamedTypeSlot::Composite(slot) => self.composites[slot].ddl = version,
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
    fn select_type_edit(&self, source: NamedTypeSlot, transaction: u32) -> Result<TypeEditTarget, SqlError> {
        let snapshot = self.type_snapshot(source, transaction)?
            .filter(|snapshot| snapshot.identity.database() == super::current_database())
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "type edit source no longer exists"))?;
        Ok(TypeEditTarget {
            source: snapshot.identity,
            transaction,
            version: self.type_definition_version(source, transaction),
        })
    }

    pub(super) fn validate_type_edit_source(&self, target: TypeEditTarget) -> Result<(), SqlError> {
        if target.source.database() != super::current_database() || !self.contains_type_source(target.source) {
            return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "type edit source has been replaced"));
        }
        Ok(())
    }

    pub(super) fn validate_type_edit(&self, target: TypeEditTarget, transaction: u32) -> Result<(), SqlError> {
        self.validate_type_edit_source(target)?;
        let current = self.select_type_edit(target.source.slot(), transaction)?;
        if current.source != target.source {
            return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "type edit source has been replaced"));
        }
        if target.transaction != transaction || current.version != target.version {
            return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE, "type definition changed after edit preparation"));
        }
        Ok(())
    }

    pub(super) fn type_definition_version(&self, source: NamedTypeSlot, transaction: u32) -> u64 {
        let pending = match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot].pending_definition.as_ref()
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
            NamedTypeSlot::Enum(slot) => self.enums[slot].pending_definition.as_ref()
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
            NamedTypeSlot::Composite(slot) => self.composites[slot].pending_definition.as_ref()
                .filter(|pending| pending.txid == transaction).map(|pending| pending.version),
        };
        pending.unwrap_or_else(|| self.versions.get(source))
    }

    pub(super) fn contains_type_source(&self, source: TypeIdentity) -> bool {
        let matches = |database, created_at, state| {
            state != super::CatalogDdlState::Absent
                && TypeIdentity::new(self.origin, database, source.slot(), created_at) == source
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

    pub(super) fn conflicting_type_owner(&self, source: NamedTypeSlot, transaction: u32) -> Option<u32> {
        let owners = match source {
            NamedTypeSlot::Domain(slot) => [
                self.domains[slot].ddl_state.pending_txid(),
                self.domains[slot].pending_definition.as_ref().map(|pending| pending.txid),
            ],
            NamedTypeSlot::Enum(slot) => [
                self.enums[slot].ddl_state.pending_txid(),
                self.enums[slot].pending_definition.as_ref().map(|pending| pending.txid),
            ],
            NamedTypeSlot::Composite(slot) => [
                self.composites[slot].ddl_state.pending_txid(),
                self.composites[slot].pending_definition.as_ref().map(|pending| pending.txid),
            ],
        };
        owners.into_iter().flatten().find(|&owner| owner != transaction)
    }
}

impl super::Storage {
    pub(crate) fn finish_type_ddl(&self, undo: TypeDdlUndo, decision: TypeDdlDecision) {
        use super::CatalogDdlState as State;
        use TypeDdlDecision::{Commit, Rollback};
        use TypeDdlOperation::{Create, Drop};

        let mut catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        if !catalog.contains_type_source(undo.source)
            || undo.source.database() != super::current_database()
        { return; }
        let (state, schema, name) = match undo.source.slot() {
            NamedTypeSlot::Domain(slot) => {
                let definition = &catalog.domains[slot];
                (definition.ddl_state, definition.schema, definition.name)
            }
            NamedTypeSlot::Enum(slot) => {
                let definition = &catalog.enums[slot];
                (definition.ddl_state, definition.schema, definition.name)
            }
            NamedTypeSlot::Composite(slot) => {
                let definition = &catalog.composites[slot];
                (definition.ddl_state, definition.schema, definition.name)
            }
        };
        if state.pending_txid() != Some(undo.transaction) { return; }
        if catalog.versions.ddl(undo.source.slot()) != undo.version
            && !matches!((undo.operation, decision, state), (Create, Commit, State::PendingCreateDrop { .. }))
        { return; }
        let next = match (undo.operation, decision, state) {
            (Create, Commit, State::PendingCreate { .. } | State::PendingCreateDrop { .. }) => state.commit_create(),
            (Create, Rollback, State::PendingCreate { .. }) => state.rollback_create(),
            (Drop, Commit, State::PendingDrop { .. }) => state.commit_drop(),
            (Drop, Rollback, State::PendingDrop { .. } | State::PendingCreateDrop { .. }) => state.rollback_drop(undo.transaction),
            _ => return,
        };
        if undo.operation == Drop && decision == Rollback {
            catalog.versions.set_ddl(undo.source.slot(), undo.prior_version);
        }
        // Keep the source owner until metadata removal and enum image release finish.
        if undo.operation == Drop && decision == Commit {
            self.drop_object_comments(super::CommentClass::Type, schema.as_str(), name.as_str());
        }
        if matches!(undo.source.slot(), NamedTypeSlot::Enum(_)) && next == State::Absent {
            self.release_enum_definition_images(&mut catalog, undo.slot());
        }
        let ownership = match undo.source.slot() {
            NamedTypeSlot::Domain(slot) => {
                catalog.domains[slot].ddl_state = next;
                if next == State::Absent { catalog.domains[slot].pending_definition = None; }
                &mut catalog.domains[slot].ownership
            }
            NamedTypeSlot::Enum(slot) => {
                catalog.enums[slot].ddl_state = next;
                &mut catalog.enums[slot].ownership
            }
            NamedTypeSlot::Composite(slot) => {
                catalog.composites[slot].ddl_state = next;
                if next == State::Absent { catalog.composites[slot].pending_definition = None; }
                &mut catalog.composites[slot].ownership
            }
        };
        if undo.operation == Create && decision == Commit
            && let Some(pending) = ownership.pending.filter(|pending| pending.txid == undo.transaction)
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn prepare_domain_edit(&self, slot: usize, transaction: u32) -> Result<(super::DomainDef, TypeEditTarget), SqlError> {
        let catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        let target = catalog.select_type_edit(NamedTypeSlot::Domain(slot), transaction)?;
        Ok((catalog.domains[slot].definition_for(transaction), target))
    }

    pub(crate) fn prepare_enum_edit(&self, slot: usize, transaction: u32) -> Result<(super::EnumDefinition, TypeEditTarget), SqlError> {
        let catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        let target = catalog.select_type_edit(NamedTypeSlot::Enum(slot), transaction)?;
        let definition = catalog.enums[slot].definition_for(transaction);
        let image = super::EnumImageRead::capture(&self.enum_member_images, definition.member_image);
        Ok((super::EnumDefinition {
            image,
            member_count: definition.member_count as usize,
            safe_member_count: catalog.enums[slot].safe_member_count_for(transaction),
            database: definition.database,
            created_at: definition.created_at,
            schema: definition.schema,
            name: definition.name,
            ownership: definition.ownership,
            ddl_state: definition.ddl_state,
        }, target))
    }

    pub(crate) fn prepare_composite_edit(&self, slot: usize, transaction: u32) -> Result<(super::CompositeDef, TypeEditTarget), SqlError> {
        let catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        let target = catalog.select_type_edit(NamedTypeSlot::Composite(slot), transaction)?;
        Ok((catalog.composites[slot].definition_for(transaction), target))
    }

    pub(crate) fn stage_type_drop(
        &self,
        kind: super::NamedTypeKind,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<Option<TypeDdlUndo>, crate::sql::eval::SqlError> {
        let mut catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        let mut selected = None;
        let mut blocked = None;
        let matches = |candidate: super::UserTypeName| {
            candidate.schema.as_str() == schema && candidate.name.as_str() == name
        };
        for reservation in catalog.name_reservations() {
            if reservation.slot.kind() != kind
                || reservation.database != super::current_database()
                || reservation.state == super::CatalogDdlState::Absent
            { continue; }
            if (matches(reservation.current)
                || reservation.pending.is_some_and(|(_, pending)| matches(pending)))
                && let Some(owner) = catalog.conflicting_type_owner(reservation.slot, transaction)
            {
                blocked = Some(owner);
                break;
            }
            let visible = reservation.pending.filter(|&(owner, _)| owner == transaction)
                .map_or(reservation.current, |(_, pending)| pending);
            if reservation.state.visible_to(transaction) && matches(visible) {
                selected = Some((reservation.slot, reservation.state));
            }
        }
        if let Some(owner) = blocked {
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name));
        }
        let Some((source, state)) = selected else { return Ok(None); };
        let identity = catalog.type_snapshot(source, transaction)?
            .expect("a visible type selected for deletion has an identity").identity;
        let version = self.catalog_sequence.next()?;
        let prior_version = catalog.versions.ddl(source);
        catalog.versions.set_ddl(source, version);
        let state = state.drop_by(transaction);
        match source {
            NamedTypeSlot::Domain(slot) => catalog.domains[slot].ddl_state = state,
            NamedTypeSlot::Enum(slot) => catalog.enums[slot].ddl_state = state,
            NamedTypeSlot::Composite(slot) => catalog.composites[slot].ddl_state = state,
        }
        Ok(Some(TypeDdlUndo { source: identity, transaction, operation: TypeDdlOperation::Drop, version, prior_version }))
    }
}
