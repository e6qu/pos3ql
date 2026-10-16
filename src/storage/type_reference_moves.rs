//! A type move updates only references belonging to its owning database.

use super::{
    ArrElem, CatalogDdlState, ColType, CommentClass, DatabaseOid, DependencyClass,
    RoutineArgumentDef, RoutineKind, RoutineResult, Storage, TableDef, UserTypeName,
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

    fn rewrite(self, ctype: ColType, identity: &mut Option<UserTypeName>) -> bool {
        let matches = match self.kind {
            TypeReferenceKind::Domain => {
                *identity == Some(self.prior)
                    || matches!(ctype, ColType::Array(ArrElem::Domain { slot, .. })
                        if usize::from(slot) == self.slot)
            }
            TypeReferenceKind::Enum => {
                identity.is_some()
                    && matches!(ctype, ColType::Enum(slot) | ColType::Array(ArrElem::Enum(slot))
                        if usize::from(slot) == self.slot)
            }
            TypeReferenceKind::Composite => {
                *identity == Some(self.prior)
                    && matches!(ctype, ColType::Composite(slot) | ColType::Array(ArrElem::Composite(slot))
                        if usize::from(slot) == self.slot)
            }
        };
        if matches {
            *identity = Some(self.current);
        }
        matches
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

    fn class(self) -> DependencyClass {
        match self.kind {
            TypeReferenceKind::Domain => DependencyClass::Domain,
            TypeReferenceKind::Enum => DependencyClass::Enum,
            TypeReferenceKind::Composite => DependencyClass::Composite,
        }
    }
}

impl Storage {
    pub(super) fn move_type_references(&mut self, movement: TypeReferenceMove) {
        for table in self.tables.iter_mut().filter(|table| {
            table.database() == movement.database && table.existence() != CatalogDdlState::Absent
        }) {
            if self
                .pending_table_defs
                .move_type_references(&mut table.definition, movement)
            {
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
        self.rename_stored_query_dependency(
            movement.class(),
            movement.slot,
            None,
            movement.current.schema,
            movement.current.name,
        );
    }

    fn move_routine_type_references(&mut self, movement: TypeReferenceMove) {
        let move_argument = |argument: &mut RoutineArgumentDef| {
            movement.rewrite(argument.ctype, &mut argument.user_type);
        };
        let move_result = |result: &mut RoutineResult| {
            movement.rewrite(result.ctype, &mut result.user_type);
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
