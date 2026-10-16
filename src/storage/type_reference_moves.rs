//! A type move updates only references belonging to its owning database.

use super::table_definitions::TableIdentity;
use super::{
    ArrElem, CatalogDdlState, ColType, CommentClass, DatabaseOid, DependencyClass,
    OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition, RoutineArgumentDef,
    RoutineKind, RoutineResult, Storage, TableDef, UserTypeName,
};

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
            TypeReferenceKind::Domain => {
                identity == Some(self.prior)
                    || matches!(ctype, ColType::Array(ArrElem::Domain { slot, .. })
                        if usize::from(slot) == self.slot)
            }
            TypeReferenceKind::Enum => {
                identity.is_some()
                    && matches!(ctype, ColType::Enum(slot) | ColType::Array(ArrElem::Enum(slot))
                        if usize::from(slot) == self.slot)
            }
            TypeReferenceKind::Composite => {
                identity == Some(self.prior)
                    && matches!(ctype, ColType::Composite(slot) | ColType::Array(ArrElem::Composite(slot))
                        if usize::from(slot) == self.slot)
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

impl Storage {
    // Type commit callers still retain exclusive storage ownership. The
    // shared walk waits for one table without holding catalogs or allocators.
    pub(super) fn move_type_references(&self, movement: TypeReferenceMove) {
        for table in self.tables.iter() {
            let _lifecycle = table
                .lifecycle
                .lock()
                .expect("table lifecycle lock poisoned");
            {
                let state = table.definition.read();
                if !movement.applies_to(state.identity)
                    || !self.pending_table_defs.references_type(&state, movement)
                {
                    continue;
                }
            }
            let changed = {
                let mut state = table.definition.write();
                self.pending_table_defs
                    .move_type_references_locked(&mut state, movement)
            };
            if changed {
                table.mark_dirty();
            }
        }
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        for composite in catalog.composites.iter_mut().filter(|composite| {
            composite.database == movement.database
                && composite.ddl_state != CatalogDdlState::Absent
        }) {
            for field in composite.fields.iter_mut().take(composite.n_fields) {
                movement.rewrite(field.ctype, &mut field.user_type);
            }
            if let Some(pending) = &mut composite.pending_definition {
                for field in pending.fields.iter_mut().take(pending.n_fields) {
                    movement.rewrite(field.ctype, &mut field.user_type);
                }
            }
        }
        for domain in catalog.domains.iter_mut().filter(|domain| {
            domain.database == movement.database && domain.ddl_state != CatalogDdlState::Absent
        }) {
            movement.rewrite_domain(
                domain.base,
                &mut domain.base_domain,
                &mut domain.base_user_type,
            );
            if let Some(pending) = &mut domain.pending_definition {
                movement.rewrite_domain(
                    pending.spec.base,
                    &mut pending.spec.base_domain,
                    &mut pending.spec.base_user_type,
                );
            }
        }
        drop(catalog);
        for comment in self.comment_catalog().entries.iter_mut().filter(|comment| {
            comment.used
                && comment.database == Some(movement.database)
                && comment.class == CommentClass::Type
                && comment.schema == movement.prior.schema
                && comment.name == movement.prior.name
        }) {
            comment.schema = movement.current.schema;
            comment.name = movement.current.name;
        }
        self.move_routine_type_references(movement);
        self.move_callable_type_references(movement);
        self.rename_stored_query_dependency(
            movement.class(),
            movement.slot,
            None,
            movement.current.schema,
            movement.current.name,
        );
    }

    fn move_callable_type_references(&self, movement: TypeReferenceMove) {
        {
            let mut catalog = self
                .operator_catalog
                .lock()
                .expect("operator catalog lock poisoned");
            for operator in catalog.operators.iter_mut().filter(|operator| {
                operator.database == movement.database
                    && operator.ddl_state != CatalogDdlState::Absent
            }) {
                movement.rewrite_operator(&mut operator.definition);
                if let Some(pending) = &mut operator.pending {
                    movement.rewrite_operator(&mut pending.definition);
                }
            }
            for family in catalog.families.iter_mut().filter(|family| {
                family.database == movement.database && family.ddl_state != CatalogDdlState::Absent
            }) {
                movement.rewrite_family(&mut family.definition);
                if let Some(pending) = &mut family.pending {
                    movement.rewrite_family(&mut pending.definition);
                }
            }
            for class in catalog.classes.iter_mut().filter(|class| {
                class.database == movement.database && class.ddl_state != CatalogDdlState::Absent
            }) {
                movement.rewrite_class(&mut class.definition);
                if let Some(pending) = &mut class.pending {
                    movement.rewrite_class(&mut pending.definition);
                }
            }
        }
        let mut casts = self.casts.lock().expect("cast catalog lock poisoned");
        for cast in casts.iter_mut().filter(|cast| {
            cast.database == movement.database && cast.ddl_state != CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Cast(cast)
                .for_each(|result| movement.rewrite_result(result));
        }
    }

    fn move_routine_type_references(&self, movement: TypeReferenceMove) {
        let move_argument = |argument: &mut RoutineArgumentDef| {
            movement.rewrite(argument.ctype, &mut argument.user_type);
        };
        let move_result = |result: &mut RoutineResult| {
            movement.rewrite_result(result);
        };
        let mut routines = self.routines.lock().expect("routine catalog lock poisoned");
        for routine in routines
            .iter_mut()
            .filter(|routine| routine.database == movement.database)
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

    fn publish_source(storage: &Storage, movement: TypeReferenceMove) {
        // Reproduce the caller's completed source update before reference fanout.
        let mut catalog = storage.type_catalog.lock().unwrap();
        match movement.kind {
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
        }
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
                        publish_source(storage, movement);
                        storage.move_type_references(movement);
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
                            publish_source(storage, movement);
                            storage.move_type_references(movement);
                        });
                        done_send.send(()).unwrap();
                    });
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let waiting = loop {
                        match storage.tables[related].lifecycle.try_lock() {
                            Err(std::sync::TryLockError::WouldBlock) => break true,
                            Ok(owner) => drop(owner),
                            Err(std::sync::TryLockError::Poisoned(_)) => break false,
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
                    assert!(
                        waiting,
                        "the move did not acquire its selected lifecycle owner"
                    );
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
                publish_source(&storage, movement);
                storage.move_type_references(movement);
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
                        publish_source(storage, movement);
                        storage.move_type_references(movement);
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
}
