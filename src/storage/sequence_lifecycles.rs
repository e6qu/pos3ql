//! Sequence edits retain their selected object and definition revision.

use super::catalog_sources::{
    CatalogDdlDecision, CatalogDdlOperation, CatalogDdlUndo, CatalogEditTarget, CatalogObjectSlot,
};
use super::type_contracts::CatalogPublicationInput;
use super::{
    AccessClass, AccessObject, CatalogDdlState, CommentClass, PendingSequenceDefinition,
    SequenceAlteration, SequenceCatalog, SequenceCreateSpec, SequenceDef, SequenceValue,
    SequenceValues, SqlError, Storage, current_database, sqlstate,
};
use crate::sql_err;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SequenceAlterUndo {
    pub(super) target: CatalogEditTarget,
    pub(super) prior: Option<PendingSequenceDefinition>,
}

impl SequenceAlterUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

// Undo stays inline in the startup-reserved transaction pool.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum SequenceChange {
    Lifecycle(CatalogDdlUndo),
    Definition(SequenceAlterUndo),
}

impl SequenceChange {
    pub(crate) fn slot(self) -> usize {
        match self {
            Self::Lifecycle(undo) => undo.slot(),
            Self::Definition(undo) => undo.slot(),
        }
    }

    pub(crate) fn is_creation(self) -> bool {
        matches!(self, Self::Lifecycle(undo) if undo.operation == CatalogDdlOperation::Create)
    }

    pub(crate) fn is_drop(self) -> bool {
        matches!(self, Self::Lifecycle(undo) if undo.operation == CatalogDdlOperation::Drop)
    }
}

impl Storage {
    pub(crate) fn sequence_position(&self, slot: usize) -> super::CatalogPosition {
        let sequence = self.sequence(slot);
        super::CatalogPosition::new(slot, sequence.created_at)
            .expect("sequence position selects a live object")
    }

