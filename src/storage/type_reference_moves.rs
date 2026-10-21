//! A type move updates only references belonging to its owning database.

use core::mem::{align_of, size_of};
use {crate::mem::mutex::Mutex, crate::mem::mutex::MutexGuard};

#[cfg(test)]
use super::DatabaseOid;
use super::row_ownership::{LockWait, try_mutex};
use super::table_definitions::TableIdentity;
use super::table_names::DefinitionWrite;
use super::type_names::NamedTypeSlot;
use super::type_references::{TypeBinding, TypeIdentity};
use super::{
    ArrElem, CatalogDdlState, ColType, CommentClass, DependencyClass, EnumMemberImages,
    OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition, RoutineResult, Storage,
    TableDef, TablePublication, TypeCatalog, TypeEditTarget, UserTypeName,
};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};

#[derive(Clone, Copy)]
pub(super) enum TypePublicationSource {
    Pending(TypeEditTarget),
    Committed {
        identity: TypeIdentity,
        revision: u64,
    },
}

impl TypePublicationSource {
    pub(super) fn committed(
        catalog: &TypeCatalog,
        source: NamedTypeSlot,
    ) -> Result<Self, SqlError> {
        let snapshot = catalog
            .type_snapshot(source, 0)?
            .filter(|snapshot| snapshot.identity.database() == super::current_database())
            .ok_or_else(|| {
                crate::sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type publication source no longer exists"
                )
            })?;
        if catalog.has_pending_type_change(source) {
            return Err(crate::sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "type publication requires a committed source"
            ));
        }
        Ok(Self::Committed {
            identity: snapshot.identity,
            revision: catalog.versions.get(source),
        })
    }

    fn validate(
        self,
        catalog: &TypeCatalog,
        movement: TypeReferenceMove,
    ) -> Result<bool, SqlError> {
        match self {
            Self::Pending(target) => Ok(target.identity_move(catalog).is_some_and(|current| {
                current.identity == movement.identity
                    && current.prior.schema == movement.prior.schema
                    && current.prior.name == movement.prior.name
                    && current.current.schema == movement.current.schema
                    && current.current.name == movement.current.name
            })),
            Self::Committed { identity, revision } => {
                if identity.database() != super::current_database()
                    || !catalog.contains_type_source(identity)
                {
                    return Err(crate::sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type publication source has been replaced"
                    ));
                }
                if catalog.versions.get(identity.slot()) != revision
                    || catalog.has_pending_type_change(identity.slot())
                {
                    return Err(crate::sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "type publication source changed after preparation"
                    ));
                }
                let (schema, name) = match identity.slot() {
                    NamedTypeSlot::Domain(slot) => {
                        (catalog.domains[slot].schema, catalog.domains[slot].name)
                    }
                    NamedTypeSlot::Enum(slot) => {
                        (catalog.enums[slot].schema, catalog.enums[slot].name)
                    }
                    NamedTypeSlot::Composite(slot) => (
                        catalog.composites[slot].schema,
                        catalog.composites[slot].name,
                    ),
                };
                if identity != movement.identity
                    || schema != movement.prior.schema
                    || name != movement.prior.name
                {
                    return Err(crate::sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "type identity changed before publication"
                    ));
                }
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum TypeReferenceKind {
    Domain,
    Enum,
    Composite,
}

#[derive(Clone, Copy)]
pub(super) struct TypeReferenceMove {
    identity: TypeIdentity,
    prior: UserTypeName,
    current: UserTypeName,
}

impl TypeReferenceMove {
    pub(super) fn new(identity: TypeIdentity, prior: UserTypeName, current: UserTypeName) -> Self {
        Self {
            identity,
            prior,
            current,
        }
    }

    pub(super) fn applies_to(self, identity: TableIdentity) -> bool {
        identity.database == self.identity.database()
            && identity.existence != CatalogDdlState::Absent
    }

    fn matches(self, ctype: ColType, identity: Option<UserTypeName>) -> bool {
        let Some(identity) = identity else {
            return false;
        };
        match identity.binding {
            TypeBinding::Captured(key) => return key == self.identity,
            TypeBinding::Stored(key) => {
                return key.slot() == self.identity.slot()
                    && key.created_at() == self.identity.created_at();
            }
            TypeBinding::Polymorphic(_) => return false,
            TypeBinding::Declaration => {}
        }
        if identity.schema != self.prior.schema || identity.name != self.prior.name {
            return false;
        }
        // Recovery declarations have names before their runtime slots bind.
        match self.identity.slot() {
            NamedTypeSlot::Domain(_) => true,
            NamedTypeSlot::Enum(expected) => matches!(ctype,
                ColType::Enum(slot) | ColType::Array(ArrElem::Enum(slot))
                    if usize::from(slot) == expected || slot == ColType::ENUM_SLOT_UNRESOLVED),
            NamedTypeSlot::Composite(expected) => matches!(ctype,
                ColType::Composite(slot) | ColType::Array(ArrElem::Composite(slot))
                    if usize::from(slot) == expected || slot == ColType::COMPOSITE_SLOT_UNRESOLVED),
        }
    }

    fn rewrite(self, ctype: ColType, identity: &mut Option<UserTypeName>) -> bool {
        let Some(identity) = identity else {
            return false;
        };
        if !self.matches(ctype, Some(*identity)) {
            return false;
        }
        identity.schema = self.current.schema;
        identity.name = self.current.name;
        true
    }

    pub(super) fn references_definition(self, definition: &TableDef) -> bool {
        definition
            .columns()
            .iter()
            .any(|column| self.matches(column.ctype, column.user_type))
    }

    pub(super) fn rewrite_definition(self, definition: &mut TableDef) -> bool {
        let mut changed = false;
        for column in definition.columns.iter_mut().take(definition.n_columns) {
            changed |= self.rewrite(column.ctype, &mut column.user_type);
        }
        changed
    }

    fn rewrite_domain(
        self,
        base: ColType,
        parent: &mut Option<UserTypeName>,
        user_type: &mut Option<UserTypeName>,
    ) {
        if matches!(self.identity.slot(), NamedTypeSlot::Domain(_)) {
            self.rewrite(base, parent);
            if parent.is_none() {
                self.rewrite(base, user_type);
            }
        } else {
            self.rewrite(base, user_type);
        }
    }

    fn rewrite_result(self, result: &mut RoutineResult) {
        self.rewrite(result.ctype, &mut result.user_type);
    }

    fn rewrite_operator(self, definition: &mut OperatorDefinition) {
        super::type_contracts::CatalogTypes::Operator(definition)
            .for_each(|result| self.rewrite_result(result));
    }

    fn rewrite_family(self, definition: &mut OperatorFamilyDefinition) {
        super::type_contracts::CatalogTypes::Family(definition)
            .for_each(|result| self.rewrite_result(result));
    }

    fn rewrite_class(self, definition: &mut OperatorClassDefinition) {
        super::type_contracts::CatalogTypes::Class(definition)
            .for_each(|result| self.rewrite_result(result));
    }

    fn class(self) -> DependencyClass {
        match self.identity.slot() {
            NamedTypeSlot::Domain(_) => DependencyClass::Domain,
            NamedTypeSlot::Enum(_) => DependencyClass::Enum,
            NamedTypeSlot::Composite(_) => DependencyClass::Composite,
        }
    }
}

/// Only type-identity publication uses this startup workspace. Ordinary reads
/// and writes do not acquire its mutex.
pub(super) struct TypePublication {
    scratch: Mutex<Arena>,
    #[cfg(test)]
    pub(super) waits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    scan_pause: std::sync::atomic::AtomicU8,
}

struct TableOwner<'storage> {
    definition: DefinitionWrite<'storage>,
    publication: MutexGuard<'storage, TablePublication>,
    _lifecycle: MutexGuard<'storage, ()>,
}

struct TableAttempt<'slots, 'storage> {
    slots: &'slots mut [Option<TableOwner<'storage>>],
}

impl Drop for TableAttempt<'_, '_> {
    fn drop(&mut self) {
        for owner in self.slots.iter_mut() {
            drop(owner.take());
        }
    }
}

