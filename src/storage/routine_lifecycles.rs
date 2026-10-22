//! Routine bodies, names and dependencies publish against one selected revision.

use super::catalog_sources::{CatalogDdlOperation, CatalogEditTarget, CatalogObjectSlot};
use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineEditUndo {
    target: CatalogEditTarget,
    prior_identity: Option<PendingRoutineIdentity>,
    prior_definition: Option<PendingRoutineDefinition>,
}

// Undo stays inline in the startup-reserved transaction pool.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoutineChange {
    Lifecycle(CatalogDdlUndo),
    Edit(RoutineEditUndo),
}

impl RoutineChange {
    pub(crate) fn slot(self) -> usize {
        match self {
            Self::Lifecycle(undo) => undo.slot(),
            Self::Edit(undo) => undo.target.slot(),
        }
    }

    pub(crate) fn is_creation(self) -> bool {
        matches!(self, Self::Lifecycle(undo) if undo.operation == CatalogDdlOperation::Create)
    }

    pub(crate) fn is_drop(self) -> bool {
        matches!(self, Self::Lifecycle(undo) if undo.operation == CatalogDdlOperation::Drop)
    }
}

pub(crate) enum RoutineDependencies<'input> {
    Replace(StoredQueryDependencyView<'input>),
    Preserve,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    fn fixture() -> Storage {
        let mut config = test_config();
        config.max_routines = 1;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget).unwrap()
    }

    fn name(value: &str) -> SqlName {
        SqlName::parse(value).unwrap()
    }

    fn create(storage: &mut Storage, value: &str, transaction: u32) -> RoutineChange {
        storage
            .create_routine(
                RoutineSpec {
                    identity: RoutineIdentity::Allocate,
                    schema: name("public"),
                    name: name(value),
                    arguments: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
                    argument_count: 0,
                    parameters: [RoutineParameterDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
                    parameter_count: 0,
                    kind: RoutineKind::Function {
                        result: RoutineResult::TEXT,
                    },
                    result_columns: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_OUTPUT_COLUMNS],
                    result_column_count: 0,
                    language: RoutineLanguage::Sql,
                    attributes: RoutineAttributes::DEFAULT,
                    configs: [RoutineConfig::EMPTY; MAX_ROUTINE_CONFIGS],
                    config_count: 0,
                    body_kind: RoutineBodyKind::String,
                    body: StackStr::from_str("SELECT 'original'"),
                    creation_path: StackStr::from_str("public"),
                    dependencies: StoredQueryDependencies::EMPTY,
                },
                transaction,
            )
            .unwrap()
    }

    fn replacement(storage: &Storage, slot: usize, transaction: u32) -> RoutineDefinition {
        let definition = storage.routine_for(slot, transaction);
        RoutineDefinition {
            arguments: definition.arguments,
            argument_count: definition.argument_count,
            parameters: definition.parameters,
            parameter_count: definition.parameter_count,
            kind: definition.kind,
            result_columns: definition.result_columns,
            result_column_count: definition.result_column_count,
            language: definition.language,
            attributes: definition.attributes,
            configs: definition.configs,
            config_count: definition.config_count,
            body_kind: definition.body_kind,
            body: StackStr::from_str("SELECT 'replacement'"),
            creation_path: definition.creation_path,
        }
    }

    #[test]
    fn routine_lifecycle_name_body_and_dependencies_follow_one_revision() {
        let mut storage = fixture();
        let created = create(&mut storage, "original_routine", 7);
        storage.finish_routine_change(created, CatalogDdlDecision::Commit);
        let birth = storage.routine(created.slot()).created_at;
        crate::mem::guard::forbid_alloc(|| {
            let selected = storage.prepare_routine_edit(created.slot(), 9).unwrap();
            let renamed = storage
                .alter_routine_identity(selected, name("public"), name("renamed_routine"), 9)
                .unwrap();
            assert_eq!(
                storage
                    .replace_routine(
                        selected,
                        replacement(&storage, created.slot(), 9),
                        RoutineDependencies::Preserve,
                        9
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let current = storage.prepare_routine_edit(created.slot(), 9).unwrap();
            let replaced = storage
                .replace_routine(
                    current,
                    replacement(&storage, created.slot(), 9),
                    RoutineDependencies::Preserve,
                    9,
                )
                .unwrap();
            storage.finish_routine_change(renamed, CatalogDdlDecision::Rollback);
            assert_eq!(
                storage.routine_for(created.slot(), 9).name,
                name("renamed_routine")
            );
            assert_eq!(
                storage.routine_for(created.slot(), 9).body.as_str(),
                "SELECT 'replacement'"
            );
            storage.finish_routine_change(replaced, CatalogDdlDecision::Rollback);
            assert_eq!(
                storage.routine_for(created.slot(), 9).body.as_str(),
                "SELECT 'original'"
            );
            storage.finish_routine_change(renamed, CatalogDdlDecision::Rollback);
            assert_eq!(
                storage.routine_for(created.slot(), 9).name,
                name("original_routine")
            );
            let current = storage.prepare_routine_edit(created.slot(), 9).unwrap();
            let renamed = storage
                .alter_routine_identity(current, name("public"), name("committed_routine"), 9)
                .unwrap();
            let current = storage.prepare_routine_edit(created.slot(), 9).unwrap();
            let replaced = storage
                .replace_routine(
                    current,
                    replacement(&storage, created.slot(), 9),
                    RoutineDependencies::Preserve,
                    9,
                )
                .unwrap();
            storage.finish_routine_change(renamed, CatalogDdlDecision::Commit);
            assert_eq!(
                storage.routine_for(created.slot(), 0).name,
                name("original_routine")
            );
            storage.finish_routine_change(replaced, CatalogDdlDecision::Commit);
            assert_eq!(
                storage.routine_for(created.slot(), 0).name,
                name("committed_routine")
            );
            assert_eq!(
                storage.routine_for(created.slot(), 0).body.as_str(),
                "SELECT 'replacement'"
            );
            assert_eq!(storage.routine(created.slot()).created_at, birth);
            assert!(storage.routine(created.slot()).pending_definition.is_none());
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

    #[test]
    fn routine_lifecycle_stale_drop_and_creation_cannot_retire_replacement() {
        let mut storage = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let original = create(&mut storage, "original_routine", 7);
            let stale = storage.prepare_routine_edit(original.slot(), 7).unwrap();
            storage.finish_routine_change(original, CatalogDdlDecision::Rollback);
            let replacement = create(&mut storage, "replacement_routine", 9);
            assert_eq!(original.slot(), replacement.slot());
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_routine_change(original, decision);
            }
            assert_eq!(
                storage.drop_routine(stale, 7).unwrap_err().sqlstate,
                sqlstate::UNDEFINED_FUNCTION
            );
            storage.finish_routine_change(replacement, CatalogDdlDecision::Commit);
            let first = storage
                .drop_routine(
                    storage
                        .prepare_routine_edit(replacement.slot(), 11)
                        .unwrap(),
                    11,
                )
                .unwrap();
            storage.finish_routine_change(first, CatalogDdlDecision::Rollback);
            let second = storage
                .drop_routine(
                    storage
                        .prepare_routine_edit(replacement.slot(), 11)
                        .unwrap(),
                    11,
                )
                .unwrap();
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_routine_change(first, decision);
            }
            assert!(!storage.routine(replacement.slot()).visible_to(11));
            storage.finish_routine_change(second, CatalogDdlDecision::Rollback);
            assert_eq!(
                storage.routine_for(replacement.slot(), 0).name,
                name("replacement_routine")
            );
        });
    }
}

