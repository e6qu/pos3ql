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
    pub(super) fn dependent_table_revision(self) -> super::TableDefinitionRevision {
        super::TableDefinitionRevision::named_type(self.source, self.version)
    }

    pub(super) fn identity_move(
        self,
        catalog: &super::TypeCatalog,
    ) -> Option<super::type_reference_moves::TypeReferenceMove> {
        if !catalog.matches_type_edit(self) {
            return None;
        }
        let (schema, name, next_schema, next_name) = match self.source.slot() {
            NamedTypeSlot::Domain(slot) => {
                let definition = &catalog.domains[slot];
                let pending = definition.pending_definition.as_ref()?.identity?;
                (
                    definition.schema,
                    definition.name,
                    pending.schema,
                    pending.name,
                )
            }
            NamedTypeSlot::Enum(slot) => {
                let definition = &catalog.enums[slot];
                let pending = definition.pending_definition.as_ref()?;
                (
                    definition.schema,
                    definition.name,
                    pending.schema,
                    pending.name,
                )
            }
            NamedTypeSlot::Composite(slot) => {
                let definition = &catalog.composites[slot];
                let pending = definition.pending_definition.as_ref()?;
                (
                    definition.schema,
                    definition.name,
                    pending.schema,
                    pending.name,
                )
            }
        };
        if (schema, name) == (next_schema, next_name) {
            return None;
        }
        Some(super::type_reference_moves::TypeReferenceMove::new(
            self.source,
            super::UserTypeName::declaration(schema, name),
            super::UserTypeName::declaration(next_schema, next_name),
        ))
    }

    pub(crate) fn matches_reference(self, name: super::UserTypeName) -> Result<bool, SqlError> {
        let reference = super::TypeReference::from_bound_name(name)?;
        Ok(reference.refers_to(self.source, self.source.database()))
    }

    pub(super) fn transaction(self) -> u32 {
        self.transaction
    }

    pub(super) fn slot(self, kind: super::NamedTypeKind) -> Result<usize, SqlError> {
        if self.source.slot().kind() != kind {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "type edit has the wrong source kind"
            ));
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
        Self {
            source,
            transaction,
            operation: TypeDdlOperation::Create,
            version: source.created_at(),
            prior_version: 0,
        }
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

#[derive(Clone, Copy)]
pub(crate) enum TypeCommitChange {
    Definition(TypeEditTarget),
    Lifecycle(TypeDdlUndo),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TypeAlterUndo<Definition> {
    pub(super) source: TypeIdentity,
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) prior: Option<Definition>,
}

impl<Definition> TypeAlterUndo<Definition> {
    pub(crate) fn slot(&self) -> usize {
        self.source.slot().index()
    }

    pub(crate) fn target(&self) -> TypeEditTarget {
        TypeEditTarget {
            source: self.source,
            transaction: self.transaction,
            version: self.version,
        }
    }
}

pub(crate) type DomainAlterUndo = TypeAlterUndo<PendingDomainDefinition>;
pub(crate) type EnumAlterUndo = TypeAlterUndo<PendingEnumDefinition>;
pub(crate) type CompositeAlterUndo = TypeAlterUndo<PendingCompositeDefinition>;

#[derive(Debug)]
pub(super) struct TypeRevisions {
    sequence: u64,
    domains: FixedVec<TypeRevision>,
    enums: FixedVec<TypeRevision>,
    composites: FixedVec<TypeRevision>,
}

#[derive(Debug, Clone, Copy)]
struct TypeRevision {
    definition: u64,
    ddl: u64,
}

