//! A type move updates only references belonging to its owning database.

use core::mem::{align_of, size_of};
use std::sync::{Mutex, MutexGuard};

use super::row_ownership::{LockWait, try_mutex};
use super::table_definitions::TableIdentity;
use super::table_names::DefinitionWrite;
use super::{
    ArrElem, CatalogDdlState, ColType, CommentClass, DatabaseOid, DependencyClass,
    EnumMemberImages, OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition,
    RoutineArgumentDef, RoutineKind, RoutineResult, Storage, TableDef, TablePublication,
    TypeCatalog, UserTypeName,
};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};

#[derive(Clone, Copy)]
pub(super) enum TypeReferenceKind {
    Domain,
    Enum,
    Composite,
}

#[derive(Clone, Copy)]
pub(super) struct TypeReferenceMove {
    database: DatabaseOid,
    kind: TypeReferenceKind,
    slot: usize,
    prior: UserTypeName,
    current: UserTypeName,
}

impl TypeReferenceMove {
    pub(super) fn new(
        database: DatabaseOid,
        kind: TypeReferenceKind,
        slot: usize,
        prior: UserTypeName,
        current: UserTypeName,
    ) -> Self {
        Self {
            database,
            kind,
            slot,
            prior,
            current,
        }
    }

    pub(super) fn applies_to(self, identity: TableIdentity) -> bool {
        identity.database == self.database && identity.existence != CatalogDdlState::Absent
    }

    fn matches(self, ctype: ColType, identity: Option<UserTypeName>) -> bool {
        match self.kind {
            TypeReferenceKind::Domain => identity == Some(self.prior),
            // Journal declarations retain names before startup resolves slots.
            // An explicit unresolved slot is valid only with the exact prior name.
            TypeReferenceKind::Enum => {
                identity == Some(self.prior)
                    && matches!(ctype, ColType::Enum(slot) | ColType::Array(ArrElem::Enum(slot))
                        if usize::from(slot) == self.slot || slot == ColType::ENUM_SLOT_UNRESOLVED)
            }
            TypeReferenceKind::Composite => {
                identity == Some(self.prior)
                    && matches!(ctype, ColType::Composite(slot) | ColType::Array(ArrElem::Composite(slot))
                        if usize::from(slot) == self.slot || slot == ColType::COMPOSITE_SLOT_UNRESOLVED)
            }
        }
    }

    fn rewrite(self, ctype: ColType, identity: &mut Option<UserTypeName>) -> bool {
        let matches = self.matches(ctype, *identity);
        if matches {
            *identity = Some(self.current);
        }
        matches
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
        if matches!(self.kind, TypeReferenceKind::Domain) {
            if *parent == Some(self.prior) {
                *parent = Some(self.current);
            }
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
        match self.kind {
            TypeReferenceKind::Domain => DependencyClass::Domain,
            TypeReferenceKind::Enum => DependencyClass::Enum,
            TypeReferenceKind::Composite => DependencyClass::Composite,
        }
    }
}

/// Only type-identity publication uses this startup workspace. Ordinary reads
/// and writes do not acquire its mutex.
pub(super) struct TypePublication {
    scratch: Mutex<Arena>,
    #[cfg(test)]
    waits: std::sync::atomic::AtomicUsize,
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
        })
    }

    fn acquire_tables<'storage>(
        storage: &'storage Storage,
        movement: TypeReferenceMove,
        owners: &mut TableAttempt<'_, 'storage>,
    ) -> Result<(), LockWait<'storage>> {
        for (slot, table) in storage.tables.iter().enumerate() {
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

    // The public caller requires &mut Storage: a by-value definition built
    // before this move must not be submitted afterward with its old type name.
    // This internal shared operation establishes the publication boundary; it
    // does not make earlier reference construction safe for parallel writers.
    fn publish(
        &self,
        storage: &Storage,
        movement: TypeReferenceMove,
        install_source: impl FnOnce(&mut TypeCatalog, &mut EnumMemberImages),
    ) {
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
        let mut install_source = Some(install_source);
        loop {
            let mut owners = TableAttempt { slots: &mut slots };
            let attempt = (|| {
                Self::acquire_tables(storage, movement, &mut owners)?;
                let types = try_mutex(&storage.type_catalog, "type catalog lock poisoned")?;
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
                Ok((
                    types,
                    members,
                    comments,
                    routines,
                    operators,
                    casts,
                    dependencies,
                ))
            })();
            match attempt {
                Err(wait) => {
                    drop(owners);
                    #[cfg(test)]
                    self.waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                }
                Ok((
                    mut types,
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
                        owner
                            .publication
                            .generation
                            .checked_add(1)
                            .expect("table publication generation exhausted");
                    }
                    install_source.take().expect("source installs once")(&mut types, &mut members);
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
                            && comment.database == Some(movement.database)
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
                        movement.slot,
                        None,
                        movement.current.schema,
                        movement.current.name,
                    );
                    return;
                }
            }
        }
    }
}