impl Storage {
    pub(crate) fn prepare_routine_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<CatalogEditTarget, SqlError> {
        let routines = self.routines.lock().expect("routine catalog lock poisoned");
        let routine = routines
            .get(slot)
            .filter(|routine| {
                routine.database == current_database() && routine.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "routine edit source no longer exists"
                )
            })?;
        if let Some(owner) = Self::routine_edit_owner(routine).filter(|owner| *owner != transaction)
        {
            let name = routine.name;
            drop(routines);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        Ok(CatalogEditTarget {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Routine(slot),
                routine.database,
                routine.created_at,
            ),
            transaction,
            version: routine.revision_for(transaction),
        })
    }

    fn routine_edit_owner(routine: &RoutineDef) -> Option<u32> {
        routine
            .pending_definition
            .map(|pending| pending.txid)
            .or_else(|| routine.pending_identity.map(|pending| pending.txid))
            .or_else(|| routine.ddl_state.pending_txid())
    }

    fn validate_routine_edit_in(
        &self,
        routines: &FixedVec<RoutineDef>,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let slot = target.routine_slot()?;
        let routine = routines
            .get(slot)
            .filter(|routine| {
                target.source.matches(
                    self,
                    routine.database,
                    routine.created_at,
                    routine.ddl_state,
                ) && routine.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "routine edit source has been replaced"
                )
            })?;
        if target.transaction != transaction {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "routine edit belongs to another transaction"
            ));
        }
        if let Some(owner) = Self::routine_edit_owner(routine).filter(|owner| *owner != transaction)
        {
            return Err(self.catalog_ddl_wait_error(transaction, owner, routine.name.as_str()));
        }
        if routine.revision_for(transaction) != target.version {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "routine changed after edit preparation"
            ));
        }
        Ok(slot)
    }

    fn routine_edit_slot(
        &self,
        routines: &FixedVec<RoutineDef>,
        undo: RoutineEditUndo,
    ) -> Option<usize> {
        let slot = undo.target.routine_slot().ok()?;
        let routine = routines.get(slot)?;
        (undo.target.source.matches(
            self,
            routine.database,
            routine.created_at,
            routine.ddl_state,
        ) && Self::routine_edit_owner(routine) == Some(undo.target.transaction)
            && routine.revision_for(undo.target.transaction) == undo.target.version)
            .then_some(slot)
    }

    pub(crate) fn replace_routine(
        &self,
        target: CatalogEditTarget,
        mut definition: RoutineDefinition,
        dependencies: RoutineDependencies<'_>,
        transaction: u32,
    ) -> Result<RoutineChange, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::RoutineContract,
            None,
        );
        let slot = self.validate_routine_edit_in(&owners.catalog, target, transaction)?;
        owners.bind_routine_contract(
            &mut definition.arguments[..definition.argument_count],
            &mut definition.parameters[..definition.parameter_count],
            &mut definition.result_columns[..definition.result_column_count],
            &mut definition.kind,
            transaction,
        )?;
        let version = self.catalog_sequence.next()?;
        let routine = owners.catalog[slot];
        let previous = routine
            .pending_definition
            .map(|pending| pending.dependency_slot);
        let owner = StoredQueryDependencyOwner::Routine(slot as u16);
        let dependency_slot = match dependencies {
            RoutineDependencies::Replace(dependencies) => {
                owners.allocate_query_image(self, owner, previous, dependencies, transaction)?
            }
            RoutineDependencies::Preserve => {
                owners.allocate_preserved_query_image(self, owner, previous, transaction)?
            }
        };
        owners.catalog[slot].pending_definition = Some(PendingRoutineDefinition {
            txid: transaction,
            version,
            dependency_slot,
            definition,
        });
        Ok(RoutineChange::Edit(RoutineEditUndo {
            target: CatalogEditTarget { version, ..target },
            prior_identity: routine.pending_identity,
            prior_definition: routine.pending_definition,
        }))
    }

    fn stage_routine_comment_identity_in(
        comments: &mut CommentCatalog,
        routine: RoutineDef,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
    ) {
        let oid = routine_oid(&routine) as u32;
        for comment in comments.entries.iter_mut().filter(|comment| {
            comment.used
                && comment.database == Some(routine.database)
                && comment.class == CommentClass::Routine
                && comment.subid == oid
                && comment
                    .pending_drop
                    .is_none_or(|pending| pending.txid != transaction)
        }) {
            comment.pending_identity = (comment.schema != schema || comment.name != name)
                .then_some(PendingCommentIdentity {
                    txid: transaction,
                    schema,
                    name,
                });
        }
    }

    pub(crate) fn alter_routine_identity(
        &self,
        target: CatalogEditTarget,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
    ) -> Result<RoutineChange, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let slot = self.validate_routine_edit_in(&owners.catalog, target, transaction)?;
        let routine = owners.catalog[slot];
        let definition = routine.definition_for(transaction);
        for (_, candidate) in owners
            .catalog
            .iter()
            .enumerate()
            .filter(|(other, candidate)| {
                *other != slot
                    && candidate.database == current_database()
                    && candidate.ddl_state != CatalogDdlState::Absent
            })
        {
            let signature = candidate.definition_for(transaction);
            if signature.argument_count != definition.argument_count
                || !signature
                    .arguments()
                    .iter()
                    .zip(definition.arguments())
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    })
            {
                continue;
            }
            let committed_matches = candidate.schema == schema && candidate.name == name;
            let pending_matches = candidate
                .pending_identity
                .is_some_and(|pending| pending.schema == schema && pending.name == name);
            if (committed_matches || pending_matches)
                && let Some(owner) =
                    Self::routine_edit_owner(candidate).filter(|owner| *owner != transaction)
            {
                return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
            }
            if candidate.visible_to(transaction)
                && candidate.schema_for(transaction) == schema
                && candidate.name_for(transaction) == name
            {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_FUNCTION,
                    "routine \"{}\" already exists with same argument types",
                    name.as_str()
                ));
            }
        }
        let version = self.catalog_sequence.next()?;
        Self::stage_routine_comment_identity_in(
            owners
                .comments
                .as_mut()
                .expect("routine identity owns comments"),
            routine,
            schema,
            name,
            transaction,
        );
        owners.catalog[slot].pending_identity = Some(PendingRoutineIdentity {
            txid: transaction,
            version,
            schema,
            name,
        });
        Ok(RoutineChange::Edit(RoutineEditUndo {
            target: CatalogEditTarget { version, ..target },
            prior_identity: routine.pending_identity,
            prior_definition: routine.pending_definition,
        }))
    }

    fn publish_routine_edit_in(
        &self,
        owners: &mut type_contracts::CatalogDependencyOwners<'_, FixedVec<RoutineDef>>,
        slot: usize,
        transaction: u32,
    ) {
        let old = owners.catalog[slot];
        if let Some(pending) = old
            .pending_definition
            .filter(|pending| pending.txid == transaction)
        {
            self.commit_pending_dependencies_in(
                &mut owners.dependencies,
                StoredQueryDependencyOwner::Routine(slot as u16),
                pending.dependency_slot,
            );
        }
        let mut definition = old.definition_for(transaction);
        definition.version = old.revision_for(transaction);
        definition.schema = old.schema_for(transaction);
        definition.name = old.name_for(transaction);
        definition.pending_identity = None;
        definition.pending_definition = None;
        let oid = routine_oid(&old) as u32;
        for comment in owners
            .comments
            .as_mut()
            .expect("routine publication owns comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.used
                    && comment.database == Some(old.database)
                    && comment.class == CommentClass::Routine
                    && comment.subid == oid
                    && comment
                        .pending_drop
                        .is_none_or(|pending| pending.txid != transaction)
            })
        {
            if let Some(identity) = comment
                .pending_identity
                .filter(|identity| identity.txid == transaction)
            {
                comment.schema = identity.schema;
                comment.name = identity.name;
                comment.pending_identity = None;
            }
        }
        if old.schema != definition.schema || old.name != definition.name {
            self.rename_stored_query_dependency_locked(
                &mut owners.dependencies,
                DependencyClass::Routine,
                slot,
                None,
                definition.schema,
                definition.name,
            );
        }
        owners.catalog[slot] = definition;
    }

    fn finish_routine_edit(&self, undo: RoutineEditUndo, decision: CatalogDdlDecision) {
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(slot) = self.routine_edit_slot(&owners.catalog, undo) else {
            return;
        };
        match decision {
            CatalogDdlDecision::Commit => {
                self.publish_routine_edit_in(&mut owners, slot, undo.target.transaction)
            }
            CatalogDdlDecision::Rollback => {
                let current = owners.catalog[slot];
                if let Some(pending) = current.pending_definition
                    && undo
                        .prior_definition
                        .is_none_or(|prior| prior.dependency_slot != pending.dependency_slot)
                {
                    owners.dependencies.pending[pending.dependency_slot as usize] =
                        PendingStoredQueryDependencies::EMPTY;
                }
                owners.catalog[slot].pending_definition = undo.prior_definition;
                owners.catalog[slot].pending_identity = undo.prior_identity;
                let prior = owners.catalog[slot];
                Self::stage_routine_comment_identity_in(
                    owners
                        .comments
                        .as_mut()
                        .expect("routine rollback owns comments"),
                    current,
                    prior.schema_for(undo.target.transaction),
                    prior.name_for(undo.target.transaction),
                    undo.target.transaction,
                );
            }
        }
    }

    pub(crate) fn drop_routine(
        &self,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<RoutineChange, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::CatalogRetirement,
            None,
        );
        let slot = self.validate_routine_edit_in(&owners.catalog, target, transaction)?;
        let routine = owners.catalog[slot];
        let version = self.catalog_sequence.next()?;
        let oid = routine_oid(&routine) as u32;
        self.stage_catalog_comment_drop(
            owners
                .comments
                .as_mut()
                .expect("routine retirement owns comments"),
            target.source,
            transaction,
            |comment| {
                comment.used
                    && comment.database == Some(routine.database)
                    && comment.class == CommentClass::Routine
                    && comment.subid == oid
            },
        )?;
        owners.catalog[slot].ddl_state = routine.ddl_state.drop_by(transaction);
        owners.catalog[slot].ddl_version = version;
        Ok(RoutineChange::Lifecycle(CatalogDdlUndo {
            source: target.source,
            transaction,
            operation: CatalogDdlOperation::Drop,
            version,
            prior_version: routine.ddl_version,
        }))
    }

    fn routine_ddl_slot(
        &self,
        routines: &FixedVec<RoutineDef>,
        undo: CatalogDdlUndo,
    ) -> Option<usize> {
        let CatalogObjectSlot::Routine(slot) = undo.source.slot() else {
            return None;
        };
        let routine = routines.get(slot)?;
        if !undo.source.matches(
            self,
            routine.database,
            routine.created_at,
            routine.ddl_state,
        ) {
            return None;
        }
        let matched = match undo.operation {
            CatalogDdlOperation::Create => matches!(routine.ddl_state,
                CatalogDdlState::PendingCreate { txid } | CatalogDdlState::PendingCreateDrop { txid }
                    if txid == undo.transaction),
            CatalogDdlOperation::Drop => {
                routine.ddl_state.pending_drop_txid() == Some(undo.transaction)
                    && routine.ddl_version == undo.version
            }
        };
        matched.then_some(slot)
    }

    fn finish_routine_ddl(&self, undo: CatalogDdlUndo, decision: CatalogDdlDecision) {
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::CatalogRetirement,
            None,
        );
        let Some(slot) = self.routine_ddl_slot(&owners.catalog, undo) else {
            return;
        };
        match (undo.operation, decision) {
            (CatalogDdlOperation::Create, CatalogDdlDecision::Commit) => {
                if matches!(
                    owners.catalog[slot].ddl_state,
                    CatalogDdlState::PendingCreateDrop { .. }
                ) {
                    return;
                }
                self.publish_routine_edit_in(&mut owners, slot, undo.transaction);
                let routine = &mut owners.catalog[slot];
                routine.ddl_state = routine.ddl_state.commit_create();
                routine.ownership = routine.ownership.committed();
            }
            (CatalogDdlOperation::Drop, CatalogDdlDecision::Rollback) => {
                self.finish_catalog_comment_drop(
                    owners
                        .comments
                        .as_mut()
                        .expect("routine rollback owns comments"),
                    undo.source,
                    undo.transaction,
                    decision,
                );
                owners.catalog[slot].ddl_state = owners.catalog[slot]
                    .ddl_state
                    .rollback_drop(undo.transaction);
                owners.catalog[slot].ddl_version = undo.prior_version;
            }
            (CatalogDdlOperation::Create, CatalogDdlDecision::Rollback)
            | (CatalogDdlOperation::Drop, CatalogDdlDecision::Commit) => {
                let routine = owners.catalog[slot];
                let oid = routine_oid(&routine);
                for comment in owners
                    .comments
                    .as_mut()
                    .expect("routine retirement owns comments")
                    .entries
                    .iter_mut()
                    .filter(|comment| {
                        comment.used
                            && comment.database == Some(routine.database)
                            && comment.class == CommentClass::Routine
                            && comment.subid == oid as u32
                    })
                {
                    *comment = CommentEntry::empty();
                }
                let mut next = routine
                    .pending_definition
                    .map(|pending| pending.dependency_slot);
                while let Some(slot) = next {
                    next = owners.dependencies.pending[slot as usize].previous;
                    owners.dependencies.pending[slot as usize] =
                        PendingStoredQueryDependencies::EMPTY;
                }
                let image = self
                    .committed_dependency_image(StoredQueryDependencyOwner::Routine(slot as u16));
                owners.dependencies.counts[image] = 0;
                let object = Self::routine_access_object(slot);
                Self::clear_object_acl_entries_locked(
                    owners
                        .permissions
                        .as_mut()
                        .expect("routine retirement owns permissions"),
                    object,
                );
                Self::clear_extension_dependencies_for_object_locked(
                    owners
                        .extensions
                        .as_mut()
                        .expect("routine retirement owns extension references"),
                    object,
                );
                owners.catalog[slot] = RoutineDef::EMPTY;
                drop(owners);
                self.reset_function_statistics(oid);
            }
        }
    }

    pub(crate) fn validate_routine_change(
        &self,
        change: RoutineChange,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let routines = self.routines.lock().expect("routine catalog lock poisoned");
        let valid = match change {
            RoutineChange::Lifecycle(undo) => {
                undo.transaction == transaction && self.routine_ddl_slot(&routines, undo).is_some()
            }
            RoutineChange::Edit(undo) => {
                undo.target.transaction == transaction
                    && routines.get(undo.target.slot()).is_some_and(|routine| {
                        undo.target.source.matches(
                            self,
                            routine.database,
                            routine.created_at,
                            routine.ddl_state,
                        ) && Self::routine_edit_owner(routine) == Some(transaction)
                            && routine.revision_for(transaction) >= undo.target.version
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "routine change no longer belongs to its selected source"
            ))
        }
    }

    pub(crate) fn finish_routine_change(
        &self,
        change: RoutineChange,
        decision: CatalogDdlDecision,
    ) {
        match change {
            RoutineChange::Lifecycle(undo) => self.finish_routine_ddl(undo, decision),
            RoutineChange::Edit(undo) => self.finish_routine_edit(undo, decision),
        }
    }
}