    pub(crate) fn sequence_replay_slot(
        &self,
        position: super::CatalogPosition,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let catalog = self.sequence_catalog();
        let valid = catalog
            .definitions
            .get(position.slot())
            .is_some_and(|sequence| {
                let definition = sequence.definition_for(transaction);
                sequence.database == current_database()
                    && sequence.visible_to(transaction)
                    && sequence.created_at == position.created_at()
                    && definition.schema.as_str() == schema
                    && definition.name.as_str() == name
            });
        if valid {
            Ok(position.slot())
        } else {
            Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal sequence source identity or name does not match"
            ))
        }
    }

    pub(crate) fn replay_sequence_definition(
        &mut self,
        create: SequenceCreateSpec,
        restart: Option<i64>,
        transaction: u32,
    ) -> Result<SequenceChange, SqlError> {
        let super::CatalogAllocation::Restore(position) = create.allocation else {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "sequence replay requires a durable catalog identity"
            ));
        };
        let existing = self
            .sequence_catalog()
            .definitions
            .get(position.slot())
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "saved sequence position exceeds configured capacity"
                )
            })?;
        if existing.ddl_state == CatalogDdlState::Absent {
            if restart.is_some() {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "journal restarts an absent sequence"
                ));
            }
            return self.create_sequence(create, transaction);
        }
        let slot = self.sequence_replay_slot(
            position,
            create.schema.as_str(),
            create.name.as_str(),
            transaction,
        )?;
        let target = self.prepare_sequence_edit(slot, transaction)?;
        self.stage_sequence_alter(
            target,
            SequenceAlteration {
                schema: create.schema,
                name: create.name,
                spec: create.spec,
                owner: create.owner,
                generator_for: create.generator_for,
                persistence: create.persistence,
                restart,
            },
            transaction,
        )
    }

    pub(crate) fn prepare_sequence_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<CatalogEditTarget, SqlError> {
        let catalog = self.sequence_catalog();
        let sequence = catalog
            .definitions
            .get(slot)
            .filter(|sequence| {
                sequence.database == current_database() && sequence.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "sequence edit source no longer exists"
                )
            })?;
        if let Some(owner) = sequence
            .pending_definition
            .map(|pending| pending.txid)
            .or_else(|| sequence.ddl_state.pending_txid())
            .filter(|owner| *owner != transaction)
        {
            let name = sequence.name;
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        Ok(CatalogEditTarget {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Sequence(slot),
                sequence.database,
                sequence.created_at,
            ),
            transaction,
            version: sequence
                .pending_definition
                .filter(|pending| pending.txid == transaction)
                .map_or(sequence.version, |pending| pending.version),
        })
    }

    fn validate_sequence_edit(
        &self,
        catalog: &SequenceCatalog,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let slot = target.sequence_slot()?;
        let sequence = catalog
            .definitions
            .get(slot)
            .filter(|sequence| {
                target.source.matches(
                    self,
                    sequence.database,
                    sequence.created_at,
                    sequence.ddl_state,
                ) && sequence.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "sequence edit source has been replaced"
                )
            })?;
        if target.transaction != transaction {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "sequence edit belongs to another transaction"
            ));
        }
        if let Some(pending) = sequence
            .pending_definition
            .filter(|pending| pending.txid != transaction)
        {
            return Err(self.catalog_ddl_wait_error(
                transaction,
                pending.txid,
                sequence.name.as_str(),
            ));
        }
        let version = sequence
            .pending_definition
            .map_or(sequence.version, |pending| pending.version);
        if version != target.version {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "sequence definition changed after edit preparation"
            ));
        }
        Ok(slot)
    }

    pub(crate) fn stage_sequence_alter(
        &self,
        target: CatalogEditTarget,
        alteration: SequenceAlteration,
        transaction: u32,
    ) -> Result<SequenceChange, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::MetadataCommit,
            None,
        );
        let slot = self.validate_sequence_edit(&owners.catalog, target, transaction)?;
        let version = self.catalog_sequence.next()?;
        let current_identity = owners.catalog.definitions[slot].definition_for(transaction);
        let pending = owners.catalog.definitions[slot].pending_definition;
        let sequence_values = &mut owners.catalog.values[slot];
        let prior = pending.map(|pending| PendingSequenceDefinition {
            last_value: sequence_values.pending.last_value,
            is_called: sequence_values.pending.is_called,
            log_count: sequence_values.pending.log_count,
            ..pending
        });
        let active = if pending.is_some() {
            sequence_values.pending
        } else {
            sequence_values.committed
        };
        let (last_value, is_called) = alteration
            .restart
            .map_or((active.last_value, active.is_called), |value| {
                (value, false)
            });
        let restarted =
            pending.is_some_and(|pending| pending.restarted) || alteration.restart.is_some();
        sequence_values.pending = SequenceValue {
            last_value,
            is_called,
            log_count: 0,
            dirty: alteration.restart.is_some(),
            generation: sequence_values.pending.generation.wrapping_add(1),
            staged_generation: 0,
        };
        owners.catalog.definitions[slot].pending_definition = Some(PendingSequenceDefinition {
            txid: transaction,
            version,
            schema: alteration.schema,
            name: alteration.name,
            spec: alteration.spec,
            owner: alteration.owner,
            generator_for: alteration.generator_for,
            persistence: alteration.persistence,
            last_value,
            is_called,
            log_count: 0,
            restarted,
        });
        if current_identity.schema != alteration.schema || current_identity.name != alteration.name
        {
            self.stage_object_comment_identity_locked(
                owners
                    .comments
                    .as_mut()
                    .expect("sequence edits own comments"),
                CommentClass::Relation,
                (current_identity.schema, current_identity.name),
                (alteration.schema, alteration.name),
                transaction,
            );
        }

        Ok(SequenceChange::Definition(SequenceAlterUndo {
            target: CatalogEditTarget { version, ..target },
            prior,
        }))
    }

    fn sequence_alter_slot(
        &self,
        catalog: &SequenceCatalog,
        undo: SequenceAlterUndo,
    ) -> Option<usize> {
        let slot = undo.target.sequence_slot().ok()?;
        let sequence = catalog.definitions.get(slot)?;
        (undo.target.source.matches(
            self,
            sequence.database,
            sequence.created_at,
            sequence.ddl_state,
        ) && sequence.pending_definition.is_some_and(|pending| {
            pending.txid == undo.target.transaction && pending.version == undo.target.version
        }))
        .then_some(slot)
    }

    fn commit_sequence_alter(&self, undo: SequenceAlterUndo) {
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(slot) = self.sequence_alter_slot(&owners.catalog, undo) else {
            return;
        };
        self.publish_sequence_definition_in(&mut owners, slot, undo.target.transaction);
    }

    fn publish_sequence_definition_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, SequenceCatalog>,
        slot: usize,
        transaction: u32,
    ) {
        let old = owners.catalog.definitions[slot];
        let definition = old.definition_for(transaction);
        owners.catalog.definitions[slot] = definition;
        let values = &mut owners.catalog.values[slot];
        values.committed = values.pending;
        values.committed.dirty = false;
        values.committed.staged_generation = 0;
        values.pending.dirty = false;
        values.pending.staged_generation = 0;
        self.commit_object_comment_identity_locked(
            owners
                .comments
                .as_mut()
                .expect("sequence publication owns comments"),
            CommentClass::Relation,
            old.schema,
            old.name,
            transaction,
        );
        if old.schema != definition.schema || old.name != definition.name {
            self.rename_stored_query_dependency_locked(
                &mut owners.dependencies,
                super::DependencyClass::Sequence,
                slot,
                None,
                definition.schema,
                definition.name,
            );
        }
    }

    fn rollback_sequence_alter(&self, undo: SequenceAlterUndo) {
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(slot) = self.sequence_alter_slot(&owners.catalog, undo) else {
            return;
        };
        let current = owners.catalog.definitions[slot]
            .pending_definition
            .expect("matched pending edit");
        owners.catalog.definitions[slot].pending_definition = undo.prior;
        let values = &mut owners.catalog.values[slot];
        // RESTART owns a temporary value image; ordinary advances survive rollback.
        if !current.restarted {
            values.committed = values.pending;
        }
        if let Some(prior) = undo.prior {
            // An ordinary edit must carry issued values through every earlier
            // edit in the rollback chain. RESTART restores its saved image.
            let (last_value, is_called, log_count) = if current.restarted {
                (prior.last_value, prior.is_called, prior.log_count)
            } else {
                (
                    values.pending.last_value,
                    values.pending.is_called,
                    values.pending.log_count,
                )
            };
            values.pending = SequenceValue {
                last_value,
                is_called,
                log_count,
                dirty: false,
                generation: values.pending.generation.wrapping_add(1),
                staged_generation: 0,
            };
        } else {
            values.pending.dirty = false;
        }
        let visible = owners.catalog.definitions[slot].definition_for(undo.target.transaction);
        if current.schema != visible.schema || current.name != visible.name {
            self.stage_object_comment_identity_locked(
                owners
                    .comments
                    .as_mut()
                    .expect("sequence rollback owns comments"),
                CommentClass::Relation,
                (current.schema, current.name),
                (visible.schema, visible.name),
                undo.target.transaction,
            );
        }
    }

    pub(crate) fn create_sequence(
        &mut self,
        create: SequenceCreateSpec,
        transaction: u32,
    ) -> Result<SequenceChange, SqlError> {
        let SequenceCreateSpec {
            allocation,
            schema,
            name,
            spec,
            owner,
            generator_for,
            persistence,
        } = create;
        self.require_schema_create(schema.as_str(), transaction)?;
        let ownership = self.initial_ownership(transaction)?;
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::TypedCatalogCreation,
            None,
        );
        self.require_row_type_name(
            owners
                .types
                .as_ref()
                .expect("sequence creation owns type names"),
            schema,
            name,
            transaction,
            None,
            None,
        )?;
        for sequence in owners.catalog.definitions.iter().filter(|sequence| {
            sequence.database == current_database() && sequence.ddl_state != CatalogDdlState::Absent
        }) {
            let pending = sequence.pending_definition;
            let name_matches = (sequence.schema == schema && sequence.name == name)
                || pending.is_some_and(|pending| pending.schema == schema && pending.name == name);
            if name_matches {
                if let Some(blocker) = sequence
                    .ddl_state
                    .pending_txid()
                    .or_else(|| pending.map(|pending| pending.txid))
                    .filter(|blocker| *blocker != transaction)
                {
                    return Err(self.catalog_ddl_wait_error(transaction, blocker, name.as_str()));
                }
                let visible = sequence.definition_for(transaction);
                if sequence.visible_to(transaction)
                    && visible.schema == schema
                    && visible.name == name
                {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_TABLE,
                        "relation \"{}\" already exists",
                        name.as_str()
                    ));
                }
            }
        }
        let (slot, created_at) = match allocation {
            super::CatalogAllocation::New => {
                let slot = owners
                    .catalog
                    .definitions
                    .iter()
                    .position(|sequence| sequence.ddl_state == CatalogDdlState::Absent)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "too many sequences (limit {})",
                            owners.catalog.definitions.len()
                        )
                    })?;
                (
                    slot,
                    self.catalog_sequence
                        .next_bounded(super::MAX_RELATION_GENERATION, "relation")?,
                )
            }
            super::CatalogAllocation::Restore(position) => {
                super::RelationIdentity::parse(position.created_at()).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "saved sequence has an invalid relation identity"
                    )
                })?;
                if owners
                    .catalog
                    .definitions
                    .get(position.slot())
                    .is_none_or(|sequence| sequence.ddl_state != CatalogDdlState::Absent)
                    || owners.catalog.definitions.iter().any(|sequence| {
                        sequence.database == current_database()
                            && sequence.ddl_state != CatalogDdlState::Absent
                            && sequence.created_at == position.created_at()
                    })
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "saved sequence position is unavailable or its creation identity is already used"
                    ));
                }
                self.catalog_sequence.observe(position.created_at());
                (position.slot(), position.created_at())
            }
        };
        Self::clear_object_acl_entries_locked(
            owners
                .permissions
                .as_mut()
                .expect("sequence creation owns permissions"),
            AccessObject {
                class: AccessClass::Sequence,
                slot: slot as u16,
            },
        );
        owners.catalog.definitions[slot] = SequenceDef {
            database: current_database(),
            created_at,
            version: created_at,
            ddl_version: created_at,
            cache_generation: 0,
            schema,
            name,
            ownership,
            data_type: spec.data_type,
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
            persistence,
            owner,
            generator_for,
            pending_definition: None,
            ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
        };
        owners.catalog.values[slot] = SequenceValues::initial(spec.start_value);
        let undo = CatalogDdlUndo {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Sequence(slot),
                current_database(),
                created_at,
            ),
            transaction,
            operation: CatalogDdlOperation::Create,
            version: created_at,
            prior_version: 0,
        };
        drop(owners);
        if persistence == super::RelationPersistence::Temporary {
            self.mark_temporary_transaction(transaction);
        }
        Ok(SequenceChange::Lifecycle(undo))
    }

    pub(crate) fn drop_sequence(
        &self,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<Option<SequenceChange>, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::CatalogRetirement,
            None,
        );
        for sequence in owners.catalog.definitions.iter().filter(|sequence| {
            sequence.database == current_database() && sequence.ddl_state != CatalogDdlState::Absent
        }) {
            let visible = sequence.definition_for(transaction);
            if visible.schema.as_str() == schema
                && visible.name.as_str() == name
                && let Some(blocker) = sequence
                    .ddl_state
                    .pending_txid()
                    .or_else(|| sequence.pending_definition.map(|pending| pending.txid))
                    .filter(|blocker| *blocker != transaction)
            {
                return Err(self.catalog_ddl_wait_error(transaction, blocker, name));
            }
        }
        let Some(slot) = owners.catalog.definitions.iter().position(|sequence| {
            let visible = sequence.definition_for(transaction);
            sequence.database == current_database()
                && sequence.visible_to(transaction)
                && visible.schema.as_str() == schema
                && visible.name.as_str() == name
        }) else {
            return Ok(None);
        };
        let sequence = owners.catalog.definitions[slot];
        let source = self.catalog_object_identity(
            CatalogObjectSlot::Sequence(slot),
            sequence.database,
            sequence.created_at,
        );
        let version = self.catalog_sequence.next()?;
        self.stage_catalog_comment_drop(
            owners
                .comments
                .as_mut()
                .expect("sequence retirement owns comments"),
            source,
            transaction,
            |comment| {
                comment.matches_to(
                    Some(current_database()),
                    CommentClass::Relation,
                    schema,
                    name,
                    comment.subid,
                    transaction,
                )
            },
        )?;
        owners.catalog.definitions[slot].ddl_state = sequence.ddl_state.drop_by(transaction);
        owners.catalog.definitions[slot].ddl_version = version;
        Ok(Some(SequenceChange::Lifecycle(CatalogDdlUndo {
            source,
            transaction,
            operation: CatalogDdlOperation::Drop,
            version,
            prior_version: sequence.ddl_version,
        })))
    }

    fn sequence_ddl_slot(&self, catalog: &SequenceCatalog, undo: CatalogDdlUndo) -> Option<usize> {
        let CatalogObjectSlot::Sequence(slot) = undo.source.slot() else {
            return None;
        };
        let sequence = catalog.definitions.get(slot)?;
        if !undo.source.matches(
            self,
            sequence.database,
            sequence.created_at,
            sequence.ddl_state,
        ) {
            return None;
        }
        let matches = match undo.operation {
            CatalogDdlOperation::Create => matches!(sequence.ddl_state,
                CatalogDdlState::PendingCreate { txid } | CatalogDdlState::PendingCreateDrop { txid }
                    if txid == undo.transaction),
            CatalogDdlOperation::Drop => {
                sequence.ddl_state.pending_drop_txid() == Some(undo.transaction)
                    && sequence.ddl_version == undo.version
            }
        };
        matches.then_some(slot)
    }

    fn finish_sequence_ddl(&self, undo: CatalogDdlUndo, decision: CatalogDdlDecision) {
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::CatalogRetirement,
            None,
        );
        let Some(slot) = self.sequence_ddl_slot(&owners.catalog, undo) else {
            return;
        };
        match (undo.operation, decision) {
            (CatalogDdlOperation::Create, CatalogDdlDecision::Commit) => {
                if matches!(
                    owners.catalog.definitions[slot].ddl_state,
                    CatalogDdlState::PendingCreateDrop { .. }
                ) {
                    return;
                }
                if owners.catalog.definitions[slot]
                    .pending_definition
                    .is_some_and(|pending| pending.txid == undo.transaction)
                {
                    self.publish_sequence_definition_in(&mut owners, slot, undo.transaction);
                }
                let sequence = &mut owners.catalog.definitions[slot];
                sequence.ddl_state = sequence.ddl_state.commit_create();
                if let Some(pending) = sequence
                    .ownership
                    .pending
                    .filter(|pending| pending.txid == undo.transaction)
                {
                    sequence.ownership.owner = pending.owner;
                    sequence.ownership.pending = None;
                }
            }
            (CatalogDdlOperation::Drop, CatalogDdlDecision::Rollback) => {
                self.finish_catalog_comment_drop(
                    owners
                        .comments
                        .as_mut()
                        .expect("sequence rollback owns comments"),
                    undo.source,
                    undo.transaction,
                    decision,
                );
                let sequence = &mut owners.catalog.definitions[slot];
                sequence.ddl_state = sequence.ddl_state.rollback_drop(undo.transaction);
                sequence.ddl_version = undo.prior_version;
            }
            (CatalogDdlOperation::Create, CatalogDdlDecision::Rollback)
            | (CatalogDdlOperation::Drop, CatalogDdlDecision::Commit) => {
                self.retire_sequence_in(&mut owners, slot, undo.transaction);
            }
        }
    }

    fn retire_sequence_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, SequenceCatalog>,
        slot: usize,
        transaction: u32,
    ) {
        let sequence = owners.catalog.definitions[slot];
        let source = self.catalog_object_identity(
            CatalogObjectSlot::Sequence(slot),
            sequence.database,
            sequence.created_at,
        );
        let definition = sequence.definition_for(transaction);
        for comment in owners
            .comments
            .as_mut()
            .expect("sequence retirement owns comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.pending_drop.map_or_else(
                    || {
                        sequence.ddl_state.pending_drop_txid().is_none()
                            && comment.matches_to(
                                Some(sequence.database),
                                CommentClass::Relation,
                                definition.schema.as_str(),
                                definition.name.as_str(),
                                comment.subid,
                                transaction,
                            )
                    },
                    |pending| pending.txid == transaction && pending.source == source,
                )
            })
        {
            *comment = super::CommentEntry::empty();
        }
        let object = AccessObject {
            class: AccessClass::Sequence,
            slot: slot as u16,
        };
        Self::clear_object_acl_entries_locked(
            owners
                .permissions
                .as_mut()
                .expect("sequence retirement owns permissions"),
            object,
        );
        Self::clear_extension_dependencies_for_object_locked(
            owners
                .extensions
                .as_mut()
                .expect("sequence retirement owns extension references"),
            object,
        );
        owners.catalog.definitions[slot] = SequenceDef::EMPTY;
        owners.catalog.values[slot] = SequenceValues::initial(1);
    }

    pub(super) fn retire_temporary_sequence_source(
        &self,
        source: super::catalog_sources::CatalogObjectIdentity,
    ) {
        let CatalogObjectSlot::Sequence(slot) = source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.sequence_catalog,
            CatalogPublicationInput::CatalogRetirement,
            None,
        );
        if owners
            .catalog
            .definitions
            .get(slot)
            .is_some_and(|sequence| {
                source.matches(
                    self,
                    sequence.database,
                    sequence.created_at,
                    sequence.ddl_state,
                ) && sequence.ddl_state == CatalogDdlState::Present
                    && sequence.persistence == super::RelationPersistence::Temporary
            })
        {
            self.retire_sequence_in(&mut owners, slot, 0);
        }
    }

    pub(crate) fn validate_sequence_change(
        &self,
        change: SequenceChange,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let catalog = self.sequence_catalog();
        let valid = match change {
            SequenceChange::Lifecycle(undo) => {
                undo.transaction == transaction && self.sequence_ddl_slot(&catalog, undo).is_some()
            }
            SequenceChange::Definition(undo) => {
                let slot = undo.target.sequence_slot()?;
                undo.target.transaction == transaction
                    && catalog.definitions.get(slot).is_some_and(|sequence| {
                        undo.target.source.matches(
                            self,
                            sequence.database,
                            sequence.created_at,
                            sequence.ddl_state,
                        ) && sequence.pending_definition.is_some_and(|pending| {
                            pending.txid == transaction && pending.version >= undo.target.version
                        })
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "sequence change no longer belongs to its selected source"
            ))
        }
    }

    pub(crate) fn finish_sequence_change(
        &self,
        change: SequenceChange,
        decision: CatalogDdlDecision,
    ) {
        match change {
            SequenceChange::Lifecycle(undo) => self.finish_sequence_ddl(undo, decision),
            SequenceChange::Definition(undo) => match decision {
                CatalogDdlDecision::Commit => self.commit_sequence_alter(undo),
                CatalogDdlDecision::Rollback => self.rollback_sequence_alter(undo),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};
    use crate::storage::{RelationPersistence, SeqSpec, SeqType, SqlName};

    fn fixture() -> Storage {
        let mut config = test_config();
        config.max_sequences = 1;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    fn create(storage: &mut Storage, name: &str, transaction: u32) -> SequenceChange {
        storage
            .create_sequence(
                SequenceCreateSpec {
                    allocation: crate::storage::CatalogAllocation::New,
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse(name).unwrap(),
                    spec: SeqSpec {
                        data_type: SeqType::Bigint,
                        increment: 1,
                        min_value: 1,
                        max_value: i64::MAX,
                        start_value: 1,
                        cache: 1,
                        cycle: false,
                    },
                    owner: None,
                    generator_for: None,
                    persistence: RelationPersistence::Permanent,
                },
                transaction,
            )
            .unwrap()
    }

    fn alteration(
        storage: &Storage,
        slot: usize,
        name: &str,
        transaction: u32,
    ) -> SequenceAlteration {
        let definition = storage.sequence_for(slot, transaction);
        SequenceAlteration {
            schema: definition.schema,
            name: SqlName::parse(name).unwrap(),
            spec: SeqSpec {
                data_type: definition.data_type,
                increment: definition.increment,
                min_value: definition.min_value,
                max_value: definition.max_value,
                start_value: definition.start_value,
                cache: definition.cache,
                cycle: definition.cycle,
            },
            owner: definition.owner,
            generator_for: definition.generator_for,
            persistence: definition.persistence,
            restart: None,
        }
    }

    #[test]
    fn sequence_lifecycle_stale_edits_and_finalizers_cannot_touch_reused_slots() {
        let mut storage = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let original = create(&mut storage, "original_sequence", 7);
            let selected = storage.prepare_sequence_edit(original.slot(), 7).unwrap();
            storage.finish_sequence_change(original, CatalogDdlDecision::Rollback);
            let replacement = create(&mut storage, "replacement_sequence", 9);
            assert_eq!(original.slot(), replacement.slot());
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_sequence_change(original, decision);
            }
            assert_eq!(
                storage
                    .stage_sequence_alter(
                        selected,
                        alteration(&storage, replacement.slot(), "stolen_name", 9),
                        7,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_TABLE
            );
            assert!(
                storage
                    .sequence_slot("public", "replacement_sequence", 9)
                    .is_some()
            );
            assert!(
                storage
                    .sequence_slot("public", "replacement_sequence", 0)
                    .is_none()
            );
            storage.finish_sequence_change(replacement, CatalogDdlDecision::Commit);
            let first = storage
                .drop_sequence("public", "replacement_sequence", 11)
                .unwrap()
                .unwrap();
            storage.finish_sequence_change(first, CatalogDdlDecision::Rollback);
            let second = storage
                .drop_sequence("public", "replacement_sequence", 11)
                .unwrap()
                .unwrap();
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_sequence_change(first, decision);
            }
            assert!(
                storage
                    .sequence_slot("public", "replacement_sequence", 11)
                    .is_none()
            );
            storage.finish_sequence_change(second, CatalogDdlDecision::Rollback);
            assert!(
                storage
                    .sequence_slot("public", "replacement_sequence", 0)
                    .is_some()
            );
        });
    }

    #[test]
    fn sequence_lifecycle_edits_follow_revision_and_savepoint_order() {
        let mut storage = fixture();
        let created = create(&mut storage, "original_sequence", 7);
        storage.finish_sequence_change(created, CatalogDdlDecision::Commit);
        crate::mem::guard::forbid_alloc(|| {
            let selected = storage.prepare_sequence_edit(created.slot(), 9).unwrap();
            let first = storage
                .stage_sequence_alter(
                    selected,
                    alteration(&storage, created.slot(), "first_sequence", 9),
                    9,
                )
                .unwrap();
            assert_eq!(
                storage
                    .stage_sequence_alter(
                        selected,
                        alteration(&storage, created.slot(), "stale_sequence", 9),
                        9,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let current = storage.prepare_sequence_edit(created.slot(), 9).unwrap();
            let second = storage
                .stage_sequence_alter(
                    current,
                    alteration(&storage, created.slot(), "second_sequence", 9),
                    9,
                )
                .unwrap();
            storage.finish_sequence_change(first, CatalogDdlDecision::Rollback);
            assert!(
                storage
                    .sequence_slot("public", "second_sequence", 9)
                    .is_some()
            );
            storage.finish_sequence_change(second, CatalogDdlDecision::Rollback);
            assert!(
                storage
                    .sequence_slot("public", "first_sequence", 9)
                    .is_some()
            );
            storage.finish_sequence_change(first, CatalogDdlDecision::Rollback);
            assert!(
                storage
                    .sequence_slot("public", "original_sequence", 9)
                    .is_some()
            );
            let current = storage.prepare_sequence_edit(created.slot(), 9).unwrap();
            let third = storage
                .stage_sequence_alter(
                    current,
                    alteration(&storage, created.slot(), "third_sequence", 9),
                    9,
                )
                .unwrap();
            for decision in [CatalogDdlDecision::Commit, CatalogDdlDecision::Rollback] {
                storage.finish_sequence_change(second, decision);
            }
            assert!(
                storage
                    .sequence_slot("public", "third_sequence", 9)
                    .is_some()
            );
            storage.finish_sequence_change(third, CatalogDdlDecision::Commit);
            assert!(
                storage
                    .sequence_slot("public", "third_sequence", 0)
                    .is_some()
            );
        });
    }

    #[test]
    fn sequence_lifecycle_source_rejects_cross_storage_and_transaction_reuse() {
        let mut storage = fixture();
        let mut other = fixture();
        let original = create(&mut storage, "source_sequence", 7);
        let replacement = create(&mut other, "other_sequence", 7);
        let selected = storage.prepare_sequence_edit(original.slot(), 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                other
                    .stage_sequence_alter(
                        selected,
                        alteration(&other, replacement.slot(), "stolen_name", 7),
                        7,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_TABLE
            );
            assert_eq!(
                storage
                    .stage_sequence_alter(
                        selected,
                        alteration(&storage, original.slot(), "stolen_name", 7),
                        9,
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_TABLE
            );
            other.finish_sequence_change(original, CatalogDdlDecision::Rollback);
            assert!(other.sequence_slot("public", "other_sequence", 7).is_some());
            assert!(
                storage
                    .sequence_slot("public", "source_sequence", 7)
                    .is_some()
            );
        });
    }
}