impl TypePublication {
    pub(super) fn validate_generations(
        storage: &Storage,
        changes: impl Iterator<Item = super::TypeCommitChange> + Clone,
    ) -> Result<(), crate::sql::eval::SqlError> {
        for table in storage.tables.iter() {
            loop {
                let attempt: Result<_, LockWait<'_>> = (|| {
                    let state = table.definition.try_read_for_batch()?;
                    let publication =
                        try_mutex(&table.publication, "table publication lock poisoned")?;
                    let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
                    let count = changes
                        .clone()
                        .filter_map(|change| match change {
                            super::TypeCommitChange::Definition(target) => {
                                target.identity_move(&types)
                            }
                            super::TypeCommitChange::Lifecycle(_) => None,
                        })
                        .filter(|movement| {
                            movement.applies_to(state.identity)
                                && storage
                                    .pending_table_defs
                                    .references_type(&state, *movement)
                        })
                        .count();
                    Ok(publication.generation.checked_add(count as u64).is_some())
                })();
                match attempt {
                    Err(wait) => wait.wait(),
                    Ok(true) => break,
                    Ok(false) => {
                        return Err(crate::sql_err!(
                            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "type change exhausts a dependent table publication generation"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn scratch_bytes(config: &Config) -> usize {
        super::table_slot_capacity(config)
            .saturating_mul(size_of::<Option<TableOwner<'static>>>())
            .saturating_add(align_of::<Option<TableOwner<'static>>>() - 1)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>().saturating_add(Self::scratch_bytes(config))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "type publication controls")?;
        Ok(Self {
            scratch: Mutex::new(Arena::new(
                budget,
                "type publication",
                Self::scratch_bytes(config),
            )?),
            #[cfg(test)]
            waits: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            scan_pause: std::sync::atomic::AtomicU8::new(0),
        })
    }

    fn acquire_tables<'storage>(
        storage: &'storage Storage,
        movement: TypeReferenceMove,
        owners: &mut TableAttempt<'_, 'storage>,
    ) -> Result<(), LockWait<'storage>> {
        for (slot, table) in storage.tables.iter().enumerate() {
            if owners.slots[slot].is_some() {
                continue;
            }
            let state = table.definition.try_read_for_batch()?;
            if !movement.applies_to(state.identity)
                || !storage.pending_table_defs.references_type(&state, movement)
            {
                continue;
            }
            drop(state);
            let lifecycle = try_mutex(&table.lifecycle, "table lifecycle lock poisoned")?;
            let definition = table.definition.try_write_for_batch()?;
            // A shared drop may retire the selected table between the read and
            // write owners. Never dirty or rewrite that retired incarnation.
            if !movement.applies_to(definition.identity)
                || !storage
                    .pending_table_defs
                    .references_type(&definition, movement)
            {
                continue;
            }
            let publication = try_mutex(&table.publication, "table publication lock poisoned")?;
            owners.slots[slot] = Some(TableOwner {
                definition,
                publication,
                _lifecycle: lifecycle,
            });
        }
        Ok(())
    }

    fn publish(
        &self,
        storage: &Storage,
        movement: TypeReferenceMove,
        source: TypePublicationSource,
        install_source: impl FnOnce(&mut TypeCatalog, &mut EnumMemberImages),
    ) -> Result<(), crate::sql::eval::SqlError> {
        let mut scratch = self
            .scratch
            .lock()
            .expect("type publication scratch poisoned");
        scratch.reset();
        // Startup reserves one slot per physical table, including the internal
        // relation. This is the only allocation from the reset workspace.
        let mut slots = scratch
            .alloc_owned_slice_with(storage.tables.len(), |_| None)
            .expect("startup type publication reservation covers every table");
        loop {
            let mut owners = TableAttempt { slots: &mut slots };
            let attempt: Result<_, LockWait<'_>> = (|| {
                Self::acquire_tables(storage, movement, &mut owners)?;
                #[cfg(test)]
                if self
                    .scan_pause
                    .compare_exchange(
                        1,
                        2,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
                {
                    while self.scan_pause.load(std::sync::atomic::Ordering::Acquire) == 2 {
                        std::thread::yield_now();
                    }
                }
                try_mutex(&storage.type_catalog, "type catalog lock poisoned")
            })();
            let mut types = match attempt {
                Ok(types) => types,
                Err(wait) => {
                    drop(owners);
                    #[cfg(test)]
                    self.waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                    continue;
                }
            };
            if !source.validate(&types, movement)? {
                return Ok(());
            }
            let attempt: Result<_, LockWait<'_>> = (|| {
                // Dependent creation and edits bind while owning this catalog.
                // Catch submissions that arrived after the first table scan.
                Self::acquire_tables(storage, movement, &mut owners)?;
                let members = try_mutex(
                    &storage.enum_member_images,
                    "enum member-image lock poisoned",
                )?;
                let comments = try_mutex(&storage.comments, "comment catalog lock poisoned")?;
                let routines = try_mutex(&storage.routines, "routine catalog lock poisoned")?;
                let operators =
                    try_mutex(&storage.operator_catalog, "operator catalog lock poisoned")?;
                let casts = try_mutex(&storage.casts, "cast catalog lock poisoned")?;
                let dependencies = try_mutex(
                    &storage.stored_query_dependencies,
                    "stored-query dependency catalog lock poisoned",
                )?;
                Ok((members, comments, routines, operators, casts, dependencies))
            })();
            match attempt {
                Err(wait) => {
                    drop(types);
                    drop(owners);
                    #[cfg(test)]
                    self.waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                }
                Ok((
                    mut members,
                    mut comments,
                    mut routines,
                    mut operators,
                    mut casts,
                    mut dependencies,
                )) => {
                    // Preflight every fallible counter before changing any
                    // source, pending version or dependent definition.
                    for owner in owners.slots.iter().flatten() {
                        owner.publication.generation.checked_add(1).ok_or_else(|| {
                            crate::sql_err!(
                                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "type change exhausts a dependent table publication generation"
                            )
                        })?;
                    }
                    install_source(&mut types, &mut members);
                    for owner in owners.slots.iter_mut().flatten() {
                        assert!(
                            storage
                                .pending_table_defs
                                .move_type_references_locked(&mut owner.definition, movement)
                        );
                        owner.publication.generation += 1;
                        owner.publication.dirty = true;
                    }
                    movement.rewrite_catalog(&mut types);
                    for comment in comments.entries.iter_mut().filter(|comment| {
                        comment.used
                            && comment.database == Some(movement.identity.database())
                            && comment.class == CommentClass::Type
                            && comment.schema == movement.prior.schema
                            && comment.name == movement.prior.name
                    }) {
                        comment.schema = movement.current.schema;
                        comment.name = movement.current.name;
                    }
                    movement.rewrite_routines(&mut routines);
                    movement.rewrite_operators(&mut operators);
                    movement.rewrite_casts(&mut casts);
                    storage.rename_stored_query_dependency_locked(
                        &mut dependencies,
                        movement.class(),
                        movement.identity.slot().index(),
                        None,
                        movement.current.schema,
                        movement.current.name,
                    );
                    return Ok(());
                }
            }
        }
    }
}

impl Storage {
    pub(super) fn publish_type_move(
        &self,
        movement: TypeReferenceMove,
        source: TypePublicationSource,
        install_source: impl FnOnce(&mut TypeCatalog, &mut EnumMemberImages),
    ) -> Result<(), crate::sql::eval::SqlError> {
        self.type_publication
            .publish(self, movement, source, install_source)
    }
}

impl TypeReferenceMove {
    fn rewrite_catalog(self, catalog: &mut TypeCatalog) {
        for composite in catalog.composites.iter_mut().filter(|composite| {
            composite.database == self.identity.database()
                && composite.ddl_state != CatalogDdlState::Absent
        }) {
            for field in composite.fields.iter_mut().take(composite.n_fields) {
                self.rewrite(field.ctype, &mut field.user_type);
            }
            if let Some(pending) = &mut composite.pending_definition {
                for field in pending.fields.iter_mut().take(pending.n_fields) {
                    self.rewrite(field.ctype, &mut field.user_type);
                }
            }
        }
        for domain in catalog.domains.iter_mut().filter(|domain| {
            domain.database == self.identity.database()
                && domain.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_domain(
                domain.base,
                &mut domain.base_domain,
                &mut domain.base_user_type,
            );
            if let Some(pending) = &mut domain.pending_definition {
                self.rewrite_domain(
                    pending.spec.base,
                    &mut pending.spec.base_domain,
                    &mut pending.spec.base_user_type,
                );
            }
        }
    }

    fn rewrite_operators(self, catalog: &mut super::OperatorCatalog) {
        for operator in catalog.operators.iter_mut().filter(|operator| {
            operator.database == self.identity.database()
                && operator.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_operator(&mut operator.definition);
            if let Some(pending) = &mut operator.pending {
                self.rewrite_operator(&mut pending.definition);
            }
        }
        for family in catalog.families.iter_mut().filter(|family| {
            family.database == self.identity.database()
                && family.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_family(&mut family.definition);
            if let Some(pending) = &mut family.pending {
                self.rewrite_family(&mut pending.definition);
            }
        }
        for class in catalog.classes.iter_mut().filter(|class| {
            class.database == self.identity.database() && class.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_class(&mut class.definition);
            if let Some(pending) = &mut class.pending {
                self.rewrite_class(&mut pending.definition);
            }
        }
    }

    fn rewrite_casts(self, casts: &mut crate::mem::fixed_vec::FixedVec<super::CastDef>) {
        for cast in casts.iter_mut().filter(|cast| {
            cast.database == self.identity.database() && cast.ddl_state != CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Cast(cast)
                .for_each(|result| self.rewrite_result(result));
        }
    }

    fn rewrite_routines(self, routines: &mut crate::mem::fixed_vec::FixedVec<super::RoutineDef>) {
        for routine in routines.iter_mut().filter(|routine| {
            routine.database == self.identity.database()
                && routine.ddl_state != CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Routine(routine)
                .for_each(|result| self.rewrite_result(result));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config};
    use super::super::{
        CheckConstraint, CompositeFieldDef, CompositeSpec, DomainSpec, EnumMember, EnumSpec,
        MAX_COLUMNS, MAX_COMPOSITE_FIELDS, MAX_DOMAIN_CHECKS, SqlName, comment_stackstr,
    };
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const KINDS: [TypeReferenceKind; 3] = [
        TypeReferenceKind::Domain,
        TypeReferenceKind::Enum,
        TypeReferenceKind::Composite,
    ];

    enum MoveUndo {
        Domain(super::super::DomainAlterUndo),
        Enum(super::super::EnumAlterUndo),
        Composite(super::super::CompositeAlterUndo),
    }

    impl MoveUndo {
        fn target(&self) -> TypeEditTarget {
            match self {
                Self::Domain(undo) => undo.target(),
                Self::Enum(undo) => undo.target(),
                Self::Composite(undo) => undo.target(),
            }
        }

        fn rollback(self, storage: &Storage) {
            match self {
                Self::Domain(undo) => storage.rollback_domain_alter(undo),
                Self::Enum(undo) => storage.rollback_enum_alter(undo),
                Self::Composite(undo) => storage.rollback_composite_alter(undo),
            }
        }
    }

    fn stage_move(
        storage: &Storage,
        kind: TypeReferenceKind,
        slot: usize,
        destination: UserTypeName,
        transaction: u32,
    ) -> MoveUndo {
        match kind {
            TypeReferenceKind::Domain => MoveUndo::Domain(
                storage
                    .stage_domain_identity(
                        storage.prepare_domain_edit(slot, transaction).unwrap().1,
                        destination.schema,
                        destination.name,
                        transaction,
                    )
                    .unwrap(),
            ),
            TypeReferenceKind::Enum => {
                let (definition, target) = storage.prepare_enum_edit(slot, transaction).unwrap();
                let members = [definition.members().next().unwrap()];
                MoveUndo::Enum(
                    storage
                        .stage_enum_alter(
                            target,
                            destination.schema,
                            destination.name,
                            &members,
                            transaction,
                        )
                        .unwrap(),
                )
            }
            TypeReferenceKind::Composite => {
                let (mut definition, target) =
                    storage.prepare_composite_edit(slot, transaction).unwrap();
                definition.schema = destination.schema;
                definition.name = destination.name;
                MoveUndo::Composite(
                    storage
                        .stage_composite_alter(target, definition, transaction)
                        .unwrap(),
                )
            }
        }
    }

    fn commit_move(
        storage: &Storage,
        kind: TypeReferenceKind,
        target: TypeEditTarget,
    ) -> Result<(), SqlError> {
        match kind {
            TypeReferenceKind::Domain => storage.commit_domain_alter(target),
            TypeReferenceKind::Enum => storage.commit_enum_alter(target),
            TypeReferenceKind::Composite => storage.commit_composite_alter(target),
        }
    }

    struct ResumeScan<'a>(&'a std::sync::atomic::AtomicU8);

    impl Drop for ResumeScan<'_> {
        fn drop(&mut self) {
            self.0.store(3, std::sync::atomic::Ordering::Release);
        }
    }

    #[test]
    fn shared_type_commit_catches_dependents_submitted_after_its_first_scan() {
        use std::sync::atomic::Ordering;
        for kind in KINDS {
            for submission in 0..3 {
                let (storage, related, unrelated, movement) = fixture(kind);
                let database = storage.current_database_oid();
                let undo = stage_move(
                    &storage,
                    kind,
                    movement.identity.slot().index(),
                    movement.current,
                    7,
                );
                let target = undo.target();
                let mut definition = *storage.table_def(related, 0);
                definition.name = SqlName::parse(if submission == 0 {
                    "arrived_late"
                } else {
                    "unrelated"
                })
                .unwrap();
                storage
                    .type_publication
                    .scan_pause
                    .store(1, Ordering::Release);
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::sql::set_execution_database_oid(database);
                        crate::mem::guard::forbid_alloc(|| commit_move(&storage, kind, target))
                    });
                    let resume = ResumeScan(&storage.type_publication.scan_pause);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while storage.type_publication.scan_pause.load(Ordering::Acquire) != 2 {
                        assert!(
                            Instant::now() < deadline,
                            "type publication did not reach its scan boundary"
                        );
                        std::thread::yield_now();
                    }
                    let (ready, received) = mpsc::sync_channel(1);
                    let submitting_storage = &storage;
                    let submitter = scope.spawn(move || {
                        crate::sql::set_execution_database_oid(database);
                        let result = crate::mem::guard::forbid_alloc(|| {
                            let result = if submission == 0 {
                                submitting_storage.create_table_in(definition, 11)
                            } else {
                                submitting_storage
                                    .write_table_def(unrelated, 11, definition, &mapping("id"), false)
                                    .map(|()| unrelated)
                            };
                            if submission == 2 {
                                submitting_storage.tables[unrelated].publication().generation = u64::MAX;
                            }
                            result
                        });
                        ready.send(()).unwrap();
                        result
                    });
                    // Release the artificial pause before joining a blocked submitter.
                    let progress = received.recv_timeout(Duration::from_secs(5));
                    drop(resume);
                    let result = writer.join().unwrap();
                    let dependent = submitter.join().unwrap().unwrap();
                    assert!(progress.is_ok(), "late dependent submission waited on an unrelated definition");
                    if submission == 2 {
                        assert_eq!(
                            result.unwrap_err().sqlstate,
                            sqlstate::PROGRAM_LIMIT_EXCEEDED
                        );
                        assert_eq!(
                            source_identity(&storage.type_catalog.lock().unwrap(), movement),
                            movement.prior
                        );
                        assert_identity(&storage, related, 0, movement.prior);
                        assert_identity(&storage, dependent, 11, movement.prior);
                        storage.tables[dependent].publication().generation = 0;
                        crate::mem::guard::forbid_alloc(|| {
                            commit_move(&storage, kind, target).unwrap()
                        });
                    } else {
                        result.unwrap();
                    }
                    assert_identity(&storage, related, 0, movement.current);
                    assert_identity(&storage, dependent, 11, movement.current);
                    assert!(storage.type_catalog.try_lock().is_ok());
                    assert!(storage.type_publication.scratch.try_lock().is_ok());
                });
            }
        }
    }

    #[test]
    fn shared_type_commit_revalidates_superseded_and_rolled_back_edits_after_waiting() {
        #[derive(Clone, Copy)]
        enum Outcome {
            Rollback,
            NewRevision,
            RenameAgain,
        }
        for kind in KINDS {
            for outcome in [
                Outcome::Rollback,
                Outcome::NewRevision,
                Outcome::RenameAgain,
            ] {
                let (storage, related, _, movement) = fixture(kind);
                let database = storage.current_database_oid();
                let undo = stage_move(
                    &storage,
                    kind,
                    movement.identity.slot().index(),
                    movement.current,
                    7,
                );
                let target = undo.target();
                let selected = storage.table_def(related, 0);
                let next_name = match outcome {
                    Outcome::RenameAgain => UserTypeName::declaration(
                        movement.current.schema,
                        SqlName::parse("newest_type").unwrap(),
                    ),
                    Outcome::Rollback | Outcome::NewRevision => movement.current,
                };
                let (finished, received) = mpsc::sync_channel(1);
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::sql::set_execution_database_oid(database);
                        let result =
                            crate::mem::guard::forbid_alloc(|| commit_move(&storage, kind, target));
                        finished.send(()).unwrap();
                        result
                    });
                    let waiting = wait_until_contended(&storage);
                    let next = match outcome {
                        Outcome::NewRevision | Outcome::RenameAgain => Some(stage_move(
                            &storage,
                            kind,
                            movement.identity.slot().index(),
                            next_name,
                            7,
                        )),
                        Outcome::Rollback => {
                            undo.rollback(&storage);
                            None
                        }
                    };
                    let unrelated_metadata = storage.stored_query_dependencies.lock().unwrap();
                    drop(selected);
                    let completed = received.recv_timeout(Duration::from_secs(5));
                    drop(unrelated_metadata);
                    writer.join().unwrap().unwrap();
                    assert!(
                        completed.is_ok(),
                        "an obsolete edit waited on unrelated metadata"
                    );
                    assert!(
                        waiting,
                        "type publication did not wait for the selected table"
                    );
                    assert_eq!(
                        source_identity(&storage.type_catalog.lock().unwrap(), movement),
                        movement.prior
                    );
                    assert_identity(&storage, related, 0, movement.prior);
                    if let Some(next) = next {
                        crate::mem::guard::forbid_alloc(|| {
                            commit_move(&storage, kind, next.target()).unwrap()
                        });
                        assert_identity(&storage, related, 0, next_name);
                    }
                    assert!(storage.type_catalog.try_lock().is_ok());
                    assert!(storage.type_publication.scratch.try_lock().is_ok());
                });
            }
        }
    }

    #[test]
    fn shared_type_move_rejects_a_new_definition_or_owner_after_waiting() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for kind in KINDS {
            for commit_update in [false, true] {
                let (storage, related, _, movement) = fixture(kind);
                let database = storage.current_database_oid();
                let source = TypePublicationSource::committed(
                    &storage.type_catalog.lock().unwrap(),
                    movement.identity.slot(),
                )
                .unwrap();
                let selected = storage.table_def(related, 0);
                let installed = AtomicBool::new(false);
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::sql::set_execution_database_oid(database);
                        crate::mem::guard::forbid_alloc(|| {
                            storage.publish_type_move(movement, source, |_catalog, _members| {
                                installed.store(true, Ordering::Release);
                            })
                        })
                    });
                    let waiting = wait_until_contended(&storage);
                    let update = stage_move(
                        &storage,
                        kind,
                        movement.identity.slot().index(),
                        movement.prior,
                        7,
                    );
                    let changed = commit_update.then(|| {
                        crate::mem::guard::forbid_alloc(|| {
                            commit_move(&storage, kind, update.target())
                        })
                    });
                    if !commit_update {
                        let captured = TypePublicationSource::committed(
                            &storage.type_catalog.lock().unwrap(),
                            movement.identity.slot(),
                        );
                        assert!(
                            matches!(captured, Err(error) if error.sqlstate == sqlstate::SERIALIZATION_FAILURE)
                        );
                    }
                    drop(selected);
                    let result = writer.join().unwrap();
                    if let Some(changed) = changed {
                        changed.unwrap();
                    } else {
                        update.rollback(&storage);
                    }
                    assert!(
                        waiting,
                        "type publication did not wait for the selected table"
                    );
                    assert_eq!(
                        result.unwrap_err().sqlstate,
                        sqlstate::SERIALIZATION_FAILURE
                    );
                    assert!(!installed.load(Ordering::Acquire));
                    assert_identity(&storage, related, 0, movement.prior);
                    assert_eq!(
                        source_identity(&storage.type_catalog.lock().unwrap(), movement),
                        movement.prior
                    );
                    let next = stage_move(
                        &storage,
                        kind,
                        movement.identity.slot().index(),
                        movement.current,
                        8,
                    );
                    crate::mem::guard::forbid_alloc(|| {
                        commit_move(&storage, kind, next.target()).unwrap()
                    });
                    assert_identity(&storage, related, 0, movement.current);
                });
            }
        }
    }

    fn named_slot(kind: TypeReferenceKind, slot: usize) -> NamedTypeSlot {
        match kind {
            TypeReferenceKind::Domain => NamedTypeSlot::Domain(slot),
            TypeReferenceKind::Enum => NamedTypeSlot::Enum(slot),
            TypeReferenceKind::Composite => NamedTypeSlot::Composite(slot),
        }
    }

    fn create_kind(
        storage: &mut Storage,
        kind: TypeReferenceKind,
        identity: UserTypeName,
    ) -> (usize, ColType, ColType) {
        match kind {
            TypeReferenceKind::Domain => {
                let slot = storage
                    .create_domain(
                        identity.schema,
                        identity.name,
                        DomainSpec {
                            base_domain: None,
                            base_user_type: None,
                            base: ColType::Int4,
                            base_type_mod: -1,
                            not_null: false,
                            default_expr: None,
                            checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                            n_checks: 0,
                        },
                        0,
                    )
                    .unwrap()
                    .slot();
                (
                    slot,
                    ColType::Int4,
                    ColType::Array(ArrElem::domain(slot as u16, ColType::Int4).unwrap()),
                )
            }
            TypeReferenceKind::Enum => {
                let slot = storage
                    .create_enum(
                        identity.schema,
                        identity.name,
                        EnumSpec {
                            members: &[EnumMember {
                                label: SqlName::parse("ready").unwrap(),
                                legacy_label: None,
                                sort: 1.0,
                            }],
                        },
                        0,
                    )
                    .unwrap()
                    .slot();
                (
                    slot,
                    ColType::Enum(slot as u16),
                    ColType::Array(ArrElem::Enum(slot as u16)),
                )
            }
            TypeReferenceKind::Composite => {
                let mut fields = [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS];
                fields[0] = CompositeFieldDef {
                    attribute_number: 1,
                    name: SqlName::parse("value").unwrap(),
                    ctype: ColType::Int4,
                    dropped: false,
                    ..CompositeFieldDef::EMPTY
                };
                let slot = storage
                    .create_composite(
                        identity.schema,
                        identity.name,
                        CompositeSpec {
                            fields,
                            n_fields: 1,
                        },
                        0,
                    )
                    .unwrap()
                    .slot();
                (
                    slot,
                    ColType::Composite(slot as u16),
                    ColType::Array(ArrElem::Composite(slot as u16)),
                )
            }
        }
    }

    fn fixture(kind: TypeReferenceKind) -> (Storage, usize, usize, TypeReferenceMove) {
        let mut config = test_config();
        config.max_catalog_versions_per_object = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let prior = UserTypeName::declaration(
            SqlName::parse("public").unwrap(),
            SqlName::parse("prior_type").unwrap(),
        );
        let (slot, scalar, array) = create_kind(&mut storage, kind, prior);
        let mut definition = make_def("related", &[("id", scalar, false), ("items", array, false)]);
        for column in definition.columns.iter_mut().take(2) {
            column.user_type = Some(prior);
        }
        let related = storage.create_table(definition).unwrap();
        let unrelated = storage
            .create_table(make_def("unrelated", &[("id", ColType::Int4, false)]))
            .unwrap();
        storage
            .apply_comment(
                CommentClass::Type,
                prior.schema,
                prior.name,
                0,
                Some(comment_stackstr("type reference ownership").unwrap()),
            )
            .unwrap();
        let identity = {
            let catalog = storage.type_catalog.lock().unwrap();
            catalog
                .type_snapshot(named_slot(kind, slot), 0)
                .unwrap()
                .unwrap()
                .identity
        };
        let movement = TypeReferenceMove::new(
            identity,
            prior,
            UserTypeName::declaration(prior.schema, SqlName::parse("current_type").unwrap()),
        );
        (storage, related, unrelated, movement)
    }

    fn mapping(name: &str) -> [Option<SqlName>; MAX_COLUMNS] {
        let mut result = [None; MAX_COLUMNS];
        result[0] = Some(SqlName::parse(name).unwrap());
        result[1] = Some(SqlName::parse("items").unwrap());
        result
    }

    #[test]
    fn type_publication_exhaustion_leaves_source_and_all_dependents_unchanged() {
        use crate::storage::TypeCommitChange;
        for kind in KINDS {
            let (storage, related, unrelated, movement) = fixture(kind);
            let source = movement.identity.slot().index();
            let target = match kind {
                TypeReferenceKind::Domain => storage
                    .stage_domain_identity(
                        storage.prepare_domain_edit(source, 7).unwrap().1,
                        movement.current.schema,
                        movement.current.name,
                        7,
                    )
                    .unwrap()
                    .target(),
                TypeReferenceKind::Enum => {
                    let (definition, target) = storage.prepare_enum_edit(source, 7).unwrap();
                    let members = [definition.members().next().unwrap()];
                    storage
                        .stage_enum_alter(
                            target,
                            movement.current.schema,
                            movement.current.name,
                            &members,
                            7,
                        )
                        .unwrap()
                        .target()
                }
                TypeReferenceKind::Composite => {
                    let (mut definition, target) =
                        storage.prepare_composite_edit(source, 7).unwrap();
                    definition.schema = movement.current.schema;
                    definition.name = movement.current.name;
                    storage
                        .stage_composite_alter(target, definition, 7)
                        .unwrap()
                        .target()
                }
            };
            crate::mem::guard::forbid_alloc(|| {
                storage.tables[unrelated].publication().generation = u64::MAX;
                storage.tables[related].publication().generation = u64::MAX - 1;
                storage
                    .validate_type_commit(7, [TypeCommitChange::Definition(target)].into_iter())
                    .unwrap();
                storage.tables[related].publication().generation = u64::MAX;
                assert_eq!(
                    storage
                        .validate_type_commit(7, [TypeCommitChange::Definition(target)].into_iter())
                        .unwrap_err()
                        .sqlstate,
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                let error = match kind {
                    TypeReferenceKind::Domain => storage.commit_domain_alter(target),
                    TypeReferenceKind::Enum => storage.commit_enum_alter(target),
                    TypeReferenceKind::Composite => storage.commit_composite_alter(target),
                }
                .unwrap_err();
                assert_eq!(
                    error.sqlstate,
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_eq!(
                    source_identity(&storage.type_catalog.lock().unwrap(), movement),
                    movement.prior
                );
                assert!(
                    storage.table_def(related, 0).columns()[0]
                        .user_type
                        .unwrap()
                        .is_named(movement.prior.schema, movement.prior.name)
                );
                assert!(storage.type_catalog.try_lock().is_ok());
                assert!(storage.tables[related].publication.try_lock().is_ok());
                storage.tables[related].publication().generation = 0;
                match kind {
                    TypeReferenceKind::Domain => storage.commit_domain_alter(target),
                    TypeReferenceKind::Enum => storage.commit_enum_alter(target),
                    TypeReferenceKind::Composite => storage.commit_composite_alter(target),
                }
                .unwrap();
                assert_eq!(
                    source_identity(&storage.type_catalog.lock().unwrap(), movement),
                    movement.current
                );
            });
        }
    }

    fn source_identity(catalog: &TypeCatalog, movement: TypeReferenceMove) -> UserTypeName {
        let (schema, name) = match movement.identity.slot() {
            NamedTypeSlot::Domain(_) => {
                let definition = catalog.domains[movement.identity.slot().index()];
                (definition.schema, definition.name)
            }
            NamedTypeSlot::Enum(_) => {
                let definition = catalog.enums[movement.identity.slot().index()];
                (definition.schema, definition.name)
            }
            NamedTypeSlot::Composite(_) => {
                let definition = catalog.composites[movement.identity.slot().index()];
                (definition.schema, definition.name)
            }
        };
        UserTypeName::declaration(schema, name)
    }

    fn wait_until_contended(storage: &Storage) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while storage
            .type_publication
            .waits
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    #[test]
    fn dependent_type_publication_releases_destination_before_source_wait() {
        use crate::storage::{CastContext, CastDef, CastMethod, RoutineResult};
        for kind in KINDS {
            let (storage, related, _, movement) = fixture(kind);
            let column = storage.table_def(related, 0).columns[0];
            let database = storage.current_database_oid();
            let mut definition = CastDef {
                database,
                created_at: 1,
                source: RoutineResult {
                    ctype: column.ctype,
                    user_type: column.user_type,
                },
                target: RoutineResult::builtin(ColType::Text),
                method: CastMethod::InOut,
                context: CastContext::Explicit,
                ddl_state: CatalogDdlState::Present,
            };
            let mut source = storage.type_catalog.lock().unwrap();
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    crate::sql::set_execution_database_oid(database);
                    crate::mem::guard::forbid_alloc(|| {
                        let mut owners = storage.catalog_type_owners(&storage.casts);
                        owners.bind_definition(&mut definition, 0).unwrap();
                        assert!(storage.type_catalog.try_lock().is_err());
                        owners.catalog[0] = definition;
                        assert!(storage.type_catalog.try_lock().is_err());
                    });
                });
                let waiting = wait_until_contended(&storage);
                let destination_free = storage.casts.try_lock().is_ok();
                crate::mem::guard::forbid_alloc(|| match movement.identity.slot() {
                    NamedTypeSlot::Domain(slot) => {
                        source.domains[slot].schema = movement.current.schema;
                        source.domains[slot].name = movement.current.name;
                    }
                    NamedTypeSlot::Enum(slot) => {
                        source.enums[slot].schema = movement.current.schema;
                        source.enums[slot].name = movement.current.name;
                    }
                    NamedTypeSlot::Composite(slot) => {
                        source.composites[slot].schema = movement.current.schema;
                        source.composites[slot].name = movement.current.name;
                    }
                });
                drop(source);
                writer.join().unwrap();
                assert!(waiting, "writer did not reach the protected publication");
                assert!(destination_free, "source wait retained the destination");
            });
            let submitted = storage.cast(0).source.user_type.unwrap();
            assert_eq!(submitted, column.user_type.unwrap());
            assert_eq!(submitted.schema, movement.current.schema);
            assert_eq!(submitted.name, movement.current.name);
            assert!(storage.type_catalog.try_lock().is_ok());
            assert!(storage.casts.try_lock().is_ok());
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum SavedQueryKind {
        View,
        Rule,
        MaterializedView,
        Policy,
    }

    const SAVED_QUERIES: [SavedQueryKind; 4] = [
        SavedQueryKind::View,
        SavedQueryKind::Rule,
        SavedQueryKind::MaterializedView,
        SavedQueryKind::Policy,
    ];

    fn query_dependency(
        storage: &Storage,
        movement: TypeReferenceMove,
    ) -> super::super::StoredQueryDependency {
        super::super::StoredQueryDependency {
            class: movement.class(),
            slot: movement.identity.slot().index() as u16,
            identity: storage
                .capture_type_dependency(movement.class(), movement.identity.slot().index(), 0)
                .unwrap(),
            referenced_columns: super::super::ColumnSet::EMPTY,
            schema: movement.prior.schema,
            name: movement.prior.name,
            referenced_schema: movement.prior.schema,
            referenced_name: movement.prior.name,
        }
    }

    fn publish_saved_query(
        storage: &Storage,
        publication: SavedQueryKind,
        related: usize,
        dependency: super::super::StoredQueryDependency,
    ) -> Result<usize, crate::sql::eval::SqlError> {
        use super::super::tests::test_view_definition;
        use super::super::{
            PUBLIC_ROLE, PolicyCommandKind, PolicyDefinition, PolicySpec, RuleDefinition,
            RuleTarget, StoredQueryDependencies, StoredQueryDependencyOwner,
        };
        let mut dependencies = StoredQueryDependencies::EMPTY;
        dependencies.push(dependency)?;
        let definition = test_view_definition(dependencies);
        match publication {
            SavedQueryKind::View => {
                let (slot, _) = storage.create_view(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("typed_submission").unwrap(),
                    definition,
                    false,
                    7,
                )?;
                Ok(
                    storage.committed_dependency_image(StoredQueryDependencyOwner::Rule(
                        storage.view(slot).return_rule,
                    )),
                )
            }
            SavedQueryKind::Rule => {
                let (slot, _) = storage.create_rule(
                    RuleDefinition {
                        name: SqlName::parse("typed_submission").unwrap(),
                        target: RuleTarget::Table(related as u16),
                        ..RuleDefinition::EMPTY
                    },
                    definition.query.dependencies.view(),
                    false,
                    7,
                )?;
                Ok(storage
                    .committed_dependency_image(StoredQueryDependencyOwner::Rule(slot as u16)))
            }
            SavedQueryKind::MaterializedView => storage
                .create_matview(related, definition.query, false, 7)
                .map(|slot| storage.matview_dependency_image(slot)),
            SavedQueryKind::Policy => storage
                .create_policy(
                    PolicySpec {
                        name: SqlName::parse("typed_submission").unwrap(),
                        table: related,
                        command: PolicyCommandKind::All,
                        permissive: true,
                        definition: PolicyDefinition {
                            using: Some(crate::util::StackStr::from_str("true")),
                            with_check: None,
                        },
                        roles: &[PUBLIC_ROLE],
                    },
                    &definition.query.dependencies,
                    7,
                )
                .map(|slot| {
                    storage
                        .committed_dependency_image(StoredQueryDependencyOwner::Policy(slot as u16))
                }),
        }
    }

    #[test]
    fn saved_query_publication_releases_owners_and_refreshes_type_sources() {
        use super::super::StoredQueryDependencies;
        use super::super::tests::test_view_definition;
        for kind in KINDS {
            for publication in SAVED_QUERIES {
                let (storage, related, _, movement) = fixture(kind);
                let database = storage.current_database_oid();
                let dependency = query_dependency(&storage, movement);
                let identity = dependency.identity;
                let mut source = storage.type_catalog.lock().unwrap();
                std::thread::scope(|scope| {
                    let storage = &storage;
                    let writer =
                        scope.spawn(move || -> Result<usize, crate::sql::eval::SqlError> {
                            crate::sql::set_execution_database_oid(database);
                            crate::mem::guard::forbid_alloc(|| {
                                publish_saved_query(storage, publication, related, dependency)
                            })
                        });
                    let waiting = wait_until_contended(storage);
                    let available = storage.view_rule_catalog.try_lock().is_ok()
                        && storage.matviews.try_lock().is_ok()
                        && storage.policy_catalog.try_lock().is_ok()
                        && storage.stored_query_dependencies.try_lock().is_ok()
                        && storage.tables[related].lifecycle.try_lock().is_ok()
                        && storage.tables[related].definition.test_write().is_ok();
                    let unrelated = crate::mem::guard::forbid_alloc(|| {
                        storage.create_view(
                            SqlName::parse("public").unwrap(),
                            SqlName::parse("builtin_submission").unwrap(),
                            test_view_definition(StoredQueryDependencies::EMPTY),
                            false,
                            9,
                        )
                    });
                    match movement.identity.slot() {
                        NamedTypeSlot::Domain(slot) => {
                            source.domains[slot].name = movement.current.name
                        }
                        NamedTypeSlot::Enum(slot) => {
                            source.enums[slot].name = movement.current.name
                        }
                        NamedTypeSlot::Composite(slot) => {
                            source.composites[slot].name = movement.current.name
                        }
                    }
                    drop(source);
                    let image = writer.join().unwrap().unwrap();
                    assert!(waiting, "submission did not reach the source wait");
                    assert!(
                        available,
                        "source wait retained a destination or dependency owner"
                    );
                    unrelated.unwrap();
                    let submitted = storage.stored_query_dependencies.lock().unwrap();
                    let entries = submitted.image(
                        image,
                        submitted.image_count(image),
                        storage.stored_query_dependencies_per_image,
                    );
                    assert_eq!(entries[0].identity, identity);
                    assert_eq!(entries[0].name, movement.current.name);
                    assert_eq!(entries[0].referenced_name, movement.prior.name);
                });
            }
        }
    }

    #[test]
    fn saved_query_dependency_wait_releases_source_and_destination_owners() {
        for publication in SAVED_QUERIES {
            let (storage, related, _, movement) = fixture(TypeReferenceKind::Domain);
            let database = storage.current_database_oid();
            let dependency = query_dependency(&storage, movement);
            let dependencies = storage.stored_query_dependencies.lock().unwrap();
            std::thread::scope(|scope| {
                let storage = &storage;
                let writer = scope.spawn(move || {
                    crate::sql::set_execution_database_oid(database);
                    crate::mem::guard::forbid_alloc(|| {
                        publish_saved_query(storage, publication, related, dependency)
                    })
                });
                let waiting = wait_until_contended(storage);
                let available = storage.type_catalog.try_lock().is_ok()
                    && storage.view_rule_catalog.try_lock().is_ok()
                    && storage.matviews.try_lock().is_ok()
                    && storage.policy_catalog.try_lock().is_ok()
                    && storage.tables[related].lifecycle.try_lock().is_ok()
                    && storage.tables[related].definition.test_write().is_ok();
                drop(dependencies);
                writer.join().unwrap().unwrap();
                assert!(
                    waiting && available,
                    "{publication:?} retained earlier owners during dependency wait"
                );
            });
        }
    }

    #[test]
    fn saved_query_recovery_rejects_reused_type_before_changing_live_entries() {
        use super::super::{
            PUBLIC_ROLE, PolicyCommandKind, PolicyDefinition, PolicySpec, RecoveredPolicySpec,
            RuleDefinition, RuleEnabled, RuleTarget, StoredQueryDependencies,
        };
        for kind in KINDS {
            let (mut storage, related, _, movement) = fixture(kind);
            let dependency = query_dependency(&storage, movement);
            let mut dependencies = StoredQueryDependencies::EMPTY;
            dependencies.push(dependency).unwrap();
            let rule = RuleDefinition {
                name: SqlName::parse("recovered_rule").unwrap(),
                target: RuleTarget::Table(related as u16),
                ..RuleDefinition::EMPTY
            };
            let (rule_slot, _) = storage
                .create_rule(rule, dependencies.view(), false, 0)
                .unwrap();
            storage.commit_rule_create(rule_slot);
            let policy = PolicySpec {
                name: SqlName::parse("recovered_policy").unwrap(),
                table: related,
                command: PolicyCommandKind::All,
                permissive: true,
                definition: PolicyDefinition {
                    using: Some(crate::util::StackStr::from_str("true")),
                    with_check: None,
                },
                roles: &[PUBLIC_ROLE],
            };
            let policy_slot = storage.create_policy(policy, &dependencies, 0).unwrap();
            storage.commit_policy_create(policy_slot);
            let replacement = storage.catalog_sequence.next().unwrap();
            {
                let mut source = storage.type_catalog.lock().unwrap();
                match movement.identity.slot() {
                    NamedTypeSlot::Domain(slot) => source.domains[slot].created_at = replacement,
                    NamedTypeSlot::Enum(slot) => source.enums[slot].created_at = replacement,
                    NamedTypeSlot::Composite(slot) => {
                        source.composites[slot].created_at = replacement
                    }
                }
            }
            let created_at = storage.rule(rule_slot).created_at;
            crate::mem::guard::forbid_alloc(|| {
                let mut recovered_dependencies = StoredQueryDependencies::EMPTY;
                recovered_dependencies.push(dependency).unwrap();
                assert_eq!(
                    storage
                        .replay_rule(
                            rule_slot,
                            created_at,
                            RuleDefinition {
                                enabled: RuleEnabled::Always,
                                ..rule
                            },
                            recovered_dependencies
                        )
                        .unwrap_err()
                        .sqlstate,
                    crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                );
                assert_eq!(storage.rule(rule_slot).definition.enabled, rule.enabled);
                assert_eq!(
                    storage
                        .replay_set_policy(
                            RecoveredPolicySpec {
                                name: policy.name,
                                table: related,
                                command: PolicyCommandKind::All,
                                permissive: true,
                                definition: PolicyDefinition {
                                    using: Some(crate::util::StackStr::from_str("false")),
                                    with_check: None
                                },
                            },
                            ["public", "public"].into_iter(),
                            dependencies
                        )
                        .unwrap_err()
                        .sqlstate,
                    crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                );
                assert_eq!(
                    storage.policy(policy_slot).definition.using,
                    policy.definition.using
                );
                assert_eq!(&*storage.policy_roles(policy_slot, 0), &[PUBLIC_ROLE]);
                assert_eq!(
                    storage.rule_dependencies(rule_slot, 0).entries(),
                    &[dependency]
                );
                assert_eq!(
                    storage.policy_dependencies(policy_slot, 0).entries(),
                    &[dependency]
                );
                assert!(
                    storage
                        .stored_query_dependencies
                        .lock()
                        .unwrap()
                        .pending
                        .iter()
                        .all(|image| !image.used)
                );
            });
        }
    }

    #[test]
    fn saved_query_publication_rejects_reused_sources_and_targets_without_partial_state() {
        use crate::sql::eval::sqlstate;
        for kind in KINDS {
            for publication in SAVED_QUERIES {
                for replace_source in [false, true] {
                    if !replace_source && matches!(publication, SavedQueryKind::View) {
                        continue;
                    }
                    let (storage, related, _, movement) = fixture(kind);
                    let database = storage.current_database_oid();
                    let dependency = query_dependency(&storage, movement);
                    let mut source = storage.type_catalog.lock().unwrap();
                    std::thread::scope(|scope| {
                        let storage = &storage;
                        let writer = scope.spawn(move || {
                            crate::sql::set_execution_database_oid(database);
                            crate::mem::guard::forbid_alloc(|| {
                                publish_saved_query(storage, publication, related, dependency)
                            })
                        });
                        let waiting = wait_until_contended(storage);
                        crate::mem::guard::forbid_alloc(|| {
                            let replacement = storage.catalog_sequence.next().unwrap();
                            if replace_source {
                                // Simulate retirement and slot reuse while every earlier owner is free.
                                match movement.identity.slot() {
                                    NamedTypeSlot::Domain(slot) => {
                                        source.domains[slot].created_at = replacement
                                    }
                                    NamedTypeSlot::Enum(slot) => {
                                        source.enums[slot].created_at = replacement
                                    }
                                    NamedTypeSlot::Composite(slot) => {
                                        source.composites[slot].created_at = replacement
                                    }
                                }
                            } else {
                                let _lifecycle =
                                    storage.tables[related].lifecycle.try_lock().unwrap();
                                let mut definition =
                                    storage.tables[related].definition.test_write().unwrap();
                                definition.identity.created_at = replacement;
                            }
                        });
                        drop(source);
                        let error = writer.join().unwrap().unwrap_err();
                        assert!(waiting, "{publication:?} did not reach the source wait");
                        assert_eq!(
                            error.sqlstate,
                            if replace_source {
                                sqlstate::UNDEFINED_OBJECT
                            } else {
                                sqlstate::SERIALIZATION_FAILURE
                            }
                        );
                    });
                    let queries = storage.view_rule_catalog.lock().unwrap();
                    assert!(
                        queries
                            .rules
                            .iter()
                            .all(|rule| rule.ddl_state == CatalogDdlState::Absent)
                    );
                    assert!(
                        queries
                            .views
                            .iter()
                            .all(|view| view.ddl_state == CatalogDdlState::Absent)
                    );
                    assert!(
                        storage
                            .matviews
                            .lock()
                            .unwrap()
                            .iter()
                            .all(|view| view.ddl_state == CatalogDdlState::Absent)
                    );
                    assert!(
                        storage
                            .policy_catalog
                            .lock()
                            .unwrap()
                            .definitions
                            .iter()
                            .all(|policy| policy.ddl_state == CatalogDdlState::Absent)
                    );
                    let dependencies = storage.stored_query_dependencies.lock().unwrap();
                    assert!(dependencies.counts.iter().all(|count| *count == 0));
                    assert!(dependencies.pending.iter().all(|image| !image.used));
                }
            }
        }
    }

    #[test]
    fn waiting_rule_rename_preserves_current_definition_and_releases_pending_image() {
        use super::super::{
            RuleDefinition, RuleEnabled, RuleMetadataChange, RuleTarget, StoredQueryDependencies,
            StoredQueryDependencyOwner,
        };
        for kind in KINDS {
            let (mut storage, related, _, movement) = fixture(kind);
            let database = storage.current_database_oid();
            let mut dependencies = StoredQueryDependencies::EMPTY;
            dependencies
                .push(query_dependency(&storage, movement))
                .unwrap();
            let name = SqlName::parse("original_rule").unwrap();
            let renamed = SqlName::parse("renamed_rule").unwrap();
            let target = RuleTarget::Table(related as u16);
            let (slot, _) = storage
                .create_rule(
                    RuleDefinition {
                        name,
                        target,
                        ..RuleDefinition::EMPTY
                    },
                    dependencies.view(),
                    false,
                    0,
                )
                .unwrap();
            storage.commit_rule_create(slot);
            storage
                .apply_comment(
                    CommentClass::Rule,
                    SqlName::EMPTY,
                    name,
                    target.comment_subid(),
                    Some(comment_stackstr("keep this comment").unwrap()),
                )
                .unwrap();
            let mut source = storage.type_catalog.lock().unwrap();
            std::thread::scope(|scope| {
                let storage = &storage;
                let writer = scope.spawn(move || {
                    crate::sql::set_execution_database_oid(database);
                    crate::mem::guard::forbid_alloc(|| {
                        storage.alter_rule(slot, RuleMetadataChange::Rename(renamed), 7)
                    })
                });
                let waiting = wait_until_contended(storage);
                let available = storage.comments.try_lock().is_ok()
                    && storage.stored_query_dependencies.try_lock().is_ok()
                    && storage.tables[related].lifecycle.try_lock().is_ok()
                    && storage.tables[related].definition.test_write().is_ok();
                storage.view_rule_catalog.lock().unwrap().rules[slot]
                    .definition
                    .enabled = RuleEnabled::Always;
                match movement.identity.slot() {
                    NamedTypeSlot::Domain(slot) => {
                        source.domains[slot].name = movement.current.name
                    }
                    NamedTypeSlot::Enum(slot) => source.enums[slot].name = movement.current.name,
                    NamedTypeSlot::Composite(slot) => {
                        source.composites[slot].name = movement.current.name
                    }
                }
                drop(source);
                let (prior, installed) = writer.join().unwrap().unwrap();
                assert!(waiting && available);
                assert!(prior.is_none());
                assert_eq!(installed.name, renamed);
                assert_eq!(installed.enabled, RuleEnabled::Always);
                let pending = storage.rule(slot).pending.unwrap();
                assert_eq!(pending.definition.name, installed.name);
                assert_eq!(pending.definition.enabled, installed.enabled);
                let image =
                    storage.stored_query_dependency_pending_base + pending.dependency_slot as usize;
                let catalog = storage.stored_query_dependencies.lock().unwrap();
                let entries = catalog.image(
                    image,
                    catalog.image_count(image),
                    storage.stored_query_dependencies_per_image,
                );
                assert_eq!(entries[0].name, movement.current.name);
                assert_eq!(entries[0].referenced_name, movement.prior.name);
            });
            crate::mem::guard::forbid_alloc(|| storage.rollback_rule_alter(slot, None));
            assert!(storage.rule(slot).pending.is_none());
            assert!(
                storage
                    .comment_catalog()
                    .entries
                    .iter()
                    .filter(|comment| comment.used && comment.class == CommentClass::Rule)
                    .all(|comment| comment.name == name && comment.pending_identity.is_none())
            );
            let catalog = storage.stored_query_dependencies.lock().unwrap();
            assert!(catalog.pending.iter().all(|image| !image.used));
            let image =
                storage.committed_dependency_image(StoredQueryDependencyOwner::Rule(slot as u16));
            assert_eq!(catalog.image_count(image), 1);
        }
    }

    #[test]
    fn catalog_submission_rejects_retired_operator_destinations() {
        use super::super::{OperatorDefinition, OperatorFamilyDefinition, OperatorSignature};
        let (storage, _, _, _) = fixture(TypeReferenceKind::Domain);
        let owner = Storage::role_oid(storage.current_role_slot(0).unwrap());
        let definition = OperatorDefinition {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("#@#").unwrap(),
            owner,
            signature: OperatorSignature {
                left: None,
                right: Some(RoutineResult::builtin(ColType::Int4)),
            },
            ..OperatorDefinition::EMPTY
        };
        let operator = storage.create_operator(definition, 0).unwrap();
        storage.commit_operator_create(operator);
        let family_definition = OperatorFamilyDefinition {
            schema: definition.schema,
            name: SqlName::parse("retired_family").unwrap(),
            owner,
            ..OperatorFamilyDefinition::EMPTY
        };
        let family = storage
            .create_operator_family(family_definition, 0)
            .unwrap();
        storage.commit_operator_family_create(family);
        storage.drop_operator(operator, 0);
        storage.commit_operator_drop(operator);
        storage.drop_operator_family(family, 0);
        storage.commit_operator_family_drop(family);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .stage_operator_definition(operator, definition, 7)
                    .unwrap_err()
                    .sqlstate,
                crate::sql::eval::sqlstate::UNDEFINED_OBJECT
            );
            assert_eq!(
                storage
                    .stage_operator_family_definition(family, family_definition, 7)
                    .unwrap_err()
                    .sqlstate,
                crate::sql::eval::sqlstate::UNDEFINED_OBJECT
            );
            let catalog = storage.operator_catalog.lock().unwrap();
            assert!(catalog.operators[operator].pending.is_none());
            assert!(catalog.families[family].pending.is_none());
        });
    }

    #[test]
    fn table_submission_releases_all_owners_before_source_wait() {
        for kind in KINDS {
            for create in [false, true] {
                let (storage, related, _, movement) = fixture(kind);
                let mut definition = *storage.table_def(related, 0);
                definition.name = SqlName::parse("submitted_table").unwrap();
                let database = storage.current_database_oid();
                let source = storage.type_catalog.lock().unwrap();
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::sql::set_execution_database_oid(database);
                        crate::mem::guard::forbid_alloc(|| {
                            if create {
                                storage.create_table_in(definition, 7)
                            } else {
                                storage
                                    .write_table_def(related, 7, definition, &mapping("id"), false)
                                    .map(|()| related)
                            }
                        })
                    });
                    let waiting = wait_until_contended(&storage);
                    let available = storage.table_creation.try_lock().is_ok()
                        && storage.comments.try_lock().is_ok()
                        && storage.tables.iter().all(|table| {
                            table.lifecycle.try_lock().is_ok()
                                && table.definition.test_write().is_ok()
                        });
                    drop(source);
                    let slot = writer.join().unwrap().unwrap();
                    assert!(waiting, "submission never reached the source wait");
                    assert!(available, "source wait retained a table or catalog owner");
                    assert_identity(&storage, slot, 7, movement.prior);
                });
            }
        }
    }

    #[test]
    fn captured_type_references_follow_moves_before_late_table_publication() {
        for kind in KINDS {
            let (storage, table, _, movement) = fixture(kind);
            let mut captured = *storage.table_def(table, 0);
            let originals = [captured.columns[0].user_type, captured.columns[1].user_type];
            let oids = captured
                .columns()
                .iter()
                .map(|column| {
                    storage
                        .routine_type_oid(column.ctype, column.user_type, 0)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            crate::mem::guard::forbid_alloc(|| {
                publish_move(&storage, movement);
                captured.name = SqlName::parse("late_create").unwrap();
                let created = storage.create_table_in(captured, 7).unwrap();
                assert_identity(&storage, created, 7, movement.current);
                for (index, column) in storage.table_def(created, 7).columns().iter().enumerate() {
                    assert_eq!(column.user_type, originals[index]);
                    assert_eq!(
                        storage.routine_type_oid(column.ctype, column.user_type, 7),
                        Some(oids[index])
                    );
                }
                storage
                    .write_table_def(table, 9, captured, &mapping("id"), false)
                    .unwrap();
                assert_identity(&storage, table, 9, movement.current);
                storage.rollback_table_def(table, 9);
                assert_identity(&storage, table, 0, movement.current);
            });
        }
    }

    #[test]
    fn captured_type_dependencies_bind_lifetimes_instead_of_reused_names() {
        use super::super::{
            ColumnSet, SerializedStoredQueryDependency, StoredDependencyIdentity,
            StoredQueryDependencies, StoredQueryDependency,
        };
        for kind in KINDS {
            let (mut storage, _, _, movement) = fixture(kind);
            let class = movement.class();
            let slot = movement.identity.slot().index();
            let identity = storage.capture_type_dependency(class, slot, 0).unwrap();
            let mut captured = StoredQueryDependencies::EMPTY;
            captured
                .push(StoredQueryDependency {
                    class,
                    slot: slot as u16,
                    identity,
                    referenced_columns: ColumnSet::EMPTY,
                    schema: movement.prior.schema,
                    name: movement.prior.name,
                    referenced_schema: movement.prior.schema,
                    referenced_name: movement.prior.name,
                })
                .unwrap();
            let mut saved = StoredQueryDependencies::EMPTY;
            saved
                .serialized_push(SerializedStoredQueryDependency {
                    class,
                    identity: StoredDependencyIdentity::from_storage(
                        class,
                        identity.storage_key().as_str(),
                    )
                    .unwrap(),
                    referenced_columns: ColumnSet::EMPTY,
                    schema: movement.prior.schema,
                    name: movement.prior.name,
                    referenced_schema: movement.prior.schema,
                    referenced_name: movement.prior.name,
                })
                .unwrap();
            publish_move(&storage, movement);
            // Reusing the original SQL spelling must not redirect either a
            // captured dependency or its serialized form to the new object.
            create_kind(&mut storage, kind, movement.prior);
            for dependencies in [&captured, &saved] {
                let bound = storage
                    .rebind_stored_query_dependencies(dependencies.clone(), 0)
                    .unwrap();
                assert_eq!(bound.entries()[0].slot as usize, slot);
                assert_eq!(bound.entries()[0].identity, identity);
                assert_eq!(bound.entries()[0].name, movement.current.name);
                assert_eq!(bound.entries()[0].referenced_name, movement.prior.name);
            }
            match kind {
                TypeReferenceKind::Domain => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Domain,
                            movement.current.schema.as_str(),
                            movement.current.name.as_str(),
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Enum => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Enum,
                            movement.current.schema.as_str(),
                            movement.current.name.as_str(),
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Composite => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Composite,
                            movement.current.schema.as_str(),
                            movement.current.name.as_str(),
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
            }
            assert_eq!(create_kind(&mut storage, kind, movement.current).0, slot);
            for dependencies in [captured, saved] {
                assert_eq!(
                    storage
                        .rebind_stored_query_dependencies(dependencies, 0)
                        .unwrap_err()
                        .sqlstate,
                    crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                );
            }
        }
    }

    #[test]
    fn captured_type_references_reject_retired_and_reused_source_lifetimes() {
        for kind in KINDS {
            let (mut storage, table, _, movement) = fixture(kind);
            let captured = *storage.table_def(table, 0);
            let key = movement.identity;
            match kind {
                TypeReferenceKind::Domain => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Domain,
                            "public",
                            "prior_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Enum => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Enum,
                            "public",
                            "prior_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Composite => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Composite,
                            "public",
                            "prior_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
            }
            for replace in [false, true] {
                if replace {
                    assert_eq!(
                        create_kind(&mut storage, kind, movement.prior).0,
                        key.slot().index()
                    );
                }
                crate::mem::guard::forbid_alloc(|| {
                    for column in captured.columns() {
                        let result = RoutineResult {
                            ctype: column.ctype,
                            user_type: column.user_type,
                        };
                        assert_eq!(
                            storage.bind_routine_result(result, 0).unwrap_err().sqlstate,
                            crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                        );
                        assert!(
                            storage
                                .routine_type_oid(result.ctype, result.user_type, 0)
                                .is_none()
                        );
                        assert!(
                            storage
                                .user_type_identity_oid(result.user_type.unwrap(), false, 0)
                                .is_none()
                        );
                    }
                    let mut submitted = captured;
                    submitted.name = SqlName::parse("invalid_create").unwrap();
                    assert_eq!(
                        storage.create_table_in(submitted, 7).unwrap_err().sqlstate,
                        crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                    );
                    assert_eq!(
                        storage
                            .write_table_def(table, 7, submitted, &mapping("id"), false)
                            .unwrap_err()
                            .sqlstate,
                        crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                    );
                    assert!(
                        storage
                            .find_visible("public", "invalid_create", 7)
                            .is_none()
                    );
                    assert!(storage.pending_table_def(table).is_none());
                });
            }
        }
    }

    #[test]
    fn captured_type_references_reject_other_catalogs_and_invalid_representations() {
        for kind in KINDS {
            let (mut storage, table, _, _) = fixture(kind);
            let (other, _, _, _) = fixture(kind);
            let definition = *storage.table_def(table, 0);
            crate::mem::guard::forbid_alloc(|| {
                for column in definition.columns() {
                    let result = RoutineResult {
                        ctype: column.ctype,
                        user_type: column.user_type,
                    };
                    assert_eq!(
                        other.bind_routine_result(result, 0).unwrap_err().sqlstate,
                        crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                    );
                    assert_eq!(
                        storage
                            .bind_routine_result(
                                RoutineResult {
                                    ctype: ColType::Int8,
                                    user_type: result.user_type
                                },
                                0
                            )
                            .unwrap_err()
                            .sqlstate,
                        crate::sql::eval::sqlstate::DATATYPE_MISMATCH
                    );
                }
            });
            storage.select_database(DatabaseOid::TEMPLATE1).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                for column in definition.columns() {
                    assert_eq!(
                        storage
                            .bind_routine_result(
                                RoutineResult {
                                    ctype: column.ctype,
                                    user_type: column.user_type,
                                },
                                0
                            )
                            .unwrap_err()
                            .sqlstate,
                        crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                    );
                }
            });
            storage.select_database(DatabaseOid::POSTGRES).unwrap();
        }
    }

    #[test]
    fn captured_type_references_keep_persisted_keys_through_moves_and_reuse() {
        for kind in KINDS {
            let (mut storage, table, _, movement) = fixture(kind);
            let column = storage.table_def(table, 0).columns()[0];
            let captured = column.user_type.unwrap();
            let encoded = captured.storage_name();
            let stored = UserTypeName::from_storage(captured.schema, encoded.as_str()).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                publish_move(&storage, movement);
                let rebound = storage
                    .bind_routine_result(
                        RoutineResult {
                            ctype: column.ctype,
                            user_type: Some(stored),
                        },
                        0,
                    )
                    .unwrap();
                assert_eq!(rebound.user_type, Some(captured));
                assert!(
                    rebound
                        .user_type
                        .unwrap()
                        .is_named(movement.current.schema, movement.current.name)
                );
            });
            match kind {
                TypeReferenceKind::Domain => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Domain,
                            "public",
                            "current_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Enum => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Enum,
                            "public",
                            "current_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
                TypeReferenceKind::Composite => {
                    let dropped = storage
                        .stage_type_drop(
                            crate::storage::NamedTypeKind::Composite,
                            "public",
                            "current_type",
                            0,
                        )
                        .unwrap()
                        .unwrap();
                    storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
                }
            }
            assert_eq!(
                create_kind(&mut storage, kind, movement.current).0,
                movement.identity.slot().index()
            );
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    storage
                        .bind_routine_result(
                            RoutineResult {
                                ctype: column.ctype,
                                user_type: Some(stored)
                            },
                            0
                        )
                        .unwrap_err()
                        .sqlstate,
                    crate::sql::eval::sqlstate::UNDEFINED_OBJECT
                );
            });
        }
    }

    #[test]
    fn type_publication_workspace_exact_startup_budget() {
        for max_tables in [1, 3, 128] {
            let mut config = test_config();
            config.max_tables = max_tables;
            let bytes = TypePublication::budget_bytes(&config);
            let mut insufficient = Budget::new(bytes - 1);
            let error = TypePublication::new(&config, &mut insufficient)
                .err()
                .unwrap();
            assert_eq!(error.what, "type publication");
            let mut exact = Budget::new(bytes);
            let publication = TypePublication::new(&config, &mut exact).unwrap();
            assert_eq!(exact.remaining(), 0);
            crate::mem::guard::forbid_alloc(|| {
                for _ in 0..3 {
                    let mut scratch = publication.scratch.lock().unwrap();
                    scratch.reset();
                    let owners = scratch
                        .alloc_owned_slice_with::<Option<TableOwner<'_>>>(
                            super::super::table_slot_capacity(&config),
                            |_| None,
                        )
                        .unwrap();
                    assert_eq!(owners.len(), max_tables + 1);
                }
            });
        }
    }

    #[test]
    fn shared_type_move_catalog_wait_releases_every_table_and_catalog_prefix() {
        for kind in KINDS {
            for blocked in 0..7 {
                let (storage, related, _, movement) = fixture(kind);
                let generation = storage.tables[related].generation();
                let types = (blocked == 0).then(|| storage.type_catalog.lock().unwrap());
                let members = (blocked == 1).then(|| storage.enum_member_images.lock().unwrap());
                let comments = (blocked == 2).then(|| storage.comments.lock().unwrap());
                let routines = (blocked == 3).then(|| storage.routines.lock().unwrap());
                let operators = (blocked == 4).then(|| storage.operator_catalog.lock().unwrap());
                let casts = (blocked == 5).then(|| storage.casts.lock().unwrap());
                let dependencies =
                    (blocked == 6).then(|| storage.stored_query_dependencies.lock().unwrap());
                std::thread::scope(|scope| {
                    let writer = scope.spawn(|| {
                        crate::mem::guard::forbid_alloc(|| {
                            publish_move(&storage, movement);
                        })
                    });
                    let waiting = wait_until_contended(&storage);
                    // Try every prefix before releasing the injected blocker.
                    // A regression must fail without stranding the writer.
                    let prefix_free = crate::mem::guard::forbid_alloc(|| {
                        storage.tables[related].definition.test_write().is_ok()
                            && storage.tables[related].lifecycle.try_lock().is_ok()
                            && storage.tables[related].publication.try_lock().is_ok()
                            && (blocked == 0 || storage.type_catalog.try_lock().is_ok())
                            && (blocked <= 1 || storage.enum_member_images.try_lock().is_ok())
                            && (blocked <= 2 || storage.comments.try_lock().is_ok())
                            && (blocked <= 3 || storage.routines.try_lock().is_ok())
                            && (blocked <= 4 || storage.operator_catalog.try_lock().is_ok())
                            && (blocked <= 5 || storage.casts.try_lock().is_ok())
                    });
                    let unchanged = if prefix_free {
                        let references_unchanged = storage
                            .table_def(related, 0)
                            .columns()
                            .iter()
                            .all(|column| {
                                name_of(column.user_type) == name_of(Some(movement.prior))
                            });
                        let source = types.as_ref().map_or_else(
                            || source_identity(&storage.type_catalog.lock().unwrap(), movement),
                            |catalog| source_identity(catalog, movement),
                        );
                        references_unchanged
                            && source == movement.prior
                            && storage.tables[related].generation() == generation
                    } else {
                        false
                    };
                    drop((
                        types,
                        members,
                        comments,
                        routines,
                        operators,
                        casts,
                        dependencies,
                    ));
                    writer.join().unwrap();
                    assert!(
                        waiting,
                        "publication never reached the injected catalog wait"
                    );
                    assert!(prefix_free, "catalog wait retained an earlier owner");
                    assert!(
                        unchanged,
                        "publication changed state before acquiring every owner"
                    );
                });
                assert_identity(&storage, related, 0, movement.current);
                assert_eq!(
                    source_identity(&storage.type_catalog.lock().unwrap(), movement),
                    movement.current
                );
                assert_eq!(storage.tables[related].generation(), generation + 1);
            }
        }
    }

    #[test]
    fn shared_type_move_later_table_wait_releases_earlier_definitions() {
        for kind in KINDS {
            let (mut storage, first, _, movement) = fixture(kind);
            let mut definition = *storage.table_def(first, 0);
            definition.name = SqlName::parse("last_dependency").unwrap();
            let last = storage.create_table(definition).unwrap();
            let held = storage.table_def(last, 0);
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    crate::mem::guard::forbid_alloc(|| {
                        publish_move(&storage, movement);
                    })
                });
                let waiting = wait_until_contended(&storage);
                let prefix_free = storage.tables[first].definition.test_write().is_ok()
                    && storage.tables[first].lifecycle.try_lock().is_ok()
                    && storage.tables[first].publication.try_lock().is_ok()
                    && storage.tables[last].lifecycle.try_lock().is_ok();
                let source = source_identity(&storage.type_catalog.lock().unwrap(), movement);
                drop(held);
                writer.join().unwrap();
                assert!(waiting);
                assert!(prefix_free, "later-table wait retained an earlier table");
                assert_eq!(source, movement.prior);
            });
            assert_identity(&storage, first, 0, movement.current);
            assert_identity(&storage, last, 0, movement.current);
        }
    }

    #[test]
    fn shared_type_move_catalog_wait_reselects_new_pending_references() {
        for kind in KINDS {
            let (storage, related, unrelated, movement) = fixture(kind);
            let mut pending = *storage.table_def(related, 0);
            pending.name = storage.table_def(unrelated, 0).name;
            let blocker = storage.casts.lock().unwrap();
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    crate::mem::guard::forbid_alloc(|| {
                        publish_move(&storage, movement);
                    })
                });
                let waiting = wait_until_contended(&storage);
                let available = storage.tables[unrelated].definition.test_write().is_ok()
                    && storage.tables[unrelated].lifecycle.try_lock().is_ok()
                    && storage.comments.try_lock().is_ok();
                let staged = if waiting && available {
                    crate::mem::guard::forbid_alloc(|| {
                        storage.write_table_def(unrelated, 7, pending, &mapping("id"), false)
                    })
                    .is_ok()
                } else {
                    false
                };
                drop(blocker);
                writer.join().unwrap();
                assert!(waiting);
                assert!(available);
                assert!(staged);
            });
            crate::mem::guard::forbid_alloc(|| {
                assert_identity(&storage, unrelated, 7, movement.current);
                assert!(
                    storage.table_def(unrelated, 0).columns()[0]
                        .user_type
                        .is_none()
                );
                storage.rollback_table_def(unrelated, 7);
                assert!(
                    storage.table_def(unrelated, 7).columns()[0]
                        .user_type
                        .is_none()
                );
                assert!(storage.pending_table_defs.test_write().is_ok());
            });
        }
    }

    fn publish_move(storage: &Storage, movement: TypeReferenceMove) {
        let source = TypePublicationSource::committed(
            &storage.type_catalog.lock().unwrap(),
            movement.identity.slot(),
        )
        .unwrap();
        storage
            .type_publication
            .publish(
                storage,
                movement,
                source,
                |catalog, _members| match movement.identity.slot() {
                    NamedTypeSlot::Domain(_) => {
                        catalog.domains[movement.identity.slot().index()].schema =
                            movement.current.schema;
                        catalog.domains[movement.identity.slot().index()].name =
                            movement.current.name;
                    }
                    NamedTypeSlot::Enum(_) => {
                        catalog.enums[movement.identity.slot().index()].schema =
                            movement.current.schema;
                        catalog.enums[movement.identity.slot().index()].name =
                            movement.current.name;
                    }
                    NamedTypeSlot::Composite(_) => {
                        catalog.composites[movement.identity.slot().index()].schema =
                            movement.current.schema;
                        catalog.composites[movement.identity.slot().index()].name =
                            movement.current.name;
                    }
                },
            )
            .unwrap();
    }

    fn assert_identity(storage: &Storage, table: usize, transaction: u32, expected: UserTypeName) {
        for column in storage.table_def(table, transaction).columns() {
            assert_eq!(name_of(column.user_type), name_of(Some(expected)));
            assert!(matches!(
                column.user_type.map(|name| name.binding),
                Some(TypeBinding::Captured(_))
            ));
        }
    }

    fn name_of(name: Option<UserTypeName>) -> Option<(SqlName, SqlName)> {
        name.map(|name| (name.schema, name.name))
    }

    #[test]
    fn shared_type_move_skips_unrelated_retained_definitions() {
        for kind in KINDS {
            let (storage, related, unrelated, movement) = fixture(kind);
            let held = storage.table_def(unrelated, 0);
            let generation = storage.tables[unrelated].generation();
            let (send, receive) = mpsc::sync_channel(1);
            std::thread::scope(|scope| {
                let storage = &storage;
                let writer = scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        publish_move(storage, movement);
                    });
                    send.send(()).unwrap();
                });
                let progress = receive.recv_timeout(Duration::from_secs(5));
                drop(held);
                writer.join().unwrap();
                assert!(
                    progress.is_ok(),
                    "an unrelated definition blocked a type move"
                );
            });
            crate::mem::guard::forbid_alloc(|| {
                assert_identity(&storage, related, 0, movement.current);
                assert_eq!(storage.tables[unrelated].generation(), generation);
                assert_eq!(
                    storage
                        .comment_text(
                            CommentClass::Type,
                            movement.current.schema.as_str(),
                            movement.current.name.as_str(),
                            0,
                            0,
                        )
                        .unwrap()
                        .as_str(),
                    "type reference ownership"
                );
            });
        }
    }

    #[test]
    fn shared_type_move_wait_keeps_unrelated_edits_and_catalogs_available() {
        for kind in KINDS {
            for pending_reader in [false, true] {
                let (storage, related, unrelated, movement) = fixture(kind);
                for name in ["first_id", "second_id"] {
                    let mut definition = *storage.table_def(related, 7);
                    definition.columns[0].name = SqlName::parse(name).unwrap();
                    storage
                        .write_table_def(related, 7, definition, &mapping(name), false)
                        .unwrap();
                }
                let held = storage.table_def(related, if pending_reader { 7 } else { 0 });
                let (done_send, done_receive) = mpsc::sync_channel(1);
                let (other_send, other_receive) = mpsc::sync_channel(1);
                std::thread::scope(|scope| {
                    let storage = &storage;
                    let writer = scope.spawn(move || {
                        crate::mem::guard::forbid_alloc(|| {
                            publish_move(storage, movement);
                        });
                        done_send.send(()).unwrap();
                    });
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let waiting = loop {
                        if storage
                            .type_publication
                            .waits
                            .load(std::sync::atomic::Ordering::Acquire)
                            > 0
                        {
                            break true;
                        }
                        if Instant::now() >= deadline {
                            break false;
                        }
                        std::thread::yield_now();
                    };
                    let other = scope.spawn(move || {
                        let progress = crate::mem::guard::forbid_alloc(|| {
                            if storage.type_catalog.try_lock().is_err()
                                || storage.routines.try_lock().is_err()
                                || storage.operator_catalog.try_lock().is_err()
                                || storage.casts.try_lock().is_err()
                            {
                                return false;
                            }
                            if storage.tables[related].lifecycle.try_lock().is_err() {
                                return false;
                            }
                            let catalog = storage.type_catalog.lock().unwrap();
                            let source = source_identity(&catalog, movement);
                            drop(catalog);
                            if source != movement.prior {
                                return false;
                            }
                            for _ in 0..16 {
                                let mut definition = *storage.table_def(unrelated, 9);
                                definition.name = SqlName::parse("other_pending").unwrap();
                                storage
                                    .write_table_def(
                                        unrelated,
                                        9,
                                        definition,
                                        &mapping("id"),
                                        false,
                                    )
                                    .unwrap();
                                storage.rollback_table_def(unrelated, 9);
                            }
                            storage
                                .apply_comment(
                                    CommentClass::Relation,
                                    SqlName::parse("public").unwrap(),
                                    SqlName::parse("unrelated").unwrap(),
                                    0,
                                    Some(comment_stackstr("available during a type move").unwrap()),
                                )
                                .unwrap();
                            true
                        });
                        other_send.send(progress).unwrap();
                    });
                    let progress = other_receive.recv_timeout(Duration::from_secs(5));
                    let completed_early = done_receive.try_recv().is_ok();
                    let retained_identity = held.columns()[0].user_type;
                    drop(held);
                    writer.join().unwrap();
                    other.join().unwrap();
                    assert_eq!(name_of(retained_identity), name_of(Some(movement.prior)));
                    assert!(waiting, "the move did not reach its retained definition");
                    assert!(
                        !completed_early,
                        "a type move changed a retained definition"
                    );
                    assert_eq!(
                        progress,
                        Ok(true),
                        "a type move reserved unrelated resources"
                    );
                });
                crate::mem::guard::forbid_alloc(|| {
                    assert_identity(&storage, related, 0, movement.current);
                    assert_identity(&storage, related, 7, movement.current);
                    assert_eq!(
                        storage.table_def(related, 7).columns()[0].name.as_str(),
                        "second_id"
                    );
                    storage.rollback_table_def(related, 7);
                    assert_identity(&storage, related, 7, movement.current);
                    assert_eq!(
                        storage.table_def(related, 7).columns()[0].name.as_str(),
                        "first_id"
                    );
                    storage.rollback_table_def(related, 7);
                    assert_identity(&storage, related, 0, movement.current);
                    assert!(storage.pending_table_def(related).is_none());
                    assert!(storage.pending_table_defs.test_write().is_ok());
                });
            }
        }
    }

    #[test]
    fn shared_type_move_updates_references_found_only_in_pending_versions() {
        for kind in KINDS {
            let (mut storage, related, _, movement) = fixture(kind);
            let pending = *storage.table_def(related, 0);
            {
                let mut definition = storage.tables[related].definition_mut();
                definition.columns[0].ctype = ColType::Int4;
                definition.columns[1].ctype = ColType::Array(ArrElem::Int4);
                for column in definition.columns.iter_mut().take(2) {
                    column.user_type = None;
                }
            }
            storage
                .write_table_def(related, 7, pending, &mapping("id"), false)
                .unwrap();
            crate::mem::guard::forbid_alloc(|| {
                publish_move(&storage, movement);
                assert_identity(&storage, related, 7, movement.current);
                assert!(
                    storage
                        .table_def(related, 0)
                        .columns()
                        .iter()
                        .all(|column| column.user_type.is_none())
                );
                storage.rollback_table_def(related, 7);
                assert!(
                    storage
                        .table_def(related, 0)
                        .columns()
                        .iter()
                        .all(|column| column.user_type.is_none())
                );
                assert!(storage.pending_table_defs.test_write().is_ok());
            });
        }
    }

    #[test]
    fn shared_type_move_skips_foreign_and_retired_slots_before_reuse() {
        for kind in KINDS {
            let (mut storage, related, _, movement) = fixture(kind);
            let retired = storage
                .create_table({
                    let mut definition = *storage.table_def(related, 0);
                    definition.name = SqlName::parse("retired").unwrap();
                    definition
                })
                .unwrap();
            let retired_stamp = storage.table(retired).created_at();
            storage.select_database(DatabaseOid::TEMPLATE1).unwrap();
            let (_, scalar, array) = create_kind(&mut storage, kind, movement.prior);
            let mut definition = make_def(
                "foreign_type_refs",
                &[("id", scalar, false), ("items", array, false)],
            );
            for column in definition.columns.iter_mut().take(2) {
                column.user_type = Some(UserTypeName::declaration(
                    movement.prior.schema,
                    movement.prior.name,
                ));
            }
            let foreign = storage.create_table(definition).unwrap();
            storage.select_database(DatabaseOid::POSTGRES).unwrap();
            storage.drop_table(retired);
            let retired_view = storage.table_def(retired, 0);
            let foreign_view = storage.table_def(foreign, 0);
            let (send, receive) = mpsc::sync_channel(1);
            std::thread::scope(|scope| {
                let storage = &storage;
                let writer = scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        publish_move(storage, movement);
                    });
                    send.send(()).unwrap();
                });
                let progress = receive.recv_timeout(Duration::from_secs(5));
                let retired_identity = retired_view.columns()[0].user_type;
                let foreign_identity = foreign_view.columns()[0].user_type;
                drop(retired_view);
                drop(foreign_view);
                writer.join().unwrap();
                assert_eq!(name_of(retired_identity), name_of(Some(movement.prior)));
                assert_eq!(name_of(foreign_identity), name_of(Some(movement.prior)));
                assert!(progress.is_ok(), "an excluded table blocked a type move");
            });
            assert_identity(&storage, related, 0, movement.current);
            let replacement = storage
                .create_table(make_def("replacement", &[("id", ColType::Int4, false)]))
                .unwrap();
            assert_eq!(replacement, retired);
            assert!(storage.table(replacement).created_at() > retired_stamp);
            assert!(
                storage.table_def(replacement, 0).columns()[0]
                    .user_type
                    .is_none()
            );
        }
    }
    #[test]
    fn enum_value_identity_type_moves_preserve_outer_domains() {
        let identity = |name| {
            UserTypeName::declaration(
                super::super::SqlName::parse("public").unwrap(),
                super::super::SqlName::parse(name).unwrap(),
            )
        };
        let prior = identity("prior");
        let current = identity("current");
        let outer = identity("outer");
        for (kind, ctype) in [
            (TypeReferenceKind::Enum, ColType::Enum(0)),
            (TypeReferenceKind::Enum, ColType::Array(ArrElem::Enum(0))),
            (
                TypeReferenceKind::Enum,
                ColType::Enum(ColType::ENUM_SLOT_UNRESOLVED),
            ),
            (
                TypeReferenceKind::Enum,
                ColType::Array(ArrElem::Enum(ColType::ENUM_SLOT_UNRESOLVED)),
            ),
            (TypeReferenceKind::Composite, ColType::Composite(0)),
            (
                TypeReferenceKind::Composite,
                ColType::Array(ArrElem::Composite(0)),
            ),
            (
                TypeReferenceKind::Composite,
                ColType::Composite(ColType::COMPOSITE_SLOT_UNRESOLVED),
            ),
            (
                TypeReferenceKind::Composite,
                ColType::Array(ArrElem::Composite(ColType::COMPOSITE_SLOT_UNRESOLVED)),
            ),
            (
                TypeReferenceKind::Domain,
                ColType::Array(ArrElem::domain(0, ColType::Enum(0)).unwrap()),
            ),
        ] {
            let movement = TypeReferenceMove::new(
                TypeIdentity::new(
                    super::super::type_references::TypeOrigin::new().unwrap(),
                    DatabaseOid::POSTGRES,
                    named_slot(kind, 0),
                    1,
                ),
                prior,
                current,
            );
            let mut declared = None;
            assert!(!movement.rewrite(ctype, &mut declared));
            assert_eq!(declared, None);
            let mut declared = Some(outer);
            assert!(!movement.rewrite(ctype, &mut declared));
            assert_eq!(declared, Some(outer));
            declared = Some(prior);
            assert!(movement.rewrite(ctype, &mut declared));
            assert_eq!(declared, Some(current));
            let mut parent = None;
            let mut base = Some(prior);
            movement.rewrite_domain(ctype, &mut parent, &mut base);
            assert_eq!(base, Some(current));
            parent = Some(outer);
            base = None;
            movement.rewrite_domain(ctype, &mut parent, &mut base);
            assert_eq!(parent, Some(outer));
            assert_eq!(base, None);
        }
        for (kind, ctype) in [
            (TypeReferenceKind::Enum, ColType::Enum(1)),
            (TypeReferenceKind::Enum, ColType::Array(ArrElem::Enum(1))),
            (TypeReferenceKind::Composite, ColType::Composite(1)),
            (
                TypeReferenceKind::Composite,
                ColType::Array(ArrElem::Composite(1)),
            ),
        ] {
            let movement = TypeReferenceMove::new(
                TypeIdentity::new(
                    super::super::type_references::TypeOrigin::new().unwrap(),
                    DatabaseOid::POSTGRES,
                    named_slot(kind, 0),
                    1,
                ),
                prior,
                current,
            );
            let mut declared = Some(prior);
            assert!(!movement.rewrite(ctype, &mut declared));
            assert_eq!(declared, Some(prior));
        }
    }
}