impl Storage {
    pub(super) fn publish_type_move(
        &mut self,
        movement: TypeReferenceMove,
        install_source: impl FnOnce(&mut TypeCatalog, &mut EnumMemberImages),
    ) {
        self.type_publication
            .publish(self, movement, install_source);
    }
}

impl TypeReferenceMove {
    fn rewrite_catalog(self, catalog: &mut TypeCatalog) {
        for composite in catalog.composites.iter_mut().filter(|composite| {
            composite.database == self.database && composite.ddl_state != CatalogDdlState::Absent
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
            domain.database == self.database && domain.ddl_state != CatalogDdlState::Absent
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
            operator.database == self.database && operator.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_operator(&mut operator.definition);
            if let Some(pending) = &mut operator.pending {
                self.rewrite_operator(&mut pending.definition);
            }
        }
        for family in catalog.families.iter_mut().filter(|family| {
            family.database == self.database && family.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_family(&mut family.definition);
            if let Some(pending) = &mut family.pending {
                self.rewrite_family(&mut pending.definition);
            }
        }
        for class in catalog.classes.iter_mut().filter(|class| {
            class.database == self.database && class.ddl_state != CatalogDdlState::Absent
        }) {
            self.rewrite_class(&mut class.definition);
            if let Some(pending) = &mut class.pending {
                self.rewrite_class(&mut pending.definition);
            }
        }
    }

    fn rewrite_casts(self, casts: &mut crate::mem::fixed_vec::FixedVec<super::CastDef>) {
        for cast in casts.iter_mut().filter(|cast| {
            cast.database == self.database && cast.ddl_state != CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Cast(cast)
                .for_each(|result| self.rewrite_result(result));
        }
    }

    fn rewrite_routines(self, routines: &mut crate::mem::fixed_vec::FixedVec<super::RoutineDef>) {
        let move_argument = |argument: &mut RoutineArgumentDef| {
            self.rewrite(argument.ctype, &mut argument.user_type);
        };
        let move_result = |result: &mut RoutineResult| {
            self.rewrite_result(result);
        };
        for routine in routines
            .iter_mut()
            .filter(|routine| routine.database == self.database)
        {
            for argument in routine.arguments.iter_mut().take(routine.argument_count) {
                move_argument(argument);
            }
            for column in routine
                .result_columns
                .iter_mut()
                .take(routine.result_column_count)
            {
                move_argument(column);
            }
            for parameter in routine.parameters.iter_mut().take(routine.parameter_count) {
                let mut argument = RoutineArgumentDef {
                    name: parameter.name,
                    ctype: parameter.ctype,
                    user_type: parameter.user_type,
                };
                move_argument(&mut argument);
                parameter.user_type = argument.user_type;
            }
            match &mut routine.kind {
                RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                    move_result(result)
                }
                RoutineKind::Aggregate(aggregate) => {
                    move_result(&mut aggregate.state_type);
                    move_result(&mut aggregate.result_type);
                    if let Some(moving) = &mut aggregate.moving {
                        move_result(&mut moving.state_type);
                    }
                }
                _ => {}
            }
            if let Some(pending) = &mut routine.pending_definition {
                for argument in pending.arguments.iter_mut().take(pending.argument_count) {
                    move_argument(argument);
                }
                for column in pending
                    .result_columns
                    .iter_mut()
                    .take(pending.result_column_count)
                {
                    move_argument(column);
                }
                for parameter in pending.parameters.iter_mut().take(pending.parameter_count) {
                    let mut argument = RoutineArgumentDef {
                        name: parameter.name,
                        ctype: parameter.ctype,
                        user_type: parameter.user_type,
                    };
                    move_argument(&mut argument);
                    parameter.user_type = argument.user_type;
                }
                match &mut pending.kind {
                    RoutineKind::Function { result } | RoutineKind::SetFunction { result } => {
                        move_result(result)
                    }
                    RoutineKind::Aggregate(aggregate) => {
                        move_result(&mut aggregate.state_type);
                        move_result(&mut aggregate.result_type);
                        if let Some(moving) = &mut aggregate.moving {
                            move_result(&mut moving.state_type);
                        }
                    }
                    _ => {}
                }
            }
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
                    .unwrap();
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
                    .unwrap();
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
                    .unwrap();
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
        let prior = UserTypeName {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("prior_type").unwrap(),
        };
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
        let movement = TypeReferenceMove::new(
            DatabaseOid::POSTGRES,
            kind,
            slot,
            prior,
            UserTypeName {
                schema: prior.schema,
                name: SqlName::parse("current_type").unwrap(),
            },
        );
        (storage, related, unrelated, movement)
    }

    fn mapping(name: &str) -> [Option<SqlName>; MAX_COLUMNS] {
        let mut result = [None; MAX_COLUMNS];
        result[0] = Some(SqlName::parse(name).unwrap());
        result[1] = Some(SqlName::parse("items").unwrap());
        result
    }

    fn source_identity(catalog: &TypeCatalog, movement: TypeReferenceMove) -> UserTypeName {
        let (schema, name) = match movement.kind {
            TypeReferenceKind::Domain => {
                let definition = catalog.domains[movement.slot];
                (definition.schema, definition.name)
            }
            TypeReferenceKind::Enum => {
                let definition = catalog.enums[movement.slot];
                (definition.schema, definition.name)
            }
            TypeReferenceKind::Composite => {
                let definition = catalog.composites[movement.slot];
                (definition.schema, definition.name)
            }
        };
        UserTypeName { schema, name }
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
                            .all(|column| column.user_type == Some(movement.prior));
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
        storage
            .type_publication
            .publish(storage, movement, |catalog, _members| match movement.kind {
                TypeReferenceKind::Domain => {
                    catalog.domains[movement.slot].schema = movement.current.schema;
                    catalog.domains[movement.slot].name = movement.current.name;
                }
                TypeReferenceKind::Enum => {
                    catalog.enums[movement.slot].schema = movement.current.schema;
                    catalog.enums[movement.slot].name = movement.current.name;
                }
                TypeReferenceKind::Composite => {
                    catalog.composites[movement.slot].schema = movement.current.schema;
                    catalog.composites[movement.slot].name = movement.current.name;
                }
            });
    }

    fn assert_identity(storage: &Storage, table: usize, transaction: u32, expected: UserTypeName) {
        for column in storage.table_def(table, transaction).columns() {
            assert_eq!(column.user_type, Some(expected));
        }
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
                    assert_eq!(retained_identity, Some(movement.prior));
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
                column.user_type = Some(movement.prior);
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
                assert_eq!(retired_identity, Some(movement.prior));
                assert_eq!(foreign_identity, Some(movement.prior));
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
        let identity = |name| UserTypeName {
            schema: super::super::SqlName::parse("public").unwrap(),
            name: super::super::SqlName::parse(name).unwrap(),
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
            let movement = TypeReferenceMove::new(DatabaseOid::POSTGRES, kind, 0, prior, current);
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
            let movement = TypeReferenceMove::new(DatabaseOid::POSTGRES, kind, 0, prior, current);
            let mut declared = Some(prior);
            assert!(!movement.rewrite(ctype, &mut declared));
            assert_eq!(declared, Some(prior));
        }
    }
}