impl TypeRevisions {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>().saturating_add(
            config
                .max_domains
                .saturating_add(config.max_enums)
                .saturating_add(config.max_composites)
                .saturating_mul(size_of::<TypeRevision>()),
        )
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "type revision controls")?;
        fn reserve(
            budget: &mut Budget,
            count: usize,
        ) -> Result<FixedVec<TypeRevision>, BudgetError> {
            let mut values = FixedVec::new(budget, "type revisions", count)?;
            for _ in 0..count {
                values
                    .push(TypeRevision {
                        definition: 0,
                        ddl: 0,
                    })
                    .expect("reserved type revisions");
            }
            Ok(values)
        }
        Ok(Self {
            sequence: 0,
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
        self.sequence = self.sequence.max(version);
        self.set(source, version);
        self.set_ddl(source, version);
    }

    pub(super) fn next(&mut self) -> Result<u64, SqlError> {
        let next = self.sequence.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "type change revisions exhausted"
            )
        })?;
        self.sequence = next;
        Ok(next)
    }

    #[cfg(test)]
    pub(super) fn replace_sequence_for_test(&mut self, sequence: u64) -> u64 {
        core::mem::replace(&mut self.sequence, sequence)
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
            (NamedTypeSlot::Composite(old), NamedTypeSlot::Composite(new)) => {
                self.composites.swap(old, new)
            }
            _ => unreachable!("type relocation preserves its source kind"),
        }
    }
}

impl super::TypeCatalog {
    pub(super) fn matches_type_edit(&self, target: TypeEditTarget) -> bool {
        if !self.contains_type_source(target.source)
            || target.source.database() != super::current_database()
        {
            return false;
        }
        let pending = match target.source.slot() {
            NamedTypeSlot::Domain(slot) => self.domains[slot]
                .pending_definition
                .as_ref()
                .map(|pending| (pending.txid, pending.version)),
            NamedTypeSlot::Enum(slot) => self.enums[slot]
                .pending_definition
                .as_ref()
                .map(|pending| (pending.txid, pending.version)),
            NamedTypeSlot::Composite(slot) => self.composites[slot]
                .pending_definition
                .as_ref()
                .map(|pending| (pending.txid, pending.version)),
        };
        pending == Some((target.transaction, target.version))
    }

    fn select_type_edit(
        &self,
        source: NamedTypeSlot,
        transaction: u32,
    ) -> Result<TypeEditTarget, SqlError> {
        let snapshot = self
            .type_snapshot(source, transaction)?
            .filter(|snapshot| snapshot.identity.database() == super::current_database())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type edit source no longer exists"
                )
            })?;
        Ok(TypeEditTarget {
            source: snapshot.identity,
            transaction,
            version: self.type_definition_version(source, transaction),
        })
    }

    pub(super) fn validate_type_edit_source(&self, target: TypeEditTarget) -> Result<(), SqlError> {
        if target.source.database() != super::current_database()
            || !self.contains_type_source(target.source)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type edit source has been replaced"
            ));
        }
        Ok(())
    }

    pub(super) fn validate_type_edit(
        &self,
        target: TypeEditTarget,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let current = self.select_type_edit(target.source.slot(), transaction)?;
        if current.source != target.source {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type edit source has been replaced"
            ));
        }
        if target.transaction != transaction || current.version != target.version {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "type definition changed after edit preparation"
            ));
        }
        Ok(())
    }

    pub(super) fn type_definition_version(&self, source: NamedTypeSlot, transaction: u32) -> u64 {
        let pending = match source {
            NamedTypeSlot::Domain(slot) => self.domains[slot]
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
                .map(|pending| pending.version),
            NamedTypeSlot::Enum(slot) => self.enums[slot]
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
                .map(|pending| pending.version),
            NamedTypeSlot::Composite(slot) => self.composites[slot]
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
                .map(|pending| pending.version),
        };
        pending.unwrap_or_else(|| self.versions.get(source))
    }

    pub(super) fn contains_type_source(&self, source: TypeIdentity) -> bool {
        let matches = |database, created_at, state| {
            state != super::CatalogDdlState::Absent
                && TypeIdentity::new(self.origin, database, source.slot(), created_at) == source
        };
        match source.slot() {
            NamedTypeSlot::Domain(slot) => self.domains.get(slot).is_some_and(|definition| {
                matches(
                    definition.database,
                    definition.created_at,
                    definition.ddl_state,
                )
            }),
            NamedTypeSlot::Enum(slot) => self.enums.get(slot).is_some_and(|definition| {
                matches(
                    definition.database,
                    definition.created_at,
                    definition.ddl_state,
                )
            }),
            NamedTypeSlot::Composite(slot) => self.composites.get(slot).is_some_and(|definition| {
                matches(
                    definition.database,
                    definition.created_at,
                    definition.ddl_state,
                )
            }),
        }
    }

    fn type_change_owners(&self, source: NamedTypeSlot) -> [Option<u32>; 2] {
        match source {
            NamedTypeSlot::Domain(slot) => [
                self.domains[slot].ddl_state.pending_txid(),
                self.domains[slot]
                    .pending_definition
                    .as_ref()
                    .map(|pending| pending.txid),
            ],
            NamedTypeSlot::Enum(slot) => [
                self.enums[slot].ddl_state.pending_txid(),
                self.enums[slot]
                    .pending_definition
                    .as_ref()
                    .map(|pending| pending.txid),
            ],
            NamedTypeSlot::Composite(slot) => [
                self.composites[slot].ddl_state.pending_txid(),
                self.composites[slot]
                    .pending_definition
                    .as_ref()
                    .map(|pending| pending.txid),
            ],
        }
    }

    pub(super) fn has_pending_type_change(&self, source: NamedTypeSlot) -> bool {
        self.type_change_owners(source)
            .into_iter()
            .any(|owner| owner.is_some())
    }

    pub(super) fn conflicting_type_owner(
        &self,
        source: NamedTypeSlot,
        transaction: u32,
    ) -> Option<u32> {
        self.type_change_owners(source)
            .into_iter()
            .flatten()
            .find(|&owner| owner != transaction)
    }
}