impl Storage {
    pub(crate) fn create_routine(
        &mut self,
        spec: RoutineSpec,
        txid: u32,
    ) -> Result<RoutineChange, SqlError> {
        let RoutineSpec {
            identity,
            schema,
            name,
            mut arguments,
            argument_count,
            mut parameters,
            parameter_count,
            mut kind,
            mut result_columns,
            result_column_count,
            language,
            attributes,
            configs,
            config_count,
            body_kind,
            body,
            creation_path,
            dependencies,
        } = spec;
        if config_count > configs.len()
            || configs[..config_count]
                .iter()
                .enumerate()
                .any(|(index, config)| {
                    configs[..index]
                        .iter()
                        .any(|prior| prior.name == config.name)
                })
        {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "routine configuration contract is invalid"
            ));
        }
        if parameter_count > parameters.len()
            || parameters[..parameter_count]
                .iter()
                .filter(|parameter| parameter.mode.is_input())
                .count()
                != argument_count
        {
            return Err(sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "routine parameter contract does not match its input signature"
            ));
        }
        let output_parameter_count = parameters[..parameter_count]
            .iter()
            .filter(|parameter| parameter.mode.is_output())
            .count();
        let mut saw_default = false;
        let mut saw_variadic = false;
        for parameter in &parameters[..parameter_count] {
            if saw_variadic && !matches!(parameter.mode, RoutineParameterMode::Out) {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "VARIADIC parameter must be the last input parameter"
                ));
            }
            if parameter.mode.is_input() {
                if parameter.mode.default().is_some() {
                    saw_default = true;
                } else if saw_default {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "input parameters after one with a default value must also have defaults"
                    ));
                }
            }
            saw_variadic |= matches!(parameter.mode, RoutineParameterMode::Variadic { .. });
        }
        match kind {
            RoutineKind::TableFunction => {
                if result_column_count == 0 || result_column_count > result_columns.len() {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "record-returning function must have between one and {} result columns",
                        result_columns.len()
                    ));
                }
                if result_columns[..result_column_count]
                    .iter()
                    .enumerate()
                    .any(|(index, column)| {
                        result_columns[..index]
                            .iter()
                            .any(|prior| prior.name == column.name)
                    })
                {
                    return Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "routine result column names must be distinct"
                    ));
                }
            }
            RoutineKind::RecordFunction { .. }
                if result_column_count < 2
                    || result_column_count != output_parameter_count
                    || result_column_count > result_columns.len() =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "record function result contract does not match its output parameters"
                ));
            }
            RoutineKind::RecordFunction { .. } => {}
            RoutineKind::Function { .. } | RoutineKind::SetFunction { .. }
                if result_column_count != 0
                    && (result_column_count != 1 || output_parameter_count != 1) =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "scalar function result contract does not match its output parameter"
                ));
            }
            RoutineKind::Function { .. } | RoutineKind::SetFunction { .. } => {}
            RoutineKind::Trigger
            | RoutineKind::EventTrigger
            | RoutineKind::Procedure
            | RoutineKind::Aggregate(_)
                if result_column_count != 0 =>
            {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "routine kind cannot define result columns"
                ));
            }
            RoutineKind::Trigger
            | RoutineKind::EventTrigger
            | RoutineKind::Procedure
            | RoutineKind::Aggregate(_) => {}
        }
        type_contracts::routine_type_fields(
            &mut arguments[..argument_count],
            &mut parameters[..parameter_count],
            &mut result_columns[..result_column_count],
            &mut kind,
            &mut |result| {
                *result = self.bind_routine_result(*result, txid)?;
                Ok::<(), SqlError>(())
            },
        )?;
        self.require_schema_create(schema.as_str(), txid)?;
        let allocated_ownership = if matches!(identity, RoutineIdentity::Allocate) {
            Some(self.initial_ownership(txid)?)
        } else {
            None
        };
        let mut owners = self.catalog_dependency_owners(
            &self.routines,
            type_contracts::CatalogPublicationInput::TypedCatalogCreation,
            None,
        );
        owners.bind_routine_contract(
            &mut arguments[..argument_count],
            &mut parameters[..parameter_count],
            &mut result_columns[..result_column_count],
            &mut kind,
            txid,
        )?;
        owners.validate_dependencies(dependencies.view(), txid)?;
        let routines = &mut owners.catalog;
        if let Some(blocker) = routines.iter().find_map(|routine| {
            (routine.database == current_database()
                && routine.schema_for(txid) == schema
                && routine.name_for(txid) == name
                && routine.argument_count == argument_count
                && routine.arguments()[..argument_count]
                    .iter()
                    .zip(&arguments[..argument_count])
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    }))
            .then_some(routine.ddl_state.pending_txid()?)
            .filter(|&owner| owner != txid)
        }) {
            drop(owners);
            return Err(self.catalog_ddl_wait_error(txid, blocker, name.as_str()));
        }
        if routines.iter().any(|routine| {
            routine.database == current_database()
                && routine.visible_to(txid)
                && routine.schema_for(txid) == schema
                && routine.name_for(txid) == name
                && routine.argument_count == argument_count
                && routine.arguments()[..argument_count]
                    .iter()
                    .zip(&arguments[..argument_count])
                    .all(|(left, right)| {
                        left.ctype == right.ctype && left.user_type == right.user_type
                    })
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_FUNCTION,
                "function \"{}\" already exists with same argument types",
                name.as_str()
            ));
        }
        let Some(slot) = routines
            .iter()
            .position(|routine| routine.ddl_state == CatalogDdlState::Absent)
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many routines (limit {})",
                routines.len()
            ));
        };
        let (created_at, ownership) = match identity {
            RoutineIdentity::Allocate => (
                self.catalog_sequence.next()?,
                allocated_ownership.expect("allocated routine ownership was prepared"),
            ),
            RoutineIdentity::Preserve {
                created_at,
                ownership,
            } => {
                self.catalog_sequence.observe(created_at);
                (created_at, ownership)
            }
        };
        owners.write_query_image(
            self,
            self.committed_dependency_image(StoredQueryDependencyOwner::Routine(slot as u16)),
            dependencies.view(),
            txid,
        )?;
        owners.catalog[slot] = RoutineDef {
            database: current_database(),
            created_at,
            version: created_at,
            ddl_version: created_at,
            schema,
            name,
            pending_identity: None,
            pending_definition: None,
            arguments,
            argument_count,
            parameters,
            parameter_count,
            kind,
            result_columns,
            result_column_count,
            language,
            attributes,
            configs,
            config_count,
            body_kind,
            body,
            creation_path,
            ownership,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Self::clear_object_acl_entries_locked(
            owners
                .permissions
                .as_mut()
                .expect("routine creation owns permissions"),
            Self::routine_access_object(slot),
        );
        Ok(RoutineChange::Lifecycle(CatalogDdlUndo {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Routine(slot),
                current_database(),
                created_at,
            ),
            transaction: txid,
            operation: CatalogDdlOperation::Create,
            version: created_at,
            prior_version: 0,
        }))
    }
}
