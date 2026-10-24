//! Stored type fields share one walk for renames and journal publication.

use super::{
    CastDef, ColType, OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition,
    OperatorFamilyFunction, OperatorFamilyOperator, OperatorImplementation, RoutineArgumentDef,
    RoutineDef, RoutineKind, RoutineParameterDef, RoutineResult, Storage, TypeCatalog,
    UserTypeName,
};
use crate::sql::eval::SqlError;
use core::convert::Infallible;
use {crate::mem::mutex::Mutex, crate::mem::mutex::MutexGuard, std::sync::TryLockError};

pub(crate) enum CatalogTypes<'a> {
    Routine(&'a mut RoutineDef),
    Cast(&'a mut CastDef),
    Operator(&'a mut OperatorDefinition),
    Family(&'a mut OperatorFamilyDefinition),
    Class(&'a mut OperatorClassDefinition),
}

pub(crate) trait CatalogTypeDefinition {
    fn catalog_types(&mut self) -> CatalogTypes<'_>;
}

macro_rules! catalog_type_definition {
    ($definition:ty, $variant:ident) => {
        impl CatalogTypeDefinition for $definition {
            fn catalog_types(&mut self) -> CatalogTypes<'_> {
                CatalogTypes::$variant(self)
            }
        }
    };
}
catalog_type_definition!(CastDef, Cast);
catalog_type_definition!(RoutineDef, Routine);
catalog_type_definition!(OperatorDefinition, Operator);
catalog_type_definition!(OperatorFamilyDefinition, Family);
catalog_type_definition!(OperatorClassDefinition, Class);

fn member_types<E>(
    operators: &mut [OperatorFamilyOperator],
    functions: &mut [OperatorFamilyFunction],
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    for member in operators.iter_mut().filter(|member| member.used) {
        visit(&mut member.left)?;
        visit(&mut member.right)?;
    }
    for member in functions.iter_mut().filter(|member| member.used) {
        visit(&mut member.left)?;
        visit(&mut member.right)?;
    }
    Ok(())
}

impl CatalogTypes<'_> {
    pub(super) fn try_for_each<E>(
        self,
        mut visit: impl FnMut(&mut RoutineResult) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Routine(definition) => {
                routine_type_fields(
                    &mut definition.arguments[..definition.argument_count],
                    &mut definition.parameters[..definition.parameter_count],
                    &mut definition.result_columns[..definition.result_column_count],
                    &mut definition.kind,
                    &mut visit,
                )?;
                if let Some(pending) = &mut definition.pending_definition {
                    routine_type_fields(
                        &mut pending.definition.arguments[..pending.definition.argument_count],
                        &mut pending.definition.parameters[..pending.definition.parameter_count],
                        &mut pending.definition.result_columns
                            [..pending.definition.result_column_count],
                        &mut pending.definition.kind,
                        &mut visit,
                    )?;
                }
            }
            Self::Cast(definition) => {
                visit(&mut definition.source)?;
                visit(&mut definition.target)?;
            }
            Self::Operator(definition) => {
                for argument in [
                    &mut definition.signature.left,
                    &mut definition.signature.right,
                ]
                .into_iter()
                .flatten()
                {
                    visit(argument)?;
                }
                if let OperatorImplementation::Function { result, .. } =
                    &mut definition.implementation
                {
                    visit(result)?;
                }
            }
            Self::Family(definition) => member_types(
                &mut definition.operators,
                &mut definition.functions,
                &mut visit,
            )?,
            Self::Class(definition) => {
                visit(&mut definition.input)?;
                visit(&mut definition.storage)?;
                member_types(
                    &mut definition.operators,
                    &mut definition.functions,
                    &mut visit,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn for_each(self, mut visit: impl FnMut(&mut RoutineResult)) {
        let result: Result<(), Infallible> = self.try_for_each(|value| {
            visit(value);
            Ok(())
        });
        match result {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
}

fn type_pair<E>(
    ctype: &mut ColType,
    name: &mut Option<UserTypeName>,
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    let mut result = RoutineResult {
        ctype: *ctype,
        user_type: *name,
    };
    visit(&mut result)?;
    *ctype = result.ctype;
    *name = result.user_type;
    Ok(())
}

pub(super) fn routine_type_fields<E>(
    arguments: &mut [RoutineArgumentDef],
    parameters: &mut [RoutineParameterDef],
    columns: &mut [RoutineArgumentDef],
    kind: &mut RoutineKind,
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    for argument in arguments.iter_mut().chain(columns.iter_mut()) {
        type_pair(&mut argument.ctype, &mut argument.user_type, visit)?;
    }
    for parameter in parameters {
        type_pair(&mut parameter.ctype, &mut parameter.user_type, visit)?;
    }
    match kind {
        RoutineKind::Function { result } | RoutineKind::SetFunction { result } => visit(result)?,
        RoutineKind::Aggregate(aggregate) => {
            visit(&mut aggregate.state_type)?;
            visit(&mut aggregate.result_type)?;
            if let Some(moving) = &mut aggregate.moving {
                visit(&mut moving.state_type)?;
            }
        }
        RoutineKind::RecordFunction { .. }
        | RoutineKind::TableFunction
        | RoutineKind::Trigger
        | RoutineKind::EventTrigger
        | RoutineKind::Procedure => {}
    }
    Ok(())
}

// Restrict destinations so the type catalog cannot be locked twice by one attempt.
/// These destinations cannot alias the shared type catalog acquired with them.
pub(super) trait CatalogPublicationDestination {}
impl CatalogPublicationDestination for super::OperatorCatalog {}
impl CatalogPublicationDestination for crate::mem::fixed_vec::FixedVec<RoutineDef> {}
impl CatalogPublicationDestination for crate::mem::fixed_vec::FixedVec<CastDef> {}
impl CatalogPublicationDestination for super::ViewRuleCatalog {}
impl CatalogPublicationDestination for super::SequenceCatalog {}
impl CatalogPublicationDestination for super::PolicyCatalog {}
impl CatalogPublicationDestination for crate::mem::fixed_vec::FixedVec<super::MatviewDef> {}

pub(super) struct CatalogTypeOwners<'storage, Catalog: CatalogPublicationDestination> {
    pub(super) catalog: MutexGuard<'storage, Catalog>,
    types: MutexGuard<'storage, TypeCatalog>,
}

impl<Catalog: CatalogPublicationDestination> CatalogTypeOwners<'_, Catalog> {
    pub(super) fn bind_definition<T: CatalogTypeDefinition>(
        &self,
        definition: &mut T,
        transaction: u32,
    ) -> Result<(), SqlError> {
        self.types.bind_definition(definition, transaction)
    }
}

pub(super) enum CatalogPublicationInput<'input> {
    RoutineContract,
    TypedCatalogCreation,
    ViewDefinition,
    ViewMetadataEdit,
    MetadataCommit,
    CatalogLifecycle,
    CatalogRetirement,
    QueryDefinition(super::StoredQueryDependencyView<'input>),
    QueryCreation(super::StoredQueryDependencyView<'input>),
    RuleMetadataEdit,
}

#[derive(Clone, Copy)]
pub(super) struct QueryTableTarget {
    slot: usize,
    identity: super::table_definitions::TableIdentity,
}

impl QueryTableTarget {
    pub(super) fn slot(self) -> usize {
        self.slot
    }
    pub(super) fn created_at(self) -> u64 {
        self.identity.created_at
    }
}

#[derive(Clone, Copy)]
pub(super) enum QueryTargetUse {
    Read,
    Retirement,
    JournalRetirement,
}

impl QueryTargetUse {
    fn accepts(self, existence: super::CatalogDdlState, transaction: u32) -> bool {
        existence.visible_to(transaction)
            || match self {
                Self::Read => false,
                Self::Retirement => existence.pending_drop_txid() == Some(transaction),
                Self::JournalRetirement => {
                    transaction == 0 && existence == super::CatalogDdlState::Absent
                }
            }
    }
}

#[derive(Clone, Copy)]
pub(super) enum QueryRuleTarget {
    Table(QueryTableTarget),
    View {
        slot: u16,
        database: super::DatabaseOid,
        created_at: u64,
    },
}

impl QueryRuleTarget {
    pub(super) fn table(self) -> Option<QueryTableTarget> {
        match self {
            Self::Table(target) => Some(target),
            Self::View { .. } => None,
        }
    }
}

struct QueryTableOwners<'storage> {
    definition: std::sync::RwLockReadGuard<'storage, super::table_definitions::DefinitionState>,
    _lifecycle: MutexGuard<'storage, ()>,
    selected: QueryTableTarget,
}

struct CatalogOwnerRequirements {
    types: bool,
    permissions: bool,
    comments: bool,
    extensions: bool,
}

impl CatalogPublicationInput<'_> {
    fn requirements(&self) -> CatalogOwnerRequirements {
        let (types, permissions, comments, extensions) = match self {
            Self::RoutineContract => (true, false, true, false),
            Self::TypedCatalogCreation => (true, true, true, false),
            Self::ViewDefinition => (true, true, false, false),
            Self::ViewMetadataEdit | Self::RuleMetadataEdit => (true, false, true, false),
            Self::MetadataCommit => (false, false, true, false),
            Self::CatalogLifecycle => (false, true, true, false),
            Self::CatalogRetirement => (false, true, true, true),
            Self::QueryDefinition(dependencies) | Self::QueryCreation(dependencies) => (
                dependencies.entries().iter().any(|dependency| {
                    matches!(
                        dependency.class,
                        super::DependencyClass::Domain
                            | super::DependencyClass::Enum
                            | super::DependencyClass::Composite
                    )
                }),
                matches!(self, Self::QueryCreation(_)),
                matches!(self, Self::QueryCreation(_)),
                false,
            ),
        };
        CatalogOwnerRequirements {
            types,
            permissions,
            comments,
            extensions,
        }
    }
}

pub(super) struct CatalogDependencyOwners<'storage, Catalog: CatalogPublicationDestination> {
    pub(super) catalog: MutexGuard<'storage, Catalog>,
    pub(super) dependencies: MutexGuard<'storage, super::StoredQueryDependencyCatalog>,
    pub(super) types: Option<MutexGuard<'storage, TypeCatalog>>,
    pub(super) permissions: Option<MutexGuard<'storage, super::AclCatalog>>,
    target: Option<QueryTableOwners<'storage>>,
    pub(super) comments: Option<MutexGuard<'storage, super::CommentCatalog>>,
    pub(super) extensions: Option<MutexGuard<'storage, super::ExtensionCatalog>>,
}

impl<Catalog: CatalogPublicationDestination> CatalogDependencyOwners<'_, Catalog> {
    pub(super) fn allocate_preserved_query_image(
        &mut self,
        storage: &Storage,
        owner: super::StoredQueryDependencyOwner,
        previous: Option<u32>,
        transaction: u32,
    ) -> Result<u32, SqlError> {
        let image = previous.map_or(storage.committed_dependency_image(owner), |slot| {
            storage.stored_query_dependency_pending_base + slot as usize
        });
        let count = self.dependencies.image_count(image);
        self.validate_dependencies(
            super::StoredQueryDependencyView::new(self.dependencies.image(
                image,
                count,
                storage.stored_query_dependencies_per_image,
            )),
            transaction,
        )?;
        let slot = storage.allocate_pending_stored_query_dependencies_from_current_in(
            &mut self.dependencies,
            owner,
            previous,
        )?;
        self.refresh_dependencies(
            storage.stored_query_dependency_pending_base + slot as usize,
            self.dependencies.pending[slot as usize].count,
            storage.stored_query_dependencies_per_image,
            transaction,
        );
        Ok(slot)
    }

    pub(super) fn validate_target(
        &self,
        transaction: u32,
        usage: QueryTargetUse,
    ) -> Result<(), SqlError> {
        if let Some(target) = &self.target {
            let current = target.definition.identity;
            if current.database != target.selected.identity.database
                || current.created_at != target.selected.identity.created_at
                || !usage.accepts(current.existence, transaction)
                || current
                    .existence
                    .pending_drop_txid()
                    .is_some_and(|owner| owner != transaction)
            {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                    "saved-query target relation changed before publication"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn target_names(
        &self,
        transaction: u32,
    ) -> Option<(super::SqlName, super::SqlName)> {
        let target = self.target.as_ref()?;
        let definition = &target.definition;
        Some(
            definition
                .pending
                .filter(|pending| pending.transaction == transaction)
                .map_or(
                    (definition.committed().schema, definition.committed().name),
                    |pending| (pending.schema, pending.name),
                ),
        )
    }

    pub(super) fn write_query_image(
        &mut self,
        storage: &Storage,
        image: usize,
        dependencies: super::StoredQueryDependencyView<'_>,
        transaction: u32,
    ) -> Result<(), SqlError> {
        self.validate_dependencies(dependencies, transaction)?;
        storage.write_dependency_image_in(&mut self.dependencies, image, dependencies)?;
        self.refresh_dependencies(
            image,
            self.dependencies.counts[image],
            storage.stored_query_dependencies_per_image,
            transaction,
        );
        Ok(())
    }

    pub(super) fn allocate_query_image(
        &mut self,
        storage: &Storage,
        owner: super::StoredQueryDependencyOwner,
        previous: Option<u32>,
        dependencies: super::StoredQueryDependencyView<'_>,
        transaction: u32,
    ) -> Result<u32, SqlError> {
        self.validate_dependencies(dependencies, transaction)?;
        let slot = storage.allocate_pending_stored_query_dependencies_in(
            &mut self.dependencies,
            owner,
            previous,
            dependencies,
        )?;
        self.refresh_dependencies(
            storage.stored_query_dependency_pending_base + slot as usize,
            self.dependencies.pending[slot as usize].count,
            storage.stored_query_dependencies_per_image,
            transaction,
        );
        Ok(slot)
    }

    pub(super) fn validate_dependencies(
        &self,
        dependencies: super::StoredQueryDependencyView<'_>,
        transaction: u32,
    ) -> Result<(), SqlError> {
        for mut dependency in dependencies.entries().iter().copied() {
            if !dependency.identity.valid_for(dependency.class) {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::INTERNAL_ERROR,
                    "stored-query dependency has an invalid catalog identity"
                ));
            }
            if matches!(
                dependency.class,
                super::DependencyClass::Domain
                    | super::DependencyClass::Enum
                    | super::DependencyClass::Composite
            ) {
                self.types
                    .as_ref()
                    .expect("type dependencies request source ownership")
                    .bind_query_type_dependency(&mut dependency, transaction)?;
            }
        }
        Ok(())
    }

    pub(super) fn refresh_dependencies(
        &mut self,
        image: usize,
        count: u8,
        entries_per_image: usize,
        transaction: u32,
    ) {
        for dependency in self.dependencies.image_mut(image, count, entries_per_image) {
            if matches!(
                dependency.class,
                super::DependencyClass::Domain
                    | super::DependencyClass::Enum
                    | super::DependencyClass::Composite
            ) {
                self.types
                    .as_ref()
                    .expect("type dependencies request source ownership")
                    .bind_query_type_dependency(dependency, transaction)
                    .expect("source owner preserves validated dependency lifetimes");
            }
        }
    }
}

impl CatalogDependencyOwners<'_, crate::mem::fixed_vec::FixedVec<RoutineDef>> {
    pub(super) fn bind_routine_contract(
        &self,
        arguments: &mut [RoutineArgumentDef],
        parameters: &mut [RoutineParameterDef],
        columns: &mut [RoutineArgumentDef],
        kind: &mut RoutineKind,
        transaction: u32,
    ) -> Result<(), SqlError> {
        routine_type_fields(arguments, parameters, columns, kind, &mut |result| {
            *result = self
                .types
                .as_ref()
                .expect("routine contracts request source ownership")
                .bind_type_reference(*result, super::current_database(), transaction)?;
            Ok(())
        })
    }
}

impl CatalogDependencyOwners<'_, super::ViewRuleCatalog> {
    pub(super) fn validate_rule_target(
        &self,
        selected: QueryRuleTarget,
        transaction: u32,
    ) -> Result<(), SqlError> {
        match selected {
            QueryRuleTarget::Table(_) => self.validate_target(transaction, QueryTargetUse::Read),
            QueryRuleTarget::View {
                slot,
                database,
                created_at,
            } => {
                if self
                    .catalog
                    .views
                    .get(usize::from(slot))
                    .is_some_and(|view| {
                        view.database == database
                            && view.created_at == created_at
                            && view.visible_to(transaction)
                            && view
                                .ddl_state
                                .pending_drop_txid()
                                .is_none_or(|owner| owner == transaction)
                    })
                {
                    Ok(())
                } else {
                    Err(crate::sql_err!(
                        crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                        "rewrite-rule target view changed before publication"
                    ))
                }
            }
        }
    }
}

impl TypeCatalog {
    fn bind_definition<T: CatalogTypeDefinition>(
        &self,
        definition: &mut T,
        transaction: u32,
    ) -> Result<(), SqlError> {
        definition.catalog_types().try_for_each(|result| {
            *result = self.bind_type_reference(*result, super::current_database(), transaction)?;
            Ok::<(), SqlError>(())
        })
    }
}

impl Storage {
    pub(super) fn query_rule_target(
        &self,
        target: super::RuleTarget,
        transaction: u32,
    ) -> Result<QueryRuleTarget, SqlError> {
        match target {
            super::RuleTarget::Table(slot) => self
                .query_table_target(usize::from(slot), transaction, QueryTargetUse::Read)
                .map(QueryRuleTarget::Table),
            super::RuleTarget::View(slot) => {
                let catalog = self.view_rule_catalog();
                let view = catalog
                    .views
                    .get(usize::from(slot))
                    .filter(|view| {
                        view.database == super::current_database() && view.visible_to(transaction)
                    })
                    .ok_or_else(|| {
                        crate::sql_err!(
                            crate::sql::eval::sqlstate::UNDEFINED_TABLE,
                            "rewrite-rule target view does not exist"
                        )
                    })?;
                Ok(QueryRuleTarget::View {
                    slot,
                    database: view.database,
                    created_at: view.created_at,
                })
            }
        }
    }

    pub(super) fn query_table_target(
        &self,
        slot: usize,
        transaction: u32,
        usage: QueryTargetUse,
    ) -> Result<QueryTableTarget, SqlError> {
        let table = self.tables.get(slot).ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::UNDEFINED_TABLE,
                "saved-query target relation does not exist"
            )
        })?;
        let state = table.definition.read();
        if state.identity.database != super::current_database()
            || !usage.accepts(state.identity.existence, transaction)
        {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::UNDEFINED_TABLE,
                "saved-query target relation does not exist"
            ));
        }
        Ok(QueryTableTarget {
            slot,
            identity: state.identity,
        })
    }

    pub(super) fn catalog_dependency_owners<'storage, Catalog: CatalogPublicationDestination>(
        &'storage self,
        destination: &'storage Mutex<Catalog>,
        input: CatalogPublicationInput<'_>,
        target: Option<QueryTableTarget>,
    ) -> CatalogDependencyOwners<'storage, Catalog> {
        let requirements = input.requirements();
        loop {
            let attempt: Result<_, super::row_ownership::LockWait<'storage>> = (|| {
                let permissions = if requirements.permissions {
                    Some(super::row_ownership::try_mutex(
                        &self.acl_catalog,
                        "ACL catalog lock poisoned",
                    )?)
                } else {
                    None
                };
                let target = match target {
                    Some(selected) => {
                        let table = &self.tables[selected.slot];
                        let lifecycle = super::row_ownership::try_mutex(
                            &table.lifecycle,
                            "table lifecycle lock poisoned",
                        )?;
                        let definition = table.definition.try_read_for_batch()?;
                        Some(QueryTableOwners {
                            definition,
                            _lifecycle: lifecycle,
                            selected,
                        })
                    }
                    None => None,
                };
                let comments = if requirements.comments {
                    Some(super::row_ownership::try_mutex(
                        &self.comments,
                        "comment catalog lock poisoned",
                    )?)
                } else {
                    None
                };
                let catalog = super::row_ownership::try_mutex(
                    destination,
                    "dependent catalog publication lock poisoned",
                )?;
                let types = if requirements.types {
                    Some(super::row_ownership::try_mutex(
                        &self.type_catalog,
                        "type catalog lock poisoned",
                    )?)
                } else {
                    None
                };
                let dependencies = super::row_ownership::try_mutex(
                    &self.stored_query_dependencies,
                    "stored-query dependency catalog lock poisoned",
                )?;
                let extensions = if requirements.extensions {
                    Some(super::row_ownership::try_mutex(
                        &self.extension_catalog,
                        "extension catalog lock poisoned",
                    )?)
                } else {
                    None
                };
                Ok(CatalogDependencyOwners {
                    catalog,
                    types,
                    permissions,
                    dependencies,
                    target,
                    comments,
                    extensions,
                })
            })();
            match attempt {
                Ok(owners) => return owners,
                Err(wait) => {
                    #[cfg(test)]
                    self.definition_publication
                        .waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    wait.wait();
                }
            }
        }
    }

    /// Final validation and destination installation share these owners. A
    /// source wait releases the destination, matching type-name publication's
    /// rule that no earlier owner remains held while waiting for another one.
    pub(super) fn catalog_type_owners<'storage, Catalog: CatalogPublicationDestination>(
        &'storage self,
        destination: &'storage Mutex<Catalog>,
    ) -> CatalogTypeOwners<'storage, Catalog> {
        loop {
            let catalog = destination
                .lock()
                .expect("dependent catalog publication lock poisoned");
            match self.type_catalog.try_lock() {
                Ok(types) => return CatalogTypeOwners { catalog, types },
                Err(TryLockError::WouldBlock) => {
                    drop(catalog);
                    #[cfg(test)]
                    self.definition_publication
                        .waits
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    drop(
                        self.type_catalog
                            .lock()
                            .expect("type catalog lock poisoned"),
                    );
                }
                Err(TryLockError::Poisoned(_)) => panic!("type catalog lock poisoned"),
            }
        }
    }

    /// Validate every type field and refresh its name from the captured source.
    /// Journal images also use this walk before discarding process-local keys.
    pub(crate) fn bind_catalog_definition<T: CatalogTypeDefinition>(
        &self,
        mut definition: T,
        txid: u32,
    ) -> Result<T, SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        catalog.bind_definition(&mut definition, txid)?;
        Ok(definition)
    }
}