impl super::Storage {
    pub(crate) fn domain_reference_slot(
        &self,
        name: super::UserTypeName,
        transaction: u32,
    ) -> Result<Option<usize>, SqlError> {
        let reference = super::TypeReference::from_bound_name(name)?;
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let source = catalog
            .resolve_source_reference(reference, super::current_database(), transaction)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "domain dependency source no longer exists"
                )
            })?;
        Ok(match source.identity.slot() {
            NamedTypeSlot::Domain(slot) => Some(slot),
            NamedTypeSlot::Enum(_) | NamedTypeSlot::Composite(_) => None,
        })
    }

    pub(crate) fn domain_depends_on(
        &self,
        mut slot: usize,
        target: usize,
        transaction: u32,
    ) -> Result<bool, SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let selected = catalog.select_type_edit(NamedTypeSlot::Domain(target), transaction)?;
        for _ in 0..catalog.domains.len() {
            let current = catalog.select_type_edit(NamedTypeSlot::Domain(slot), transaction)?;
            if current.source == selected.source {
                return Ok(true);
            }
            let definition = &catalog.domains[slot];
            let parent = definition
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
                .map_or(definition.base_domain, |pending| pending.spec.base_domain);
            let Some(parent) = parent else {
                return Ok(false);
            };
            let parent = catalog
                .resolve_source_reference(
                    super::TypeReference::from_bound_name(parent)?,
                    super::current_database(),
                    transaction,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "domain parent source no longer exists"
                    )
                })?;
            let NamedTypeSlot::Domain(parent) = parent.identity.slot() else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "domain parent source is not a domain"
                ));
            };
            slot = parent;
        }
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "domain dependency cycle detected"
        ))
    }

    pub(crate) fn validate_type_commit(
        &self,
        transaction: u32,
        changes: impl Iterator<Item = TypeCommitChange> + Clone,
    ) -> Result<(), SqlError> {
        let mut changes = changes.peekable();
        if changes.peek().is_none() {
            return Ok(());
        }
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let mut has_identity_moves = false;
        for change in changes.clone() {
            let (source, owner) = match change {
                TypeCommitChange::Definition(target) => (target.source, target.transaction),
                TypeCommitChange::Lifecycle(undo) => (undo.source, undo.transaction),
            };
            if source.database() != super::current_database()
                || !catalog.contains_type_source(source)
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "transaction type source has been replaced"
                ));
            }
            if owner != transaction
                || catalog
                    .conflicting_type_owner(source.slot(), transaction)
                    .is_some()
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "type change no longer belongs to its transaction"
                ));
            }
            let valid = match change {
                TypeCommitChange::Definition(target) => {
                    has_identity_moves |= target.identity_move(&catalog).is_some();
                    let current = catalog.type_definition_version(source.slot(), transaction);
                    let pending = match source.slot() {
                        NamedTypeSlot::Domain(slot) => catalog.domains[slot]
                            .pending_definition
                            .as_ref()
                            .map(|pending| pending.txid),
                        NamedTypeSlot::Enum(slot) => catalog.enums[slot]
                            .pending_definition
                            .as_ref()
                            .map(|pending| pending.txid),
                        NamedTypeSlot::Composite(slot) => catalog.composites[slot]
                            .pending_definition
                            .as_ref()
                            .map(|pending| pending.txid),
                    };
                    // Earlier edits are superseded by this transaction's final edit.
                    pending == Some(transaction) && current >= target.version
                }
                TypeCommitChange::Lifecycle(undo) => {
                    let state = match source.slot() {
                        NamedTypeSlot::Domain(slot) => catalog.domains[slot].ddl_state,
                        NamedTypeSlot::Enum(slot) => catalog.enums[slot].ddl_state,
                        NamedTypeSlot::Composite(slot) => catalog.composites[slot].ddl_state,
                    };
                    use super::CatalogDdlState as State;
                    state.pending_txid() == Some(transaction)
                        && match undo.operation {
                            TypeDdlOperation::Create => matches!(
                                state,
                                State::PendingCreate { .. } | State::PendingCreateDrop { .. }
                            ),
                            TypeDdlOperation::Drop => {
                                matches!(
                                    state,
                                    State::PendingDrop { .. } | State::PendingCreateDrop { .. }
                                ) && catalog.versions.ddl(source.slot()) == undo.version
                            }
                        }
                }
            };
            if !valid {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "type change no longer belongs to its transaction"
                ));
            }
        }
        drop(catalog);
        if has_identity_moves {
            super::type_reference_moves::DefinitionPublication::validate_generations(
                self, changes,
            )?;
        }
        Ok(())
    }

    pub(crate) fn finish_type_ddl(&self, undo: TypeDdlUndo, decision: TypeDdlDecision) {
        use super::CatalogDdlState as State;
        use TypeDdlDecision::{Commit, Rollback};
        use TypeDdlOperation::{Create, Drop};

        let (mut catalog, next, schema, name, mut comments, mut images) = loop {
            let catalog = self
                .type_catalog
                .lock()
                .expect("type catalog lock poisoned");
            if !catalog.contains_type_source(undo.source)
                || undo.source.database() != super::current_database()
            {
                return;
            }
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
            if state.pending_txid() != Some(undo.transaction) {
                return;
            }
            if catalog.versions.ddl(undo.source.slot()) != undo.version
                && !matches!(
                    (undo.operation, decision, state),
                    (Create, Commit, State::PendingCreateDrop { .. })
                )
            {
                return;
            }
            let next = match (undo.operation, decision, state) {
                (Create, Commit, State::PendingCreate { .. } | State::PendingCreateDrop { .. }) => {
                    state.commit_create()
                }
                (Create, Rollback, State::PendingCreate { .. }) => state.rollback_create(),
                (Drop, Commit, State::PendingDrop { .. }) => state.commit_drop(),
                (Drop, Rollback, State::PendingDrop { .. } | State::PendingCreateDrop { .. }) => {
                    state.rollback_drop(undo.transaction)
                }
                _ => return,
            };
            let attempt: Result<_, super::row_ownership::LockWait<'_>> = (|| {
                let images = if matches!(undo.source.slot(), NamedTypeSlot::Enum(_))
                    && next == State::Absent
                {
                    Some(super::row_ownership::try_mutex(
                        &self.enum_member_images,
                        "enum member-image lock poisoned",
                    )?)
                } else {
                    None
                };
                let comments = if undo.operation == Drop && decision == Commit {
                    Some(super::row_ownership::try_mutex(
                        &self.comments,
                        "comment catalog lock poisoned",
                    )?)
                } else {
                    None
                };
                Ok((comments, images))
            })();
            match attempt {
                Ok((comments, images)) => break (catalog, next, schema, name, comments, images),
                Err(wait) => {
                    drop(catalog);
                    #[cfg(test)]
                    self.definition_publication
                        .waits
                        .fetch_add(1, core::sync::atomic::Ordering::Release);
                    wait.wait();
                }
            }
        };
        if undo.operation == Drop && decision == Rollback {
            catalog
                .versions
                .set_ddl(undo.source.slot(), undo.prior_version);
        }
        // All destination owners are present before the first mutation. A
        // blocked destination releases the source and revalidates on retry.
        if let Some(comments) = &mut comments {
            for slot in 0..comments.entries.len() {
                let comment = &comments.entries[slot];
                if comment.used
                    && comment.database == Some(undo.source.database())
                    && comment.class == super::CommentClass::Type
                    && comment.pending.is_none()
                    && comment.schema == schema
                    && comment.name == name
                {
                    comments.entries[slot].live = None;
                    comments.reap(slot);
                }
            }
        }
        if let Some(images) = &mut images {
            Self::release_enum_definition_images_locked(&mut catalog, images, undo.slot());
        }
        let ownership = match undo.source.slot() {
            NamedTypeSlot::Domain(slot) => {
                catalog.domains[slot].ddl_state = next;
                if next == State::Absent {
                    catalog.domains[slot].pending_definition = None;
                }
                &mut catalog.domains[slot].ownership
            }
            NamedTypeSlot::Enum(slot) => {
                catalog.enums[slot].ddl_state = next;
                &mut catalog.enums[slot].ownership
            }
            NamedTypeSlot::Composite(slot) => {
                catalog.composites[slot].ddl_state = next;
                if next == State::Absent {
                    catalog.composites[slot].pending_definition = None;
                }
                &mut catalog.composites[slot].ownership
            }
        };
        if undo.operation == Create
            && decision == Commit
            && let Some(pending) = ownership
                .pending
                .filter(|pending| pending.txid == undo.transaction)
        {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn prepare_domain_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<(super::DomainDef, TypeEditTarget), SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let target = catalog.select_type_edit(NamedTypeSlot::Domain(slot), transaction)?;
        Ok((catalog.domains[slot].definition_for(transaction), target))
    }

    pub(crate) fn prepare_enum_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<(super::EnumDefinition, TypeEditTarget), SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let target = catalog.select_type_edit(NamedTypeSlot::Enum(slot), transaction)?;
        let definition = catalog.enums[slot].definition_for(transaction);
        let image =
            super::EnumImageRead::capture(&self.enum_member_images, definition.member_image);
        Ok((
            super::EnumDefinition {
                image,
                member_count: definition.member_count as usize,
                safe_member_count: catalog.enums[slot].safe_member_count_for(transaction),
                database: definition.database,
                created_at: definition.created_at,
                schema: definition.schema,
                name: definition.name,
                ownership: definition.ownership,
                ddl_state: definition.ddl_state,
            },
            target,
        ))
    }

    pub(crate) fn prepare_composite_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<(super::CompositeDef, TypeEditTarget), SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
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
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let mut selected = None;
        let mut blocked = None;
        let matches = |candidate: super::UserTypeName| {
            candidate.schema.as_str() == schema && candidate.name.as_str() == name
        };
        for reservation in catalog.name_reservations() {
            if reservation.slot.kind() != kind
                || reservation.database != super::current_database()
                || reservation.state == super::CatalogDdlState::Absent
            {
                continue;
            }
            if (matches(reservation.current)
                || reservation
                    .pending
                    .is_some_and(|(_, pending)| matches(pending)))
                && let Some(owner) = catalog.conflicting_type_owner(reservation.slot, transaction)
            {
                blocked = Some(owner);
                break;
            }
            let visible = reservation
                .pending
                .filter(|&(owner, _)| owner == transaction)
                .map_or(reservation.current, |(_, pending)| pending);
            if reservation.state.visible_to(transaction) && matches(visible) {
                selected = Some((reservation.slot, reservation.state));
            }
        }
        if let Some(owner) = blocked {
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name));
        }
        let Some((source, state)) = selected else {
            return Ok(None);
        };
        let identity = catalog
            .type_snapshot(source, transaction)?
            .expect("a visible type selected for deletion has an identity")
            .identity;
        let version = catalog.versions.next()?;
        let prior_version = catalog.versions.ddl(source);
        catalog.versions.set_ddl(source, version);
        let state = state.drop_by(transaction);
        match source {
            NamedTypeSlot::Domain(slot) => catalog.domains[slot].ddl_state = state,
            NamedTypeSlot::Enum(slot) => catalog.enums[slot].ddl_state = state,
            NamedTypeSlot::Composite(slot) => catalog.composites[slot].ddl_state = state,
        }
        Ok(Some(TypeDdlUndo {
            source: identity,
            transaction,
            operation: TypeDdlOperation::Drop,
            version,
            prior_version,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CatalogDdlState, NamedTypeKind, SqlName, Storage};

    fn fixture() -> Storage {
        let config = crate::storage::tests::test_config();
        let mut budget = crate::storage::tests::test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    fn create(storage: &mut Storage, kind: NamedTypeKind) -> TypeDdlUndo {
        use crate::storage::{
            CheckConstraint, ColType, CompositeFieldDef, CompositeSpec, DomainSpec, EnumMember,
            EnumSpec, MAX_COMPOSITE_FIELDS, MAX_DOMAIN_CHECKS,
        };
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("owned_type").unwrap();
        match kind {
            NamedTypeKind::Domain => storage
                .create_domain(
                    schema,
                    name,
                    DomainSpec {
                        base_domain: None,
                        base_user_type: None,
                        base: ColType::Int4,
                        base_type_mod: -1,
                        not_null: false,
                        not_null_name: None,
                        not_null_identity: None,
                        default_expr: None,
                        checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                        n_checks: 0,
                    },
                    7,
                )
                .unwrap(),
            NamedTypeKind::Enum => storage
                .create_enum(
                    schema,
                    name,
                    EnumSpec {
                        members: &[EnumMember {
                            identity: None,
                            label: SqlName::parse("ready").unwrap(),
                            legacy_label: None,
                            sort: 1.0,
                        }],
                    },
                    7,
                )
                .unwrap(),
            NamedTypeKind::Composite => storage
                .create_composite(
                    schema,
                    name,
                    CompositeSpec {
                        fields: [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS],
                        n_fields: 0,
                    },
                    7,
                )
                .unwrap(),
        }
    }

    fn state(storage: &Storage, source: TypeDdlUndo) -> CatalogDdlState {
        let catalog = storage.type_catalog.lock().unwrap();
        match source.source.slot() {
            NamedTypeSlot::Domain(slot) => catalog.domains[slot].ddl_state,
            NamedTypeSlot::Enum(slot) => catalog.enums[slot].ddl_state,
            NamedTypeSlot::Composite(slot) => catalog.composites[slot].ddl_state,
        }
    }

    #[test]
    fn type_retirement_wait_releases_catalog_and_revalidates_rollback() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        for (kind, block_images) in [
            (NamedTypeKind::Domain, false),
            (NamedTypeKind::Enum, false),
            (NamedTypeKind::Composite, false),
            (NamedTypeKind::Enum, true),
        ] {
            for rollback in [false, true] {
                let mut storage = fixture();
                let created = create(&mut storage, kind);
                storage.finish_type_ddl(created, TypeDdlDecision::Commit);
                let retained =
                    (kind == NamedTypeKind::Enum).then(|| storage.enum_for(created.slot(), 0));
                storage
                    .apply_comment(
                        crate::storage::CommentClass::Type,
                        SqlName::parse("public").unwrap(),
                        SqlName::parse("owned_type").unwrap(),
                        0,
                        Some(crate::storage::comment_stackstr("retained type comment").unwrap()),
                    )
                    .unwrap();
                let dropped = storage
                    .stage_type_drop(kind, "public", "owned_type", 7)
                    .unwrap()
                    .unwrap();
                let before = storage.definition_publication.waits.load(Ordering::Acquire);
                let comments = (!block_images).then(|| storage.comments.lock().unwrap());
                let images = block_images.then(|| storage.enum_member_images.lock().unwrap());
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::mem::guard::forbid_alloc(|| {
                            storage.finish_type_ddl(dropped, TypeDdlDecision::Commit)
                        })
                    });
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while storage.definition_publication.waits.load(Ordering::Acquire) == before
                        && Instant::now() < deadline
                    {
                        std::thread::yield_now();
                    }
                    let waiting =
                        storage.definition_publication.waits.load(Ordering::Acquire) != before;
                    let released = waiting
                        && storage.type_catalog.try_lock().is_ok()
                        && (block_images || storage.enum_member_images.try_lock().is_ok());
                    if released {
                        crate::mem::guard::forbid_alloc(|| {
                            let other = storage
                                .create_composite(
                                    SqlName::parse("public").unwrap(),
                                    SqlName::parse("unrelated_retirement_type").unwrap(),
                                    crate::storage::CompositeSpec {
                                        fields: [crate::storage::CompositeFieldDef::EMPTY;
                                            crate::storage::MAX_COMPOSITE_FIELDS],
                                        n_fields: 0,
                                    },
                                    9,
                                )
                                .unwrap();
                            storage.finish_type_ddl(other, TypeDdlDecision::Commit);
                            if rollback {
                                storage.finish_type_ddl(dropped, TypeDdlDecision::Rollback);
                            }
                        });
                    }
                    drop((comments, images));
                    writer.join().unwrap();
                    assert!(waiting && released);
                });
                crate::mem::guard::forbid_alloc(|| {
                    assert_eq!(
                        state(&storage, dropped),
                        if rollback {
                            CatalogDdlState::Present
                        } else {
                            CatalogDdlState::Absent
                        }
                    );
                    assert_eq!(
                        storage
                            .comment_text(
                                crate::storage::CommentClass::Type,
                                "public",
                                "owned_type",
                                0,
                                0
                            )
                            .is_some(),
                        rollback
                    );
                    if let Some(retained) = &retained {
                        assert_eq!(retained.members().next().unwrap().label.as_str(), "ready");
                    }
                });
            }
        }
    }

    #[test]
    fn type_ddl_finalization_rejects_foreign_replaced_repeated_and_wrong_owner_records() {
        use CatalogDdlState::{Absent, PendingCreate, PendingDrop, Present};
        use TypeDdlDecision::{Commit, Rollback};
        for kind in [
            NamedTypeKind::Domain,
            NamedTypeKind::Enum,
            NamedTypeKind::Composite,
        ] {
            let mut storage = fixture();
            let mut foreign = fixture();
            crate::mem::guard::forbid_alloc(|| {
                let created = create(&mut storage, kind);
                let foreign_created = create(&mut foreign, kind);
                for decision in [Commit, Rollback] {
                    foreign.finish_type_ddl(created, decision);
                    storage.finish_type_ddl(
                        TypeDdlUndo {
                            transaction: 9,
                            ..created
                        },
                        decision,
                    );
                    assert_eq!(state(&foreign, foreign_created), PendingCreate { txid: 7 });
                    assert_eq!(state(&storage, created), PendingCreate { txid: 7 });
                }
                assert_eq!(
                    foreign
                        .validate_type_commit(7, [TypeCommitChange::Lifecycle(created)].into_iter())
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::UNDEFINED_OBJECT
                );
                storage.finish_type_ddl(created, Commit);
                storage.finish_type_ddl(created, Rollback);
                assert_eq!(state(&storage, created), Present);
                let earlier = storage
                    .stage_type_drop(kind, "public", "owned_type", 7)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(earlier, Rollback);
                let latest = storage
                    .stage_type_drop(kind, "public", "owned_type", 7)
                    .unwrap()
                    .unwrap();
                for decision in [Commit, Rollback] {
                    storage.finish_type_ddl(earlier, decision);
                    assert_eq!(state(&storage, latest), PendingDrop { txid: 7 });
                }
                assert_eq!(
                    storage
                        .validate_type_commit(7, [TypeCommitChange::Lifecycle(earlier)].into_iter())
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                storage
                    .validate_type_commit(7, [TypeCommitChange::Lifecycle(latest)].into_iter())
                    .unwrap();
                storage.finish_type_ddl(latest, Commit);
                assert_eq!(state(&storage, latest), Absent);
                let replacement = create(&mut storage, kind);
                assert_eq!(replacement.slot(), created.slot());
                storage.finish_type_ddl(replacement, Commit);
                storage
                    .apply_comment(
                        crate::storage::CommentClass::Type,
                        SqlName::parse("public").unwrap(),
                        SqlName::parse("owned_type").unwrap(),
                        0,
                        Some(crate::storage::comment_stackstr("replacement comment").unwrap()),
                    )
                    .unwrap();
                for obsolete in [created, earlier, latest] {
                    for decision in [Commit, Rollback] {
                        storage.finish_type_ddl(obsolete, decision);
                    }
                }
                assert_eq!(state(&storage, replacement), Present);
                assert_eq!(
                    storage
                        .comment_text(
                            crate::storage::CommentClass::Type,
                            "public",
                            "owned_type",
                            0,
                            0
                        )
                        .unwrap()
                        .as_str(),
                    "replacement comment"
                );
                if kind == NamedTypeKind::Enum {
                    assert_eq!(
                        storage
                            .enum_for(replacement.slot(), 0)
                            .members()
                            .next()
                            .unwrap()
                            .label
                            .as_str(),
                        "ready"
                    );
                }
            });
        }
    }

    #[test]
    fn type_create_then_drop_commits_or_rolls_back_in_record_order() {
        use TypeDdlDecision::{Commit, Rollback};
        for kind in [
            NamedTypeKind::Domain,
            NamedTypeKind::Enum,
            NamedTypeKind::Composite,
        ] {
            let mut storage = fixture();
            crate::mem::guard::forbid_alloc(|| {
                let created = create(&mut storage, kind);
                let dropped = storage
                    .stage_type_drop(kind, "public", "owned_type", 7)
                    .unwrap()
                    .unwrap();
                storage
                    .validate_type_commit(
                        7,
                        [
                            TypeCommitChange::Lifecycle(created),
                            TypeCommitChange::Lifecycle(dropped),
                        ]
                        .into_iter(),
                    )
                    .unwrap();
                storage.finish_type_ddl(created, Commit);
                storage.finish_type_ddl(dropped, Commit);
                assert_eq!(state(&storage, dropped), CatalogDdlState::Absent);
                let created = create(&mut storage, kind);
                let dropped = storage
                    .stage_type_drop(kind, "public", "owned_type", 7)
                    .unwrap()
                    .unwrap();
                storage.finish_type_ddl(dropped, Rollback);
                storage.finish_type_ddl(created, Rollback);
                assert_eq!(state(&storage, created), CatalogDdlState::Absent);
                // A new enum needs the image released by both paths above.
                create(&mut storage, kind);
            });
        }
    }
}
